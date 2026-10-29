use crate::Result;
use std::path::Path;

#[cfg(windows)]
mod native {
    use super::*;
    use crate::invalid;
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::{PCWSTR, PWSTR},
        Win32::{
            Foundation::{
                CloseHandle, FILETIME, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
            },
            System::Threading::{
                CreateMutexW, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW,
                ReleaseMutex, WaitForSingleObject, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            },
        },
    };

    pub(crate) struct Process(HANDLE);
    impl Process {
        pub(crate) fn open(
            pid: u32,
            expected_path: &Path,
            expected_creation: Option<u64>,
        ) -> Result<Self> {
            let handle = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    false,
                    pid,
                )
            }
            .map_err(|e| invalid(format!("无法确认原程序进程：{e}")))?;
            let process = Self(handle);
            let mut path = vec![0_u16; 32768];
            let mut length = path.len() as u32;
            unsafe {
                QueryFullProcessImageNameW(
                    handle,
                    PROCESS_NAME_WIN32,
                    PWSTR(path.as_mut_ptr()),
                    &mut length,
                )
            }
            .map_err(|e| invalid(format!("无法读取原程序路径：{e}")))?;
            let actual =
                std::path::PathBuf::from(String::from_utf16_lossy(&path[..length as usize]));
            if !same_path(&actual, expected_path)?
                || expected_creation.is_some_and(|v| process.creation_time().ok() != Some(v))
            {
                return Err(invalid("原程序PID身份或路径已改变"));
            }
            Ok(process)
        }
        pub(crate) fn creation_time(&self) -> Result<u64> {
            let mut creation = FILETIME::default();
            let mut exit = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            unsafe { GetProcessTimes(self.0, &mut creation, &mut exit, &mut kernel, &mut user) }
                .map_err(|e| invalid(format!("无法读取原程序创建时间：{e}")))?;
            Ok(((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64)
        }
        pub(crate) fn exited(&self) -> Result<bool> {
            let status = unsafe { WaitForSingleObject(self.0, 0) };
            if status == WAIT_OBJECT_0 {
                Ok(true)
            } else if status == WAIT_TIMEOUT {
                Ok(false)
            } else {
                Err(invalid("无法等待原程序退出"))
            }
        }
    }
    impl Drop for Process {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    // This is the exact named gate used by RustClipHostLauncher.RunPackage.
    // It serializes self-update, Quicker package sync and Quicker cold launch.
    pub(crate) struct InstallGate(HANDLE);
    impl InstallGate {
        pub(crate) fn acquire(root: &Path, fixture: bool) -> Result<Self> {
            use sha2::{Digest, Sha256};
            let mutex_name = if fixture {
                format!(
                    "Local\\EaClipUpdaterFixture_{:x}",
                    Sha256::digest(root.to_string_lossy().as_bytes())
                )
            } else {
                "Local\\ClipHostRust_Startup_28965".into()
            };
            let name: Vec<_> = mutex_name.encode_utf16().chain(Some(0)).collect();
            let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }
                .map_err(|e| invalid(format!("无法建立安装互斥锁：{e}")))?;
            let result = unsafe { WaitForSingleObject(handle, 3000) };
            if result != WAIT_OBJECT_0 && result != WAIT_ABANDONED {
                unsafe {
                    let _ = CloseHandle(handle);
                }
                return Err(invalid("另一个安装或启动操作正在进行，请稍后重试"));
            }
            Ok(Self(handle))
        }
    }
    impl Drop for InstallGate {
        fn drop(&mut self) {
            unsafe {
                let _ = ReleaseMutex(self.0);
                let _ = CloseHandle(self.0);
            }
        }
    }

    pub(crate) fn same_path(left: &Path, right: &Path) -> Result<bool> {
        Ok(std::fs::canonicalize(left)?
            .as_os_str()
            .encode_wide()
            .map(|c| if (65..=90).contains(&c) { c + 32 } else { c })
            .collect::<Vec<_>>()
            == std::fs::canonicalize(right)?
                .as_os_str()
                .encode_wide()
                .map(|c| if (65..=90).contains(&c) { c + 32 } else { c })
                .collect::<Vec<_>>())
    }

    pub(crate) fn publish_new_file(temporary: &Path, destination: &Path) -> Result<()> {
        use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVE_FILE_FLAGS};
        let from: Vec<_> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<_> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        // No REPLACE_EXISTING: signal publication is atomic and cannot replace
        // an existing session marker. Works on FAT portable volumes as well.
        unsafe {
            MoveFileExW(
                PCWSTR(from.as_ptr()),
                PCWSTR(to.as_ptr()),
                MOVE_FILE_FLAGS(0),
            )
        }
        .map_err(|e| invalid(format!("无法发布更新信号：{e}")))
    }
}
#[cfg(windows)]
pub(crate) use native::*;

#[cfg(not(windows))]
pub(crate) struct Process;
#[cfg(not(windows))]
impl Process {
    pub(crate) fn open(_: u32, _: &Path, _: Option<u64>) -> Result<Self> {
        Err(crate::invalid("内置安装仅支持Windows"))
    }
    pub(crate) fn creation_time(&self) -> Result<u64> {
        Err(crate::invalid("内置安装仅支持Windows"))
    }
    pub(crate) fn exited(&self) -> Result<bool> {
        Err(crate::invalid("内置安装仅支持Windows"))
    }
}
#[cfg(not(windows))]
pub(crate) struct InstallGate;
#[cfg(not(windows))]
impl InstallGate {
    pub(crate) fn acquire(_: &Path, _: bool) -> Result<Self> {
        Err(crate::invalid("内置安装仅支持Windows"))
    }
}
#[cfg(not(windows))]
pub(crate) fn same_path(a: &Path, b: &Path) -> Result<bool> {
    Ok(std::fs::canonicalize(a)? == std::fs::canonicalize(b)?)
}

#[cfg(not(windows))]
pub(crate) fn publish_new_file(temporary: &Path, destination: &Path) -> Result<()> {
    std::fs::hard_link(temporary, destination)?;
    std::fs::remove_file(temporary)?;
    Ok(())
}
