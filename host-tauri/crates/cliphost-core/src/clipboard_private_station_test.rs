use std::ffi::c_void;

#[link(name = "user32")]
unsafe extern "system" {
    fn CreateWindowStationW(name: *const u16, flags: u32, access: u32, security: *const c_void) -> isize;
    fn GetProcessWindowStation() -> isize;
    fn SetProcessWindowStation(station: isize) -> i32;
    fn CloseWindowStation(station: isize) -> i32;
    fn GetThreadDesktop(thread: u32) -> isize;
    fn SetThreadDesktop(desktop: isize) -> i32;
    fn CreateDesktopW(name: *const u16, device: *const u16, mode: *const c_void, flags: u32, access: u32, security: *const c_void) -> isize;
    fn CloseDesktop(desktop: isize) -> i32;
    fn GetUserObjectInformationW(object: isize, index: i32, info: *mut c_void, size: u32, needed: *mut u32) -> i32;
}

pub(super) struct PrivateStation {
    original_station: isize,
    original_desktop: isize,
    station: isize,
    pub(super) desktop: isize,
    pub(super) name: String,
}

impl PrivateStation {
    /// Fail closed before a fixture touches the real Windows clipboard. The
    /// station must still be ours, non-interactive, and on this thread's desktop.
    pub(super) fn verify_current(&self) -> Result<(), String> {
        use windows::Win32::System::Threading::GetCurrentThreadId;
        #[repr(C)] struct Flags { inherit: i32, reserved: i32, flags: u32 }
        unsafe {
            let mut flags = Flags { inherit: 0, reserved: 0, flags: 0 };
            let mut needed = 0;
            if self.station == 0 || self.desktop == 0
                || GetProcessWindowStation() != self.station
                || GetThreadDesktop(GetCurrentThreadId()) != self.desktop
                || self.name.eq_ignore_ascii_case("WinSta0")
                || GetUserObjectInformationW(self.station, 1, (&mut flags as *mut Flags).cast(),
                    std::mem::size_of::<Flags>() as u32, &mut needed) == 0
                || flags.flags & 1 != 0 {
                return Err("Private clipboard isolation changed; refusing clipboard access".into());
            }
        }
        Ok(())
    }

    pub(super) fn create() -> Result<Self, String> {
        use windows::Win32::System::Threading::GetCurrentThreadId;
        unsafe {
            let mut station = Self {
                original_station: GetProcessWindowStation(),
                original_desktop: GetThreadDesktop(GetCurrentThreadId()),
                station: 0, desktop: 0, name: String::new(),
            };
            let name = wide(&format!("ClipHostRustProbe_{}", uuid::Uuid::new_v4().simple()));
            station.station = CreateWindowStationW(name.as_ptr(), 1, 0xF037F, std::ptr::null());
            if station.station == 0 {
                // Named stations require an administrator token. Preserve this
                // error: an existing unnamed station otherwise masks access
                // denied as ERROR_ALREADY_EXISTS and looks like a UUID clash.
                let named_error = std::io::Error::last_os_error();
                station.station = CreateWindowStationW(std::ptr::null(), 1, 0xF037F, std::ptr::null());
                if station.station == 0 {
                    return Err(format!("CreateWindowStation(create-only): named private station: {named_error}; unnamed fallback: {}",
                        std::io::Error::last_os_error()));
                }
            }
            let mut buffer = [0u16; 256];
            let mut needed = 0;
            if GetUserObjectInformationW(station.station, 2, buffer.as_mut_ptr().cast(), 512, &mut needed) == 0 {
                return Err("Cannot identify private window station".into());
            }
            station.name = String::from_utf16_lossy(&buffer[..buffer.iter().position(|value| *value == 0).unwrap_or(256)]);
            #[repr(C)] struct Flags { inherit: i32, reserved: i32, flags: u32 }
            let mut flags = Flags { inherit: 0, reserved: 0, flags: 0 };
            if station.name.eq_ignore_ascii_case("WinSta0")
                || GetUserObjectInformationW(station.station, 1, (&mut flags as *mut Flags).cast(), std::mem::size_of::<Flags>() as u32, &mut needed) == 0
                || flags.flags & 1 != 0 {
                return Err("Refusing an interactive/visible window station".into());
            }
            if SetProcessWindowStation(station.station) == 0 { return Err("SetProcessWindowStation failed".into()); }
            station.desktop = CreateDesktopW(wide("Default").as_ptr(), std::ptr::null(), std::ptr::null(), 0, 0xF01FF, std::ptr::null());
            if station.desktop == 0 { return Err("CreateDesktop failed".into()); }
            attach(station.desktop)?;
            Ok(station)
        }
    }
}

pub(super) fn attach(desktop: isize) -> Result<(), String> {
    if unsafe { SetThreadDesktop(desktop) } == 0 { Err(format!("SetThreadDesktop: {}", std::io::Error::last_os_error())) }
    else { Ok(()) }
}

fn wide(text: &str) -> Vec<u16> { text.encode_utf16().chain([0]).collect() }

impl Drop for PrivateStation {
    fn drop(&mut self) {
        unsafe {
            let _ = SetThreadDesktop(self.original_desktop);
            let _ = SetProcessWindowStation(self.original_station);
            if self.desktop != 0 { let _ = CloseDesktop(self.desktop); }
            if self.station != 0 { let _ = CloseWindowStation(self.station); }
        }
    }
}
