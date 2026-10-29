//! A replacement waits for its exact predecessor before acquiring the Tauri
//! singleton or opening data. PID plus creation time avoids waiting on a reused PID.

pub(crate) const ARG_PREFIX: &str = "--cliphost-restart-parent=";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Parent {
    pid: u32,
    created: u64,
}

fn parse_parent(args: impl IntoIterator<Item = String>) -> Result<Option<Parent>, String> {
    let mut parent = None;
    for arg in args {
        let Some(value) = arg.strip_prefix(ARG_PREFIX) else { continue };
        if parent.is_some() {
            return Err("Duplicate restart parent argument.".into());
        }
        let parsed = value.split_once(':').and_then(|(pid, created)| {
            Some(Parent { pid: pid.parse().ok()?, created: created.parse().ok()? })
        }).filter(|value| value.pid > 0 && value.created > 0);
        parent = Some(parsed.ok_or("Invalid restart parent argument.")?);
    }
    Ok(parent)
}

pub(crate) fn replacement_arguments() -> Result<Vec<String>, String> {
    let mut args: Vec<_> = std::env::args().skip(1).filter(|arg| {
        !arg.starts_with(ARG_PREFIX)
            && !arg.starts_with(crate::restart_environment::ARG_PREFIX)
            && !arg.eq_ignore_ascii_case(crate::process_restart::DEMOTE_ATTEMPTED_ARG)
    }).collect();
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::GetCurrentProcess;
        let created = creation_time(unsafe { GetCurrentProcess() })?;
        args.push(format!("{ARG_PREFIX}{}:{created}", std::process::id()));
    }
    Ok(args)
}

pub(crate) fn wait_for_predecessor() -> Result<(), String> {
    if let Some(parent) = parse_parent(std::env::args().skip(1))? {
        #[cfg(windows)]
        wait_for_parent(parent, 30_000)?;
        #[cfg(not(windows))]
        let _ = parent;
    }
    Ok(())
}

#[cfg(windows)]
fn creation_time(handle: windows::Win32::Foundation::HANDLE) -> Result<u64, String> {
    use windows::Win32::{Foundation::FILETIME, System::Threading::GetProcessTimes};
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }
        .map_err(|error| format!("Cannot identify restart parent: {error}"))?;
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

#[cfg(windows)]
fn wait_for_parent(parent: Parent, timeout_ms: u32) -> Result<(), String> {
    use windows::Win32::{
        Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE},
    };
    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, false, parent.pid) } {
        Ok(handle) => handle,
        // The predecessor may already have exited before this child starts.
        Err(error) if error.code() == windows_core::HRESULT::from_win32(ERROR_INVALID_PARAMETER.0) => return Ok(()),
        Err(error) => return Err(format!("Cannot wait for restart parent: {error}")),
    };
    let result = (|| {
        if creation_time(handle)? != parent.created {
            return Ok(()); // Original process exited and Windows reused its PID.
        }
        match unsafe { WaitForSingleObject(handle, timeout_ms) } {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_TIMEOUT => Err("Previous ClipHost did not exit; replacement was not started.".into()),
            _ => Err(format!("Restart wait failed: {}", windows_core::Error::from_win32())),
        }
    })();
    unsafe { let _ = CloseHandle(handle); }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_parent_requires_one_valid_identity() {
        assert_eq!(parse_parent(["clip".into()]).unwrap(), None);
        assert_eq!(parse_parent([format!("{ARG_PREFIX}42:123")]).unwrap(), Some(Parent { pid: 42, created: 123 }));
        for value in ["", "42", "0:1", "1:0", "-1:42", "1:2:3", "word:4"] {
            assert!(parse_parent([format!("{ARG_PREFIX}{value}")]).is_err(), "{value}");
        }
        assert!(parse_parent([format!("{ARG_PREFIX}42:123"), format!("{ARG_PREFIX}42:123")]).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn restart_wait_observes_identity_and_never_terminates_a_live_parent() {
        use windows::Win32::System::Threading::GetCurrentProcess;
        let created = creation_time(unsafe { GetCurrentProcess() }).unwrap();
        let parent = Parent { pid: std::process::id(), created };
        assert!(wait_for_parent(parent, 0).unwrap_err().contains("did not exit"));
        assert!(wait_for_parent(Parent { created: created + 1, ..parent }, 0).is_ok());
        assert_eq!(creation_time(unsafe { GetCurrentProcess() }).unwrap(), created);
        let args = replacement_arguments().unwrap();
        assert_eq!(parse_parent(args).unwrap(), Some(parent));
    }
}
