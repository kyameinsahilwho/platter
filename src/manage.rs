//! Everyday drive management: rename, change letter, check for errors, open/reveal in Explorer,
//! and move things to the Recycle Bin.

use crate::util;
use serde::Serialize;
use std::os::windows::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

fn parse_letter(letter: &str) -> Result<char, String> {
    let t = letter.trim().trim_end_matches('\\').trim_end_matches(':');
    let mut it = t.chars();
    match (it.next(), it.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => Ok(c.to_ascii_uppercase()),
        _ => Err("That isn't a drive letter.".into()),
    }
}

fn drive_mask() -> u32 {
    unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() }
}

fn letter_in_use(l: char) -> bool {
    drive_mask() & (1 << (l as u8 - b'A')) != 0
}

fn system_letter() -> char {
    util::system_drive_letter().chars().next().unwrap_or('C')
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|_| "The operation stopped unexpectedly.".to_string())?
}

// ---------------------------------------------------------------------------------------------
// Label
// ---------------------------------------------------------------------------------------------

const LABEL_INVALID: &[char] = &['*', '?', '/', '\\', '|', ',', ';', ':', '+', '=', '<', '>', '[', ']', '"'];

fn validate_label(label: &str) -> Result<String, String> {
    let label = label.trim();
    if label.chars().count() > 32 {
        return Err("Drive names can be at most 32 characters long.".into());
    }
    if let Some(c) = label.chars().find(|c| LABEL_INVALID.contains(c) || c.is_control()) {
        return Err(format!("Drive names can't contain {c}."));
    }
    Ok(label.to_string())
}

fn set_label_sync(letter: &str, label: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Storage::FileSystem::SetVolumeLabelW;
    let l = parse_letter(letter)?;
    let label = validate_label(label)?;
    if !letter_in_use(l) {
        return Err(format!("Drive {l}: wasn't found."));
    }
    let root = util::wide(&format!("{l}:\\"));
    let name = util::wide(&label);
    let ok = unsafe { SetVolumeLabelW(root.as_ptr(), name.as_ptr()) } != 0;
    if ok {
        return Ok(());
    }
    Err(match unsafe { GetLastError() } {
        5 => "Renaming drives needs administrator rights. Restart Platter as administrator.".into(),
        19 => format!("Drive {l}: is write-protected, so it can't be renamed."),
        21 => format!("Drive {l}: isn't ready. Check that it's connected and try again."),
        123 | 154 => format!("That name is too long or not allowed for the file system on {l}:."),
        50 | 1 => format!("Drive {l}: doesn't support names."),
        e => format!("Windows couldn't rename drive {l}: (error {e})."),
    })
}

#[tauri::command]
pub async fn set_label(letter: String, label: String) -> Result<(), String> {
    blocking(move || set_label_sync(&letter, &label)).await
}

// ---------------------------------------------------------------------------------------------
// Letters
// ---------------------------------------------------------------------------------------------

fn bit(l: char) -> u32 {
    1 << (l as u8 - b'A')
}

/// Letters the signed-in user has given to network shares. An elevated Platter runs in its own UAC
/// logon session with its own drive map, so `GetLogicalDrives` there misses the drives mapped in the
/// user's normal session. Look at that session (through Explorer's token) and at the user's saved
/// mappings (`HKCU\Network`, which also lists ones that haven't reconnected yet).
fn network_mask() -> u32 {
    let session = if util::is_elevated() { shell_session_mask() } else { 0 };
    session | saved_mappings_mask()
}

