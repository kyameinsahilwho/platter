//! Small helpers shared by every module: wide strings, PowerShell, elevation.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// UTF-16, NUL-terminated.
pub fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Run a PowerShell script hidden and return stdout. Errors carry stderr.
pub fn powershell(script: &str) -> Result<String, String> {
    let full = format!(
        "$ProgressPreference='SilentlyContinue';[Console]::OutputEncoding=[Text.Encoding]::UTF8;{script}"
    );
    // Absolute path: when elevated, a bare name would be searched in the app's own folder first.
    let exe = std::env::var_os("SystemRoot")
        .map(|r| std::path::PathBuf::from(r).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"));
    let out = Command::new(exe)
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &full])
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("Couldn't start PowerShell: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(clean_ps_error(if stderr.is_empty() { &stdout } else { &stderr }));
    }
    Ok(stdout)
}

/// PowerShell errors are noisy multi-line blobs; keep the first meaningful line.
pub fn clean_ps_error(s: &str) -> String {
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("At line") && !l.starts_with('+'))
        .unwrap_or("The operation failed.");
    let line = line.trim_start_matches("powershell.exe : ");
    if line.contains("Access denied") || line.contains("Access is denied") || line.contains("0x80070005") {
        return "Windows denied access. Restart Platter as administrator and try again.".into();
    }
    line.to_string()
}

pub fn is_elevated() -> bool {
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

/// Relaunch this executable through the UAC prompt. Returns true if the user accepted.
///
/// Blocks until the consent prompt is answered, so call it off the main thread. `owner` is the
/// HWND (as an integer) the prompt belongs to, or 0. The elevated copy gets `--wait-for-pid <our pid>`
/// so it holds off starting WebView2 until this instance has exited (see [`wait_for_parent_from_args`]).
pub fn relaunch_elevated(owner: usize) -> bool {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    #[link(name = "ole32")]
    extern "system" {
        fn CoInitializeEx(reserved: *const std::ffi::c_void, coinit: u32) -> i32;
        fn CoUninitialize();
    }
    const COINIT_APARTMENTTHREADED: u32 = 0x2;
    const COINIT_DISABLE_OLE1DDE: u32 = 0x4;

    let Ok(exe) = std::env::current_exe() else { return false };
    let exe = wide(&exe.to_string_lossy());
    let verb = wide("runas");
    let params = wide(&format!("{WAIT_FOR_PID_ARG} {}", std::process::id()));
    // ShellExecute may hand off to shell extensions, which expect COM on the calling thread.
    let hr = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    let r = unsafe {
        ShellExecuteW(
            owner as windows_sys::Win32::Foundation::HWND,
            verb.as_ptr(),
            exe.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            1, // SW_SHOWNORMAL
        )
    };
    if hr >= 0 {
        unsafe { CoUninitialize() };
    }
    (r as isize) > 32
}

pub const WAIT_FOR_PID_ARG: &str = "--wait-for-pid";

/// The pid passed with `--wait-for-pid <pid>`, if any.
pub fn wait_for_pid_arg<I: IntoIterator<Item = String>>(args: I) -> Option<u32> {
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == WAIT_FOR_PID_ARG {
            return it.next().and_then(|p| p.trim().parse().ok());
        }
    }
    None
}

/// When started by [`relaunch_elevated`], wait (up to 10 s) for the instance that launched us to exit
/// before WebView2 starts, so the two never run side by side.
pub fn wait_for_parent_from_args() {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
    const SYNCHRONIZE: u32 = 0x0010_0000;
    let Some(pid) = wait_for_pid_arg(std::env::args().skip(1)) else { return };
    if pid == 0 || pid == std::process::id() {
        return;
    }
    unsafe {
        let h = OpenProcess(SYNCHRONIZE, 0, pid);
        if !h.is_null() {
            WaitForSingleObject(h, 10_000);
            CloseHandle(h);
        }
    }
}

/// Stop Windows from showing "There is no disk in the drive" and similar critical-error boxes
/// while Platter polls removable and optical drives; the calls fail quietly instead.
pub fn suppress_critical_error_dialogs() {
    #[link(name = "kernel32")]
    extern "system" {
        fn SetErrorMode(mode: u32) -> u32;
    }
    const SEM_FAILCRITICALERRORS: u32 = 0x0001;
    const SEM_NOOPENFILEERRORBOX: u32 = 0x8000;
    let want = SEM_FAILCRITICALERRORS | SEM_NOOPENFILEERRORBOX;
    unsafe {
        let prev = SetErrorMode(want);
        SetErrorMode(prev | want);
    }
}

