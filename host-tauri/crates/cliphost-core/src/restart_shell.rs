//! Obtain the desktop Explorer's automation object, rather than creating a
//! Shell.Application inside the elevated caller. Based on Microsoft's
//! ExecInExplorer sample; no window, task, UAC prompt or token is modified.

use windows::core::{Interface, VARIANT};
use windows::Win32::{
    Foundation::{CloseHandle, E_FAIL, HANDLE},
    Security::{
        EqualSid, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
        TokenElevation, TokenIntegrityLevel, TokenUser, TOKEN_ELEVATION,
        TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER,
    },
    System::{
        Com::{CoCreateInstance, IDispatch, IServiceProvider, CLSCTX_LOCAL_SERVER},
        Threading::{GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION},
    },
    UI::{
        Shell::{IShellBrowser, IShellDispatch2, IShellFolderViewDual, IShellWindows, ShellWindows,
            SID_STopLevelBrowser, SVGIO_BACKGROUND, SWC_DESKTOP, SWFO_NEEDDISPATCH},
        WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId},
    },
};

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) { unsafe { let _ = CloseHandle(self.0); } }
}

unsafe fn token_for(process: HANDLE) -> windows::core::Result<Handle> {
    let mut token = HANDLE::default();
    OpenProcessToken(process, TOKEN_QUERY, &mut token)?;
    Ok(Handle(token))
}

unsafe fn token_info(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> windows::core::Result<Vec<usize>> {
    let mut size = 0;
    let _ = GetTokenInformation(token, class, None, 0, &mut size);
    if size == 0 || size > 1024 * 1024 { return Err(windows::core::Error::new(E_FAIL, "Cannot size desktop token information")); }
    // Token structures contain pointers, so their buffer must be aligned.
    let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    GetTokenInformation(token, class, Some(buffer.as_mut_ptr().cast()), size, &mut size)?;
    Ok(buffer)
}

fn acceptable_desktop(elevated: bool, integrity: u32, same_user: bool) -> bool {
    !elevated && (0x2000..0x3000).contains(&integrity) && same_user
}

unsafe fn verify_desktop_token(pid: u32) -> windows::core::Result<()> {
    let process = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?);
    let token = token_for(process.0)?;
    let elevation = token_info(token.0, TokenElevation)?;
    let elevation = &*(elevation.as_ptr().cast::<TOKEN_ELEVATION>());
    let label_buffer = token_info(token.0, TokenIntegrityLevel)?;
    let label = &*(label_buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>());
    let count = *GetSidSubAuthorityCount(label.Label.Sid);
    if count == 0 { return Err(windows::core::Error::new(E_FAIL, "Desktop token has no integrity level")); }
    let integrity = *GetSidSubAuthority(label.Label.Sid, u32::from(count - 1));
    let desktop_user_buffer = token_info(token.0, TokenUser)?;
    let desktop_user = &*(desktop_user_buffer.as_ptr().cast::<TOKEN_USER>());
    let current_token = token_for(GetCurrentProcess())?;
    let current_user_buffer = token_info(current_token.0, TokenUser)?;
    let current_user = &*(current_user_buffer.as_ptr().cast::<TOKEN_USER>());
    let same_user = EqualSid(desktop_user.User.Sid, current_user.User.Sid).is_ok();
    if !acceptable_desktop(elevation.TokenIsElevated != 0, integrity, same_user) {
        return Err(windows::core::Error::new(E_FAIL, format!("Desktop token cannot satisfy ordinary restart: elevated={}, integrity={integrity:x}, same_user={same_user}", elevation.TokenIsElevated != 0)));
    }
    Ok(())
}

/// Caller owns a COM apartment for the lifetime of the returned interface.
pub(crate) unsafe fn desktop_dispatch() -> windows::core::Result<IShellDispatch2> {
    let shell_window = GetShellWindow();
    let mut pid = 0;
    if shell_window.0.is_null() || GetWindowThreadProcessId(shell_window, Some(&mut pid)) == 0 || pid == 0 {
        return Err(windows::core::Error::new(E_FAIL, "No desktop Shell window in this session"));
    }
    // UAC-disabled/elevated desktops and a different signed-in user cannot
    // satisfy this host's same-data ordinary-permission restart request.
    verify_desktop_token(pid).map_err(|error| windows::core::Error::new(error.code(), format!("Verify desktop token: {error}")))?;
    let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER)?;
    let empty = VARIANT::default();
    let mut found_window = 0;
    let desktop = windows.FindWindowSW(&empty, &empty, SWC_DESKTOP, &mut found_window, SWFO_NEEDDISPATCH)?;
    // FindWindowSW uses a LONG handle; compare its shared 32-bit representation.
    if found_window as u32 != shell_window.0 as usize as u32 {
        return Err(windows::core::Error::new(E_FAIL, format!("Desktop window mismatch: Shell={} automation={}", shell_window.0 as usize, found_window as u32)));
    }
    let provider = desktop.cast::<IServiceProvider>()
        .map_err(|error| windows::core::Error::new(error.code(), format!("Desktop service provider: {error}")))?;
    let browser = provider.QueryService::<IShellBrowser>(&SID_STopLevelBrowser)
        .map_err(|error| windows::core::Error::new(error.code(), format!("Desktop browser service: {error}")))?;
    let view = browser.QueryActiveShellView()
        .map_err(|error| windows::core::Error::new(error.code(), format!("Desktop active view: {error}")))?;
    // GetItemObject accepts the background dispatch interface. Requesting the
    // folder dual interface directly can return E_NOINTERFACE on Windows 11.
    let background = view.GetItemObject::<IDispatch>(SVGIO_BACKGROUND)
        .map_err(|error| windows::core::Error::new(error.code(), format!("Desktop background object: {error}")))?;
    let folder = background.cast::<IShellFolderViewDual>()
        .map_err(|error| windows::core::Error::new(error.code(), format!("Desktop folder automation: {error}")))?;
    let dispatch = folder.Application()?.cast::<IShellDispatch2>()
        .map_err(|error| windows::core::Error::new(error.code(), format!("Desktop Shell automation: {error}")))?;
    let mut current_pid = 0;
    if GetShellWindow() != shell_window || GetWindowThreadProcessId(shell_window, Some(&mut current_pid)) == 0 || current_pid != pid {
        return Err(windows::core::Error::new(E_FAIL, "Desktop Shell changed during restart lookup"));
    }
    Ok(dispatch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_desktop_requires_same_user_medium_non_elevated_token() {
        assert!(acceptable_desktop(false, 0x2000, true));
        assert!(acceptable_desktop(false, 0x2100, true));
        for (elevated, integrity, same_user) in [
            (true, 0x2000, true), (false, 0x3000, true),
            (false, 0x4000, true), (false, 0x1000, true),
            (false, 0x2000, false),
        ] { assert!(!acceptable_desktop(elevated, integrity, same_user)); }
    }
}