/// Letters defined in the logon session of the desktop shell (the user's non-elevated Explorer).
fn shell_session_mask() -> u32 {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf, TOKEN_DUPLICATE, TOKEN_QUERY};
    use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;
    use windows_sys::Win32::System::Threading::{OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CloseHandle(self.0) };
            }
        }
    }
    struct Revert;
    impl Drop for Revert {
        fn drop(&mut self) {
            unsafe { RevertToSelf() };
        }
    }

    unsafe {
        let shell = GetShellWindow();
        if shell.is_null() {
            return 0;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(shell, &mut pid);
        if pid == 0 || pid == std::process::id() {
            return 0;
        }
        let process = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid));
        if process.0.is_null() {
            return 0;
        }
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(process.0, TOKEN_QUERY | TOKEN_DUPLICATE, &mut token) == 0 {
            return 0;
        }
        let token = Handle(token);
        if ImpersonateLoggedOnUser(token.0) == 0 {
            return 0;
        }
        let _revert = Revert;
        // While impersonating, "X:" is looked up in that token's logon session (then the global one).
        let mut buf = vec![0u16; 1024];
        let mut mask = 0;
        for c in b'A'..=b'Z' {
            let name = util::wide(&format!("{}:", c as char));
            if QueryDosDeviceW(name.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) != 0
                || windows_sys::Win32::Foundation::GetLastError() == 122
            {
                mask |= bit(c as char);
            }
        }
        mask
    }
}

/// Saved ("Reconnect at sign-in") network drives of the current user: subkeys of `HKCU\Network`.
fn saved_mappings_mask() -> u32 {
    let reg = util::env_path("SystemRoot").unwrap_or_else(|| PathBuf::from(r"C:\Windows")).join(r"System32\reg.exe");
    let Ok(out) = Command::new(reg)
        .args(["query", r"HKCU\Network"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(util::CREATE_NO_WINDOW)
        .output()
    else {
        return 0;
    };
    parse_saved_mappings(&String::from_utf8_lossy(&out.stdout))
}

fn parse_saved_mappings(text: &str) -> u32 {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            let i = l.rfind('\\')?;
            l[..i].to_ascii_uppercase().ends_with(r"\NETWORK").then(|| &l[i + 1..])
        })
        .filter_map(|name| parse_letter(name).ok())
        .fold(0, |m, l| m | bit(l))
}

fn free_letters_sync() -> Vec<String> {
    let mask = drive_mask() | network_mask();
    (b'D'..=b'Z').filter(|&c| mask & (1 << (c - b'A')) == 0).map(|c| (c as char).to_string()).collect()
}

#[tauri::command]
pub async fn free_letters() -> Result<Vec<String>, String> {
    Ok(free_letters_sync())
}

/// All the checks that can be done without touching the disk.
fn change_letter_preflight(letter: &str, new_letter: &str) -> Result<(char, char), String> {
    let from = parse_letter(letter)?;
    let to = parse_letter(new_letter)?;
    if from == system_letter() {
        return Err(format!("{from}: is the Windows drive. Windows needs it to keep its letter, so it can't be changed."));
    }
    if !letter_in_use(from) {
        return Err(format!("Drive {from}: wasn't found."));
    }
    if from == to {
        return Err(format!("The drive already uses {to}:."));
    }
    if letter_in_use(to) {
        return Err(format!("{to}: is already used by another drive. Pick a free letter."));
    }
    if !('D'..='Z').contains(&to) {
        return Err("Pick a letter from D to Z.".into());
    }
    if !util::is_elevated() {
        return Err("Changing drive letters needs administrator rights. Restart Platter as administrator.".into());
    }
    if network_mask() & bit(to) != 0 {
        return Err(format!("{to}: is used by a mapped network drive. Pick a free letter."));
    }
    Ok((from, to))
}

fn change_letter_sync(letter: &str, new_letter: &str) -> Result<(), String> {
    let (from, to) = change_letter_preflight(letter, new_letter)?;
    let script = format!(
        "$p=Get-Partition -DriveLetter {from} -ErrorAction SilentlyContinue; \
         if(-not $p){{ throw 'NOPARTITION' }}; \
         $p | Set-Partition -NewDriveLetter {to} -ErrorAction Stop"
    );
    util::powershell(&script).map(|_| ()).map_err(|e| {
        if e.contains("NOPARTITION") {
            format!("{from}: isn't a partition on a local disk, so Platter can't change its letter.")
        } else if e.contains("Windows denied access") {
            "Changing drive letters needs administrator rights. Restart Platter as administrator.".into()
        } else {
            format!("Windows couldn't change the letter of {from}: {}.", e.trim().trim_end_matches('.'))
        }
    })
}