/// Whether Windows apps are set to the light theme (`AppsUseLightTheme`). `None` if the value is missing.
pub fn system_uses_light_theme() -> Option<bool> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let value = wide("AppsUseLightTheme");
    let mut data = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut std::ffi::c_void,
            &mut size,
        )
    };
    (err == 0).then_some(data != 0)
}

/// Milliseconds between this process being created and now (covers loader time before `main`).
pub fn ms_since_process_created() -> Option<f64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user, mut now) = (zero, zero, zero, zero, zero);
    let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) };
    if ok == 0 {
        return None;
    }
    unsafe { GetSystemTimePreciseAsFileTime(&mut now) };
    let t = |f: FILETIME| (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64;
    Some(t(now).saturating_sub(t(created)) as f64 / 10_000.0)
}

// ---------------------------------------------------------------------------------------------
// Single instance
//
// The first Platter in a session holds a named mutex. A second launch finds the main window by its
// class name, asks it (through a registered window message) to come forward, and exits before any
// WebView2 work starts. An elevated Platter creates the mutex with an administrators-only DACL, so a
// normal launch gets ERROR_ACCESS_DENIED instead of ERROR_ALREADY_EXISTS; both mean "already running".
// The elevated copy started by `relaunch_elevated` (`--wait-for-pid`) takes over instead of deferring:
// the instance that launched it is exiting.

const INSTANCE_MUTEX: &str =
    if cfg!(debug_assertions) { r"Local\app.platter.disks.instance.debug" } else { r"Local\app.platter.disks.instance" };
/// Window class of the main window, so another launch can find it with `FindWindowW`.
pub const MAIN_WINDOW_CLASS: &str = if cfg!(debug_assertions) { "PlatterMainWindowDebug" } else { "PlatterMainWindow" };
const ACTIVATE_MESSAGE: &str = "app.platter.disks.activate";
/// What the running instance answers to the activate message, so the caller knows it was handled.
const ACTIVATE_ACK: isize = 0x504C_4154; // "PLAT"
/// The answer while the main window is closing: the caller should wait for the mutex instead.
const ACTIVATE_CLOSING: isize = 0x504C_4358; // "PLCX"

/// True if this process should run as Platter's single instance; false if another one already runs.
/// `takeover`: started by relaunch_elevated, so run even though the old instance may still hold the mutex.
pub fn claim_single_instance(takeover: bool) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let name = wide(INSTANCE_MUTEX);
    let h = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    let err = unsafe { GetLastError() };
    if h.is_null() {
        // Access denied: an elevated Platter owns it. Any other failure: don't block the launch.
        return takeover || err != ERROR_ACCESS_DENIED;
    }
    if err == ERROR_ALREADY_EXISTS && !takeover {
        unsafe { CloseHandle(h) };
        return false;
    }
    // The handle stays open for the life of the process; Windows releases it on exit.
    true
}

fn activate_message() -> u32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW;
    let name = wide(ACTIVATE_MESSAGE);
    unsafe { RegisterWindowMessageW(name.as_ptr()) }
}

/// Another Platter holds the single-instance mutex: bring its main window to the front. Returns true
/// when that worked (this launch should exit), false when no other instance is left after all and this
/// process now holds the mutex (it should start normally).
///
/// The other instance may still be starting (no window yet) or may be exiting (window closing or
/// already gone while WebView2 tears down and the mutex is still held), so this keeps polling for a
/// window that takes the request or for the mutex to come free. If neither happens within 10 s,
/// this launch starts anyway: showing nothing at all is worse than a second window.
pub fn defer_to_existing_instance() -> bool {
    let class = wide(MAIN_WINDOW_CLASS);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let h = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW(class.as_ptr(), std::ptr::null()) };
        if !h.is_null() && activate_window(h) {
            return true;
        }
        if claim_single_instance(false) {
            return false;
        }
        if std::time::Instant::now() >= deadline {
            return !claim_single_instance(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Ask the main window `hwnd` of another instance to come forward. False if it is closing or gone.
fn activate_window(hwnd: windows_sys::Win32::Foundation::HWND) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AllowSetForegroundWindow, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, SendMessageTimeoutW,
        SetForegroundWindow, ShowWindow, SMTO_ABORTIFHUNG, SW_RESTORE,
    };
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != 0 {
            // We were just launched by the user, so we may pass our right to take the foreground on.
            AllowSetForegroundWindow(pid);
        }
        let mut answer = 0usize;
        let sent = SendMessageTimeoutW(hwnd, activate_message(), 0, 0, SMTO_ABORTIFHUNG, 3000, &mut answer);
        if sent != 0 && answer as isize == ACTIVATE_ACK {
            return true;
        }
        if (sent != 0 && answer as isize == ACTIVATE_CLOSING) || IsWindow(hwnd) == 0 {
            return false;
        }
        // Not answered (hung, or a build without the handler): do what we can from here.
        if IsWindowVisible(hwnd) != 0 {
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            }
            SetForegroundWindow(hwnd);
        }
    }
    true
}