#[tauri::command]
pub async fn change_letter(letter: String, new_letter: String) -> Result<(), String> {
    blocking(move || change_letter_sync(&letter, &new_letter)).await
}

// ---------------------------------------------------------------------------------------------
// Error check
// ---------------------------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct VolumeCheck {
    /// "NoErrorsFound" | "ErrorsFound" | "ErrorsFixed" | "Retry" | "Failed"
    pub status: String,
    pub message: String,
}

/// Map Repair-Volume's RepairStatus to the contract's small set plus one plain sentence.
fn describe_repair(l: char, raw: &str) -> VolumeCheck {
    let (status, message) = match raw.trim() {
        "NoErrorsFound" | "ScanNoErrorsFound" | "TriageComplete" => {
            ("NoErrorsFound", format!("No problems found on {l}:."))
        }
        "ErrorsFixed" | "MinorErrorsFixedOrCleanup" | "ScanErrorsFoundAndFixedOnline" | "ScanFoundAndFixedLeaks" | "SalvageDone" => {
            ("ErrorsFixed", format!("Windows found small problems on {l}: and fixed them while the drive stayed in use."))
        }
        "ScanErrorsFoundNeedSpotFix" | "ScanErrorsFixedOnlineAlsoNeedSpotFix" | "NeedOfflineScanAndFix" | "ScanFoundLeaks" => (
            "ErrorsFound",
            "Windows found problems. Run 'Fix' from Windows' drive properties or chkdsk /f at the next restart.".to_string(),
        ),
        "ScanNeedsRetry" | "ScanNeedsMetaDataRetry" | "ScanTransientSnapshotError" | "ScanRunning" | "TriageUncertain" => {
            ("Retry", format!("The check of {l}: couldn't finish because the drive was busy. Try again in a moment."))
        }
        "Failed" | "ScanFatalSnapshotError" | "TriageFailed" => {
            ("Failed", format!("Windows couldn't check {l}:. Try 'Check' from the drive's properties in Explorer."))
        }
        other => ("Failed", format!("Windows reported \"{other}\" for {l}:.")),
    };
    VolumeCheck { status: status.into(), message }
}

fn check_volume_sync(letter: &str) -> Result<VolumeCheck, String> {
    let l = parse_letter(letter)?;
    if !letter_in_use(l) {
        return Err(format!("Drive {l}: wasn't found."));
    }
    if !util::is_elevated() {
        return Err("Checking a drive for errors needs administrator rights. Restart Platter as administrator.".into());
    }
    let out = util::powershell(&format!("\"$(Repair-Volume -DriveLetter {l} -Scan -ErrorAction Stop)\"")).map_err(|e| {
        if e.contains("Windows denied access") {
            "Checking a drive for errors needs administrator rights. Restart Platter as administrator.".to_string()
        } else if e.to_ascii_lowercase().contains("not supported") {
            format!("Windows can't check {l}: online because of its file system.")
        } else {
            format!("Windows couldn't check {l}: {}.", e.trim().trim_end_matches('.'))
        }
    })?;
    let raw = out.lines().map(str::trim).rfind(|s| !s.is_empty()).unwrap_or("");
    Ok(describe_repair(l, raw))
}

#[tauri::command]
pub async fn check_volume(letter: String) -> Result<VolumeCheck, String> {
    blocking(move || check_volume_sync(&letter)).await
}

// ---------------------------------------------------------------------------------------------
// Explorer
// ---------------------------------------------------------------------------------------------