/// The main window is closing and this process is about to exit: from now on another launch is told
/// so (instead of being "activated" by a window that is going away) and waits to take over.
pub fn note_main_window_closing() {
    CLOSING.store(true, std::sync::atomic::Ordering::SeqCst);
}

static CLOSING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// (maximized, minimized) as Windows has them right now. tao's own maximized flag lags behind: it is
/// updated on WM_SIZE, after the move event that reports the new geometry.
pub fn window_zoomed_iconic(hwnd: usize) -> (bool, bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{IsIconic, IsZoomed};
    let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
    if hwnd.is_null() {
        return (false, false);
    }
    unsafe { (IsZoomed(hwnd) != 0, IsIconic(hwnd) != 0) }
}

/// Let another launch bring `hwnd` (the main window, as an integer) forward. Must be called on the
/// thread that owns the window. When elevated, the message is let through UIPI from normal launches.
pub fn install_activation_handler(hwnd: usize) {
    use windows_sys::Win32::UI::Shell::SetWindowSubclass;
    use windows_sys::Win32::UI::WindowsAndMessaging::{ChangeWindowMessageFilterEx, MSGFLT_ALLOW};
    let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
    let msg = activate_message();
    if hwnd.is_null() || msg == 0 {
        return;
    }
    unsafe {
        ChangeWindowMessageFilterEx(hwnd, msg, MSGFLT_ALLOW, std::ptr::null_mut());
        SetWindowSubclass(hwnd, Some(activation_proc), 0x504C, msg as usize);
    }
}

unsafe extern "system" fn activation_proc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
    _id: usize,
    activate_msg: usize,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::Shell::DefSubclassProc;
    use windows_sys::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindowVisible, SetForegroundWindow, ShowWindow, SW_RESTORE};
    if msg == activate_msg as u32 {
        if CLOSING.load(std::sync::atomic::Ordering::SeqCst) {
            return ACTIVATE_CLOSING;
        }
        // Still hidden means we're starting up: the window is shown (and so activated) right after.
        if IsWindowVisible(hwnd) != 0 {
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            }
            SetForegroundWindow(hwnd);
        }
        return ACTIVATE_ACK;
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

pub fn env_path(var: &str) -> Option<std::path::PathBuf> {
    std::env::var_os(var).map(std::path::PathBuf::from)
}

pub fn system_drive_letter() -> String {
    std::env::var("SystemDrive")
        .ok()
        .and_then(|s| s.chars().next())
        .unwrap_or('C')
        .to_ascii_uppercase()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_wait_for_pid() {
        assert_eq!(wait_for_pid_arg(args(&["--wait-for-pid", "1234"])), Some(1234));
        assert_eq!(wait_for_pid_arg(args(&["x", "--wait-for-pid", "42", "y"])), Some(42));
        assert_eq!(wait_for_pid_arg(args(&["--wait-for-pid"])), None);
        assert_eq!(wait_for_pid_arg(args(&["--wait-for-pid", "abc"])), None);
        assert_eq!(wait_for_pid_arg(args(&[])), None);
    }

    #[test]
    fn theme_and_process_clock() {
        // The value can be missing on some editions; reading it must simply not fail loudly.
        let _ = system_uses_light_theme();
        let ms = ms_since_process_created().expect("process times");
        assert!(ms > 0.0 && ms < 3_600_000.0, "{ms}");
    }

    #[test]
    fn error_mode_is_set() {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetErrorMode() -> u32;
        }
        suppress_critical_error_dialogs();
        assert_eq!(unsafe { GetErrorMode() } & 0x8001, 0x8001);
    }
}