fn explorer(raw_arg: &str) -> Result<(), String> {
    let exe = util::env_path("SystemRoot").unwrap_or_else(|| PathBuf::from(r"C:\Windows")).join("explorer.exe");
    // explorer.exe exits with 1 even on success, so don't wait for it or look at its exit code.
    Command::new(exe)
        .raw_arg(raw_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(util::CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|_| "Windows Explorer couldn't be opened.".to_string())
}

/// Normalise slashes and reject anything Explorer would misread.
fn clean_path(path: &str) -> Result<String, String> {
    let p = path.trim().replace('/', "\\");
    if p.is_empty() || p.contains('"') || p.contains('\0') {
        return Err("That location isn't valid.".into());
    }
    Ok(p)
}

fn open_path_sync(path: &str) -> Result<(), String> {
    let p = clean_path(path)?;
    // "C:" opens the current directory of C:, not its root.
    let p = if p.len() == 2 && p.ends_with(':') { format!("{p}\\") } else { p };
    if !Path::new(&p).exists() {
        return Err("That location no longer exists.".into());
    }
    explorer(&format!("\"{p}\""))
}

fn reveal_path_sync(path: &str) -> Result<(), String> {
    let p = clean_path(path)?;
    let p = p.trim_end_matches('\\').to_string();
    if !Path::new(&p).exists() {
        // Fall back to the nearest folder that still exists.
        if let Some(parent) = Path::new(&p).ancestors().skip(1).find(|a| !a.as_os_str().is_empty() && a.exists()) {
            return explorer(&format!("\"{}\"", parent.display()));
        }
        return Err("That item no longer exists.".into());
    }
    explorer(&format!("/select,\"{p}\""))
}

#[tauri::command]
pub async fn open_path(path: String) -> Result<(), String> {
    blocking(move || open_path_sync(&path)).await
}

#[tauri::command]
pub async fn reveal_path(path: String) -> Result<(), String> {
    blocking(move || reveal_path_sync(&path)).await
}

#[tauri::command]
pub async fn open_recycle_bin() -> Result<(), String> {
    blocking(|| explorer("shell:RecycleBinFolder")).await
}

// ---------------------------------------------------------------------------------------------
// Recycle Bin
// ---------------------------------------------------------------------------------------------

/// Lower-case, backslashed, no `\\?\` prefix, no trailing backslash (except "c:\").
fn norm(p: &Path) -> String {
    let mut s = p.to_string_lossy().replace('/', "\\");
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        s = format!(r"\\{rest}");
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        s = rest.to_string();
    }
    let mut s = s.to_lowercase();
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    s
}

/// Resolve the parent (so `..` and junctions in the path can't sneak past the checks) but keep the
/// final component as-is, so a link is judged — and recycled — as the link, not its target.
fn resolved(p: &Path) -> String {
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => match std::fs::canonicalize(parent) {
            Ok(cp) => norm(&cp.join(name)),
            Err(_) => norm(p),
        },
        _ => norm(p),
    }
}

fn canon(p: &Path) -> String {
    std::fs::canonicalize(p).map(|c| norm(&c)).unwrap_or_else(|_| norm(p))
}

#[link(name = "ole32")]
extern "system" {
    // windows-sys has it only behind the `Win32_System_Com` feature.
    fn CoTaskMemFree(pv: *const core::ffi::c_void);
}

/// Where a Known Folder really is (it can be moved by OneDrive folder backup, policy or the user).
fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    use windows_sys::Win32::UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY};
    let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
    let hr = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DONT_VERIFY as u32, std::ptr::null_mut(), &mut p) };
    let out = if hr >= 0 && !p.is_null() {
        let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
        let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) });
        (!s.is_empty()).then(|| PathBuf::from(s))
    } else {
        None
    };
    if !p.is_null() {
        unsafe { CoTaskMemFree(p as *const _) };
    }
    out
}

/// The user's main folders wherever they live now: Desktop, Documents, Downloads, Pictures, Music,
/// Videos and the OneDrive root(s), each as both its literal and its resolved form.
fn known_main_folders() -> Vec<String> {
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music, FOLDERID_Pictures, FOLDERID_SkyDrive,
        FOLDERID_Videos,
    };
    let mut dirs: Vec<PathBuf> =
        [FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Pictures, FOLDERID_Music, FOLDERID_Videos, FOLDERID_SkyDrive]
            .iter()
            .filter_map(known_folder)
            .collect();
    dirs.extend(["OneDrive", "OneDriveConsumer", "OneDriveCommercial"].iter().filter_map(|v| util::env_path(v)));
    let mut out = Vec::new();
    for d in dirs.iter().filter(|d| d.is_absolute()) {
        for s in [norm(d), canon(d)] {
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

fn get_full_path(p: &str) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::GetFullPathNameW;
    let w = util::wide(p);
    let mut buf = vec![0u16; 512];
    loop {
        let n = unsafe { GetFullPathNameW(w.as_ptr(), buf.len() as u32, buf.as_mut_ptr(), std::ptr::null_mut()) } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            return Some(String::from_utf16_lossy(&buf[..n]));
        }
        buf.resize(n + 1, 0);
    }
}

/// Expand 8.3 short names ("PROGRA~1") to the real ones. Fails if the path doesn't exist.
fn get_long_path(p: &str) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::GetLongPathNameW;
    let w = util::wide(p);
    let mut buf = vec![0u16; 512];
    loop {
        let n = unsafe { GetLongPathNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            return Some(String::from_utf16_lossy(&buf[..n]));
        }
        buf.resize(n + 1, 0);
    }
}

/// Turn a recycle request into the exact path Windows will act on, so the safety checks judge the same
/// item that gets recycled: `.`/`..` collapsed, 8.3 short names expanded. Names ending in a dot or a
/// space are refused, because Windows would silently act on the name without them.
pub(crate) fn normalize_target(raw: &str) -> Result<PathBuf, String> {
    // clean_path trims, which would turn an item named "x " into a different item "x".
    let raw = raw.trim_start();
    if raw.ends_with(|c: char| c.is_whitespace()) {
        return Err(format!("Platter won't recycle “{raw}” because Windows would read a name ending in a dot or space as a different item."));
    }
    let p = clean_path(raw)?;
    if p.starts_with(r"\\?\") || p.starts_with(r"\\.\") {
        return Err("Platter can only recycle items given by their usual location.".into());
    }
    if !Path::new(&p).is_absolute() {
        return Err("Platter can only recycle items given by their full location.".into());
    }
    let odd_name = |c: Component| match c {
        Component::Normal(n) => {
            let n = n.to_string_lossy();
            n.ends_with('.') || n.ends_with(' ')
        }
        _ => false,
    };
    if Path::new(&p).components().any(odd_name) {
        return Err(format!("Platter won't recycle {p} because Windows would read a name ending in a dot or space as a different item."));
    }
    let full = get_full_path(&p).ok_or_else(|| "That location isn't valid.".to_string())?;
    let mut full = get_long_path(&full).unwrap_or(full);
    while full.len() > 3 && full.ends_with('\\') {
        full.pop();
    }
    let path = PathBuf::from(full);
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir) || odd_name(c)) {
        return Err("That location isn't valid.".into());
    }
    Ok(path)
}

fn is_inside(child: &str, parent: &str) -> bool {
    child == parent || (child.starts_with(parent) && child[parent.len()..].starts_with('\\')) || (parent.ends_with('\\') && child.starts_with(parent))
}

/// Paths Platter refuses to recycle: drive roots, Windows itself, program folders, the Users folder,
/// the user profile and its main folders.
pub(crate) fn refuse_reason(path: &Path) -> Option<String> {
    let shown = path.display().to_string();
    let r = resolved(path);
    let is_root = path.parent().is_none()
        || r.len() <= 3
        || (r.starts_with(r"\\") && r.trim_start_matches('\\').split('\\').filter(|s| !s.is_empty()).count() <= 2);
    if is_root {
        return Some(format!("Platter won't move a whole drive ({shown}) to the Recycle Bin."));
    }
    let env = |v: &str| util::env_path(v).map(|p| canon(&p));
    let windows = env("SystemRoot").unwrap_or_else(|| r"c:\windows".into());
    let win_temp = format!(r"{windows}\temp");
    if is_inside(&r, &windows) && !(is_inside(&r, &win_temp) && r != win_temp) {
        return Some(format!("Platter won't move {shown} to the Recycle Bin because Windows needs it."));
    }
    let exact_system = ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432", "ProgramData", "CommonProgramFiles", "CommonProgramFiles(x86)"]
        .iter()
        .filter_map(|v| env(v))
        .collect::<Vec<_>>();
    if exact_system.iter().any(|s| *s == r) {
        return Some(format!("Platter won't move {shown} to the Recycle Bin because Windows needs it."));
    }
    if let Some(profile) = env("USERPROFILE") {
        let users = Path::new(&profile).parent().map(|p| norm(p));
        if users.as_deref() == Some(r.as_str()) || r == profile {
            return Some(format!("Platter won't move {shown} to the Recycle Bin because it holds your user account."));
        }
        let main = ["Desktop", "Documents", "Downloads", "Pictures", "Music", "Videos", "AppData", "OneDrive", "Favorites", "Contacts"];
        if main.iter().any(|m| r == format!(r"{profile}\{}", m.to_lowercase())) || r == format!(r"{profile}\appdata\local") || r == format!(r"{profile}\appdata\roaming") {
            return Some(format!("Platter won't move {shown} to the Recycle Bin because it's one of your account's main folders."));
        }
    }
    if known_main_folders().iter().any(|k| *k == r) {
        return Some(format!("Platter won't move {shown} to the Recycle Bin because it's one of your account's main folders."));
    }
    None
}

/// Check and normalize every requested path; returns the ones that still exist, as Windows will see them.
pub(crate) fn recycle_targets(paths: &[String]) -> Result<Vec<String>, String> {
    let mut list: Vec<String> = Vec::new();
    for raw in paths {
        let path = normalize_target(raw)?;
        if let Some(why) = refuse_reason(&path) {
            return Err(why);
        }
        // Already gone (deleted elsewhere): nothing to do for this one.
        if std::fs::symlink_metadata(&path).is_err() {
            continue;
        }
        let p = path.to_string_lossy().into_owned();
        if !list.iter().any(|x| x.eq_ignore_ascii_case(&p)) {
            list.push(p);
        }
    }
    Ok(list)
}

pub(crate) fn recycle_sync(paths: &[String]) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING, FO_DELETE,
        SHFILEOPSTRUCTW,
    };

    // The same normalized strings that passed the checks are what SHFileOperationW gets.
    let list = recycle_targets(paths)?;
    if list.is_empty() {
        return Ok(());
    }

    // pFrom: NUL-separated, double-NUL-terminated.
    let mut from: Vec<u16> = Vec::new();
    for p in &list {
        from.extend(p.encode_utf16());
        from.push(0);
    }
    from.push(0);

    let mut op = SHFILEOPSTRUCTW {
        hwnd: std::ptr::null_mut(),
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        pTo: std::ptr::null(),
        // FOF_WANTNUKEWARNING: if an item can't go to the Recycle Bin (e.g. a USB stick has none),
        // Windows asks before deleting it permanently instead of silently destroying it.
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI | FOF_WANTNUKEWARNING) as u16,
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };
    let r = unsafe { SHFileOperationW(&mut op) };
    if r == 0 && op.fAnyOperationsAborted != 0 {
        return Err("Moving to the Recycle Bin was cancelled, so some items were left in place.".into());
    }
    match r {
        0 => Ok(()),
        // 0x4C7 = ERROR_CANCELLED
        0x4C7 => Err("Moving to the Recycle Bin was cancelled, so some items were left in place.".into()),
        5 | 0x78 => Err("Windows denied access to one of these items. Restart Platter as administrator and try again.".into()),
        32 | 33 => Err("One of these items is open in another program. Close it and try again.".into()),
        2 | 3 | 0x7C => Err("One of these items no longer exists.".into()),
        0x13 | 0x86 => Err("One of these items is on a write-protected drive.".into()),
        e => Err(format!("Windows couldn't move these items to the Recycle Bin (error {e}).")),
    }
}

#[tauri::command]
pub async fn recycle_paths(paths: Vec<String>) -> Result<(), String> {
    blocking(move || recycle_sync(&paths)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manage_free_letters_real() {
        let f = free_letters_sync();
        println!("free_letters: {f:?}");
        assert!(!f.contains(&util::system_drive_letter()));
        assert!(f.iter().all(|l| l.len() == 1 && ("D".."[").contains(&l.as_str())));
    }

    #[test]
    fn manage_change_letter_refusals() {
        let sys = util::system_drive_letter();
        let free = free_letters_sync();
        let e = change_letter_sync(&sys, free.first().map(String::as_str).unwrap_or("Z")).unwrap_err();
        println!("system: {e}");
        assert!(e.contains("Windows drive"));
        assert!(change_letter_sync("C", "A").is_err());
        if util::is_elevated() {
            return;
        }
        // A non-system drive that exists, to a free letter → must stop at the admin check (nothing runs).
        if let Some(other) = (b'D'..=b'Z').map(|c| c as char).find(|&c| letter_in_use(c)) {
            if let Some(to) = free.first() {
                let e = change_letter_sync(&other.to_string(), to).unwrap_err();
                println!("non-admin change {other}: -> {to}: {e}");
                assert!(e.contains("administrator"));
            }
            let e = change_letter_sync(&other.to_string(), &sys).unwrap_err();
            println!("in use: {e}");
            assert!(e.contains("already used"));
        }
    }

    #[test]
    fn manage_check_volume_not_admin() {
        if util::is_elevated() {
            return;
        }
        let e = check_volume_sync(&util::system_drive_letter()).unwrap_err();
        println!("check_volume: {e}");
        assert!(e.contains("administrator"));
        assert_eq!(describe_repair('C', "NoErrorsFound").message, "No problems found on C:.");
        assert_eq!(describe_repair('C', "ScanErrorsFoundNeedSpotFix").status, "ErrorsFound");
    }

    #[test]
    fn manage_validate_label() {
        assert!(validate_label("a*b").is_err());
        assert!(validate_label(&"x".repeat(33)).is_err());
        assert!(validate_label("Games 2").is_ok());
        assert!(set_label_sync("CC", "x").is_err());
    }

    #[test]
    fn manage_refuses_dangerous_paths() {
        let profile = util::env_path("USERPROFILE").unwrap();
        let windir = util::env_path("SystemRoot").unwrap();
        for p in [
            PathBuf::from(r"C:\"),
            windir.clone(),
            windir.join("System32"),
            windir.join(r"Temp\..\System32"),
            PathBuf::from(r"C:\Program Files"),
            profile.clone(),
            profile.join("Downloads"),
            profile.parent().unwrap().to_path_buf(),
        ] {
            // Check the guard directly — never call recycle_sync on these.
            let e = refuse_reason(&p).expect("must refuse");
            println!("refused {}: {e}", p.display());
        }
        assert!(refuse_reason(&profile.join(r"Downloads\something.zip")).is_none());
    }

    #[test]
    fn manage_known_folders_protected() {
        let kf = known_main_folders();
        println!("known folders: {kf:?}");
        let docs = known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Documents).expect("Documents");
        assert!(kf.contains(&canon(&docs)));
        // Wherever Documents/Desktop really are (OneDrive backup, redirection), they're refused.
        for id in [windows_sys::Win32::UI::Shell::FOLDERID_Documents, windows_sys::Win32::UI::Shell::FOLDERID_Desktop] {
            let p = known_folder(&id).unwrap();
            let e = refuse_reason(&p).expect("must refuse");
            println!("refused {}: {e}", p.display());
        }
        // A pretend OneDrive Documents under some other root, via the env var.
        assert_eq!(norm(Path::new(r"C:\Users\X\OneDrive\")), r"c:\users\x\onedrive");
    }

    /// Only checks paths — never calls recycle_sync / SHFileOperationW on anything here.
    #[test]
    fn manage_recycle_targets_normalized() {
        let profile = util::env_path("USERPROFILE").unwrap();
        let windir = util::env_path("SystemRoot").unwrap();
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let docs = known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Documents).unwrap();
        // Paths ending in ".." or with ".." inside resolve to what Windows would act on.
        for raw in [
            format!(r"{}\Temp\..", s(&windir)),
            format!(r"{}\Temp\..\System32", s(&windir)),
            format!(r"{}\..", s(&docs)),
            format!(r"{}\x\..", s(&profile)),
            r"C:\x\..".to_string(),
            r"C:\Program Files\x\..".to_string(),
            format!(r"{}\.\", s(&docs)),
            format!(r"{}/", s(&docs)),
        ] {
            let e = recycle_targets(&[raw.clone()]).expect_err("must refuse");
            println!("refused {raw}: {e}");
        }
        // Trailing dots/spaces would make Windows act on a different name.
        for raw in [format!(r"{}.", s(&docs)), format!(r"{} ", s(&docs)), r"C:\Program Files.".into(), r"C:\Program Files ".into()] {
            let e = normalize_target(&raw).expect_err("must refuse");
            println!("refused {raw:?}: {e}");
        }
        assert!(normalize_target(r"\\?\C:\Windows").is_err());
        assert!(normalize_target(r"\\.\C:").is_err());
        assert!(normalize_target(r"Windows").is_err());

        // 8.3 short names are expanded before the checks (if the volume keeps short names).
        let pf = util::env_path("ProgramFiles").unwrap();
        let w = util::wide(&s(&pf));
        let mut buf = vec![0u16; 512];
        let n = unsafe { windows_sys::Win32::Storage::FileSystem::GetShortPathNameW(w.as_ptr(), buf.as_mut_ptr(), 512) } as usize;
        let short = String::from_utf16_lossy(&buf[..n]);
        println!("short name of {}: {short}", pf.display());
        if n > 0 && !short.eq_ignore_ascii_case(&s(&pf)) {
            assert_eq!(normalize_target(&short).unwrap(), pf);
            assert!(recycle_targets(&[short]).is_err());
        }

        // A throwaway folder under %TEMP%: allowed, and given back in its normal form.
        let tmp = std::env::temp_dir().join(format!("platter-recycle-test-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        let list = recycle_targets(&[format!(r"{}\sub\..\sub\", s(&tmp)), format!(r"{}\missing", s(&tmp))]).unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(list.len(), 1);
        assert!(list[0].ends_with(r"\sub") && !list[0].contains(r"\..") && !list[0].contains('~'), "{list:?}");
    }

    #[test]
    fn manage_network_letters() {
        assert_eq!(parse_saved_mappings("\r\nHKEY_CURRENT_USER\\Network\\Z\r\nHKEY_CURRENT_USER\\Network\\y\r\n"), bit('Z') | bit('Y'));
        assert_eq!(parse_saved_mappings("ERROR: The system was unable to find the specified registry key or value."), 0);
        assert_eq!(parse_saved_mappings("HKEY_CURRENT_USER\\Network\\Printers"), 0);
        let m = network_mask();
        println!("network letters: {:?}", (b'A'..=b'Z').filter(|&c| m & bit(c as char) != 0).map(|c| c as char).collect::<String>());
        let free = free_letters_sync();
        assert!(free.iter().all(|l| m & bit(l.chars().next().unwrap()) == 0));
        // The session lookup must not leave this thread impersonating.
        let _ = shell_session_mask();
        assert!(std::fs::metadata(std::env::temp_dir()).is_ok());
    }
}
