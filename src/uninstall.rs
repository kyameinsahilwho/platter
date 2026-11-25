//! Uninstall apps (Deep Uninstaller): lists desktop apps, runs an app's own uninstaller, then finds
//! and removes what it left behind (folders, registry entries, services, scheduled tasks, startup
//! entries). See CONTRACT.md "Uninstall apps" for the shapes and the safety rules.
//!
//! How it stays safe (false positives are much worse than misses):
//! * Leftovers are found by *exact* normalized names (lower-case letters/digits only) of the app's
//!   display name, publisher, registry key, install folder and uninstaller folder; tokens shorter than
//!   4 characters and generic words are never used, and nothing is ever matched by substring except
//!   long (6+) names in service/task names whose program is already gone.
//! * Only a few well-known parent folders and registry keys are looked at, one or two levels deep;
//!   whole trees are never walked to find candidates.
//! * Anything that is, contains, or sits inside another installed app's install folder or uninstaller,
//!   anything a still-registered service/task/startup entry runs from, protected folders (Windows,
//!   profile and Known Folders, Program Files/ProgramData roots...) and registry keys outside the
//!   whitelisted parents are dropped — at scan time and again right before removal.
//! * Folders go to the Recycle Bin (one at a time); registry keys/values, services and tasks are
//!   backed up (reg.exe export / task XML) first, and an item whose backup fails is not removed.

use crate::disks::com::{self, Bstr, EmptyVariant, Unk, EMPTY};
use crate::util::{from_wide, is_elevated, wide, CREATE_NO_WINDOW};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::os::windows::fs::MetadataExt;
use std::os::windows::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};
use tauri::Manager;

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct InstalledApp {
    pub id: String,
    pub name: String,
    pub publisher: String,
    pub version: String,
    pub install_date: Option<String>,
    pub size: u64,
    pub install_location: Option<String>,
    /// "machine" | "user"
    pub scope: &'static str,
    pub can_uninstall: bool,
    pub broken: bool,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct UninstallResult {
    pub removed: bool,
    pub exit_code: Option<i32>,
    pub stopped_waiting: bool,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Leftover {
    pub id: String,
    /// "folder" | "file" | "registry" | "service" | "task" | "startup" | "uninstall-entry"
    pub kind: &'static str,
    pub path: String,
    pub label: String,
    pub reason: String,
    pub size: u64,
    /// "high" | "medium" | "low"
    pub confidence: &'static str,
    pub needs_admin: bool,
    pub default_on: bool,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LeftoverScan {
    pub scan_id: u64,
    pub app: Option<String>,
    pub items: Vec<Leftover>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RemoveItemResult {
    pub id: String,
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RemoveResult {
    pub removed: u32,
    pub failed: u32,
    pub freed: u64,
    pub backup_folder: Option<String>,
    pub items: Vec<RemoveItemResult>,
}

// ---------------------------------------------------------------------------------------------
// Sentences

const ADMIN: &str = "Needs administrator rights. Restart Platter as administrator and try again.";
const STALE: &str = "These results are out of date. Look for leftovers again.";
const BUSY: &str = "Another uninstaller is already running.";
const CANCELLED: &str = "The uninstaller was cancelled, so nothing was removed.";
const MISSING: &str = "This app's uninstaller is missing. Look for leftovers to remove what's left of it.";
const UNKNOWN_APP: &str = "This app isn't in the list anymore. Refresh the list and try again.";

const HIGH: &str = "high";
const MEDIUM: &str = "medium";
const LOW: &str = "low";

const UNINSTALL_SUBKEY: &str = r"Microsoft\Windows\CurrentVersion\Uninstall";
const APP_PATHS_SUBKEY: &str = r"Microsoft\Windows\CurrentVersion\App Paths";

// ---------------------------------------------------------------------------------------------
// Registry

mod reg {
    use crate::util::{from_wide, wide};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::System::Registry::*;

    const ERROR_SUCCESS: u32 = 0;
    const ERROR_FILE_NOT_FOUND: u32 = 2;
    const ERROR_MORE_DATA: u32 = 234;
    const ERROR_NO_MORE_ITEMS: u32 = 259;

    #[link(name = "kernel32")]
    extern "system" {
        // windows-sys has it only behind the `Win32_System_Environment` feature.
        fn ExpandEnvironmentStringsW(src: *const u16, dst: *mut u16, size: u32) -> u32;
    }

    /// `%ProgramFiles%\x` → `C:\Program Files\x`. Unknown variables stay as written.
    pub fn expand(s: &str) -> String {
        if !s.contains('%') {
            return s.to_string();
        }
        let w = wide(s);
        let mut buf = vec![0u16; 1024];
        for _ in 0..4 {
            let n = unsafe { ExpandEnvironmentStringsW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
            if n == 0 {
                return s.to_string();
            }
            if n <= buf.len() {
                return from_wide(&buf);
            }
            buf.resize(n + 1, 0);
        }
        s.to_string()
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
    pub enum Hive {
        Lm,
        Cu,
    }

    impl Hive {
        pub fn root(self) -> HKEY {
            match self {
                Hive::Lm => HKEY_LOCAL_MACHINE,
                Hive::Cu => HKEY_CURRENT_USER,
            }
        }
        pub fn short(self) -> &'static str {
            match self {
                Hive::Lm => "HKLM",
                Hive::Cu => "HKCU",
            }
        }
    }

    /// A key by its full path in the 64-bit view. 32-bit HKLM entries are written with their explicit
    /// `SOFTWARE\WOW6432Node\...` path (which the 64-bit view resolves to the 32-bit one), so one
    /// string identifies a key for opening, for `reg.exe export` and for showing to the user.
    #[derive(Clone, PartialEq, Eq, Debug, Hash)]
    pub struct RegKey {
        pub hive: Hive,
        pub path: String,
    }

    impl RegKey {
        pub fn new(hive: Hive, path: &str) -> RegKey {
            RegKey { hive, path: path.trim_matches('\\').to_string() }
        }
        pub fn child(&self, name: &str) -> RegKey {
            RegKey::new(self.hive, &format!("{}\\{}", self.path, name))
        }
        pub fn display(&self) -> String {
            format!("{}\\{}", self.hive.short(), self.path)
        }
        pub fn leaf(&self) -> &str {
            self.path.rsplit('\\').next().unwrap_or("")
        }
        pub fn parent(&self) -> Option<RegKey> {
            self.path.rfind('\\').map(|i| RegKey::new(self.hive, &self.path[..i]))
        }
        pub fn segments(&self) -> Vec<String> {
            self.path.split('\\').filter(|s| !s.is_empty()).map(|s| s.to_lowercase()).collect()
        }
        pub fn eq_ci(&self, other: &RegKey) -> bool {
            self.hive == other.hive && self.path.eq_ignore_ascii_case(&other.path)
        }
    }

    pub struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            unsafe { RegCloseKey(self.0) };
        }
    }

    pub fn open(k: &RegKey, sam: u32) -> Result<Key, u32> {
        let mut h: HKEY = null_mut();
        let p = wide(&k.path);
        let r = unsafe { RegOpenKeyExW(k.hive.root(), p.as_ptr(), 0, sam | KEY_WOW64_64KEY, &mut h) };
        if r == ERROR_SUCCESS {
            Ok(Key(h))
        } else {
            Err(r)
        }
    }

    pub fn open_read(k: &RegKey) -> Option<Key> {
        open(k, KEY_READ).ok()
    }

    /// Unsure (access denied etc.) counts as "exists": callers use it to decide that something is
    /// gone, and wrongly deciding that is the worse mistake.
    pub fn exists(k: &RegKey) -> bool {
        !matches!(open(k, KEY_QUERY_VALUE), Err(ERROR_FILE_NOT_FOUND))
    }

    impl Key {
        pub fn subkeys(&self) -> Vec<String> {
            let mut out = Vec::new();
            let mut buf = [0u16; 512];
            for i in 0..100_000u32 {
                let mut n = buf.len() as u32;
                let r = unsafe {
                    RegEnumKeyExW(self.0, i, buf.as_mut_ptr(), &mut n, null(), null_mut(), null_mut(), null_mut())
                };
                if r == ERROR_NO_MORE_ITEMS {
                    break;
                }
                if r == ERROR_SUCCESS {
                    out.push(String::from_utf16_lossy(&buf[..n as usize]));
                }
            }
            out
        }

        pub fn value_names(&self) -> Vec<String> {
            let mut out = Vec::new();
            let mut buf = vec![0u16; 16_384];
            for i in 0..100_000u32 {
                let mut n = buf.len() as u32;
                let r = unsafe {
                    RegEnumValueW(self.0, i, buf.as_mut_ptr(), &mut n, null(), null_mut(), null_mut(), null_mut())
                };
                if r == ERROR_NO_MORE_ITEMS {
                    break;
                }
                if r == ERROR_SUCCESS {
                    out.push(String::from_utf16_lossy(&buf[..n as usize]));
                }
            }
            out
        }

        fn query(&self, name: &str) -> Option<(u32, Vec<u8>)> {
            let n = wide(name);
            let mut buf: Vec<u8> = vec![0; 512];
            for _ in 0..6 {
                let mut ty = 0u32;
                let mut size = buf.len() as u32;
                let r = unsafe { RegQueryValueExW(self.0, n.as_ptr(), null(), &mut ty, buf.as_mut_ptr(), &mut size) };
                if r == ERROR_MORE_DATA {
                    buf.resize(size as usize + 2, 0);
                    continue;
                }
                if r != ERROR_SUCCESS {
                    return None;
                }
                buf.truncate(size as usize);
                return Some((ty, buf));
            }
            None
        }

        /// REG_SZ / REG_EXPAND_SZ with environment variables expanded and surrounding blanks trimmed.
        pub fn string(&self, name: &str) -> Option<String> {
            let (ty, b) = self.query(name)?;
            if ty != REG_SZ && ty != REG_EXPAND_SZ {
                return None;
            }
            let w: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let s = expand(from_wide(&w).trim());
            let s = s.trim().to_string();
            (!s.is_empty()).then_some(s)
        }

        pub fn dword(&self, name: &str) -> Option<u32> {
            let (ty, b) = self.query(name)?;
            match ty {
                REG_DWORD if b.len() >= 4 => Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                REG_QWORD if b.len() >= 8 => Some(u64::from_le_bytes(b[..8].try_into().ok()?).min(u32::MAX as u64) as u32),
                REG_SZ => self.string(name)?.parse().ok(),
                _ => None,
            }
        }
    }

    /// Deletes the key with everything below it.
    pub fn delete_tree(k: &RegKey) -> Result<(), u32> {
        {
            const DELETE: u32 = 0x0001_0000;
            let key = open(k, DELETE | KEY_ENUMERATE_SUB_KEYS | KEY_QUERY_VALUE | KEY_SET_VALUE)?;
            let r = unsafe { RegDeleteTreeW(key.0, null()) };
            if r != ERROR_SUCCESS {
                return Err(r);
            }
        }
        let p = wide(&k.path);
        let r = unsafe { RegDeleteKeyExW(k.hive.root(), p.as_ptr(), KEY_WOW64_64KEY, 0) };
        if r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(r)
        }
    }

    pub fn delete_value(k: &RegKey, name: &str) -> Result<(), u32> {
        let key = open(k, KEY_SET_VALUE)?;
        let n = wide(name);
        let r = unsafe { RegDeleteValueW(key.0, n.as_ptr()) };
        if r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(r)
        }
    }

    /// Test helper: creates `k` (and its parents) and sets a string value.
    #[cfg(test)]
    pub fn set_string(k: &RegKey, name: &str, value: &str) {
        let mut h: HKEY = null_mut();
        let p = wide(&k.path);
        let r = unsafe {
            RegCreateKeyExW(k.hive.root(), p.as_ptr(), 0, null(), 0, KEY_WRITE | KEY_WOW64_64KEY, null(), &mut h, null_mut())
        };
        assert_eq!(r, ERROR_SUCCESS, "create {}", k.display());
        let key = Key(h);
        let n = wide(name);
        let v = wide(value);
        let r = unsafe { RegSetValueExW(key.0, n.as_ptr(), 0, REG_SZ, v.as_ptr() as *const u8, (v.len() * 2) as u32) };
        assert_eq!(r, ERROR_SUCCESS);
    }
}

use reg::{Hive, RegKey};

// ---------------------------------------------------------------------------------------------
// Paths

fn env_dir(var: &str) -> Option<PathBuf> {
    crate::util::env_path(var).filter(|p| p.is_absolute())
}

fn system_root() -> PathBuf {
    env_dir("SystemRoot").or_else(|| env_dir("windir")).unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}

fn system32(name: &str) -> PathBuf {
    system_root().join("System32").join(name)
}

/// Lower-case, backslashed, no `\\?\` prefix, no trailing backslash: the form all path
/// comparisons in this module use.
fn norm_path(p: &Path) -> String {
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

/// `child` is `parent` or below it (both from [`norm_path`]).
fn within(child: &str, parent: &str) -> bool {
    !parent.is_empty()
        && (child == parent
            || (child.len() > parent.len()
                && child.starts_with(parent)
                && (parent.ends_with('\\') || child.as_bytes()[parent.len()] == b'\\')))
}

/// Both the literal and the resolved (junctions followed) form of a path, for protected lists.
fn path_forms(p: &Path) -> Vec<String> {
    let mut v = vec![norm_path(p)];
    if let Ok(c) = std::fs::canonicalize(p) {
        let c = norm_path(&c);
        if !v.contains(&c) {
            v.push(c);
        }
    }
    v
}

/// The form a candidate is judged by: its parent resolved (so a junction higher up can't hide where
/// it really is) and its own name kept, so a link is judged as itself.
fn judged_form(p: &Path) -> String {
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => match std::fs::canonicalize(parent) {
            Ok(cp) => norm_path(&cp.join(name)),
            Err(_) => norm_path(p),
        },
        _ => norm_path(p),
    }
}

fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;
    let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
    let hr = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut p) };
    let out = (hr >= 0 && !p.is_null()).then(|| unsafe {
        let len = (0..).take_while(|&i| *p.add(i) != 0).count();
        PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(p, len)))
    });
    if !p.is_null() {
        unsafe { CoTaskMemFree(p as *const _) };
    }
    out
}

/// Folders that must never be removed — nor anything that contains them. Ported from cleanup.rs
/// `protected()` plus the parent folders leftovers are searched in.
fn protected_paths() -> Vec<String> {
    use windows_sys::Win32::UI::Shell::*;
    let mut v: Vec<PathBuf> = [
        "USERPROFILE",
        "LOCALAPPDATA",
        "APPDATA",
        "ProgramData",
        "SystemRoot",
        "windir",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "CommonProgramFiles",
        "CommonProgramFiles(x86)",
        "CommonProgramW6432",
        "PUBLIC",
        "ALLUSERSPROFILE",
        "OneDrive",
        "OneDriveConsumer",
        "OneDriveCommercial",
        "TEMP",
        "TMP",
    ]
    .iter()
    .filter_map(|v| env_dir(v))
    .collect();
    if let Some(p) = env_dir("USERPROFILE") {
        for s in [
            "Downloads",
            "Documents",
            "Desktop",
            "Pictures",
            "Videos",
            "Music",
            "OneDrive",
            "Favorites",
            "Contacts",
            "Links",
            "Searches",
            "Saved Games",
            "AppData",
            r"AppData\Local",
            r"AppData\LocalLow",
            r"AppData\Roaming",
            r"AppData\Local\Programs",
            r"AppData\Local\Temp",
            r"AppData\Local\Packages",
            r"AppData\Local\Microsoft",
            r"AppData\Roaming\Microsoft",
        ] {
            v.push(p.join(s));
        }
        if let Some(users) = p.parent() {
            v.push(users.to_path_buf());
        }
    }
    if let Some(pd) = env_dir("ProgramData") {
        v.push(pd.join("Microsoft"));
        v.push(pd.join("Package Cache"));
        v.push(pd.join("Packages"));
    }
    let win = system_root();
    v.push(win.join("System32"));
    v.push(win.join("SysWOW64"));
    for id in [
        &FOLDERID_Profile,
        &FOLDERID_Desktop,
        &FOLDERID_Documents,
        &FOLDERID_Downloads,
        &FOLDERID_Pictures,
        &FOLDERID_Videos,
        &FOLDERID_Music,
        &FOLDERID_Favorites,
        &FOLDERID_SavedGames,
        &FOLDERID_SkyDrive,
        &FOLDERID_LocalAppData,
        &FOLDERID_LocalAppDataLow,
        &FOLDERID_RoamingAppData,
        &FOLDERID_UserProfiles,
        &FOLDERID_UserProgramFiles,
        &FOLDERID_UserProgramFilesCommon,
        &FOLDERID_Public,
        &FOLDERID_ProgramData,
        &FOLDERID_ProgramFiles,
        &FOLDERID_ProgramFilesX86,
        &FOLDERID_ProgramFilesX64,
        &FOLDERID_ProgramFilesCommon,
        &FOLDERID_ProgramFilesCommonX86,
        &FOLDERID_ProgramFilesCommonX64,
        &FOLDERID_Windows,
        &FOLDERID_System,
        &FOLDERID_SystemX86,
        &FOLDERID_StartMenu,
        &FOLDERID_CommonStartMenu,
        &FOLDERID_Programs,
        &FOLDERID_CommonPrograms,
        &FOLDERID_Startup,
        &FOLDERID_CommonStartup,
    ] {
        v.extend(known_folder(id));
    }
    let mut out: Vec<String> = Vec::new();
    for p in v.iter().filter(|p| p.is_absolute()) {
        for f in path_forms(p) {
            if !out.contains(&f) {
                out.push(f);
            }
        }
    }
    out
}

/// Why Platter refuses to touch this folder or file, or None if it may be offered.
fn refuse_path(p: &Path, protected: &[String]) -> Option<&'static str> {
    if !p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return Some("it isn't a full, plain location");
    }
    let normal = p.components().filter(|c| matches!(c, Component::Normal(_))).count();
    if normal < 2 {
        return Some("it's a drive or a top-level folder");
    }
    let forms = [norm_path(p), judged_form(p)];
    let win = path_forms(&system_root());
    for f in &forms {
        if win.iter().any(|w| within(f, w)) {
            return Some("Windows needs it");
        }
        if protected.iter().any(|pr| within(pr, f)) {
            return Some("it is or holds a protected folder");
        }
        if f.contains(r"\package cache\") || f.contains(r"\windowsapps") {
            return Some("Windows keeps installers there");
        }
    }
    None
}

/// The drive (or share) the path is on is reachable. A program on an unplugged drive isn't "gone".
fn drive_present(p: &Path) -> bool {
    let s = p.to_string_lossy();
    let b = s.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') {
        return Path::new(&s[..3]).exists();
    }
    false
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Presence {
    Present,
    Missing,
    /// Relative, on a network share, or on a drive that isn't there: never treated as gone.
    Unknown,
}

fn presence(p: &Path) -> Presence {
    if !drive_present(p) {
        return Presence::Unknown;
    }
    match std::fs::symlink_metadata(p) {
        Ok(_) => Presence::Present,
        Err(e) if matches!(e.raw_os_error(), Some(2) | Some(3)) => Presence::Missing,
        Err(_) => Presence::Unknown,
    }
}

fn in_windows(p: &Path) -> bool {
    // Resolved once: this runs for every service/task/startup program (~600 per scan).
    static WIN: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let f = norm_path(p);
    WIN.get_or_init(|| path_forms(&system_root())).iter().any(|w| within(&f, w))
}

// ---------------------------------------------------------------------------------------------
// Command lines

#[derive(Debug, Clone, PartialEq)]
struct Cmd {
    program: String,
    args: String,
}

/// Splits an UninstallString / ImagePath / Run value into program and arguments:
/// `"C:\x y\a.exe" /S`, unquoted `C:\x y\a.exe /S` (the longest prefix ending in `.exe` that exists;
/// the first one if none does), `MsiExec.exe /X{...}` and `RunDll32 x.dll,Entry`.
fn split_command(s: &str, exists: &dyn Fn(&str) -> bool) -> Option<Cmd> {
    let s = s.trim().trim_start_matches('\u{feff}');
    if s.is_empty() {
        return None;
    }
    let (program, args) = if let Some(rest) = s.strip_prefix('"') {
        match rest.find('"') {
            Some(i) => (rest[..i].trim().to_string(), rest[i + 1..].trim().to_string()),
            None => (rest.trim().to_string(), String::new()),
        }
    } else {
        let first = s.split(char::is_whitespace).next().unwrap_or("");
        // An unquoted program can't contain a quote, so only look for ".exe" before the first one
        // (`RunDll32 x.dll,Setup "C:\y\setup.exe"`). A bare first word (`MsiExec.exe`, `RunDll32`)
        // is the program itself.
        let region = if first.contains('\\') || first.contains(':') { s.split('"').next().unwrap_or(s) } else { first };
        let lower = region.to_ascii_lowercase();
        let mut ends: Vec<usize> = Vec::new();
        let mut from = 0;
        while let Some(i) = lower[from..].find(".exe") {
            let end = from + i + 4;
            let next = s[end..].chars().next();
            if next.map_or(true, |c| c.is_whitespace() || c == '"' || c == ',') {
                ends.push(end);
            }
            from = end;
        }
        let pick = ends.iter().rev().find(|&&e| exists(s[..e].trim())).or(ends.first()).copied();
        match pick {
            Some(e) => (s[..e].trim().to_string(), s[e..].trim().to_string()),
            None => {
                let mut it = s.splitn(2, char::is_whitespace);
                (it.next().unwrap_or("").to_string(), it.next().unwrap_or("").trim().to_string())
            }
        }
    };
    if program.is_empty() {
        return None;
    }
    Some(Cmd { program: resolve_program(&program), args })
}

/// `\??\C:\x`, `\SystemRoot\x`, `system32\x`, bare `msiexec` → absolute paths where possible.
fn resolve_program(p: &str) -> String {
    let mut p = p.trim().trim_matches('"').to_string();
    if let Some(rest) = p.strip_prefix(r"\??\") {
        p = rest.to_string();
    }
    let lower = p.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix(r"\systemroot\") {
        p = system_root().join(&p[p.len() - rest.len()..]).to_string_lossy().into_owned();
    } else if lower.starts_with(r"system32\") || lower.starts_with(r"syswow64\") {
        p = system_root().join(&p).to_string_lossy().into_owned();
    }
    p = reg::expand(&p);
    if !p.contains('\\') && !p.contains('/') && !p.contains(':') {
        let name = if Path::new(&p).extension().is_some() { p.clone() } else { format!("{p}.exe") };
        for dir in [system_root().join("System32"), system_root()] {
            let c = dir.join(&name);
            if c.is_file() {
                return c.to_string_lossy().into_owned();
            }
        }
    }
    p
}

fn file_name_lower(p: &str) -> String {
    Path::new(p).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn is_msiexec(c: &Cmd) -> bool {
    matches!(file_name_lower(&c.program).as_str(), "msiexec.exe" | "msiexec")
}

fn is_rundll(c: &Cmd) -> bool {
    matches!(file_name_lower(&c.program).as_str(), "rundll32.exe" | "rundll32")
}

/// The file whose presence decides whether this command can still run: the program, or the DLL
/// for rundll32. None for msiexec (Windows Installer keeps its own copy of the package).
fn command_target(c: &Cmd) -> Option<PathBuf> {
    if is_msiexec(c) {
        return None;
    }
    if is_rundll(c) {
        let a = c.args.trim();
        let dll = match a.strip_prefix('"') {
            Some(rest) => rest.split('"').next().unwrap_or(""),
            None => a.split(',').next().unwrap_or(""),
        };
        let dll = dll.trim();
        return (!dll.is_empty()).then(|| PathBuf::from(resolve_program(dll)));
    }
    Some(PathBuf::from(&c.program))
}

/// `{8-4-4-4-12}` product code inside a string.
fn find_guid(s: &str) -> Option<String> {
    let b = s.as_bytes();
    for i in 0..b.len() {
        if b[i] == b'{' && i + 38 <= b.len() && b[i + 37] == b'}' {
            let inner = &s[i + 1..i + 37];
            if is_bare_guid(inner) {
                return Some(s[i..i + 38].to_string());
            }
        }
    }
    None
}

fn is_bare_guid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() })
}

/// `{GUID}`, a bare GUID, 32 hex digits, optionally with Inno Setup's `_is1`.
fn is_guid_name(s: &str) -> bool {
    let s = s.trim();
    let s = s.strip_suffix("_is1").unwrap_or(s);
    let inner = s.strip_prefix('{').and_then(|x| x.strip_suffix('}')).unwrap_or(s);
    is_bare_guid(inner) || (inner.len() >= 32 && inner.chars().all(|c| c.is_ascii_hexdigit()))
}

/// `MsiExec.exe /I{code}` (repair/modify) → `%SystemRoot%\System32\msiexec.exe /X{code}` (uninstall);
/// any other msiexec command just gets the absolute msiexec path.
fn msi_rewrite(c: &Cmd) -> Cmd {
    if !is_msiexec(c) {
        return c.clone();
    }
    let program = system32("msiexec.exe").to_string_lossy().into_owned();
    match find_guid(&c.args) {
        Some(code) => Cmd { program, args: format!("/X{code}") },
        None => Cmd { program, args: c.args.clone() },
    }
}

/// `"C:\x\app.exe",0` → `C:\x\app.exe`.
fn icon_path(s: &str) -> Option<PathBuf> {
    let s = s.trim();
    let s = match s.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(""),
        None => match s.rfind(',') {
            Some(i) if s[i + 1..].trim().trim_start_matches('-').chars().all(|c| c.is_ascii_digit()) => &s[..i],
            _ => s,
        },
    };
    let p = PathBuf::from(resolve_program(s.trim()));
    p.is_absolute().then_some(p)
}

/// `"C:\x\app.exe",-101` -> -101; no index (or an .ico) -> 0.
fn icon_index(s: &str) -> i32 {
    let s = s.trim();
    let tail = match s.strip_prefix('"') {
        Some(rest) => rest.splitn(2, '"').nth(1).unwrap_or(""),
        None => s.rfind(',').map(|i| &s[i..]).unwrap_or(""),
    };
    tail.trim().trim_start_matches(',').trim().parse().unwrap_or(0)
}

/// "yyyymmdd" → "yyyy-mm-dd".
fn parse_install_date(s: &str) -> Option<String> {
    let s = s.trim();
    if s.len() == 8 && s.chars().all(|c| c.is_ascii_digit()) {
        let (y, m, d) = (&s[..4], &s[4..6], &s[6..]);
        let (mi, di): (u32, u32) = (m.parse().ok()?, d.parse().ok()?);
        if (1..=12).contains(&mi) && (1..=31).contains(&di) && y >= "1990" {
            return Some(format!("{y}-{m}-{d}"));
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Names and match tokens

/// Letters and digits only, lower-case (`+` and `#` kept so "Notepad++" isn't "Notepad").
fn norm_name(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric() || *c == '+' || *c == '#').flat_map(|c| c.to_lowercase()).collect()
}

/// Words that never identify one app on their own (compared normalized).
const GENERIC: &[&str] = &[
    "microsoft", "windows", "common", "commonfiles", "update", "updater", "updates", "setup", "installer", "install",
    "installation", "installshield", "installshieldinstallationinformation", "uninstall", "uninstaller", "uninst",
    "driver", "drivers", "runtime", "runtimes", "redistributable", "redist", "tools", "tool", "program", "programs",
    "programfiles", "programdata", "data", "temp", "tmp", "cache", "caches", "packages", "package", "packagecache",
    "google", "intel", "nvidia", "amd", "app", "apps", "application", "applications", "software", "system",
    "service", "services", "helper", "plugin", "plugins", "user", "users", "local", "roaming", "locallow", "config",
    "settings", "logs", "log", "bin", "files", "file", "shared", "library", "documents", "desktop", "games", "game",
    "client", "server", "launcher", "portable", "version", "edition", "professional", "standard", "home", "core",
    "framework", "sdk", "support", "assistant", "agent", "manager", "utility", "utilities", "components",
    "component", "module", "modules", "extension", "extensions", "addin", "addins", "cloud", "sync", "crashpad",
    "crashreports", "crashdumps", "default", "backup", "backups", "download", "downloads", "network", "security",
    "media", "player", "studio", "online", "office", "help", "free", "lite", "beta", "preview", "insider", "x64",
    "x86", "amd64", "arm64", "win32", "win64", "bit", "net", "dotnet", "java", "python", "node", "nodejs", "git",
    "steam", "steamapps", "origin", "epic", "store", "windowsapps", "modifiablewindowsapps", "package cache",
    "installer", "regid", "public", "default user", "all users", "start menu", "startmenu", "internet", "browser",
    "web", "mail", "chat", "music", "video", "videos", "pictures", "photos", "audio", "sound", "printer", "print",
    "scanner", "device", "devices", "hardware", "firmware", "bios", "monitor", "display", "graphics", "control",
    "controlpanel", "panel", "center", "centre", "hub", "suite", "pack", "packs", "kit", "toolkit", "toolbox",
];

fn is_generic(norm: &str) -> bool {
    GENERIC.iter().any(|g| norm_name(g) == norm)
}

/// A usable match token: at least 4 letters/digits, not a GUID, not all digits, and not made only of
/// generic words.
fn usable(s: &str) -> Option<String> {
    let s = s.trim();
    let n = norm_name(s);
    if n.chars().count() < 4 || is_guid_name(s) || n.chars().all(|c| c.is_ascii_digit()) || is_generic(&n) {
        return None;
    }
    let words: Vec<String> = s.split(|c: char| c.is_whitespace() || c == '-' || c == '_').map(norm_name).filter(|w| !w.is_empty()).collect();
    if !words.is_empty() && words.iter().all(|w| is_generic(w) || w.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    Some(s.to_string())
}

fn is_version_word(w: &str) -> bool {
    let lw = w.to_ascii_lowercase();
    let lw = lw.trim_matches(|c: char| matches!(c, ',' | ';' | ':' | '(' | ')' | '[' | ']'));
    if lw.is_empty() {
        return false;
    }
    if matches!(
        lw,
        "x64" | "x86" | "amd64" | "arm64" | "x86_64" | "64-bit" | "32-bit" | "64bit" | "32bit" | "win64" | "win32" | "version" | "ver" | "build" | "-" | "–" | "—"
    ) {
        return true;
    }
    // Language tags: en-us, de-DE.
    let b = lw.as_bytes();
    if b.len() == 5 && b[2] == b'-' && b[..2].iter().all(u8::is_ascii_alphabetic) && b[3..].iter().all(u8::is_ascii_alphabetic) {
        return true;
    }
    let body = lw.strip_prefix('v').unwrap_or(lw);
    let starts_digit = body.chars().next().is_some_and(|c| c.is_ascii_digit());
    if !starts_digit {
        return false;
    }
    // Dotted versions (1.2.3, 23.01, 120.0b1): always a version.
    if body.contains('.') && body.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_') {
        return true;
    }
    if body.chars().all(|c| c.is_ascii_digit()) {
        // v7 / v12: version. Plain 4-digit years (Office 2016, Visual Studio 2019) are part of the name.
        if lw.starts_with('v') {
            return true;
        }
        let n = body.len();
        return !(n == 4 && (body >= "1990" && body <= "2099"));
    }
    false
}

/// "Mozilla Firefox (x64 en-US)" → "Mozilla Firefox"; "7-Zip 23.01 (x64)" → "7-Zip";
/// "Python 3.11.4 (64-bit)" → "Python"; "Foo Bar version 2.0" → "Foo Bar".
fn clean_display_name(s: &str) -> String {
    // Drop bracketed groups: "(x64)", "[2.1]", "(remove only)".
    let mut out = String::new();
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = (depth - 1).max(0),
            '®' | '™' | '©' => {}
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let words: Vec<&str> = out.split_whitespace().collect();
    // Versions and architecture words go wherever they are; everything after the first version word
    // that looks like a build/edition suffix ("- 14.38.33130") goes too.
    let mut kept: Vec<&str> = Vec::new();
    for w in words {
        if is_version_word(w) {
            continue;
        }
        kept.push(w);
    }
    let joined = kept.join(" ");
    joined.trim_matches(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '-' | '–' | '—' | '.')).to_string()
}

/// "Google LLC" → "Google"; "Valve Corporation" → "Valve"; "Foo Co., Ltd." → "Foo".
fn clean_publisher(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = (depth - 1).max(0),
            '®' | '™' | '©' => {}
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    const SUFFIX: &[&str] = &[
        "inc", "incorporated", "llc", "ltd", "limited", "corp", "corporation", "co", "company", "gmbh", "ag", "sa", "s.a",
        "srl", "s.r.l", "bv", "b.v", "plc", "pty", "oy", "ab", "kk", "lp", "l.p", "sarl", "spa", "s.p.a", "nv", "n.v",
        "sas", "as", "a/s", "aps", "kg", "se", "group", "holdings", "the",
    ];
    let mut words: Vec<String> = out
        .split(|c: char| c.is_whitespace() || c == ',')
        .map(|w| w.trim_matches(|c: char| c == '.' || c == ',').to_string())
        .filter(|w| !w.is_empty())
        .collect();
    while words.len() > 1 && SUFFIX.contains(&words.last().map(|w| w.to_ascii_lowercase()).unwrap_or_default().as_str()) {
        words.pop();
    }
    if words.len() > 1 && words[0].eq_ignore_ascii_case("the") {
        words.remove(0);
    }
    words.join(" ")
}

#[derive(Debug, Clone, Default)]
struct Tokens {
    /// Normalized name tokens (display name, name without publisher, key name, folder leaves).
    names: Vec<String>,
    /// Normalized publisher, if one may be used to descend into `<Publisher>\<Name>`.
    publisher: Option<String>,
    /// The publisher folder itself may be offered (when every child is this app's).
    publisher_folder_ok: bool,
}

impl Tokens {
    fn has(&self, n: &str) -> bool {
        !n.is_empty() && self.names.iter().any(|t| t == n)
    }
    /// Name-match for services and tasks: equal, or containing a long (6+) token.
    fn loosely_named(&self, s: &str) -> bool {
        let n = norm_name(s);
        self.has(&n) || self.names.iter().any(|t| t.chars().count() >= 6 && n.contains(t.as_str()))
    }
}

/// A folder leaf that can stand for the app ("Foo" in `C:\Program Files\Foo`), rejecting
/// installer-cache and generic folders.
fn folder_leaf_token(dir: &Path) -> Option<String> {
    let leaf = dir.file_name()?.to_string_lossy().into_owned();
    usable(&leaf)
}

fn app_tokens(display: &str, publisher: &str, key_name: &str, location: Option<&Path>, uninstaller_dir: Option<&Path>) -> Tokens {
    let mut names: Vec<String> = Vec::new();
    let mut add = |s: Option<String>| {
        if let Some(s) = s {
            let n = norm_name(&s);
            if !names.contains(&n) {
                names.push(n);
            }
        }
    };
    let name = clean_display_name(display);
    add(usable(&name));
    let pubc = clean_publisher(publisher);
    // "Mozilla Firefox" by "Mozilla" → "Firefox" (as in AppData\Roaming\Mozilla\Firefox).
    if !pubc.is_empty() {
        let lower = name.to_lowercase();
        let pl = pubc.to_lowercase();
        if let Some(rest) = lower.strip_prefix(&pl) {
            if rest.starts_with(' ') {
                add(usable(&name[name.len() - rest.len()..]));
            }
        }
        // First word of the publisher too ("Mozilla Corporation" → "Mozilla").
        let first = pl.split_whitespace().next().unwrap_or("");
        if first != pl && first.len() >= 4 {
            if let Some(rest) = lower.strip_prefix(first) {
                if rest.starts_with(' ') {
                    add(usable(&name[name.len() - rest.len()..]));
                }
            }
        }
    }
    let key = key_name.strip_suffix("_is1").unwrap_or(key_name);
    if !is_guid_name(key) {
        add(usable(&clean_display_name(key)));
    }
    let pn = norm_name(&pubc);
    // A folder named after the publisher ("Program Files\HP" as an InstallLocation) stands for the
    // vendor, not for this app: it stays a publisher folder, with the publisher-folder rules.
    let not_publisher = |t: &String| norm_name(t) != pn;
    add(location.and_then(folder_leaf_token).filter(not_publisher));
    add(uninstaller_dir.and_then(folder_leaf_token).filter(not_publisher));
    let publisher = (pn.len() >= 2 && pn != "microsoft" && !names.contains(&pn)).then_some(pn.clone());
    let publisher_folder_ok = publisher.is_some() && usable(&pubc).is_some();
    Tokens { names, publisher, publisher_folder_ok }
}

// ---------------------------------------------------------------------------------------------
// Installed apps

#[derive(Clone, Debug)]
struct AppRecord {
    app: InstalledApp,
    key: RegKey,
    key_name: String,
    display_name: String,
    publisher: String,
    cmd: Option<Cmd>,
    /// The file whose presence decides canUninstall (None for msiexec).
    target: Option<PathBuf>,
    /// InstallLocation as written, even if the folder is gone.
    location: Option<PathBuf>,
    icon: Option<PathBuf>,
    /// The icon's index in DisplayIcon ("path,index"; negative = resource id). 0 if none given.
    icon_index: i32,
}

impl AppRecord {
    fn tokens(&self) -> Tokens {
        let udir = self.target.as_ref().and_then(|t| t.parent()).filter(|d| !in_windows(d));
        app_tokens(&self.display_name, &self.publisher, &self.key_name, self.location.as_deref(), udir)
    }
    fn uninstaller_present(&self) -> bool {
        match (&self.cmd, &self.target) {
            (None, _) => false,
            (Some(_), None) => true, // msiexec
            (Some(_), Some(t)) => presence(t) != Presence::Missing,
        }
    }
}

#[derive(Clone, Debug)]
struct UninstallRoot {
    key: RegKey,
    scope: &'static str,
    /// Id prefix: "lm64", "lm32", "cu" (tests use their own).
    tag: &'static str,
}

fn real_uninstall_roots() -> Vec<UninstallRoot> {
    vec![
        UninstallRoot { key: RegKey::new(Hive::Lm, &format!(r"SOFTWARE\{UNINSTALL_SUBKEY}")), scope: "machine", tag: "lm64" },
        UninstallRoot { key: RegKey::new(Hive::Lm, &format!(r"SOFTWARE\WOW6432Node\{UNINSTALL_SUBKEY}")), scope: "machine", tag: "lm32" },
        UninstallRoot { key: RegKey::new(Hive::Cu, &format!(r"Software\{UNINSTALL_SUBKEY}")), scope: "user", tag: "cu" },
    ]
}

/// Reads one Uninstall entry. None when it isn't shown (no name, system component, update...).
fn read_entry(root: &UninstallRoot, key_name: &str) -> Option<AppRecord> {
    let key = root.key.child(key_name);
    let k = reg::open_read(&key)?;
    let name = k.string("DisplayName")?;
    if k.dword("SystemComponent") == Some(1) || k.string("ParentKeyName").is_some() {
        return None;
    }
    if let Some(rt) = k.string("ReleaseType") {
        if matches!(rt.to_ascii_lowercase().as_str(), "update" | "hotfix" | "security update") {
            return None;
        }
    }
    let publisher = k.string("Publisher").unwrap_or_default();
    let version = k.string("DisplayVersion").unwrap_or_default();
    let mut uninstall = k.string("UninstallString");
    // MSI products sometimes have only WindowsInstaller=1 and the product code as key name.
    if uninstall.is_none() && k.dword("WindowsInstaller") == Some(1) {
        if let Some(code) = find_guid(key_name) {
            uninstall = Some(format!("MsiExec.exe /X{code}"));
        }
    }
    let exists = |p: &str| Path::new(p).is_file();
    let cmd = uninstall.as_deref().and_then(|u| split_command(u, &exists));
    let target = cmd.as_ref().and_then(command_target);
    let location = k
        .string("InstallLocation")
        .map(|s| s.trim().trim_matches('"').trim_end_matches('\\').to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    let display_icon = k.string("DisplayIcon");
    let icon = display_icon.as_deref().and_then(icon_path);
    let icon_index = display_icon.as_deref().map(icon_index).unwrap_or(0);
    let no_remove = k.dword("NoRemove") == Some(1);
    let present = match (&cmd, &target) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(_), Some(t)) => presence(t) != Presence::Missing,
    };
    let app = InstalledApp {
        id: format!("{}|{}", root.tag, key_name),
        name: name.clone(),
        publisher: publisher.clone(),
        version,
        install_date: k.string("InstallDate").and_then(|d| parse_install_date(&d)),
        size: k.dword("EstimatedSize").map(|kb| kb as u64 * 1024).unwrap_or(0),
        install_location: location.as_ref().filter(|p| p.is_dir()).map(|p| p.to_string_lossy().into_owned()),
        scope: root.scope,
        can_uninstall: present && !no_remove,
        broken: cmd.is_some() && !present,
    };
    Some(AppRecord { app, key, key_name: key_name.to_string(), display_name: name, publisher, cmd, target, location, icon, icon_index })
}

/// Every shown entry under `roots`, without the 32-bit duplicates of 64-bit machine entries, by name.
fn read_apps(roots: &[UninstallRoot]) -> Vec<AppRecord> {
    let mut out: Vec<AppRecord> = Vec::new();
    let mut seen_machine: HashSet<(String, String)> = HashSet::new();
    for root in roots {
        let Some(k) = reg::open_read(&root.key) else { continue };
        for name in k.subkeys() {
            let Some(rec) = read_entry(root, &name) else { continue };
            if root.scope == "machine" {
                let dup = (rec.app.name.to_lowercase(), rec.app.version.to_lowercase());
                if !seen_machine.insert(dup) {
                    continue;
                }
            }
            out.push(rec);
        }
    }
    out.sort_by(|a, b| a.app.name.to_lowercase().cmp(&b.app.name.to_lowercase()).then(a.app.id.cmp(&b.app.id)));
    out
}

/// Every app ever listed in this session, by id: `app_leftovers` must still work after the entry
/// is gone (that is the point of looking for leftovers after an uninstall).
static APPS: Mutex<Option<HashMap<String, AppRecord>>> = Mutex::new(None);

fn remember(apps: &[AppRecord]) {
    let mut g = APPS.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.get_or_insert_with(HashMap::new);
    for a in apps {
        map.insert(a.app.id.clone(), a.clone());
    }
}

fn remembered(id: &str) -> Option<AppRecord> {
    APPS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(id).cloned())
}

// ---------------------------------------------------------------------------------------------
// Services (SCM)

#[derive(Clone, Debug)]
struct SvcInfo {
    name: String,
    display: String,
    image: String,
    program: Option<PathBuf>,
}

/// ImagePath → the program file (`\SystemRoot\...`, `\??\C:\...`, unquoted paths with spaces).
fn service_program(image: &str) -> Option<PathBuf> {
    let exists = |p: &str| Path::new(&resolve_program(p)).is_file();
    let c = split_command(image, &exists)?;
    let p = PathBuf::from(&c.program);
    p.is_absolute().then_some(p)
}

fn pwstr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
}

struct ScHandle(windows_sys::Win32::System::Services::SC_HANDLE);
impl Drop for ScHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { windows_sys::Win32::System::Services::CloseServiceHandle(self.0) };
        }
    }
}

fn open_scm(access: u32) -> Option<ScHandle> {
    let h = unsafe { windows_sys::Win32::System::Services::OpenSCManagerW(std::ptr::null(), std::ptr::null(), access) };
    (!h.is_null()).then(|| ScHandle(h))
}

fn service_image(scm: &ScHandle, name: &str) -> Option<(String, String)> {
    use windows_sys::Win32::System::Services::*;
    let n = wide(name);
    let h = ScHandle(unsafe { OpenServiceW(scm.0, n.as_ptr(), SERVICE_QUERY_CONFIG) });
    if h.0.is_null() {
        return None;
    }
    let mut needed = 0u32;
    unsafe { QueryServiceConfigW(h.0, std::ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return None;
    }
    let mut buf = vec![0u64; (needed as usize).div_ceil(8) + 1];
    let ok = unsafe { QueryServiceConfigW(h.0, buf.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW, (buf.len() * 8) as u32, &mut needed) };
    if ok == 0 {
        return None;
    }
    let cfg = unsafe { &*(buf.as_ptr() as *const QUERY_SERVICE_CONFIGW) };
    Some((pwstr(cfg.lpBinaryPathName), pwstr(cfg.lpDisplayName)))
}

/// Win32 services only (no drivers), with their program files. Works unelevated.
fn list_services() -> Vec<SvcInfo> {
    use windows_sys::Win32::System::Services::*;
    let Some(scm) = open_scm(SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE) else { return Vec::new() };
    let mut names: Vec<(String, String)> = Vec::new();
    let mut resume = 0u32;
    let mut buf = vec![0u64; 32 * 1024];
    for _ in 0..64 {
        let (mut needed, mut count) = (0u32, 0u32);
        let ok = unsafe {
            EnumServicesStatusExW(
                scm.0,
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32,
                SERVICE_STATE_ALL,
                buf.as_mut_ptr() as *mut u8,
                (buf.len() * 8) as u32,
                &mut needed,
                &mut count,
                &mut resume,
                std::ptr::null(),
            )
        };
        let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        let recs = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW, count as usize) };
        for r in recs {
            names.push((pwstr(r.lpServiceName), pwstr(r.lpDisplayName)));
        }
        if ok != 0 {
            break;
        }
        if err != 234 {
            break; // not ERROR_MORE_DATA
        }
        if count == 0 && (needed as usize) > buf.len() * 8 {
            buf.resize((needed as usize).div_ceil(8) + 1, 0);
        }
    }
    names
        .into_iter()
        .map(|(name, display)| {
            let (image, disp2) = service_image(&scm, &name).unwrap_or_default();
            let program = service_program(&image);
            SvcInfo { display: if display.is_empty() { disp2 } else { display }, name, image, program }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Scheduled tasks (Task Scheduler 2.0 COM)

#[derive(Clone, Debug, Default)]
struct TaskInfo {
    /// `\Vendor\Task Name`
    path: String,
    name: String,
    /// Exec-action programs, environment expanded for the task's own user (see
    /// [`resolve_task_program`]); left as written when that user's folders can't be known.
    programs: Vec<String>,
    user: String,
}

impl TaskInfo {
    fn is_microsoft(&self) -> bool {
        self.path.to_ascii_lowercase().starts_with(r"\microsoft\")
    }
}

/// Environment variables that are the same for every account, so Platter's own values are right.
const MACHINE_VARS: &[&str] = &[
    "systemroot", "windir", "systemdrive", "programfiles", "programfiles(x86)", "programw6432", "commonprogramfiles",
    "commonprogramfiles(x86)", "commonprogramw6432", "programdata", "allusersprofile", "public", "comspec", "driverdata",
];

/// `%NAME%` references in `s`, lower-case.
fn env_vars_in(s: &str) -> Vec<String> {
    let parts: Vec<&str> = s.split('%').collect();
    // Between the 1st and 2nd '%', the 3rd and 4th...: odd-indexed parts that have a closing '%'.
    (1..parts.len().saturating_sub(1)).step_by(2).map(|i| parts[i].to_lowercase()).filter(|v| !v.is_empty()).collect()
}

/// A task's exec path with its variables filled in for the account the task runs as, not for
/// Platter's: `%LOCALAPPDATA%` in another user's task (or in the real user's, when Platter was
/// elevated with a different admin account) would otherwise point into the wrong profile, look
/// missing, and get the task offered as "its program file is gone".
/// * only machine-wide variables: expanded as usual;
/// * per-user ones, task runs as Platter's own user (`me`): expanded as usual;
/// * per-user ones, task runs as a SID with a known profile (`profile_of`): that profile's folders;
/// * otherwise left as written, which is never treated as present, missing or inside anything.
fn resolve_task_program(raw: &str, principal: &str, me: &str, profile_of: &dyn Fn(&str) -> Option<PathBuf>) -> String {
    let vars = env_vars_in(raw);
    if vars.iter().all(|v| MACHINE_VARS.contains(&v.as_str())) || task_owned_by(principal, me) {
        return reg::expand(raw);
    }
    let sid = principal.trim();
    let profile = if sid.len() > 4 && sid[..4].eq_ignore_ascii_case("S-1-") { profile_of(sid) } else { None };
    let Some(profile) = profile else { return raw.to_string() };
    let user_dir = |v: &str| -> Option<PathBuf> {
        Some(match v {
            "userprofile" => profile.clone(),
            "localappdata" => profile.join(r"AppData\Local"),
            "appdata" => profile.join(r"AppData\Roaming"),
            "temp" | "tmp" => profile.join(r"AppData\Local\Temp"),
            _ => return None,
        })
    };
    let mut out = String::new();
    let parts: Vec<&str> = raw.split('%').collect();
    let mut i = 0;
    while i < parts.len() {
        out.push_str(parts[i]);
        // parts[i + 1] is a variable name when a closing '%' follows it.
        if i + 2 < parts.len() {
            let v = parts[i + 1].to_lowercase();
            if MACHINE_VARS.contains(&v.as_str()) {
                out.push_str(&reg::expand(&format!("%{}%", parts[i + 1])));
            } else if let Some(d) = user_dir(&v) {
                out.push_str(&d.to_string_lossy());
            } else {
                return raw.to_string();
            }
            i += 2;
        } else {
            if i + 1 < parts.len() {
                out.push('%');
            }
            i += 1;
        }
    }
    out
}

/// A user's profile folder by SID string (`ProfileList\<SID>\ProfileImagePath`).
fn profile_of_sid(sid: &str) -> Option<PathBuf> {
    let k = RegKey::new(Hive::Lm, &format!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\{sid}"));
    reg::open_read(&k)?.string("ProfileImagePath").map(PathBuf::from).filter(|p| p.is_absolute())
}

mod tasks {
    use super::*;
    use windows_sys::core::{GUID, HRESULT};

    const CLSID_TASK_SCHEDULER: GUID = GUID::from_u128(0x0f87369f_a4e5_4cfc_bd3e_73e6154572dd);
    const IID_ITASK_SERVICE: GUID = GUID::from_u128(0x2faba4c7_4da9_4013_9697_20cc3fd40f85);
    const IID_IEXEC_ACTION: GUID = GUID::from_u128(0x4c3d624d_fd6b_49a3_b9b7_09cb3cd3f047);
    const TASK_ENUM_HIDDEN: i32 = 1;

    // Vtable slots after IUnknown (0-2) and IDispatch (3-6):
    // ITaskService: GetFolder 7, Connect 10.
    // ITaskFolder: get_Path 8, GetFolder 9, GetFolders 10, GetTask 13, GetTasks 14, DeleteTask 15.
    // Collections (ITaskFolderCollection / IRegisteredTaskCollection / IActionCollection): get_Count 7, get_Item 8.
    // IRegisteredTask: get_Name 7, get_Path 8, get_Definition 19, get_Xml 20.
    // ITaskDefinition: get_Principal 15, get_Actions 17. IPrincipal: get_UserId 11.
    // IAction: get_Type 9. IExecAction: get_Path 10.
    type Connect = unsafe extern "system" fn(*mut c_void, EmptyVariant, EmptyVariant, EmptyVariant, EmptyVariant) -> HRESULT;
    type GetByName = unsafe extern "system" fn(*mut c_void, *const u16, *mut *mut c_void) -> HRESULT;
    type GetObj = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT;
    type GetColl = unsafe extern "system" fn(*mut c_void, i32, *mut *mut c_void) -> HRESULT;
    type GetI32 = unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT;
    type GetBstr = unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT;
    type ItemByVariant = unsafe extern "system" fn(*mut c_void, VarI4, *mut *mut c_void) -> HRESULT;
    type ItemByLong = unsafe extern "system" fn(*mut c_void, i32, *mut *mut c_void) -> HRESULT;
    type QueryInterface = unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
    type DeleteTask = unsafe extern "system" fn(*mut c_void, *const u16, i32) -> HRESULT;

    /// A VT_I4 VARIANT passed by value (collection indexes are 1-based).
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct VarI4 {
        vt: u16,
        r: [u16; 3],
        val: u64,
        pad: u64,
    }
    fn var_i4(i: i32) -> VarI4 {
        VarI4 { vt: 3, r: [0; 3], val: i as u32 as u64, pad: 0 }
    }

    pub fn take_bstr(b: *mut u16) -> String {
        if b.is_null() {
            return String::new();
        }
        let len = unsafe { *(b as *const u32).offset(-1) } as usize / 2;
        let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(b, len) });
        unsafe { windows_sys::Win32::Foundation::SysFreeString(b) };
        s
    }

    fn bstr(obj: &Unk, slot: usize) -> String {
        let mut b: *mut u16 = std::ptr::null_mut();
        let hr = unsafe { com::method::<GetBstr>(obj.0, slot)(obj.0, &mut b) };
        if hr < 0 {
            return String::new();
        }
        take_bstr(b)
    }

    fn obj(o: &Unk, slot: usize) -> Option<Unk> {
        let mut p = std::ptr::null_mut();
        let hr = unsafe { com::method::<GetObj>(o.0, slot)(o.0, &mut p) };
        (hr >= 0 && !p.is_null()).then(|| Unk(p))
    }

    fn count(c: &Unk) -> i32 {
        let mut n = 0i32;
        let hr = unsafe { com::method::<GetI32>(c.0, 7)(c.0, &mut n) };
        if hr < 0 {
            0
        } else {
            n.clamp(0, 100_000)
        }
    }

    fn item(c: &Unk, i: i32) -> Option<Unk> {
        let mut p = std::ptr::null_mut();
        let hr = unsafe { com::method::<ItemByVariant>(c.0, 8)(c.0, var_i4(i), &mut p) };
        (hr >= 0 && !p.is_null()).then(|| Unk(p))
    }

    fn by_name(parent: &Unk, slot: usize, name: &str) -> Result<Unk, HRESULT> {
        let n = Bstr::new(name);
        let mut p = std::ptr::null_mut();
        let hr = unsafe { com::method::<GetByName>(parent.0, slot)(parent.0, n.0, &mut p) };
        if hr >= 0 && !p.is_null() {
            Ok(Unk(p))
        } else {
            Err(hr)
        }
    }

    pub fn connect() -> Result<Unk, HRESULT> {
        let svc = com::create(&CLSID_TASK_SCHEDULER, &IID_ITASK_SERVICE)?;
        let hr = unsafe { com::method::<Connect>(svc.0, 10)(svc.0, EMPTY, EMPTY, EMPTY, EMPTY) };
        if hr < 0 {
            return Err(hr);
        }
        Ok(svc)
    }

    fn task_info(t: &Unk, me: &str) -> TaskInfo {
        let mut info = TaskInfo { name: bstr(t, 7), path: bstr(t, 8), ..Default::default() };
        let Some(def) = obj(t, 19) else { return info };
        if let Some(pr) = obj(&def, 15) {
            info.user = bstr(&pr, 11);
        }
        let Some(actions) = obj(&def, 17) else { return info };
        for i in 1..=count(&actions) {
            let mut a = std::ptr::null_mut();
            let hr = unsafe { com::method::<ItemByLong>(actions.0, 8)(actions.0, i, &mut a) };
            if hr < 0 || a.is_null() {
                continue;
            }
            let a = Unk(a);
            let mut ty = -1i32;
            if unsafe { com::method::<GetI32>(a.0, 9)(a.0, &mut ty) } < 0 || ty != 0 {
                continue; // not TASK_ACTION_EXEC
            }
            let mut e = std::ptr::null_mut();
            let hr = unsafe { com::method::<QueryInterface>(a.0, 0)(a.0, &IID_IEXEC_ACTION, &mut e) };
            if hr < 0 || e.is_null() {
                continue;
            }
            let e = Unk(e);
            let p = bstr(&e, 10);
            let p = resolve_task_program(p.trim().trim_matches('"'), &info.user, me, &profile_of_sid);
            if !p.is_empty() {
                info.programs.push(p);
            }
        }
        info
    }

    /// Every task in every folder (hidden ones too). Folders that can't be read (unelevated:
    /// some under \Microsoft\) are returned in the second list.
    pub fn list() -> Result<(Vec<TaskInfo>, Vec<String>), HRESULT> {
        let svc = connect()?;
        let root = by_name(&svc, 7, "\\")?;
        let mut out = Vec::new();
        let mut denied = Vec::new();
        let me = current_user_name();
        let mut stack = vec![root];
        let mut guard = 0;
        while let Some(f) = stack.pop() {
            guard += 1;
            if guard > 20_000 {
                break;
            }
            let fpath = bstr(&f, 8);
            let mut coll = std::ptr::null_mut();
            let hr = unsafe { com::method::<GetColl>(f.0, 14)(f.0, TASK_ENUM_HIDDEN, &mut coll) };
            if hr >= 0 && !coll.is_null() {
                let coll = Unk(coll);
                for i in 1..=count(&coll) {
                    if let Some(t) = item(&coll, i) {
                        out.push(task_info(&t, &me));
                    }
                }
            } else {
                denied.push(fpath.clone());
            }
            let mut subs = std::ptr::null_mut();
            let hr = unsafe { com::method::<GetColl>(f.0, 10)(f.0, 0, &mut subs) };
            if hr >= 0 && !subs.is_null() {
                let subs = Unk(subs);
                for i in 1..=count(&subs) {
                    if let Some(s) = item(&subs, i) {
                        stack.push(s);
                    }
                }
            }
        }
        Ok((out, denied))
    }

    fn split_path(path: &str) -> (String, String) {
        match path.rfind('\\') {
            Some(0) | None => ("\\".to_string(), path.trim_start_matches('\\').to_string()),
            Some(i) => (path[..i].to_string(), path[i + 1..].to_string()),
        }
    }

    /// The task's XML (for the backup) and its exec programs, or None if it no longer exists.
    pub fn get(path: &str) -> Option<(String, TaskInfo)> {
        let svc = connect().ok()?;
        let (folder, name) = split_path(path);
        let f = by_name(&svc, 7, &folder).ok()?;
        let t = by_name(&f, 13, &name).ok()?;
        Some((bstr(&t, 20), task_info(&t, &current_user_name())))
    }

    pub fn delete(path: &str) -> Result<(), HRESULT> {
        let svc = connect()?;
        let (folder, name) = split_path(path);
        let f = by_name(&svc, 7, &folder)?;
        let n = Bstr::new(&name);
        let hr = unsafe { com::method::<DeleteTask>(f.0, 15)(f.0, n.0, 0) };
        if hr < 0 {
            Err(hr)
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Startup (Run/RunOnce) values and App Paths entries

#[derive(Clone, Debug)]
struct ValueRef {
    key: RegKey,
    name: String,
    data: String,
    program: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct AppPathRef {
    key: RegKey,
    program: Option<PathBuf>,
}

fn real_run_keys() -> Vec<RegKey> {
    let mut v = Vec::new();
    for (hive, base) in [(Hive::Cu, "Software"), (Hive::Lm, "SOFTWARE"), (Hive::Lm, r"SOFTWARE\WOW6432Node")] {
        for leaf in ["Run", "RunOnce"] {
            v.push(RegKey::new(hive, &format!(r"{base}\Microsoft\Windows\CurrentVersion\{leaf}")));
        }
    }
    v
}

fn real_app_paths_keys() -> Vec<RegKey> {
    vec![
        RegKey::new(Hive::Cu, &format!(r"Software\{APP_PATHS_SUBKEY}")),
        RegKey::new(Hive::Lm, &format!(r"SOFTWARE\{APP_PATHS_SUBKEY}")),
        RegKey::new(Hive::Lm, &format!(r"SOFTWARE\WOW6432Node\{APP_PATHS_SUBKEY}")),
    ]
}

fn exists_file(p: &str) -> bool {
    Path::new(&resolve_program(p)).is_file()
}

fn program_of(data: &str) -> Option<PathBuf> {
    let c = split_command(data, &exists_file)?;
    command_target(&c).filter(|p| p.is_absolute())
}

fn list_startup(keys: &[RegKey]) -> Vec<ValueRef> {
    let mut out = Vec::new();
    for key in keys {
        let Some(k) = reg::open_read(key) else { continue };
        for name in k.value_names() {
            if name.is_empty() {
                continue;
            }
            let Some(data) = k.string(&name) else { continue };
            out.push(ValueRef { key: key.clone(), program: program_of(&data), name, data });
        }
    }
    out
}

fn list_app_paths(keys: &[RegKey]) -> Vec<AppPathRef> {
    let mut out = Vec::new();
    for key in keys {
        let Some(k) = reg::open_read(key) else { continue };
        for name in k.subkeys() {
            let entry = key.child(&name);
            let program = reg::open_read(&entry)
                .and_then(|e| e.string(""))
                .map(|s| PathBuf::from(s.trim().trim_matches('"')))
                .filter(|p| p.is_absolute());
            out.push(AppPathRef { key: entry, program });
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Folder walk

#[derive(Default, Debug, Clone)]
struct WalkStats {
    size: u64,
    files: u64,
    has_binary: bool,
    newest: Option<SystemTime>,
    truncated: bool,
}

const ATTR_OFFLINE: u32 = 0x1000;
const ATTR_RECALL_ON_OPEN: u32 = 0x4_0000;
const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

/// Sizes a folder with an explicit stack (no recursion). Junctions/symlinks (name-surrogate reparse
/// points) are never followed; cloud placeholders count 0 and aren't opened (that could download
/// them). `stop` ends the walk early (orphan checks stop at the first program or recent file).
fn walk(dir: &Path, max_entries: u64, stop: &dyn Fn(&WalkStats) -> bool) -> WalkStats {
    let mut st = WalkStats::default();
    let mut stack = vec![dir.to_path_buf()];
    let mut seen = 0u64;
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > max_entries {
                st.truncated = true;
                return st;
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            // On Windows, DirEntry::metadata comes from the directory listing itself (no extra open).
            let Ok(md) = e.metadata() else { continue };
            let attrs = md.file_attributes();
            if attrs & (ATTR_OFFLINE | ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS) != 0 {
                continue;
            }
            if ft.is_dir() {
                stack.push(e.path());
                continue;
            }
            st.files += 1;
            st.size += md.len();
            if let Ok(m) = md.modified() {
                if st.newest.map_or(true, |n| m > n) {
                    st.newest = Some(m);
                }
            }
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            if name.ends_with(".exe") || name.ends_with(".dll") {
                st.has_binary = true;
            }
            if stop(&st) {
                st.truncated = true;
                return st;
            }
        }
    }
    st
}

// ---------------------------------------------------------------------------------------------
// Context: what is installed and registered right now

/// Where leftovers are looked for. Injectable so tests never touch real app folders.
#[derive(Clone, Debug)]
struct Roots {
    /// Machine-wide folders: `<Name>` and `<Publisher>\<Name>` are looked for inside.
    machine: Vec<PathBuf>,
    /// Per-user folders (AppData...).
    user: Vec<PathBuf>,
    /// Folders whose direct children are checked for orphans (path, machine-wide).
    orphan_dirs: Vec<(PathBuf, bool)>,
    /// Registry bases: `<Name>` and `<Publisher>\<Name>` below them.
    reg_bases: Vec<RegKey>,
    run_keys: Vec<RegKey>,
    app_paths_keys: Vec<RegKey>,
    uninstall: Vec<UninstallRoot>,
    /// Ask the SCM and the Task Scheduler (off in tests).
    system: bool,
    /// Where registry/task/service backups go (a timestamped folder is made inside).
    backup_base: PathBuf,
}

impl Roots {
    fn real() -> Roots {
        let pf: Vec<PathBuf> = ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"].iter().filter_map(|v| env_dir(v)).collect();
        let mut machine: Vec<PathBuf> = Vec::new();
        for p in pf {
            if !machine.iter().any(|m| norm_path(m) == norm_path(&p)) {
                machine.push(p);
            }
        }
        let pd = env_dir("ProgramData");
        let local = env_dir("LOCALAPPDATA");
        let mut orphan_dirs: Vec<(PathBuf, bool)> = machine.iter().map(|p| (p.clone(), true)).collect();
        if let Some(l) = &local {
            orphan_dirs.push((l.join("Programs"), false));
        }
        if let Some(p) = &pd {
            orphan_dirs.push((p.clone(), true));
            machine.push(p.clone());
        }
        let mut user: Vec<PathBuf> = Vec::new();
        user.extend(env_dir("APPDATA"));
        if let Some(l) = &local {
            user.push(l.clone());
            user.push(l.join("Programs"));
        }
        user.extend(known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_LocalAppDataLow).or_else(|| env_dir("USERPROFILE").map(|p| p.join(r"AppData\LocalLow"))));
        Roots {
            machine,
            user,
            orphan_dirs,
            reg_bases: vec![RegKey::new(Hive::Cu, "Software"), RegKey::new(Hive::Lm, "SOFTWARE"), RegKey::new(Hive::Lm, r"SOFTWARE\WOW6432Node")],
            run_keys: real_run_keys(),
            app_paths_keys: real_app_paths_keys(),
            uninstall: real_uninstall_roots(),
            system: true,
            backup_base: local.unwrap_or_else(std::env::temp_dir).join("app.platter.disks").join("backups"),
        }
    }

    fn is_machine_path(&self, p: &Path) -> bool {
        let f = norm_path(p);
        self.machine.iter().any(|m| within(&f, &norm_path(m))) || in_windows(p)
    }

    /// The search roots themselves are never offered.
    fn is_root(&self, p: &Path) -> bool {
        let f = norm_path(p);
        self.machine.iter().chain(self.user.iter()).chain(self.orphan_dirs.iter().map(|(p, _)| p)).any(|r| norm_path(r) == f)
    }
}

struct Context {
    apps: Vec<AppRecord>,
    services: Vec<SvcInfo>,
    tasks: Vec<TaskInfo>,
    startup: Vec<ValueRef>,
    app_paths: Vec<AppPathRef>,
    protected: Vec<String>,
    user_name: String,
    /// Normalized name parts of Store (MSIX) packages: not listed as apps, but their folders and
    /// names count as "in use".
    msix: Vec<String>,
    /// Folders named in environment variables (PATH, JAVA_HOME, ChocolateyInstall...).
    env_dirs: Vec<PathBuf>,
}

/// Store packages for this user and provisioned for all users, as normalized name parts:
/// "SerifEurope.AffinityPhoto2_2.5.0.0_x64__abc" → "serifeurope", "affinityphoto2", "serifeuropeaffinityphoto2".
fn msix_names() -> Vec<String> {
    let keys = [
        RegKey::new(Hive::Cu, r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages"),
        RegKey::new(Hive::Lm, r"SOFTWARE\Microsoft\Windows\CurrentVersion\Appx\AppxAllUserStore\Applications"),
    ];
    let mut out: HashSet<String> = HashSet::new();
    for k in &keys {
        let Some(key) = reg::open_read(k) else { continue };
        for full in key.subkeys() {
            let name = full.split('_').next().unwrap_or("");
            for part in name.split('.') {
                if usable(part).is_some() {
                    out.insert(norm_name(part));
                }
            }
            let n = norm_name(name);
            if n.len() >= 4 {
                out.insert(n);
            }
        }
    }
    out.into_iter().collect()
}

/// Absolute folders named by the user and system environment variables (each PATH entry
/// separately). Read from the registry, not from Platter's own environment, which is a snapshot from
/// when Platter started and still holds PATH entries an uninstaller has since removed.
fn env_dirs() -> Vec<PathBuf> {
    let keys = [
        RegKey::new(Hive::Cu, "Environment"),
        RegKey::new(Hive::Lm, r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment"),
    ];
    let mut out = Vec::new();
    for k in &keys {
        let Some(key) = reg::open_read(k) else { continue };
        for name in key.value_names() {
            let Some(v) = key.string(&name) else { continue };
            for part in v.split(';') {
                let part = PathBuf::from(part.trim().trim_matches('"'));
                if part.is_absolute() && part.components().count() > 2 && !in_windows(&part) {
                    out.push(part);
                }
            }
        }
    }
    out
}

fn current_user_name() -> String {
    #[link(name = "advapi32")]
    extern "system" {
        fn GetUserNameW(buf: *mut u16, size: *mut u32) -> i32;
    }
    let mut buf = [0u16; 260];
    let mut n = buf.len() as u32;
    if unsafe { GetUserNameW(buf.as_mut_ptr(), &mut n) } != 0 {
        from_wide(&buf)
    } else {
        String::new()
    }
}

/// What is installed and registered right now. Fails closed: if the services or scheduled tasks
/// can't be listed, Platter can't tell what a folder is still used by, so it reports nothing.
fn build_context(roots: &Roots) -> Result<Context, String> {
    // Measured 2026-11-24 on this PC, unelevated: 289 services 76 ms, 203 tasks ~430-750 ms (no
    // folder denied), 70 apps 29 ms, 22 startup values + 105 App Paths 10 ms. Tasks and services run
    // on their own threads so the whole context costs about as much as the task listing.
    let (apps, services, tasks, startup, app_paths, protected, msix) = std::thread::scope(|s| {
        let tasks = s.spawn(|| {
            if roots.system {
                com::on_mta_thread(tasks::list).and_then(|r| r.ok()).map(|(t, _)| t)
            } else {
                Some(Vec::new())
            }
        });
        let services = s.spawn(|| if roots.system { Some(list_services()).filter(|s| !s.is_empty()) } else { Some(Vec::new()) });
        let apps = read_apps(&roots.uninstall);
        let startup = list_startup(&roots.run_keys);
        let app_paths = list_app_paths(&roots.app_paths_keys);
        let protected = protected_paths();
        let msix = if roots.system { msix_names() } else { Vec::new() };
        (apps, services.join().ok().flatten(), tasks.join().ok().flatten(), startup, app_paths, protected, msix)
    });
    let services = services.ok_or_else(|| "Windows couldn't list its services, so Platter can't tell what's safe to remove. Try again.".to_string())?;
    let tasks = tasks.ok_or_else(|| "Windows couldn't list its scheduled tasks, so Platter can't tell what's safe to remove. Try again.".to_string())?;
    let env_dirs = if roots.system { env_dirs() } else { Vec::new() };
    // Platter's own data (backups included) is never anyone's leftover.
    let mut protected = protected;
    if let Some(own) = roots.backup_base.parent() {
        protected.extend(path_forms(own));
    }
    Ok(Context { apps, services, tasks, startup, app_paths, protected, user_name: current_user_name(), msix, env_dirs })
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Owner {
    App(String),
    Service(String),
    Task(String),
    Startup(String),
    AppPath(String),
    Env,
}

#[derive(Clone, Debug)]
struct Ref {
    path: String,
    /// An install folder: blocks folders inside it as well as folders containing it.
    location: bool,
    owner: Owner,
}

fn startup_owner(v: &ValueRef) -> Owner {
    Owner::Startup(format!("{}|{}", v.key.display().to_lowercase(), v.name.to_lowercase()))
}

/// Everything installed apps, services, tasks and startup entries point at.
fn build_refs(ctx: &Context, roots: &Roots) -> Vec<Ref> {
    let mut refs = Vec::new();
    let file_ref = |p: &Path, owner: Owner, refs: &mut Vec<Ref>| {
        if p.is_absolute() && !in_windows(p) {
            refs.push(Ref { path: judged_form(p), location: false, owner });
        }
    };
    for a in &ctx.apps {
        let owner = Owner::App(a.app.id.clone());
        if let Some(l) = &a.location {
            // A location like "C:\Program Files" or a profile folder (badly written entries) says
            // nothing about which subfolder is the app's, so it can't block anything specific.
            if refuse_path(l, &ctx.protected).is_none() && !roots.is_root(l) {
                refs.push(Ref { path: judged_form(l), location: true, owner: owner.clone() });
            }
        }
        if let Some(t) = &a.target {
            file_ref(t, owner.clone(), &mut refs);
        }
        if let Some(i) = &a.icon {
            file_ref(i, owner.clone(), &mut refs);
        }
    }
    for s in &ctx.services {
        if let Some(p) = &s.program {
            file_ref(p, Owner::Service(s.name.to_lowercase()), &mut refs);
        }
    }
    for t in &ctx.tasks {
        for p in &t.programs {
            file_ref(Path::new(p), Owner::Task(t.path.to_lowercase()), &mut refs);
            // Left unexpanded (another account's per-user path): it may still be this user's
            // folder it runs from, and blocking too much is the safe side.
            if p.contains('%') {
                file_ref(Path::new(&reg::expand(p)), Owner::Task(t.path.to_lowercase()), &mut refs);
            }
        }
    }
    for v in &ctx.startup {
        if let Some(p) = &v.program {
            file_ref(p, startup_owner(v), &mut refs);
        }
    }
    for a in &ctx.app_paths {
        if let Some(p) = &a.program {
            file_ref(p, Owner::AppPath(a.key.display().to_lowercase()), &mut refs);
        }
    }
    for d in &ctx.env_dirs {
        file_ref(d, Owner::Env, &mut refs);
    }
    refs
}

/// The first reference (not by an allowed owner) that this folder is, contains, or sits inside.
fn blocking_ref<'a>(folder: &Path, refs: &'a [Ref], allowed: &dyn Fn(&Owner) -> bool) -> Option<&'a Ref> {
    let f = judged_form(folder);
    let lit = norm_path(folder);
    refs.iter().find(|r| {
        !allowed(&r.owner)
            && (within(&r.path, &f) || within(&r.path, &lit) || (r.location && (within(&f, &r.path) || within(&lit, &r.path))))
    })
}

fn path_inside_any(p: &Path, dirs: &[PathBuf]) -> bool {
    let f = judged_form(p);
    let lit = norm_path(p);
    dirs.iter().any(|d| {
        let dn = judged_form(d);
        within(&f, &dn) || within(&lit, &norm_path(d))
    })
}

// ---------------------------------------------------------------------------------------------
// Registry whitelist

const FORBIDDEN_KEYS: &[&str] = &["microsoft", "windows", "classes", "policies", "wow6432node", "clients", "registeredapplications"];

/// Registry keys Platter may delete: `HK{CU,LM}\Software\<x>[\...]` and
/// `HKLM\Software\WOW6432Node\<x>[\...]` (x never Microsoft, Windows, Classes, Policies,
/// Wow6432Node, Clients, RegisteredApplications — nor anything below those), or exactly one entry
/// under the Uninstall or App Paths keys.
fn reg_allowed(k: &RegKey) -> bool {
    let seg = k.segments();
    if seg.first().map(String::as_str) != Some("software") {
        return false;
    }
    let rest: &[String] = if seg.get(1).map(String::as_str) == Some("wow6432node") { &seg[2..] } else { &seg[1..] };
    let one_entry_under = |sub: &str| {
        let parts: Vec<&str> = sub.split('\\').collect();
        rest.len() == parts.len() + 1 && rest.iter().zip(parts.iter()).all(|(a, b)| a == &b.to_lowercase())
    };
    if one_entry_under(UNINSTALL_SUBKEY) || one_entry_under(APP_PATHS_SUBKEY) {
        return !rest.last().is_some_and(|l| l.is_empty());
    }
    match rest.first() {
        None => false,
        Some(x) => !FORBIDDEN_KEYS.contains(&x.as_str()),
    }
}

/// A single named value directly under a Run/RunOnce key.
fn startup_value_allowed(k: &RegKey, name: &str) -> bool {
    let seg = k.segments();
    let rest: &[String] = match seg.as_slice() {
        [s, w, rest @ ..] if s == "software" && w == "wow6432node" => rest,
        [s, rest @ ..] if s == "software" => rest,
        _ => return false,
    };
    !name.is_empty()
        && rest.len() == 4
        && rest[0] == "microsoft"
        && rest[1] == "windows"
        && rest[2] == "currentversion"
        && matches!(rest[3].as_str(), "run" | "runonce")
}

// ---------------------------------------------------------------------------------------------
// Leftover items

#[derive(Clone, Debug)]
enum Target {
    Path(PathBuf),
    Key(RegKey),
    Value { key: RegKey, name: String, data: String },
    Service { name: String, image: String },
    Task { path: String, programs: Vec<String> },
}

#[derive(Clone, Debug)]
struct Item {
    lo: Leftover,
    target: Target,
    /// Orphan folder: re-check "no programs, nothing recent" before removing.
    orphan_folder: bool,
    /// Orphan uninstall entry: re-check "program and folder gone" before removing.
    orphan_entry: bool,
    /// A publisher folder or key offered whole because it held only this app's data: the names
    /// (lower-case) of what it held at scan time. Anything new inside it means another product
    /// moved in, so it is left alone.
    only_children: Option<Vec<String>>,
}

#[allow(clippy::too_many_arguments)]
fn item(kind: &'static str, path: String, label: String, reason: &str, size: u64, confidence: &'static str, needs_admin: bool, target: Target) -> Item {
    Item {
        lo: Leftover {
            id: String::new(),
            kind,
            path,
            label,
            reason: reason.to_string(),
            size,
            confidence,
            needs_admin,
            default_on: confidence == HIGH,
        },
        target,
        orphan_folder: false,
        orphan_entry: false,
        only_children: None,
    }
}

fn leaf_label(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

fn conf_rank(c: &str) -> u8 {
    match c {
        HIGH => 3,
        MEDIUM => 2,
        _ => 1,
    }
}

struct FolderHit {
    path: PathBuf,
    confidence: &'static str,
    reason: &'static str,
}

fn list_dirs(p: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(p) else { return Vec::new() };
    rd.flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir() && !t.is_symlink()).unwrap_or(false))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect()
}

fn names_claimed_by_others(ctx: &Context, self_id: Option<&str>) -> HashSet<String> {
    let mut s = HashSet::new();
    for a in &ctx.apps {
        if Some(a.app.id.as_str()) == self_id {
            continue;
        }
        for n in a.tokens().names {
            s.insert(n);
        }
    }
    s.extend(ctx.msix.iter().cloned());
    s
}

/// `n` is in `set`, or one of them starts with the other and the shorter is 5+ characters
/// ("chocolateyhttpcache" vs "chocolatey", "affinity" vs "affinityphoto2"). Only for orphans,
/// where a miss is cheap and a false positive is not.
fn related_name(n: &str, set: &HashSet<String>) -> bool {
    set.contains(n)
        || set.iter().any(|x| {
            let short = x.len().min(n.len());
            short >= 5 && (x.starts_with(n) || n.starts_with(x.as_str()))
        })
}

/// "Mozilla-1de4eec8-1241-4177-a864-e594e8d1fb38" → "Mozilla"; "Foo_2f8a9c01" → "Foo".
fn strip_id_suffix(name: &str) -> &str {
    let mut s = name.trim();
    loop {
        let Some(i) = s.rfind(['-', '_', ' ', '.']) else { break };
        let tail = &s[i + 1..];
        let hexish = tail.len() >= 4 && tail.chars().all(|c| c.is_ascii_hexdigit()) && tail.chars().any(|c| c.is_ascii_digit());
        if !hexish || i == 0 {
            break;
        }
        s = s[..i].trim_end();
    }
    s
}

fn publishers_of_others(ctx: &Context, self_id: Option<&str>) -> HashSet<String> {
    ctx.apps
        .iter()
        .filter(|a| Some(a.app.id.as_str()) != self_id)
        .map(|a| norm_name(&clean_publisher(&a.publisher)))
        .filter(|p| !p.is_empty())
        .collect()
}

const PUBLISHER_FOLDER: &str = "The publisher's folder, holding only this app's data.";
const RECORDED_FOLDER: &str = "The install folder its installer recorded, though its name doesn't match the app.";

/// `<Publisher>` folder `path` (normalized name `n`): whether it may be offered whole — the publisher
/// folder may be used, isn't generic or shared with another installed app's publisher, and every
/// child is a folder named after this app — and the children that are this app's either way.
fn publisher_folder_children(path: &Path, n: &str, toks: &Tokens, claimed: &HashSet<String>, other_pubs: &HashSet<String>) -> (bool, Vec<PathBuf>) {
    let children = std::fs::read_dir(path).map(|rd| rd.flatten().count()).unwrap_or(usize::MAX);
    let mine: Vec<PathBuf> = list_dirs(path)
        .into_iter()
        .filter(|(sname, _)| {
            let sn = norm_name(sname);
            toks.has(&sn) && !claimed.contains(&sn)
        })
        .map(|(_, p)| p)
        .collect();
    let only_mine = toks.publisher_folder_ok
        && toks.publisher.as_deref() == Some(n)
        && !mine.is_empty()
        && mine.len() == children
        && !other_pubs.contains(n)
        && !is_generic(n);
    (only_mine, mine)
}

/// `f` sits directly in `dir`, or one folder down (`dir\bin\app.exe`).
fn at_top_of(f: &Path, dir: &Path) -> bool {
    let (dn, dl) = (judged_form(dir), norm_path(dir));
    let same = |p: &Path| judged_form(p) == dn || norm_path(p) == dl;
    let parent = f.parent();
    parent.is_some_and(same) || parent.and_then(Path::parent).is_some_and(same)
}

/// How far an install location (already past refuse_path / is_root) can be trusted as the app's own
/// folder. An InstallLocation is only as good as the installer that wrote it: some point at a shared
/// vendor or games folder, or at the parent of the real one. HIGH needs the folder's own name, or the
/// uninstaller/icon sitting at its top (not somewhere deep inside), to tie it to this app. A folder
/// named after a publisher gets the same rules as any publisher folder; anything else is LOW.
fn install_folder_trust(l: &Path, rec: &AppRecord, toks: &Tokens, name_toks: &Tokens, claimed: &HashSet<String>, other_pubs: &HashSet<String>) -> (&'static str, &'static str) {
    let raw = l.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let leaf = norm_name(&raw);
    if leaf.is_empty() {
        return (LOW, RECORDED_FOLDER);
    }
    if name_toks.has(&leaf) || (rec.location.is_none() && toks.has(&leaf)) {
        return (HIGH, "The app's install folder.");
    }
    let own_pub = norm_name(&clean_publisher(&rec.publisher));
    if toks.publisher.as_deref() == Some(leaf.as_str()) || own_pub == leaf || other_pubs.contains(&leaf) {
        let (only_mine, _) = publisher_folder_children(l, &leaf, toks, claimed, other_pubs);
        return if only_mine { (MEDIUM, PUBLISHER_FOLDER) } else { (LOW, RECORDED_FOLDER) };
    }
    // Folders that hold many products (a games or tools folder, Steam, Common Files...). Other
    // generic leaves ("bin", "app") are an app's own subfolder and may still be its install folder.
    const CONTAINERS: &[&str] = &["games", "game", "apps", "applications", "programs", "program", "software", "tools", "utilities", "shared", "common", "portable", "steamapps"];
    let shared = CONTAINERS.contains(&leaf.as_str()) || shared_folder_name(&raw);
    if !shared && [rec.target.as_ref(), rec.icon.as_ref()].into_iter().flatten().any(|f| at_top_of(f, l)) {
        return (HIGH, "The app's install folder.");
    }
    (LOW, RECORDED_FOLDER)
}

/// What else on the PC may own a program: another installed app's install folder, the folder of
/// another app's uninstaller or icon, or a name another installed app (or Store package) goes by.
/// Used to keep other products' services, tasks and startup entries out of an app's leftovers, at
/// scan time and again right before removal.
struct OtherOwners {
    dirs: Vec<PathBuf>,
    toks: Vec<Tokens>,
    claimed: HashSet<String>,
}

impl OtherOwners {
    fn new(ctx: &Context, roots: &Roots, self_id: Option<&str>) -> OtherOwners {
        let usable_dir = |d: &Path| d.is_absolute() && !in_windows(d) && refuse_path(d, &ctx.protected).is_none() && !roots.is_root(d);
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut toks: Vec<Tokens> = Vec::new();
        for a in ctx.apps.iter().filter(|a| Some(a.app.id.as_str()) != self_id) {
            dirs.extend(a.location.iter().filter(|l| usable_dir(l)).cloned());
            for f in [a.target.as_ref(), a.icon.as_ref()].into_iter().flatten() {
                dirs.extend(f.parent().filter(|d| usable_dir(d)).map(Path::to_path_buf));
            }
            toks.push(a.tokens());
        }
        OtherOwners { dirs, toks, claimed: names_claimed_by_others(ctx, self_id) }
    }

    /// `prog` (run by an entry called `names`) belongs to something else. A name any other app uses
    /// exactly is never this app's; a looser match only counts when `mine` doesn't name it exactly.
    fn owns(&self, prog: &Path, names: &[&str], mine: Option<&Tokens>) -> bool {
        path_inside_any(prog, &self.dirs)
            || names.iter().any(|n| {
                let nn = norm_name(n);
                !nn.is_empty() && (self.claimed.contains(&nn) || (!mine.is_some_and(|t| t.has(&nn)) && self.toks.iter().any(|t| t.loosely_named(n))))
            })
    }
}

/// Leftovers of one app (installed or already uninstalled).
fn scan_app(rec: &AppRecord, ctx: &Context, roots: &Roots) -> Vec<Item> {
    let mut toks = rec.tokens();
    let refs = build_refs(ctx, roots);
    let self_id = rec.app.id.clone();
    let claimed = names_claimed_by_others(ctx, Some(&self_id));
    let other_pubs = publishers_of_others(ctx, Some(&self_id));
    let not_self = |o: &Owner| matches!(o, Owner::App(id) if *id == self_id);
    let mut hits: Vec<FolderHit> = Vec::new();

    // 1. The install folder (or, without one, the uninstaller's folder when it's named after the app).
    let name_toks = app_tokens(&rec.display_name, &rec.publisher, &rec.key_name, None, None);
    let mut locations: Vec<PathBuf> = Vec::new();
    if let Some(l) = &rec.location {
        locations.push(l.clone());
    } else {
        for f in [rec.target.as_ref(), rec.icon.as_ref()].into_iter().flatten() {
            if let Some(d) = f.parent() {
                if !in_windows(d) && d.file_name().is_some_and(|n| name_toks.has(&norm_name(&n.to_string_lossy()))) {
                    locations.push(d.to_path_buf());
                    break;
                }
            }
        }
    }
    // An install folder that is already gone but would have been trusted fully: entries still
    // registered to run from it (a service left behind after the folder was deleted) are the app's.
    let mut gone_locations: Vec<PathBuf> = Vec::new();
    let mut location_trusted = false;
    for l in &locations {
        if refuse_path(l, &ctx.protected).is_some() || roots.is_root(l) {
            continue;
        }
        let (confidence, reason) = install_folder_trust(l, rec, &toks, &name_toks, &claimed, &other_pubs);
        location_trusted |= confidence != LOW;
        if l.is_dir() {
            hits.push(FolderHit { path: l.clone(), confidence, reason });
        } else if confidence == HIGH && presence(l) == Presence::Missing {
            gone_locations.push(l.clone());
        }
    }
    // A recorded InstallLocation that isn't trusted (refused, shared, doubtful) lends its name to
    // nothing else either: no "<root>\<that name>" folders or keys, no services named after it.
    if let (Some(l), false) = (&rec.location, location_trusted) {
        if let Some(n) = folder_leaf_token(l).map(|t| norm_name(&t)) {
            let udir = rec.target.as_ref().and_then(|t| t.parent()).filter(|d| !in_windows(d)).and_then(folder_leaf_token).map(|t| norm_name(&t));
            if !name_toks.has(&n) && udir.as_deref() != Some(n.as_str()) {
                toks.names.retain(|x| *x != n);
            }
        }
    }

    // 2. <root>\<Name> and <root>\<Publisher>\<Name>.
    let machine_set: HashSet<String> = roots.machine.iter().map(|p| norm_path(p)).collect();
    for root in roots.machine.iter().chain(roots.user.iter()) {
        let is_pd = env_dir("ProgramData").is_some_and(|pd| norm_path(&pd) == norm_path(root)) && machine_set.contains(&norm_path(root));
        let exact_conf = if is_pd { MEDIUM } else { HIGH };
        for (name, path) in list_dirs(root) {
            let n = norm_name(&name);
            if toks.has(&n) && !claimed.contains(&n) {
                hits.push(FolderHit { path, confidence: exact_conf, reason: "Named after the app." });
                continue;
            }
            if toks.publisher.as_deref() == Some(n.as_str()) {
                let (only_mine, sub_hits) = publisher_folder_children(&path, &n, &toks, &claimed, &other_pubs);
                if only_mine {
                    hits.push(FolderHit { path, confidence: MEDIUM, reason: PUBLISHER_FOLDER });
                } else {
                    for sp in sub_hits {
                        hits.push(FolderHit { path: sp, confidence: MEDIUM, reason: "In the publisher's folder and named after the app." });
                    }
                }
            }
        }
    }

    // Safety filters; outermost folder wins, keeping the strongest confidence.
    // First pass: only other apps block (services/tasks/startup entries are judged below, once
    // it's known which of them are this app's own).
    let only_apps_block = |o: &Owner| !matches!(o, Owner::App(id) if *id != self_id);
    hits.retain(|h| refuse_path(&h.path, &ctx.protected).is_none() && !roots.is_root(&h.path) && blocking_ref(&h.path, &refs, &only_apps_block).is_none());
    hits.sort_by_key(|h| norm_path(&h.path).len());
    let mut folders: Vec<FolderHit> = Vec::new();
    for h in hits {
        let hn = norm_path(&h.path);
        // Inside a folder already found: covered by it, unless it is the stronger of the two. A
        // stronger inner folder ("Vendor\App", the app's own) never lends its confidence to a weaker
        // outer one ("Vendor", a recorded but doubtful location): both are kept, each as found.
        if folders.iter().any(|f| within(&hn, &norm_path(&f.path)) && conf_rank(h.confidence) <= conf_rank(f.confidence)) {
            continue;
        }
        folders.push(h);
    }
    // "The app's folder" for services, tasks and startup entries: only folders that passed every
    // check above and are at least MEDIUM. A LOW folder (a shared or doubtful InstallLocation) or a
    // refused one (Program Files, a profile folder) says nothing about who owns what runs from it.
    let folder_paths: Vec<PathBuf> =
        folders.iter().filter(|f| f.confidence != LOW).map(|f| f.path.clone()).chain(gone_locations.iter().filter(|l| blocking_ref(l, &refs, &only_apps_block).is_none()).cloned()).collect();

    let others = OtherOwners::new(ctx, roots, Some(&self_id));
    let classify = |prog: &Path, names: &[&str]| -> Option<(&'static str, &'static str)> {
        if in_windows(prog) || others.owns(prog, names, Some(&toks)) {
            return None;
        }
        if path_inside_any(prog, &folder_paths) {
            return Some((HIGH, "Runs a program from the app's folder."));
        }
        if presence(prog) == Presence::Missing && names.iter().any(|n| toks.loosely_named(n)) {
            return Some((MEDIUM, "Named after the app, and its program file is gone."));
        }
        None
    };

    let mut items: Vec<Item> = Vec::new();
    let mut entry_owners: HashSet<Owner> = HashSet::new();

    for s in &ctx.services {
        let Some(p) = &s.program else { continue };
        if let Some((conf, why)) = classify(p, &[&s.name, &s.display]) {
            entry_owners.insert(Owner::Service(s.name.to_lowercase()));
            let label = if s.display.is_empty() { s.name.clone() } else { s.display.clone() };
            items.push(item("service", s.name.clone(), label, why, 0, conf, true, Target::Service { name: s.name.clone(), image: s.image.clone() }));
        }
    }
    for t in ctx.tasks.iter().filter(|t| !t.is_microsoft()) {
        let hit = t.programs.iter().filter_map(|p| classify(Path::new(p), &[&t.name])).max_by_key(|(c, _)| conf_rank(c));
        if let Some((conf, why)) = hit {
            entry_owners.insert(Owner::Task(t.path.to_lowercase()));
            let needs_admin = !task_owned_by(&t.user, &ctx.user_name);
            items.push(item("task", t.path.clone(), t.name.clone(), why, 0, conf, needs_admin, Target::Task { path: t.path.clone(), programs: t.programs.clone() }));
        }
    }
    for v in &ctx.startup {
        let Some(p) = &v.program else { continue };
        if !startup_value_allowed(&v.key, &v.name) {
            continue;
        }
        if let Some((conf, why)) = classify(p, &[&v.name]) {
            entry_owners.insert(startup_owner(v));
            items.push(item(
                "startup",
                format!("{} → {}", v.key.display(), v.name),
                v.name.clone(),
                if conf == HIGH { "Starts a program from the app's folder when you sign in." } else { why },
                0,
                conf,
                v.key.hive == Hive::Lm,
                Target::Value { key: v.key.clone(), name: v.name.clone(), data: v.data.clone() },
            ));
        }
    }
    for a in &ctx.app_paths {
        let Some(p) = &a.program else { continue };
        if !reg_allowed(&a.key) {
            continue;
        }
        let stem = Path::new(a.key.leaf()).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some((conf, why)) = classify(p, &[&stem]) {
            entry_owners.insert(Owner::AppPath(a.key.display().to_lowercase()));
            items.push(item("registry", a.key.display(), a.key.leaf().to_string(), why, 0, conf, a.key.hive == Hive::Lm, Target::Key(a.key.clone())));
        }
    }

    // A folder that a service/task/startup entry *not* found here still runs from stays.
    let allowed = |o: &Owner| not_self(o) || entry_owners.contains(o);
    folders.retain(|f| blocking_ref(&f.path, &refs, &allowed).is_none());

    // Registry keys named after the app.
    if !items.is_empty() || !toks.names.is_empty() {
        items.extend(registry_hits(&toks, &claimed, &other_pubs, roots));
    }

    // Its Uninstall entry, if it's still there.
    if reg::exists(&rec.key) && reg_allowed(&rec.key) {
        let present = rec.uninstaller_present() || rec.location.as_ref().is_some_and(|l| l.is_dir());
        let (conf, why) = if present {
            (LOW, "The app still looks installed. Remove this only if its uninstaller won't run.")
        } else {
            (HIGH, "Its entry in the installed apps list, though the app is gone.")
        };
        items.push(item("uninstall-entry", rec.key.display(), rec.display_name.clone(), why, 0, conf, rec.key.hive == Hive::Lm, Target::Key(rec.key.clone())));
    }

    let mut out: Vec<Item> = folder_items(folders, roots);
    out.extend(items);
    out
}

fn folder_items(folders: Vec<FolderHit>, roots: &Roots) -> Vec<Item> {
    use rayon::prelude::*;
    let sizes: Vec<u64> = folders.par_iter().with_max_len(1).map(|f| walk(&f.path, 2_000_000, &|_| false).size).collect();
    let mut out: Vec<Item> = folders
        .into_iter()
        .zip(sizes)
        .map(|(f, size)| {
            let needs_admin = roots.is_machine_path(&f.path);
            let only_children = (f.reason == PUBLISHER_FOLDER).then(|| child_names(&f.path));
            let mut it = item("folder", f.path.display().to_string(), leaf_label(&f.path), f.reason, size, f.confidence, needs_admin, Target::Path(f.path));
            it.only_children = only_children;
            it
        })
        .collect();
    out.sort_by(|a, b| b.lo.size.cmp(&a.lo.size));
    out
}

/// Names (lower-case) of everything directly inside a folder.
fn child_names(p: &Path) -> Vec<String> {
    std::fs::read_dir(p).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_lowercase()).collect()).unwrap_or_default()
}

fn registry_hits(toks: &Tokens, claimed: &HashSet<String>, other_pubs: &HashSet<String>, roots: &Roots) -> Vec<Item> {
    let mut out = Vec::new();
    let mut seen: Vec<RegKey> = Vec::new();
    for base in &roots.reg_bases {
        let Some(bk) = reg::open_read(base) else { continue };
        for name in bk.subkeys() {
            let n = norm_name(&name);
            if FORBIDDEN_KEYS.contains(&name.to_lowercase().as_str()) {
                continue;
            }
            let key = base.child(&name);
            if toks.has(&n) && !claimed.contains(&n) {
                if reg_allowed(&key) && !seen.iter().any(|s| s.eq_ci(&key)) {
                    seen.push(key.clone());
                    out.push(item("registry", key.display(), name.clone(), "Named after the app.", 0, MEDIUM, key.hive == Hive::Lm, Target::Key(key)));
                }
                continue;
            }
            if toks.publisher.as_deref() != Some(n.as_str()) {
                continue;
            }
            let Some(pk) = reg::open_read(&key) else { continue };
            let subs = pk.subkeys();
            let values = pk.value_names();
            let mine: Vec<String> = subs.iter().filter(|s| {
                let sn = norm_name(s);
                toks.has(&sn) && !claimed.contains(&sn)
            }).cloned().collect();
            if mine.is_empty() {
                continue;
            }
            if toks.publisher_folder_ok && mine.len() == subs.len() && values.iter().all(|v| v.is_empty()) && !other_pubs.contains(&n) && reg_allowed(&key) {
                if !seen.iter().any(|s| s.eq_ci(&key)) {
                    seen.push(key.clone());
                    let mut it = item("registry", key.display(), name.clone(), "The publisher's key, holding only this app's settings.", 0, MEDIUM, key.hive == Hive::Lm, Target::Key(key));
                    it.only_children = Some(subs.iter().map(|s| s.to_lowercase()).collect());
                    out.push(it);
                }
            } else {
                for s in mine {
                    let sk = key.child(&s);
                    if reg_allowed(&sk) && !seen.iter().any(|x| x.eq_ci(&sk)) {
                        seen.push(sk.clone());
                        out.push(item("registry", sk.display(), s.clone(), "Under the publisher's key and named after the app.", 0, MEDIUM, sk.hive == Hive::Lm, Target::Key(sk)));
                    }
                }
            }
        }
    }
    out
}

/// The task runs as the signed-in user (so they may delete it without admin rights).
fn task_owned_by(task_user: &str, me: &str) -> bool {
    let t = task_user.trim().to_lowercase();
    let me = me.trim().to_lowercase();
    !t.is_empty() && !me.is_empty() && (t == me || t.ends_with(&format!("\\{me}")))
}

/// Known folders that are shared or belong to Windows, never offered as orphans.
fn shared_folder_name(name: &str) -> bool {
    let l = name.to_lowercase();
    const EXACT: &[&str] = &[
        "common files", "package cache", "packages", "installer", "windowsapps", "modifiablewindowsapps", "dotnet",
        "internet explorer", "msbuild", "reference assemblies", "uninstall information", "windowspowershell",
        "packagemanagement", "ssh", "usoshared", "usoprivate", "softwaredistribution", "application data", "desktop",
        "documents", "start menu", "templates", "favorites", "comms", "intel", "amd", "nvidia", "nvidia corporation",
        "realtek", "oem", "drivers", "chocolatey", "docker", "java", "oracle", "sun", "regid", "system32", "temp",
        "default", "public", "all users", "default user", "desktop.ini", "mozilla", "google", "adobe", "apple",
        "apple computer", "steam", "epic games", "origin", "battle.net", "riot games", "ubisoft", "ea games",
        "electronic arts", "dell", "hp", "lenovo", "asus", "acer", "msi", "logitech", "razer", "synaptics", "elan",
        "dolby", "dts", "waves", "conexant", "qualcomm", "broadcom", "killer networking", "rivet networks", "ibm",
        "git", "nodejs", "python", "powershell", "openssh", "wsl", "hyper-v", "containers", "virtualbox", "vmware",
        "hewlett-packard", "shimgen", "crossdevice", "common", "local", "roaming", "user data", "uninstall",
    ];
    EXACT.contains(&l.as_str())
        || l.starts_with("microsoft")
        || l.starts_with("windows")
        || l.starts_with("regid.")
        || l.starts_with("nvidia")
        || l.starts_with("intel")
        || l.starts_with('{')
        || l.starts_with('.')
        || l.starts_with('$')
}

const ORPHAN_AGE: Duration = Duration::from_secs(90 * 24 * 3600);

/// No programs inside, nothing changed in 90 days, small enough to check fully.
fn orphan_folder_quiet(p: &Path) -> Option<WalkStats> {
    let cutoff = SystemTime::now().checked_sub(ORPHAN_AGE)?;
    let st = walk(p, 50_000, &|s| s.has_binary || s.newest.is_some_and(|n| n > cutoff));
    if st.truncated || st.has_binary || st.newest.is_some_and(|n| n > cutoff) {
        return None;
    }
    // An empty folder tree must itself be old.
    if st.files == 0 {
        let m = std::fs::metadata(p).ok()?.modified().ok()?;
        if m > cutoff {
            return None;
        }
    }
    Some(st)
}

/// Leftovers of apps that are no longer installed.
fn scan_orphans(ctx: &Context, roots: &Roots) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    let installed_locations: Vec<PathBuf> =
        ctx.apps.iter().filter_map(|a| a.location.clone()).filter(|l| refuse_path(l, &ctx.protected).is_none() && !roots.is_root(l)).collect();
    let gone = |p: &Path| presence(p) == Presence::Missing && !in_windows(p) && !path_inside_any(p, &installed_locations);

    // Uninstall entries whose uninstaller and install folder are gone.
    for a in &ctx.apps {
        let Some(t) = &a.target else { continue };
        if a.cmd.is_none() || !gone(t) {
            continue;
        }
        if a.location.as_ref().is_some_and(|l| presence(l) != Presence::Missing) {
            continue;
        }
        if a.icon.as_ref().is_some_and(|i| presence(i) == Presence::Present) {
            continue;
        }
        if !reg_allowed(&a.key) {
            continue;
        }
        let mut it = item(
            "uninstall-entry",
            a.key.display(),
            a.display_name.clone(),
            "Its uninstaller and install folder are gone.",
            0,
            HIGH,
            a.key.hive == Hive::Lm,
            Target::Key(a.key.clone()),
        );
        it.orphan_entry = true;
        items.push(it);
    }
    for s in &ctx.services {
        let Some(p) = &s.program else { continue };
        if gone(p) {
            let label = if s.display.is_empty() { s.name.clone() } else { s.display.clone() };
            items.push(item("service", s.name.clone(), label, "Its program file is gone.", 0, MEDIUM, true, Target::Service { name: s.name.clone(), image: s.image.clone() }));
        }
    }
    for t in ctx.tasks.iter().filter(|t| !t.is_microsoft()) {
        let paths: Vec<PathBuf> = t.programs.iter().map(PathBuf::from).collect();
        if !paths.is_empty() && paths.iter().all(|p| gone(p)) {
            let needs_admin = !task_owned_by(&t.user, &ctx.user_name);
            items.push(item("task", t.path.clone(), t.name.clone(), "Its program file is gone.", 0, MEDIUM, needs_admin, Target::Task { path: t.path.clone(), programs: t.programs.clone() }));
        }
    }
    for v in &ctx.startup {
        let Some(p) = &v.program else { continue };
        if gone(p) && startup_value_allowed(&v.key, &v.name) {
            items.push(item(
                "startup",
                format!("{} → {}", v.key.display(), v.name),
                v.name.clone(),
                "Starts a program that is gone.",
                0,
                HIGH,
                v.key.hive == Hive::Lm,
                Target::Value { key: v.key.clone(), name: v.name.clone(), data: v.data.clone() },
            ));
        }
    }
    for a in &ctx.app_paths {
        let Some(p) = &a.program else { continue };
        if gone(p) && reg_allowed(&a.key) {
            items.push(item("registry", a.key.display(), a.key.leaf().to_string(), "Points at a program that is gone.", 0, HIGH, a.key.hive == Hive::Lm, Target::Key(a.key.clone())));
        }
    }

    // Folders with no programs, nothing recent, not named after or used by anything installed.
    let refs = build_refs(ctx, roots);
    // "In use" names: installed apps and Store packages, their publishers, and services (the spike
    // found C:\ProgramData\Whesvc, the folder of the running Windows service "whesvc").
    let mut in_use = names_claimed_by_others(ctx, None);
    in_use.extend(publishers_of_others(ctx, None));
    for s in &ctx.services {
        in_use.insert(norm_name(&s.name));
        in_use.insert(norm_name(&s.display));
    }
    in_use.retain(|n| !n.is_empty());
    let mut candidates: Vec<(PathBuf, bool)> = Vec::new();
    for (root, machine) in &roots.orphan_dirs {
        let dirs = list_dirs(root);
        for (name, path) in &dirs {
            let n = norm_name(strip_id_suffix(name));
            if shared_folder_name(name) || n.chars().count() < 3 || is_generic(&n) || related_name(&n, &in_use) {
                continue;
            }
            // A sibling with a related name is still there ("ChocolateyHttpCache" next to "chocolatey").
            let siblings: HashSet<String> =
                dirs.iter().filter(|(o, _)| o != name).map(|(o, _)| norm_name(strip_id_suffix(o))).filter(|o| o.len() >= 5).collect();
            if related_name(&n, &siblings) {
                continue;
            }
            let path = path.clone();
            if refuse_path(&path, &ctx.protected).is_some() || roots.is_root(&path) || blocking_ref(&path, &refs, &|_| false).is_some() {
                continue;
            }
            candidates.push((path, *machine));
        }
    }
    use rayon::prelude::*;
    let checked: Vec<Option<Item>> = candidates
        .par_iter()
        .with_max_len(1)
        .map(|(path, machine)| {
            let st = orphan_folder_quiet(path)?;
            let mut it = item(
                "folder",
                path.display().to_string(),
                leaf_label(path),
                "No installed app uses it, it holds no programs, and nothing in it changed in 90 days.",
                st.size,
                MEDIUM,
                *machine || roots.is_machine_path(path),
                Target::Path(path.clone()),
            );
            it.lo.default_on = false;
            it.orphan_folder = true;
            Some(it)
        })
        .collect();
    let mut folders: Vec<Item> = checked.into_iter().flatten().collect();
    folders.sort_by(|a, b| b.lo.size.cmp(&a.lo.size));
    folders.extend(items);
    folders
}

// ---------------------------------------------------------------------------------------------
// Scan state

#[derive(Clone)]
struct ScanState {
    scan_id: u64,
    app_id: Option<String>,
    app_name: Option<String>,
    /// The app's match tokens (app scans), to re-judge at removal what belongs to it.
    toks: Option<Tokens>,
    items: Vec<Item>,
}

static SCAN_SEQ: AtomicU64 = AtomicU64::new(0);
/// The most recent scans, by scan id. More than one: an abandoned slow scan (orphans) that finishes
/// after a newer one (an app) must not make the one on screen "out of date".
static SCANS: Mutex<Vec<ScanState>> = Mutex::new(Vec::new());
const KEEP_SCANS: usize = 4;

/// A scan's id, taken when the scan *starts*, so ids order scans by when they were asked for.
fn next_scan_id() -> u64 {
    SCAN_SEQ.fetch_add(1, Ordering::SeqCst) + 1
}

fn store_scan(scan_id: u64, app: Option<&AppRecord>, mut items: Vec<Item>) -> LeftoverScan {
    for (i, it) in items.iter_mut().enumerate() {
        it.lo.id = format!("{scan_id}-{i}");
    }
    let app_name = app.map(|a| a.display_name.clone());
    let out = LeftoverScan { scan_id, app: app_name.clone(), items: items.iter().map(|i| i.lo.clone()).collect() };
    let state = ScanState { scan_id, app_id: app.map(|a| a.app.id.clone()), app_name, toks: app.map(AppRecord::tokens), items };
    let mut g = SCANS.lock().unwrap_or_else(|e| e.into_inner());
    g.retain(|s| s.scan_id != scan_id);
    g.push(state);
    // Only the scans started last are kept: an older scan finishing late never pushes out a newer one.
    g.sort_by_key(|s| s.scan_id);
    let extra = g.len().saturating_sub(KEEP_SCANS);
    g.drain(..extra);
    out
}

fn stored_scan(scan_id: u64) -> Option<ScanState> {
    SCANS.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|s| s.scan_id == scan_id).cloned()
}

// ---------------------------------------------------------------------------------------------
// Removal

/// Re-checks an item right before removal, against a fresh context.
fn recheck(it: &Item, state: &ScanState, ctx: &Context, roots: &Roots, refs: &[Ref], scan_owners: &HashSet<Owner>, others: &OtherOwners) -> Result<(), String> {
    let self_id = state.app_id.clone();
    let allowed = |o: &Owner| scan_owners.contains(o) || matches!((o, &self_id), (Owner::App(a), Some(s)) if a == s);
    const MOVED_IN: &str = "Something new was added to it since the scan, so Platter left it alone.";
    // A service or task is re-judged as at scan time: an app's leftover must not now belong to
    // another installed app (its folder, its uninstaller's or icon's folder, its name); an orphan's
    // programs must still be gone and outside every installed app's folder.
    let foreign = |progs: &[&Path], names: &[&str]| -> Result<(), String> {
        let taken = if self_id.is_some() {
            progs.iter().any(|p| others.owns(p, names, state.toks.as_ref()))
        } else {
            progs.iter().any(|p| path_inside_any(p, &others.dirs))
        };
        if taken {
            return Err("Another installed app now uses it, so Platter left it alone.".into());
        }
        if self_id.is_none() && !progs.iter().all(|p| presence(p) == Presence::Missing) {
            return Err("Its program is back, so Platter left it alone.".into());
        }
        Ok(())
    };
    match &it.target {
        Target::Path(p) => {
            let md = std::fs::symlink_metadata(p).map_err(|_| "It's already gone.".to_string())?;
            if md.file_attributes() & 0x400 != 0 {
                return Err("It changed into a link since the scan, so Platter left it alone.".into());
            }
            if let Some(why) = refuse_path(p, &ctx.protected) {
                return Err(format!("Platter won't remove it because {why}."));
            }
            if roots.is_root(p) {
                return Err("Platter won't remove a main folder.".into());
            }
            if blocking_ref(p, refs, &allowed).is_some() {
                return Err("An installed app or a program still uses it, so Platter left it alone.".into());
            }
            if it.orphan_folder && orphan_folder_quiet(p).is_none() {
                return Err("It changed since the scan or now holds programs, so Platter left it alone.".into());
            }
            if let Some(expected) = &it.only_children {
                if child_names(p).iter().any(|c| !expected.contains(c)) {
                    return Err(MOVED_IN.into());
                }
            }
            Ok(())
        }
        Target::Key(k) => {
            if !reg::exists(k) {
                return Err("It's already gone.".into());
            }
            if !reg_allowed(k) {
                return Err("Platter doesn't remove registry keys in that place.".into());
            }
            let claimed = names_claimed_by_others(ctx, self_id.as_deref());
            let leaf = norm_name(k.leaf());
            let is_entry = k.parent().is_some_and(|p| p.leaf().eq_ignore_ascii_case("Uninstall") || p.leaf().eq_ignore_ascii_case("App Paths"));
            if !is_entry && claimed.contains(&leaf) {
                return Err("Another installed app uses that name, so Platter left it alone.".into());
            }
            // A publisher key offered whole: still only this app's settings (no new product's
            // subkey, no values of its own, nothing another app is named after).
            if let Some(expected) = &it.only_children {
                let key = reg::open_read(k).ok_or_else(|| "Platter couldn't check it again, so it left it alone.".to_string())?;
                let subs = key.subkeys();
                if subs.iter().any(|s| !expected.contains(&s.to_lowercase()) || claimed.contains(&norm_name(s))) || key.value_names().iter().any(|v| !v.is_empty()) {
                    return Err(MOVED_IN.into());
                }
            }
            if it.orphan_entry {
                if let Some(a) = ctx.apps.iter().find(|a| a.key.eq_ci(k)) {
                    if a.uninstaller_present() || a.location.as_ref().is_some_and(|l| presence(l) != Presence::Missing) {
                        return Err("This app seems to be back, so Platter left its entry alone.".into());
                    }
                }
            }
            Ok(())
        }
        Target::Value { key, name, data } => {
            if !startup_value_allowed(key, name) {
                return Err("Platter doesn't remove registry values in that place.".into());
            }
            let now = reg::open_read(key).and_then(|k| k.string(name));
            match now {
                None => Err("It's already gone.".into()),
                Some(d) if d != *data => Err("It changed since the scan, so Platter left it alone.".into()),
                Some(_) => Ok(()),
            }
        }
        Target::Service { name, image } => {
            let s = ctx.services.iter().find(|s| s.name.eq_ignore_ascii_case(name)).ok_or_else(|| "It's already gone.".to_string())?;
            if s.image != *image {
                return Err("It changed since the scan, so Platter left it alone.".into());
            }
            if s.program.as_ref().is_some_and(|p| in_windows(p)) {
                return Err("Platter doesn't remove Windows services.".into());
            }
            match &s.program {
                Some(p) => foreign(&[p.as_path()], &[&s.name, &s.display]),
                None => Err("Platter couldn't tell which program it runs, so it left it alone.".into()),
            }
        }
        Target::Task { path, programs } => {
            if path.to_ascii_lowercase().starts_with(r"\microsoft\") {
                return Err("Platter doesn't remove Windows tasks.".into());
            }
            let t = ctx.tasks.iter().find(|t| t.path.eq_ignore_ascii_case(path)).ok_or_else(|| "It's already gone.".to_string())?;
            if t.programs != *programs {
                return Err("It changed since the scan, so Platter left it alone.".into());
            }
            let progs: Vec<&Path> = t.programs.iter().map(Path::new).collect();
            foreign(&progs, &[&t.name])
        }
    }
}

/// `<base>\<yyyy-mm-dd HHmmss> <label>`, created on first use.
struct Backup {
    base: PathBuf,
    label: String,
    dir: Option<PathBuf>,
    used: HashSet<String>,
}

fn safe_file_name(s: &str, max: usize) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') { '_' } else { c })
        .collect();
    out = out.trim().trim_matches('.').trim().to_string();
    if out.chars().count() > max {
        out = out.chars().take(max).collect::<String>().trim().to_string();
    }
    if out.is_empty() {
        "item".into()
    } else {
        out
    }
}

fn local_stamp() -> String {
    let mut t: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
    format!("{:04}-{:02}-{:02} {:02}{:02}{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

impl Backup {
    fn new(base: PathBuf, label: &str) -> Backup {
        Backup { base, label: safe_file_name(label, 60), dir: None, used: HashSet::new() }
    }

    fn dir(&mut self) -> Result<PathBuf, String> {
        if let Some(d) = &self.dir {
            return Ok(d.clone());
        }
        let stamp = local_stamp();
        let mut d = self.base.join(format!("{stamp} {}", self.label));
        let mut n = 2;
        while d.exists() {
            d = self.base.join(format!("{stamp} {} ({n})", self.label));
            n += 1;
        }
        std::fs::create_dir_all(&d).map_err(|_| "Platter couldn't create its backup folder, so nothing was removed.".to_string())?;
        self.dir = Some(d.clone());
        Ok(d)
    }

    fn file(&mut self, stem: &str, ext: &str) -> Result<PathBuf, String> {
        let dir = self.dir()?;
        let stem = safe_file_name(stem, 120);
        let mut name = format!("{stem}.{ext}");
        let mut n = 2;
        while !self.used.insert(name.to_lowercase()) {
            name = format!("{stem} ({n}).{ext}");
            n += 1;
        }
        Ok(dir.join(name))
    }

    /// `reg.exe export "<key>" "<file>" /y /reg:64`; the backup must exist and be non-empty.
    fn export_key(&mut self, key: &RegKey) -> Result<PathBuf, String> {
        let file = self.file(&key.display().replace('\\', " "), "reg")?;
        let out = Command::new(system32("reg.exe"))
            .args(["export", &key.display(), &file.to_string_lossy(), "/y", "/reg:64"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
        let ok = matches!(out, Ok(s) if s.success()) && std::fs::metadata(&file).map(|m| m.len() > 0).unwrap_or(false);
        if ok {
            Ok(file)
        } else {
            Err("Platter couldn't back it up, so it wasn't removed.".into())
        }
    }
}

fn win32_sentence(code: u32, what: &str) -> String {
    match code {
        5 => ADMIN.to_string(),
        2 | 3 => "It's already gone.".to_string(),
        1072 => "Windows will finish removing it after a restart.".to_string(),
        _ => format!("Windows couldn't remove {what} (error {code})."),
    }
}

fn remove_service(name: &str) -> Result<(), String> {
    use windows_sys::Win32::System::Services::*;
    const DELETE: u32 = 0x0001_0000;
    let scm = open_scm(SC_MANAGER_CONNECT).ok_or_else(|| win32_sentence(unsafe { windows_sys::Win32::Foundation::GetLastError() }, "the service"))?;
    let n = wide(name);
    let h = ScHandle(unsafe { OpenServiceW(scm.0, n.as_ptr(), SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE) });
    if h.0.is_null() {
        let e = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        return Err(if e == 1060 { "It's already gone.".into() } else { win32_sentence(e, "the service") });
    }
    let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { QueryServiceStatus(h.0, &mut st) } != 0 && st.dwCurrentState != SERVICE_STOPPED {
        // A refused stop (1051 dependents running, 1052 doesn't accept stop...) won't finish by
        // waiting; 1062 means it wasn't running after all. A service already stopping or starting
        // (*_PENDING) refuses the control with 1061 but does reach a state on its own: wait for it.
        let pending = matches!(st.dwCurrentState, SERVICE_STOP_PENDING | SERVICE_START_PENDING | SERVICE_CONTINUE_PENDING | SERVICE_PAUSE_PENDING);
        let stop_ok = unsafe { ControlService(h.0, SERVICE_CONTROL_STOP, &mut st) } != 0;
        let err = if stop_ok { 0 } else { unsafe { windows_sys::Win32::Foundation::GetLastError() } };
        let refused = !stop_ok && err != 1062 && !(err == 1061 && pending);
        if !refused {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if unsafe { QueryServiceStatus(h.0, &mut st) } == 0 || st.dwCurrentState == SERVICE_STOPPED {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
    if unsafe { DeleteService(h.0) } == 0 {
        let e = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        if e != 1072 {
            return Err(win32_sentence(e, "the service"));
        }
    }
    // DeleteService only marks a service that is still running: it stays registered (and keeps its
    // files in use) until it stops or Windows restarts. That isn't "removed" yet.
    let stopped = unsafe { QueryServiceStatus(h.0, &mut st) } != 0 && st.dwCurrentState == SERVICE_STOPPED;
    service_outcome(stopped)
}

/// What a service removal reports once DeleteService succeeded (or it was already marked).
fn service_outcome(stopped: bool) -> Result<(), String> {
    if stopped {
        Ok(())
    } else {
        Err(win32_sentence(1072, "the service"))
    }
}

fn hresult_sentence(hr: i32, what: &str) -> String {
    let u = hr as u32;
    if u == 0x8007_0005 {
        return ADMIN.to_string();
    }
    if u == 0x8007_0002 || u == 0x8007_0003 {
        return "It's already gone.".to_string();
    }
    format!("Windows couldn't remove {what} (error 0x{u:08X}).")
}

fn write_utf16_file(path: &Path, text: &str) -> std::io::Result<()> {
    let mut bytes = vec![0xFF, 0xFE];
    for u in text.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    std::fs::write(path, bytes)
}

/// Removes one item that already passed [`recheck`]. Returns bytes freed.
fn remove_one(it: &Item, backup: &mut Backup) -> Result<u64, String> {
    match &it.target {
        Target::Path(p) => {
            crate::manage::recycle_sync(&[p.to_string_lossy().into_owned()])?;
            if std::fs::symlink_metadata(p).is_ok() {
                return Err("Windows left it in place. Close any program using it and try again.".into());
            }
            Ok(it.lo.size)
        }
        Target::Key(k) => {
            backup.export_key(k)?;
            reg::delete_tree(k).map_err(|e| win32_sentence(e, "the registry key"))?;
            if reg::exists(k) {
                return Err("Windows left part of the registry key in place.".into());
            }
            Ok(0)
        }
        Target::Value { key, name, .. } => {
            backup.export_key(key)?;
            reg::delete_value(key, name).map_err(|e| win32_sentence(e, "the startup entry"))?;
            Ok(0)
        }
        Target::Service { name, .. } => {
            let key = RegKey::new(Hive::Lm, &format!(r"SYSTEM\CurrentControlSet\Services\{name}"));
            backup.export_key(&key)?;
            remove_service(name)?;
            Ok(0)
        }
        Target::Task { path, .. } => {
            let p = path.clone();
            let (xml, _) = com::on_mta_thread(move || tasks::get(&p)).flatten().ok_or_else(|| "It's already gone.".to_string())?;
            if xml.trim().is_empty() {
                return Err("Platter couldn't back it up, so it wasn't removed.".into());
            }
            let file = backup.file(&path.trim_start_matches('\\').replace('\\', " "), "xml")?;
            write_utf16_file(&file, &xml).map_err(|_| "Platter couldn't back it up, so it wasn't removed.".to_string())?;
            let p = path.clone();
            com::on_mta_thread(move || tasks::delete(&p))
                .ok_or_else(|| "Windows couldn't remove the task.".to_string())?
                .map_err(|hr| hresult_sentence(hr, "the task"))?;
            Ok(0)
        }
    }
}

/// Sort key for removal: services, tasks, startup values, registry keys, then folders/files
/// (deeper paths before the folders holding them). Unknown ids sort first; they only fail.
fn removal_rank(it: Option<&Item>) -> (u8, std::cmp::Reverse<usize>) {
    let Some(it) = it else { return (0, std::cmp::Reverse(0)) };
    match &it.target {
        Target::Service { .. } => (1, std::cmp::Reverse(0)),
        Target::Task { .. } => (2, std::cmp::Reverse(0)),
        Target::Value { .. } => (3, std::cmp::Reverse(0)),
        Target::Key(_) => (4, std::cmp::Reverse(0)),
        Target::Path(p) => (5, std::cmp::Reverse(p.components().count())),
    }
}

fn remove_items(scan_id: u64, ids: &[String], roots: &Roots, elevated: bool) -> Result<RemoveResult, String> {
    // A copy, out of the lock: re-checking and removing can take a while.
    let state = stored_scan(scan_id).ok_or_else(|| STALE.to_string())?;
    let app_name = state.app_name.clone();
    let ctx = build_context(roots)?;
    let refs = build_refs(&ctx, roots);
    let others = OtherOwners::new(&ctx, roots, state.app_id.as_deref());
    // Entries found by this scan may point into folders found by this scan.
    let scan_owners: HashSet<Owner> = state
        .items
        .iter()
        .filter_map(|i| match &i.target {
            Target::Service { name, .. } => Some(Owner::Service(name.to_lowercase())),
            Target::Task { path, .. } => Some(Owner::Task(path.to_lowercase())),
            Target::Value { key, name, .. } => Some(Owner::Startup(format!("{}|{}", key.display().to_lowercase(), name.to_lowercase()))),
            Target::Key(k) if i.lo.kind == "registry" => Some(Owner::AppPath(k.display().to_lowercase())),
            _ => None,
        })
        .collect();
    let mut backup = Backup::new(roots.backup_base.clone(), app_name.as_deref().unwrap_or("Orphans"));
    let mut res = RemoveResult { removed: 0, failed: 0, freed: 0, backup_folder: None, items: Vec::new() };
    let mut seen: HashSet<&str> = HashSet::new();
    let unique: Vec<&String> = ids.iter().filter(|id| seen.insert(id.as_str())).collect();
    // Removed in dependency order whatever order the ids came in: what runs (services, tasks,
    // startup entries) first, so a service is stopped before its folder is recycled; then registry
    // entries; folders and files last, deepest first.
    let mut order: Vec<usize> = (0..unique.len()).collect();
    order.sort_by_key(|&i| removal_rank(state.items.iter().find(|it| it.lo.id == *unique[i])));
    let mut outcomes: Vec<Option<Result<u64, String>>> = vec![None; unique.len()];
    // Folders already recycled in this batch, with what they freed: a folder holding one of them
    // (a publisher folder around the app's own) frees only the rest.
    let mut recycled: Vec<(String, u64)> = Vec::new();
    for i in order {
        let outcome = match state.items.iter().find(|it| it.lo.id == *unique[i]) {
            None => Err(STALE.to_string()),
            Some(it) if it.lo.needs_admin && !elevated => Err(ADMIN.to_string()),
            Some(it) => recheck(it, &state, &ctx, roots, &refs, &scan_owners, &others).and_then(|_| remove_one(it, &mut backup)).map(|freed| {
                let Target::Path(p) = &it.target else { return freed };
                let pn = norm_path(p);
                let inner: u64 = recycled.iter().filter(|(r, _)| within(r, &pn)).map(|(_, s)| *s).sum();
                // Its own size already covers those inner folders.
                recycled.retain(|(r, _)| !within(r, &pn));
                recycled.push((pn, freed));
                freed.saturating_sub(inner)
            }),
        };
        outcomes[i] = Some(outcome);
    }
    for (id, outcome) in unique.into_iter().zip(outcomes) {
        let outcome = outcome.unwrap_or_else(|| Err(STALE.to_string()));
        match outcome {
            Ok(freed) => {
                res.removed += 1;
                res.freed += freed;
                res.items.push(RemoveItemResult { id: id.clone(), ok: true, error: None });
            }
            Err(e) => {
                res.failed += 1;
                res.items.push(RemoveItemResult { id: id.clone(), ok: false, error: Some(e) });
            }
        }
    }
    res.backup_folder = backup.dir.map(|d| d.display().to_string());
    Ok(res)
}

// ---------------------------------------------------------------------------------------------
// Running an uninstaller

static UNINSTALLING: AtomicBool = AtomicBool::new(false);
static STOP_WAITING: AtomicBool = AtomicBool::new(false);

struct BusyGuard;
impl Drop for BusyGuard {
    fn drop(&mut self) {
        UNINSTALLING.store(false, Ordering::SeqCst);
    }
}

/// The command to run: the entry's UninstallString, with `MsiExec /I` turned into `/X`.
fn launch_command(rec: &AppRecord) -> Result<Cmd, String> {
    let cmd = rec.cmd.as_ref().ok_or_else(|| MISSING.to_string())?;
    let cmd = msi_rewrite(cmd);
    if let Some(t) = command_target(&cmd) {
        if presence(&t) == Presence::Missing {
            return Err(MISSING.into());
        }
    }
    Ok(cmd)
}

/// ShellExecuteExW "open" (so an uninstaller that needs admin gets its own UAC prompt). Returns the
/// process handle as an integer (0 if Windows didn't give one).
fn shell_execute(cmd: &Cmd, owner: usize) -> Result<usize, String> {
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    #[link(name = "ole32")]
    extern "system" {
        fn CoInitializeEx(reserved: *const c_void, coinit: u32) -> i32;
        fn CoUninitialize();
    }
    let file = wide(&cmd.program);
    let params = wide(&cmd.args);
    let verb = wide("open");
    let dir = Path::new(&cmd.program).parent().map(|d| wide(&d.to_string_lossy()));
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    // No SEE_MASK_FLAG_NO_UI: the vendor's uninstaller may need its own UAC prompt.
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.hwnd = owner as windows_sys::Win32::Foundation::HWND;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = if cmd.args.is_empty() { std::ptr::null() } else { params.as_ptr() };
    info.lpDirectory = dir.as_ref().map_or(std::ptr::null(), |d| d.as_ptr());
    info.nShow = 1; // SW_SHOWNORMAL
    // ShellExecute may load shell extensions, which expect COM (STA) on the calling thread.
    let hr = unsafe { CoInitializeEx(std::ptr::null(), 0x2 | 0x4) };
    let ok = unsafe { ShellExecuteExW(&mut info) } != 0;
    let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    if hr >= 0 {
        unsafe { CoUninitialize() };
    }
    if ok {
        return Ok(info.hProcess as usize);
    }
    Err(match err {
        1223 => CANCELLED.to_string(),
        2 | 3 => MISSING.to_string(),
        5 | 740 => "Windows denied access to the uninstaller. Restart Platter as administrator and try again.".to_string(),
        e => format!("Windows couldn't start the uninstaller (error {e})."),
    })
}

fn sleep_unless_stopped(total: Duration) -> bool {
    let end = Instant::now() + total;
    while Instant::now() < end {
        if STOP_WAITING.load(Ordering::SeqCst) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    STOP_WAITING.load(Ordering::SeqCst)
}

fn run_uninstaller(rec: &AppRecord, owner: usize) -> Result<UninstallResult, String> {
    if !reg::exists(&rec.key) {
        return Ok(UninstallResult { removed: true, exit_code: None, stopped_waiting: false });
    }
    // Re-read the entry: the command may have changed since the list was made.
    let root = UninstallRoot { key: rec.key.parent().unwrap_or_else(|| rec.key.clone()), scope: rec.app.scope, tag: "" };
    let fresh = read_entry(&root, &rec.key_name).unwrap_or_else(|| rec.clone());
    let cmd = launch_command(&fresh)?;
    let msi = is_msiexec(&cmd);
    STOP_WAITING.store(false, Ordering::SeqCst);
    // Its own thread: ShellExecuteExW needs an STA, and pooled threads may be in another apartment.
    let process = std::thread::Builder::new()
        .name("platter-uninstaller".into())
        .spawn(move || shell_execute(&cmd, owner))
        .map_err(|_| "Windows couldn't start the uninstaller.".to_string())?
        .join()
        .map_err(|_| "Windows couldn't start the uninstaller.".to_string())??;

    let mut exit_code = None;
    let mut stopped = false;
    if process != 0 {
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
        let h = process as windows_sys::Win32::Foundation::HANDLE;
        loop {
            if STOP_WAITING.load(Ordering::SeqCst) {
                stopped = true;
                break;
            }
            if unsafe { WaitForSingleObject(h, 200) } == 0 {
                let mut code = 0u32;
                if unsafe { GetExitCodeProcess(h, &mut code) } != 0 {
                    exit_code = Some(code as i32);
                }
                break;
            }
        }
        unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
    }
    // Many uninstallers (Inno Setup, NSIS) copy themselves to %TEMP% and exit at once, so wait for
    // the entry itself to disappear: every 1 s for up to 10 min. Not after a cancel, nor after
    // msiexec, which only exits once its work is done.
    let deadline = Instant::now() + uninstall_wait(msi, exit_code);
    while !stopped && reg::exists(&rec.key) && Instant::now() < deadline {
        stopped = sleep_unless_stopped(Duration::from_secs(1));
    }
    let removed = !reg::exists(&rec.key);
    if !removed && msi && exit_code == Some(1618) {
        return Err("Another installation is running. Wait for it to finish, then try again.".into());
    }
    Ok(UninstallResult { removed, exit_code, stopped_waiting: stopped })
}

/// How long to wait for the Uninstall entry to disappear once the launched program has exited.
fn uninstall_wait(msi: bool, exit_code: Option<i32>) -> Duration {
    match exit_code {
        // 1602 (msiexec: user cancelled) / 1223: the user said no inside the uninstaller.
        Some(1602) | Some(1223) => Duration::ZERO,
        // msiexec is synchronous and never hands off to a copy of itself: success (0, 3010/1641
        // restart needed) gets a few seconds for the entry to go; any other code is final.
        Some(0) | Some(3010) | Some(1641) if msi => Duration::from_secs(5),
        Some(_) if msi => Duration::ZERO,
        _ => Duration::from_secs(600),
    }
}

// ---------------------------------------------------------------------------------------------
// Commands

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|_| "The operation stopped unexpectedly.".to_string())?
}

fn lookup_app(id: &str) -> Option<AppRecord> {
    remembered(id).or_else(|| {
        // Not listed in this session yet: read it straight from the registry.
        let (tag, key) = id.split_once('|')?;
        let root = real_uninstall_roots().into_iter().find(|r| r.tag == tag)?;
        read_entry(&root, key)
    })
}

/// Letters and digits only, lower-case: "Visual Studio Code" -> "visualstudiocode".
fn squash(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

/// Where an app's icon comes from: its DisplayIcon when that is a local file with icons in it,
/// else an .exe at the top of its install folder named after the app (never an uninstaller,
/// updater or setup program, whose icons would mislead). None: the list shows the app's initial.
fn icon_source(rec: &AppRecord) -> Option<(PathBuf, i32)> {
    // Network paths are skipped: an unreachable share would stall the list for seconds.
    let local = |p: &Path| !p.to_string_lossy().starts_with(r"\\") && presence(p) == Presence::Present;
    if let Some(i) = rec.icon.as_ref().filter(|i| local(i)) {
        let ext = i.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if matches!(ext.as_str(), "exe" | "dll" | "ico" | "icl" | "cpl" | "ocx" | "mun") {
            return Some((i.clone(), rec.icon_index));
        }
    }
    if let Some(spec) = msi_product_icon(rec) {
        if let Some(i) = icon_path(&spec).filter(|i| local(i)) {
            return Some((i, icon_index(&spec)));
        }
    }
    let dir = rec.location.as_ref().filter(|d| local(d) && d.is_dir())?;
    let name = squash(&rec.display_name);
    const NOT_THE_APP: [&str; 8] = ["unins", "uninst", "update", "setup", "install", "helper", "crash", "elevat"];
    for e in std::fs::read_dir(dir).ok()?.flatten().take(300) {
        let p = e.path();
        if !p.extension().is_some_and(|x| x.eq_ignore_ascii_case("exe")) || !p.is_file() {
            continue;
        }
        let stem = squash(&p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
        if stem.len() < 3 || NOT_THE_APP.iter().any(|w| stem.contains(w)) {
            continue;
        }
        if name.contains(&stem) || stem.contains(&name) {
            return Some((p, 0));
        }
    }
    None
}

/// Windows Installer's "packed" form of a product code, used under `Installer\Products`:
/// {12345678-ABCD-EF01-2345-6789ABCDEF01} -> 87654321DCBA10FE5432987654EBFC10 (each of the first
/// three groups reversed, then every byte of the rest nibble-swapped).
fn packed_product_code(key_name: &str) -> Option<String> {
    let s = key_name.trim();
    let inner = s.strip_prefix('{').and_then(|x| x.strip_suffix('}'))?;
    if !is_bare_guid(inner) {
        return None;
    }
    let hex: Vec<char> = inner.chars().filter(|c| *c != '-').map(|c| c.to_ascii_uppercase()).collect();
    let mut out = String::with_capacity(32);
    for (start, len) in [(0, 8), (8, 4), (12, 4)] {
        out.extend(hex[start..start + len].iter().rev());
    }
    for pair in hex[16..].chunks(2) {
        out.push(pair[1]);
        out.push(pair[0]);
    }
    Some(out)
}

/// The icon Windows Installer cached for an MSI product (`ProductIcon`, usually a file under
/// C:\Windows\Installer\{code}\): MSI entries rarely set DisplayIcon.
fn msi_product_icon(rec: &AppRecord) -> Option<String> {
    let packed = packed_product_code(&rec.key_name)?;
    [
        RegKey::new(Hive::Lm, &format!(r"SOFTWARE\Classes\Installer\Products\{packed}")),
        RegKey::new(Hive::Cu, &format!(r"Software\Microsoft\Installer\Products\{packed}")),
    ]
    .iter()
    .find_map(|k| reg::open_read(k).and_then(|k| k.string("ProductIcon")))
}

/// Icons already made this session, by app id (None = the app has none).
static ICONS: Mutex<Option<HashMap<String, Option<String>>>> = Mutex::new(None);

/// The app's icon as a PNG data URL, or null when it has none (the UI then shows its initial).
#[tauri::command]
pub async fn app_icon(id: String) -> Result<Option<String>, String> {
    if let Some(hit) = ICONS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&id).cloned()) {
        return Ok(hit);
    }
    blocking(move || {
        let rec = lookup_app(&id).ok_or_else(|| UNKNOWN_APP.to_string())?;
        let url = icon_source(&rec).and_then(|(file, index)| crate::appicon::data_url(&file, index));
        ICONS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(id, url.clone());
        Ok(url)
    })
    .await
}

#[tauri::command]
pub async fn apps_list() -> Result<Vec<InstalledApp>, String> {
    blocking(|| {
        let apps = read_apps(&real_uninstall_roots());
        remember(&apps);
        Ok(apps.into_iter().map(|a| a.app).collect())
    })
    .await
}

#[tauri::command]
pub async fn app_uninstall(app: tauri::AppHandle, id: String) -> Result<UninstallResult, String> {
    let owner = app.get_webview_window("main").and_then(|w| w.hwnd().ok()).map(|h| h.0 as usize).unwrap_or(0);
    if UNINSTALLING.swap(true, Ordering::SeqCst) {
        return Err(BUSY.into());
    }
    let guard = BusyGuard;
    blocking(move || {
        let _guard = guard;
        let rec = lookup_app(&id).ok_or_else(|| UNKNOWN_APP.to_string())?;
        run_uninstaller(&rec, owner)
    })
    .await
}

#[tauri::command]
pub async fn app_uninstall_stop_waiting() -> Result<(), String> {
    STOP_WAITING.store(true, Ordering::SeqCst);
    Ok(())
}

#[tauri::command]
pub async fn app_leftovers(id: String) -> Result<LeftoverScan, String> {
    let scan_id = next_scan_id();
    blocking(move || {
        let rec = lookup_app(&id).ok_or_else(|| UNKNOWN_APP.to_string())?;
        let roots = Roots::real();
        let ctx = build_context(&roots)?;
        let items = scan_app(&rec, &ctx, &roots);
        Ok(store_scan(scan_id, Some(&rec), items))
    })
    .await
}

#[tauri::command]
pub async fn orphan_leftovers() -> Result<LeftoverScan, String> {
    let scan_id = next_scan_id();
    blocking(move || {
        let roots = Roots::real();
        let ctx = build_context(&roots)?;
        let items = scan_orphans(&ctx, &roots);
        Ok(store_scan(scan_id, None, items))
    })
    .await
}

#[tauri::command]
pub async fn leftovers_remove(scan_id: u64, ids: Vec<String>) -> Result<RemoveResult, String> {
    blocking(move || remove_items(scan_id, &ids, &Roots::real(), is_elevated())).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn fake_exists(list: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |p: &str| list.iter().any(|x| x.eq_ignore_ascii_case(p))
    }

    #[test]
    fn uninstall_splits_commands() {
        let none = fake_exists(&[]);
        let c = split_command(r#""C:\Program Files\Foo App\unins000.exe" /SILENT"#, &none).unwrap();
        assert_eq!(c.program, r"C:\Program Files\Foo App\unins000.exe");
        assert_eq!(c.args, "/SILENT");

        // Unquoted with spaces: the longest prefix ending in .exe that exists.
        let ex = fake_exists(&[r"C:\Program Files\Foo App\uninstall.exe"]);
        let c = split_command(r"C:\Program Files\Foo App\uninstall.exe /S _?=C:\Program Files\Foo App", &ex).unwrap();
        assert_eq!(c.program, r"C:\Program Files\Foo App\uninstall.exe");
        assert_eq!(c.args, r"/S _?=C:\Program Files\Foo App");

        // Two .exe in the line, only the first exists → the first.
        let ex = fake_exists(&[r"C:\Tools\a b\setup.exe"]);
        let c = split_command(r"C:\Tools\a b\setup.exe --uninstall C:\Tools\a b\app.exe", &ex).unwrap();
        assert_eq!(c.program, r"C:\Tools\a b\setup.exe");
        // None exists → the first .exe prefix (reported as missing later).
        let c = split_command(r"C:\Gone Dir\x.exe /u C:\Gone Dir\y.exe", &none).unwrap();
        assert_eq!(c.program, r"C:\Gone Dir\x.exe");

        // Bare system tools resolve to System32.
        let c = split_command("MsiExec.exe /I{12345678-1234-1234-1234-123456789ABC}", &none).unwrap();
        assert!(c.program.to_lowercase().ends_with(r"\system32\msiexec.exe"), "{}", c.program);
        assert!(is_msiexec(&c));
        assert_eq!(command_target(&c), None);

        let c = split_command(r#"RunDll32 C:\PROGRA~2\Common Files\Vendor\Ctor.dll,LaunchSetup "C:\x\setup.exe" -l0x9"#, &none).unwrap();
        assert!(is_rundll(&c), "{c:?}");
        assert_eq!(command_target(&c), Some(PathBuf::from(r"C:\PROGRA~2\Common Files\Vendor\Ctor.dll")));
        let c = split_command(r#"rundll32.exe "C:\Program Files\V\x.dll",Uninstall"#, &none).unwrap();
        assert_eq!(command_target(&c), Some(PathBuf::from(r"C:\Program Files\V\x.dll")));

        // Kernel-style service image paths.
        let c = split_command(r"\??\C:\Program Files\Svc\svc.exe -k", &none).unwrap();
        assert_eq!(c.program, r"C:\Program Files\Svc\svc.exe");
        let c = split_command(r"\SystemRoot\System32\svchost.exe -k netsvcs", &none).unwrap();
        assert!(c.program.to_lowercase().ends_with(r"\system32\svchost.exe"));
        assert!(split_command("   ", &none).is_none());
        // ".exe" inside a folder name is not the end of the program.
        let c = split_command(r"C:\My.exeFolder\run.exe /x", &none).unwrap();
        assert_eq!(c.program, r"C:\My.exeFolder\run.exe");
    }

    #[test]
    fn uninstall_msi_rewrite() {
        let none = fake_exists(&[]);
        let c = split_command("MsiExec.exe /I{AC76BA86-7AD7-1033-7B44-AC0F074E4100}", &none).unwrap();
        let r = msi_rewrite(&c);
        assert!(r.program.to_lowercase().ends_with(r"\system32\msiexec.exe"));
        assert_eq!(r.args, "/X{AC76BA86-7AD7-1033-7B44-AC0F074E4100}");
        let c = split_command("msiexec /x {AC76BA86-7AD7-1033-7B44-AC0F074E4100} /qb", &none).unwrap();
        assert_eq!(msi_rewrite(&c).args, "/X{AC76BA86-7AD7-1033-7B44-AC0F074E4100}");
        // Not msiexec: unchanged.
        let c = split_command(r#""C:\a\u.exe" /I{AC76BA86-7AD7-1033-7B44-AC0F074E4100}"#, &none).unwrap();
        assert_eq!(msi_rewrite(&c), c);
        assert_eq!(find_guid("x{not-a-guid}y"), None);
        assert!(is_guid_name("{AC76BA86-7AD7-1033-7B44-AC0F074E4100}_is1"));
        assert!(is_guid_name("AC76BA867AD710337B44AC0F074E4100"));
        assert!(!is_guid_name("7-Zip"));
    }

    #[test]
    fn uninstall_small_parsers() {
        assert_eq!(parse_install_date("20250314").as_deref(), Some("2025-03-14"));
        assert_eq!(parse_install_date("20251399"), None);
        assert_eq!(parse_install_date("3/14/2025"), None);
        assert_eq!(icon_path(r#""C:\P F\app.exe",0"#), Some(PathBuf::from(r"C:\P F\app.exe")));
        assert_eq!(icon_path(r"C:\P F\app.exe,-101"), Some(PathBuf::from(r"C:\P F\app.exe")));
        assert_eq!(icon_path(r"C:\P F\app.ico"), Some(PathBuf::from(r"C:\P F\app.ico")));
        assert_eq!(icon_index(r#""C:\P F\app.exe",0"#), 0);
        assert_eq!(icon_index(r#""C:\P F\app.exe",-101"#), -101);
        assert_eq!(icon_index(r"C:\P F\app.exe,2"), 2);
        assert_eq!(icon_index(r"C:\P F\app.ico"), 0);
        assert_eq!(icon_index(r"C:\a,b\app.exe"), 0);
        assert_eq!(packed_product_code("{12345678-ABCD-EF01-2345-6789ABCDEF01}").as_deref(), Some("87654321DCBA10FE32547698BADCFE10"));
        assert_eq!(packed_product_code("7-Zip"), None);
        assert!(task_owned_by(r"DESKTOP-1\sam", "Sam"));
        assert!(task_owned_by("sam", "sam"));
        assert!(!task_owned_by("S-1-5-18", "sam"));
        assert!(!task_owned_by("", "sam"));
        assert!(!task_owned_by(r"X\samuel", "sam"));
    }

    #[test]
    fn uninstall_name_tokens() {
        assert_eq!(clean_display_name("Mozilla Firefox (x64 en-US)"), "Mozilla Firefox");
        assert_eq!(clean_display_name("7-Zip 23.01 (x64)"), "7-Zip");
        assert_eq!(clean_display_name("Python 3.11.4 (64-bit)"), "Python");
        assert_eq!(clean_display_name("Foo Bar version 2.0"), "Foo Bar");
        assert_eq!(clean_display_name("Microsoft Visual Studio 2019"), "Microsoft Visual Studio 2019");
        assert_eq!(clean_display_name("Notepad++ (64-bit x64)"), "Notepad++");
        assert_eq!(clean_display_name("WinRAR 6.24 x64 v6"), "WinRAR");
        assert_eq!(clean_display_name("Thing - 14.38.33130"), "Thing");
        assert_eq!(clean_publisher("Google LLC"), "Google");
        assert_eq!(clean_publisher("Valve Corporation"), "Valve");
        assert_eq!(clean_publisher("Foo Co., Ltd."), "Foo");
        assert_eq!(clean_publisher("Mozilla"), "Mozilla");
        assert_eq!(clean_publisher("Acme GmbH"), "Acme");
        assert_eq!(clean_publisher("Big S.A."), "Big");

        let t = app_tokens("Mozilla Firefox (x64 en-US)", "Mozilla", "Mozilla Firefox 120.0 (x64 en-US)", Some(Path::new(r"C:\Program Files\Mozilla Firefox")), None);
        assert!(t.has("mozillafirefox"), "{t:?}");
        assert!(t.has("firefox"), "{t:?}");
        assert_eq!(t.publisher.as_deref(), Some("mozilla"));
        assert!(t.publisher_folder_ok);

        let t = app_tokens("7-Zip 23.01 (x64)", "Igor Pavlov", "7-Zip", Some(Path::new(r"C:\Program Files\7-Zip")), None);
        assert_eq!(t.names, vec!["7zip".to_string()]);

        // Google is fine to descend into (Google\Chrome) but never offered as a folder itself.
        let t = app_tokens("Google Chrome", "Google LLC", "Google Chrome", None, None);
        assert!(t.has("chrome") && t.has("googlechrome"), "{t:?}");
        assert_eq!(t.publisher.as_deref(), Some("google"));
        assert!(!t.publisher_folder_ok);

        // Microsoft: no publisher-based matching at all.
        let t = app_tokens("Microsoft Teams", "Microsoft Corporation", "Teams", None, None);
        assert_eq!(t.publisher, None);
        assert!(t.has("teams") && t.has("microsoftteams"), "{t:?}");

        // GUID key names and generic folder leaves add nothing.
        let t = app_tokens("Git", "The Git Development Community", "{AC76BA86-7AD7-1033-7B44-AC0F074E4100}", Some(Path::new(r"C:\Program Files\Git")), Some(Path::new(r"C:\Program Files\Common Files\Setup")));
        assert!(t.names.is_empty(), "{t:?}");
    }

    #[test]
    fn uninstall_generic_words_rejected() {
        for w in ["Update", "Setup", "Microsoft", "Windows", "Common Files", "Tools", "Programs", "Data", "Temp", "Cache", "Packages", "Google", "Intel", "NVIDIA", "AMD", "Runtime", "Redistributable", "Installer", "Driver", "Media Player", "Update Tools", "App", "Git", "VLC", "2024", "{AC76BA86-7AD7-1033-7B44-AC0F074E4100}"] {
            assert_eq!(usable(w), None, "{w} should be rejected");
        }
        for w in ["Firefox", "7-Zip", "Notepad++", "VLC media player", "Zebra Quill Studio"] {
            assert!(usable(w).is_some(), "{w} should be usable");
        }
        // Name matching is equality of normalized names, never a substring of short tokens.
        let t = app_tokens("Zebra Quill", "", "ZQ", None, None);
        assert!(t.has("zebraquill"));
        assert!(!t.has("zebra"));
        assert!(!t.loosely_named("Zebra"));
        assert!(t.loosely_named("ZebraQuill Update Service"));
    }

    #[test]
    fn uninstall_registry_whitelist() {
        let cu = |p: &str| RegKey::new(Hive::Cu, p);
        let lm = |p: &str| RegKey::new(Hive::Lm, p);
        for k in [
            cu("Software"),
            cu(r"Software\Microsoft"),
            cu(r"Software\Microsoft\Office"),
            cu(r"Software\Classes"),
            cu(r"Software\Classes\.txt"),
            cu(r"Software\Policies\Vendor"),
            cu(r"Software\Windows"),
            cu(r"Software\Clients\StartMenuInternet"),
            cu(r"Software\RegisteredApplications"),
            lm(r"SOFTWARE\WOW6432Node"),
            lm(r"SOFTWARE\WOW6432Node\Microsoft\Something"),
            lm(r"SOFTWARE\WOW6432Node\Classes\x"),
            lm(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"),
            lm(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Foo\Sub"),
            lm(r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths"),
            lm(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run"),
            lm(r"SYSTEM\CurrentControlSet\Services\Foo"),
            lm(r"SOFTWARE\Wow6432Node\Wow6432Node\x"),
        ] {
            assert!(!reg_allowed(&k), "{} must be refused", k.display());
        }
        for k in [
            cu(r"Software\Vendor"),
            cu(r"Software\Vendor\App"),
            lm(r"SOFTWARE\WOW6432Node\Vendor\App"),
            lm(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Foo_is1"),
            lm(r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\{AC76BA86-7AD7-1033-7B44-AC0F074E4100}"),
            cu(r"Software\Microsoft\Windows\CurrentVersion\App Paths\foo.exe"),
        ] {
            assert!(reg_allowed(&k), "{} should be allowed", k.display());
        }
        let run = cu(r"Software\Microsoft\Windows\CurrentVersion\Run");
        assert!(startup_value_allowed(&run, "Foo"));
        assert!(!startup_value_allowed(&run, ""));
        assert!(startup_value_allowed(&lm(r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\RunOnce"), "x"));
        assert!(!startup_value_allowed(&cu(r"Software\Microsoft\Windows\CurrentVersion\RunServices"), "x"));
        assert!(!startup_value_allowed(&cu(r"Software\Vendor\Run"), "x"));
    }

    #[test]
    fn uninstall_protected_folders_refused() {
        let prot = protected_paths();
        let env = |v: &str| crate::util::env_path(v).unwrap();
        let profile = env("USERPROFILE");
        let mut refused: Vec<PathBuf> = vec![
            PathBuf::from(r"C:\"),
            PathBuf::from(r"C:\Users"),
            system_root(),
            system_root().join(r"Temp\Foo"),
            system_root().join(r"System32\drivers"),
            profile.clone(),
            profile.join("AppData"),
            profile.join(r"AppData\Local"),
            profile.join(r"AppData\Local\Programs"),
            profile.join(r"AppData\LocalLow"),
            profile.join("Documents"),
            env("APPDATA"),
            env("LOCALAPPDATA"),
            env("ProgramData"),
            env("ProgramFiles"),
            env("ProgramData").join(r"Package Cache\{AC76BA86-7AD7-1033-7B44-AC0F074E4100}"),
            env("ProgramData").join("Microsoft"),
            PathBuf::from(r"relative\path"),
            env("ProgramFiles").join(r"Foo\..\..\Windows"),
        ];
        if let Some(p) = env_dir("ProgramFiles(x86)") {
            refused.push(p.clone());
            refused.push(p.join("Common Files").join(".."));
        }
        if let Some(p) = env_dir("CommonProgramFiles") {
            refused.push(p);
        }
        for p in &refused {
            assert!(refuse_path(p, &prot).is_some(), "{} must be refused", p.display());
        }
        // A parent of a protected folder is refused too (it contains one).
        assert!(refuse_path(profile.parent().unwrap(), &prot).is_some());
        // Ordinary app folders are fine.
        assert_eq!(refuse_path(&env("ProgramFiles").join("Some Vendor App"), &prot), None);
        assert_eq!(refuse_path(&env("APPDATA").join("Some Vendor App"), &prot), None);
        assert_eq!(refuse_path(&env("LOCALAPPDATA").join(r"Programs\Some App"), &prot), None);
    }

    // ----- End to end on a throwaway tree under %TEMP% and HKCU\Software\PlatterTest-<pid> -----

    struct TestEnv {
        dir: PathBuf,
        key: RegKey,
    }

    impl TestEnv {
        fn new(tag: &str) -> TestEnv {
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let pid = std::process::id();
            let dir = std::env::temp_dir().join(format!("platter-uninstall-{tag}-{pid}-{nanos}"));
            assert!(dir.starts_with(std::env::temp_dir()));
            std::fs::create_dir_all(&dir).unwrap();
            let key = RegKey::new(Hive::Cu, &format!(r"Software\PlatterTest-{pid}-{tag}-{nanos}"));
            TestEnv { dir, key }
        }
        fn file(&self, rel: &str, bytes: usize) -> PathBuf {
            let p = self.dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, vec![b'x'; bytes]).unwrap();
            p
        }
        fn roots(&self) -> Roots {
            Roots {
                machine: vec![self.dir.join("PF")],
                user: vec![self.dir.join("Roaming"), self.dir.join("Local")],
                orphan_dirs: vec![(self.dir.join("PF"), true)],
                reg_bases: vec![self.key.clone()],
                run_keys: vec![],
                app_paths_keys: vec![],
                uninstall: vec![UninstallRoot { key: self.key.child("Uninstall"), scope: "user", tag: "t" }],
                system: false,
                backup_base: self.dir.join("backups"),
            }
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            assert!(self.dir.starts_with(std::env::temp_dir()));
            let _ = std::fs::remove_dir_all(&self.dir);
            assert!(self.key.path.starts_with(r"Software\PlatterTest-"));
            let _ = reg::delete_tree(&self.key);
        }
    }

    fn find<'a>(items: &'a [Item], kind: &str, path_end: &str) -> Option<&'a Item> {
        items.iter().find(|i| i.lo.kind == kind && i.lo.path.to_lowercase().ends_with(&path_end.to_lowercase()))
    }

    fn show(items: &[Item]) {
        for i in items {
            println!("{:>15} {:6} admin={} on={} {} ({}) — {}", i.lo.kind, i.lo.confidence, i.lo.needs_admin, i.lo.default_on, i.lo.path, i.lo.size, i.lo.reason);
        }
    }

    #[test]
    fn uninstall_install_location_needs_evidence() {
        let env = TestEnv::new("loc");
        env.file(r"PF\Shared Games\other game\game.dat", 10);
        env.file(r"PF\Kiwi Paint\kiwi.exe", 10);
        let un = env.key.child("Uninstall");
        // InstallLocation names a shared folder: offered only as low confidence, off by default.
        let a = un.child("KiwiOne");
        reg::set_string(&a, "DisplayName", "Kiwi Sketch");
        reg::set_string(&a, "InstallLocation", &env.dir.join(r"PF\Shared Games").to_string_lossy());
        // InstallLocation without a matching name, but it holds the app's own icon: high.
        let b = un.child("KiwiTwo");
        reg::set_string(&b, "DisplayName", "Kiwi Painter Pro");
        reg::set_string(&b, "InstallLocation", &env.dir.join(r"PF\Kiwi Paint").to_string_lossy());
        reg::set_string(&b, "DisplayIcon", &format!("{},0", env.dir.join(r"PF\Kiwi Paint\kiwi.exe").display()));
        let roots = env.roots();
        let ctx = build_context(&roots).unwrap();
        let one = ctx.apps.iter().find(|x| x.key_name == "KiwiOne").unwrap();
        let items = scan_app(one, &ctx, &roots);
        show(&items);
        let f = find(&items, "folder", r"PF\Shared Games").expect("recorded folder");
        assert_eq!((f.lo.confidence, f.lo.default_on), (LOW, false));
        let two = ctx.apps.iter().find(|x| x.key_name == "KiwiTwo").unwrap();
        let items = scan_app(two, &ctx, &roots);
        show(&items);
        assert_eq!(find(&items, "folder", r"PF\Kiwi Paint").expect("own folder").lo.confidence, HIGH);
    }

    fn fake_service(name: &str, program: &Path) -> SvcInfo {
        SvcInfo { name: name.into(), display: name.into(), image: format!("\"{}\"", program.display()), program: Some(program.to_path_buf()) }
    }

    #[test]
    fn uninstall_doubtful_location_claims_nothing() {
        let env = TestEnv::new("broad");
        let d = &env.dir;
        env.file(r"PF\Shared Games\other game\game.dat", 10);
        let other_svc = env.file(r"PF\Shared Games\other game\othersvc.exe", 10);
        let vendor_svc = env.file(r"PF\Vendor Update\vupd.exe", 10);
        let un = env.key.child("Uninstall");
        // A shared folder recorded as the InstallLocation (LOW)...
        let a = un.child("Kiwi");
        reg::set_string(&a, "DisplayName", "Kiwi Sketch");
        reg::set_string(&a, "InstallLocation", &d.join(r"PF\Shared Games").to_string_lossy());
        // ...and a search root recorded as one (refused outright).
        let b = un.child("Broad");
        reg::set_string(&b, "DisplayName", "Broad Thing");
        reg::set_string(&b, "InstallLocation", &d.join("PF").to_string_lossy());
        // Its install folder is gone, but it was named after the app: its service is still its own.
        let c = un.child("Gone");
        reg::set_string(&c, "DisplayName", "Gonzo Writer");
        reg::set_string(&c, "InstallLocation", &d.join(r"PF\Gonzo Writer").to_string_lossy());
        let roots = env.roots();
        let mut ctx = build_context(&roots).unwrap();
        ctx.services.push(fake_service("OtherGameSvc", &other_svc));
        ctx.services.push(fake_service("VendorUpdate", &vendor_svc));
        ctx.services.push(fake_service("GonzoHelper", &d.join(r"PF\Gonzo Writer\helper.exe")));
        ctx.tasks.push(TaskInfo { path: r"\Vendor Update Task".into(), name: "Vendor Update Task".into(), programs: vec![vendor_svc.to_string_lossy().into_owned()], user: String::new() });

        let kiwi = ctx.apps.iter().find(|x| x.key_name == "Kiwi").unwrap();
        let items = scan_app(kiwi, &ctx, &roots);
        show(&items);
        assert!(!items.iter().any(|i| i.lo.kind == "service" || i.lo.kind == "task"), "a LOW folder claims no services or tasks");
        // The other game's service still runs from the shared folder, so the folder isn't offered.
        assert!(find(&items, "folder", r"PF\Shared Games").is_none());

        let broad = ctx.apps.iter().find(|x| x.key_name == "Broad").unwrap();
        let items = scan_app(broad, &ctx, &roots);
        show(&items);
        assert!(!items.iter().any(|i| matches!(i.lo.kind, "service" | "task" | "folder")), "a refused location claims nothing");

        let gone = ctx.apps.iter().find(|x| x.key_name == "Gone").unwrap();
        let items = scan_app(gone, &ctx, &roots);
        show(&items);
        let s = find(&items, "service", "GonzoHelper").expect("service of the deleted, trusted folder");
        assert_eq!((s.lo.confidence, s.lo.default_on), (HIGH, true));
        assert!(find(&items, "service", "VendorUpdate").is_none());
    }

    #[test]
    fn uninstall_publisher_location_not_trusted() {
        let env = TestEnv::new("vendor");
        let d = &env.dir;
        // The app recorded its vendor's folder; its uninstaller sits deep inside it, next to a
        // sibling product that has no Uninstall entry of its own (no ref to protect it).
        let uninst = env.file(r"PF\Acmecorp\Writer\Setup\uninst.exe", 10);
        let sib_svc = env.file(r"PF\Acmecorp\Sibling Product\sibsvc.exe", 10);
        env.file(r"PF\Acmecorp\Writer\writer.exe", 10);
        let un = env.key.child("Uninstall");
        let a = un.child("AcmeWriter");
        reg::set_string(&a, "DisplayName", "Acme Writer");
        reg::set_string(&a, "Publisher", "Acmecorp Inc.");
        reg::set_string(&a, "InstallLocation", &d.join(r"PF\Acmecorp").to_string_lossy());
        reg::set_string(&a, "UninstallString", &format!("\"{}\" /S", uninst.display()));
        let roots = env.roots();
        let mut ctx = build_context(&roots).unwrap();
        ctx.services.push(fake_service("SiblingSvc", &sib_svc));
        let rec = ctx.apps.iter().find(|x| x.key_name == "AcmeWriter").unwrap().clone();
        let t = rec.tokens();
        assert_eq!(t.publisher.as_deref(), Some("acmecorp"), "{t:?}");
        assert!(!t.has("acmecorp"), "a publisher-named location is not an app name: {t:?}");
        let items = scan_app(&rec, &ctx, &roots);
        show(&items);
        assert!(find(&items, "service", "SiblingSvc").is_none(), "the sibling's service isn't this app's");
        // The vendor folder is never offered as the app's own: the sibling's service blocks it.
        assert!(!items.iter().any(|i| i.lo.kind == "folder" && i.lo.confidence != LOW), "nothing above LOW");
        assert!(find(&items, "folder", r"PF\Acmecorp").is_none());

        // Without the sibling, the vendor folder is still only LOW (not every child is the app's).
        ctx.services.clear();
        let items = scan_app(&rec, &ctx, &roots);
        show(&items);
        let f = find(&items, "folder", r"PF\Acmecorp").expect("recorded folder, low");
        assert_eq!((f.lo.confidence, f.lo.default_on), (LOW, false));
    }

    #[test]
    fn uninstall_install_folder_trust_rules() {
        let env = TestEnv::new("trust");
        let d = &env.dir;
        let mk = |name: &str, publisher: &str, loc: &Path, target: Option<PathBuf>| AppRecord {
            app: InstalledApp { id: format!("t|{name}"), name: name.into(), publisher: publisher.into(), version: String::new(), install_date: None, size: 0, install_location: None, scope: "user", can_uninstall: false, broken: false },
            key: env.key.child(name),
            key_name: name.into(),
            display_name: name.into(),
            publisher: publisher.into(),
            cmd: None,
            target,
            location: Some(loc.to_path_buf()),
            icon: None,
            icon_index: 0,
        };
        let none = HashSet::new();
        let judge = |rec: &AppRecord| {
            let toks = rec.tokens();
            let name_toks = app_tokens(&rec.display_name, &rec.publisher, &rec.key_name, None, None);
            install_folder_trust(rec.location.as_ref().unwrap(), rec, &toks, &name_toks, &none, &none).0
        };
        // Uninstaller at the top, or one folder down: HIGH.
        let top = d.join(r"PF\Pine Notes");
        assert_eq!(judge(&mk("Pinecone", "", &top, Some(top.join("unins000.exe")))), HIGH);
        assert_eq!(judge(&mk("Pinecone", "", &top, Some(top.join(r"bin\u.exe")))), HIGH);
        // Deeper than that: no longer evidence for the whole folder.
        assert_eq!(judge(&mk("Pinecone", "", &top, Some(top.join(r"a\b\u.exe")))), LOW);
        // A generic or shared folder name never becomes HIGH from its contents.
        let games = d.join(r"PF\Games");
        assert_eq!(judge(&mk("Pinecone", "", &games, Some(games.join("u.exe")))), LOW);
        // Named after the publisher (here a short one, "HP"): publisher rules, not the uninstaller.
        let hp = d.join(r"PF\HP");
        assert_eq!(judge(&mk("Smart Print Thing", "HP Inc.", &hp, Some(hp.join("u.exe")))), LOW);
        // Named after the app: HIGH.
        assert_eq!(judge(&mk("Pine Notes", "", &top, None)), HIGH);
    }

    #[test]
    fn uninstall_task_paths_use_the_tasks_user() {
        let profiles = |sid: &str| (sid == "S-1-5-21-1-2-3-1001").then(|| PathBuf::from(r"C:\Users\other"));
        // Machine-wide variables: expanded as always.
        let pf = reg::expand("%ProgramFiles%");
        assert_eq!(resolve_task_program(r"%ProgramFiles%\V\a.exe", "S-1-5-21-9", "me", &profiles), format!(r"{pf}\V\a.exe"));
        // Per-user variables, another account by SID: that account's profile.
        assert_eq!(
            resolve_task_program(r"%localappdata%\Microsoft\OneDrive\upd.exe", "S-1-5-21-1-2-3-1001", "me", &profiles),
            r"C:\Users\other\AppData\Local\Microsoft\OneDrive\upd.exe"
        );
        assert_eq!(resolve_task_program(r"%APPDATA%\x.exe", "S-1-5-21-1-2-3-1001", "me", &profiles), r"C:\Users\other\AppData\Roaming\x.exe");
        // Per-user variables, an account Platter can't place: left as written (never "missing").
        let raw = r"%LOCALAPPDATA%\Zoom\bin\zoom.exe";
        assert_eq!(resolve_task_program(raw, r"PC\someoneelse", "me", &profiles), raw);
        assert_eq!(resolve_task_program(raw, "", "me", &profiles), raw);
        assert_eq!(resolve_task_program(raw, "S-1-5-21-404", "me", &profiles), raw);
        assert_eq!(presence(Path::new(raw)), Presence::Unknown);
        // An unknown variable on a known profile: left as written too.
        assert_eq!(resolve_task_program(r"%ONEDRIVE%\x.exe", "S-1-5-21-1-2-3-1001", "me", &profiles), r"%ONEDRIVE%\x.exe");
        // The task runs as Platter's own user: its own environment.
        assert_eq!(resolve_task_program(raw, r"PC\me", "me", &profiles), reg::expand(raw));
        assert_eq!(env_vars_in("a%B%c%d%e"), vec!["b".to_string(), "d".to_string()]);
        assert!(env_vars_in("50% off").is_empty());
    }

    #[test]
    fn uninstall_waits_and_service_outcome() {
        assert_eq!(uninstall_wait(true, Some(1618)), Duration::ZERO);
        assert_eq!(uninstall_wait(true, Some(1603)), Duration::ZERO);
        assert_eq!(uninstall_wait(true, Some(0)), Duration::from_secs(5));
        assert_eq!(uninstall_wait(false, Some(1602)), Duration::ZERO);
        assert_eq!(uninstall_wait(false, Some(0)), Duration::from_secs(600));
        assert_eq!(uninstall_wait(true, None), Duration::from_secs(600));
        assert_eq!(service_outcome(true), Ok(()));
        assert_eq!(service_outcome(false).unwrap_err(), "Windows will finish removing it after a restart.");
    }

    #[test]
    fn uninstall_removal_order() {
        let mk = |kind: &'static str, target: Target| item(kind, String::new(), String::new(), "", 0, HIGH, false, target);
        let folder = mk("folder", Target::Path(PathBuf::from(r"C:\PF\App")));
        let inner = mk("folder", Target::Path(PathBuf::from(r"C:\PF\App\Sub")));
        let svc = mk("service", Target::Service { name: "s".into(), image: String::new() });
        let task = mk("task", Target::Task { path: r"\t".into(), programs: vec![] });
        let key = mk("registry", Target::Key(RegKey::new(Hive::Cu, r"Software\X")));
        let mut v = vec![&folder, &key, &inner, &task, &svc];
        v.sort_by_key(|i| removal_rank(Some(*i)));
        let kinds: Vec<String> = v.iter().map(|i| format!("{}:{:?}", i.lo.kind, matches!(&i.target, Target::Path(p) if p.ends_with("Sub")))).collect();
        assert_eq!(kinds, ["service:false", "task:false", "registry:false", "folder:true", "folder:false"]);
    }

    #[test]
    fn uninstall_recheck_rejudges_ownership() {
        let _slot = SCAN_SLOT.lock().unwrap_or_else(|e| e.into_inner());
        let env = TestEnv::new("rejudge");
        let d = &env.dir;
        let un = env.key.child("Uninstall");
        let zq = un.child("ZebraQuill");
        reg::set_string(&zq, "DisplayName", "Zebra Quill Studio");
        reg::set_string(&zq, "Publisher", "Zebracorp Labs");
        reg::set_string(&zq, "InstallLocation", &d.join(r"PF\Zebra Quill Studio").to_string_lossy());
        env.file(r"PF\Zebra Quill Studio\zq.exe", 10);
        let svc_prog = env.file(r"PF\Zebra Quill Studio\Shared\zqsvc.exe", 10);
        reg::set_string(&env.key.child(r"Zebracorp Labs\Zebra Quill Studio"), "Theme", "dark");
        let roots = env.roots();
        let mut ctx = build_context(&roots).unwrap();
        ctx.services.push(fake_service("ZqHelper", &svc_prog));
        let rec = ctx.apps.iter().find(|a| a.key_name == "ZebraQuill").unwrap().clone();
        let items = scan_app(&rec, &ctx, &roots);
        show(&items);
        let pubkey = find(&items, "registry", r"\Zebracorp Labs").expect("publisher key").clone();
        assert!(pubkey.only_children.is_some());
        let svc = find(&items, "service", "ZqHelper").expect("service in the app's folder").clone();
        let scan = store_scan(next_scan_id(), Some(&rec), items);
        let state = stored_scan(scan.scan_id).unwrap();
        let none = HashSet::new();

        // Between scan and Remove, the vendor installs another product: its settings key appears
        // under the publisher key, and its install folder is the folder the service runs from.
        reg::set_string(&env.key.child(r"Zebracorp Labs\Other Product"), "x", "1");
        let other = un.child("OtherProduct");
        reg::set_string(&other, "DisplayName", "Other Product");
        reg::set_string(&other, "InstallLocation", &d.join(r"PF\Zebra Quill Studio\Shared").to_string_lossy());
        let mut fresh = build_context(&roots).unwrap();
        fresh.services.push(fake_service("ZqHelper", &svc_prog));
        let refs = build_refs(&fresh, &roots);
        let others = OtherOwners::new(&fresh, &roots, state.app_id.as_deref());
        let err = recheck(&pubkey, &state, &fresh, &roots, &refs, &none, &others).unwrap_err();
        assert!(err.starts_with("Something new was added"), "{err}");
        let err = recheck(&svc, &state, &fresh, &roots, &refs, &none, &others).unwrap_err();
        assert!(err.starts_with("Another installed app now uses it"), "{err}");

        // Through the real removal path too: nothing of the other product is touched.
        let id = scan.items.iter().find(|i| i.kind == "registry" && i.path.ends_with(r"\Zebracorp Labs")).unwrap().id.clone();
        let res = remove_items(scan.scan_id, &[id], &roots, true).unwrap();
        assert!(!res.items[0].ok, "{:?}", res.items[0]);
        assert!(reg::exists(&env.key.child(r"Zebracorp Labs\Other Product")));
        assert!(res.backup_folder.is_none());
    }

    #[test]
    fn uninstall_nested_folders_removed_once() {
        let _slot = SCAN_SLOT.lock().unwrap_or_else(|e| e.into_inner());
        let env = TestEnv::new("nested");
        let d = &env.dir;
        let loc = d.join(r"Roaming\Pebblesoft\Pebble Draw");
        env.file(r"Roaming\Pebblesoft\Pebble Draw\draw.dat", 700);
        let un = env.key.child("Uninstall");
        let a = un.child("PebbleDraw");
        reg::set_string(&a, "DisplayName", "Pebble Draw");
        reg::set_string(&a, "Publisher", "Pebblesoft");
        reg::set_string(&a, "InstallLocation", &loc.to_string_lossy());
        let roots = env.roots();
        let ctx = build_context(&roots).unwrap();
        let rec = ctx.apps.iter().find(|x| x.key_name == "PebbleDraw").unwrap().clone();
        let items = scan_app(&rec, &ctx, &roots);
        show(&items);
        // The publisher folder doesn't borrow the app folder's confidence: both are offered as found.
        let outer = find(&items, "folder", r"Roaming\Pebblesoft").expect("publisher folder");
        assert_eq!((outer.lo.confidence, outer.lo.default_on), (MEDIUM, false));
        let inner = find(&items, "folder", r"Pebblesoft\Pebble Draw").expect("install folder");
        assert_eq!(inner.lo.confidence, HIGH);

        // Both selected, outer first: the inner one goes first, and its bytes count once.
        let scan = store_scan(next_scan_id(), Some(&rec), items);
        let id_of = |end: &str| scan.items.iter().find(|i| i.kind == "folder" && i.path.ends_with(end)).unwrap().id.clone();
        let ids = vec![id_of(r"Roaming\Pebblesoft"), id_of(r"Pebblesoft\Pebble Draw")];
        let res = remove_items(scan.scan_id, &ids, &roots, false).unwrap();
        println!("{}", serde_json::to_string_pretty(&res).unwrap());
        assert_eq!((res.removed, res.failed, res.freed), (2, 0, 700));
        assert_eq!(res.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ids.iter().map(String::as_str).collect::<Vec<_>>());
        assert!(!d.join(r"Roaming\Pebblesoft").exists());
    }

    /// Tests that store a scan share the one "latest scan" slot, so they run one at a time.
    static SCAN_SLOT: Mutex<()> = Mutex::new(());

    #[test]
    fn uninstall_end_to_end_leftovers() {
        let _slot = SCAN_SLOT.lock().unwrap_or_else(|e| e.into_inner());
        let env = TestEnv::new("e2e");
        let d = &env.dir;
        // The app (already "uninstalled": its uninstaller is gone, its folder and data are not).
        env.file(r"PF\Zebra Quill Studio\zq.dat", 5000);
        env.file(r"Roaming\Zebracorp Labs\Zebra Quill Studio\settings.ini", 100);
        env.file(r"Local\ZebraQuillStudio\cache.bin", 300);
        // Named after the app but holds another installed app's folder: must be left alone.
        env.file(r"Local\ZebraQuill\Otter\otter.dat", 10);
        // Short/partial names never match.
        env.file(r"Local\Zebra\z.txt", 10);
        env.file(r"Roaming\Other Vendor\x.txt", 10);

        let un = env.key.child("Uninstall");
        let zq = un.child("ZebraQuill_is1");
        let loc = d.join(r"PF\Zebra Quill Studio");
        reg::set_string(&zq, "DisplayName", "Zebra Quill Studio 4.2.1 (x64)");
        reg::set_string(&zq, "Publisher", "Zebracorp Labs, Inc.");
        reg::set_string(&zq, "DisplayVersion", "4.2.1");
        reg::set_string(&zq, "InstallLocation", &loc.to_string_lossy());
        reg::set_string(&zq, "UninstallString", &format!("\"{}\" /SILENT", loc.join("unins000.exe").display()));
        let ot = un.child("OtterNotes");
        reg::set_string(&ot, "DisplayName", "Otter Notes");
        reg::set_string(&ot, "InstallLocation", &d.join(r"Local\ZebraQuill\Otter").to_string_lossy());
        // Settings under the publisher's key.
        reg::set_string(&env.key.child(r"Zebracorp Labs\Zebra Quill Studio"), "Theme", "dark");

        let roots = env.roots();
        let apps = read_apps(&roots.uninstall);
        assert_eq!(apps.len(), 2, "{apps:?}");
        let rec = apps.iter().find(|a| a.app.id == "t|ZebraQuill_is1").unwrap().clone();
        assert_eq!(rec.app.name, "Zebra Quill Studio 4.2.1 (x64)");
        assert!(rec.app.broken && !rec.app.can_uninstall);
        assert_eq!(rec.app.scope, "user");
        assert_eq!(launch_command(&rec).unwrap_err(), MISSING);

        let ctx = build_context(&roots).unwrap();
        let items = scan_app(&rec, &ctx, &roots);
        show(&items);
        let pf = find(&items, "folder", r"PF\Zebra Quill Studio").expect("install folder");
        assert_eq!(pf.lo.confidence, HIGH);
        assert!(pf.lo.needs_admin && pf.lo.default_on);
        assert_eq!(pf.lo.size, 5000);
        let pubdir = find(&items, "folder", r"Roaming\Zebracorp Labs").expect("publisher folder holding only this app");
        assert_eq!(pubdir.lo.confidence, MEDIUM);
        assert!(!pubdir.lo.default_on);
        let local = find(&items, "folder", r"Local\ZebraQuillStudio").expect("normalized name match");
        assert_eq!(local.lo.confidence, HIGH);
        assert!(!local.lo.needs_admin);
        assert!(find(&items, "folder", r"Local\ZebraQuill").is_none(), "holds another app's install folder");
        assert!(find(&items, "folder", r"Local\Zebra").is_none());
        assert!(find(&items, "folder", r"Other Vendor").is_none());
        let regi = find(&items, "registry", r"\Zebracorp Labs").expect("publisher key");
        assert!(!regi.lo.needs_admin);
        let entry = find(&items, "uninstall-entry", r"\ZebraQuill_is1").expect("uninstall entry");
        assert_eq!(entry.lo.confidence, LOW, "its install folder still exists");
        assert_eq!(items.iter().filter(|i| i.lo.kind == "folder").count(), 3);

        // Remove: the registry key through the real backup + delete path; the machine-wide
        // folder is refused unelevated without being touched.
        let scan = store_scan(next_scan_id(), Some(&rec), items);
        let id_of = |kind: &str, end: &str| scan.items.iter().find(|i| i.kind == kind && i.path.to_lowercase().ends_with(&end.to_lowercase())).unwrap().id.clone();
        let ids = vec![id_of("registry", r"\Zebracorp Labs"), id_of("folder", r"PF\Zebra Quill Studio"), id_of("uninstall-entry", r"\ZebraQuill_is1")];
        let res = remove_items(scan.scan_id, &ids, &roots, false).unwrap();
        println!("{}", serde_json::to_string_pretty(&res).unwrap());
        assert_eq!((res.removed, res.failed), (2, 1));
        assert!(res.items[0].ok && res.items[2].ok);
        assert_eq!(res.items[1].error.as_deref(), Some(ADMIN));
        assert!(!reg::exists(&env.key.child("Zebracorp Labs")));
        assert!(!reg::exists(&zq));
        assert!(reg::exists(&ot), "the other app's entry stays");
        assert!(loc.join("zq.dat").exists(), "refused folder untouched");
        let bdir = PathBuf::from(res.backup_folder.expect("backup folder"));
        assert!(bdir.starts_with(&roots.backup_base));
        assert!(bdir.file_name().unwrap().to_string_lossy().ends_with(" Zebra Quill Studio 4.2.1 (x64)"));
        let regs: Vec<_> = std::fs::read_dir(&bdir).unwrap().flatten().map(|e| e.path()).collect();
        assert_eq!(regs.len(), 2, "{regs:?}");
        for r in &regs {
            assert_eq!(r.extension().unwrap(), "reg");
            let raw = std::fs::read(r).unwrap();
            let text = String::from_utf16_lossy(&raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>());
            assert!(text.contains("PlatterTest-"), "{text}");
        }

        // Removing again: already gone; and a stale scan id is refused.
        let again = remove_items(scan.scan_id, &ids[..1], &roots, true).unwrap();
        assert_eq!(again.items[0].error.as_deref(), Some("It's already gone."));
        assert_eq!(remove_items(scan.scan_id + 1000, &ids, &roots, true).unwrap_err(), STALE);
        // A newer scan doesn't make this one stale; enough newer ones push it out.
        let other = store_scan(next_scan_id(), None, vec![]);
        assert!(other.scan_id > scan.scan_id);
        assert!(stored_scan(scan.scan_id).is_some());
        for _ in 0..KEEP_SCANS {
            store_scan(next_scan_id(), None, vec![]);
        }
        assert_eq!(remove_items(scan.scan_id, &ids, &roots, true).unwrap_err(), STALE);
    }

    #[test]
    fn uninstall_scan_ids_follow_start_order() {
        let _slot = SCAN_SLOT.lock().unwrap_or_else(|e| e.into_inner());
        // The slow orphan scan starts first, the app scan second but finishes first.
        let orphans = next_scan_id();
        let app = next_scan_id();
        let shown = store_scan(app, None, vec![]);
        let late = store_scan(orphans, None, vec![]);
        assert!(late.scan_id < shown.scan_id);
        // Both still usable: the late finisher replaced nothing.
        assert!(stored_scan(shown.scan_id).is_some() && stored_scan(late.scan_id).is_some());
        // Once more scans are kept than KEEP_SCANS, the ones *started* first go.
        for _ in 0..KEEP_SCANS - 1 {
            store_scan(next_scan_id(), None, vec![]);
        }
        assert!(stored_scan(late.scan_id).is_none());
        assert!(stored_scan(shown.scan_id).is_some());
        // A scan that started long ago and finishes now doesn't push out a newer one.
        let ancient = orphans.saturating_sub(1).max(1);
        let before: Vec<u64> = SCANS.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|s| s.scan_id).collect();
        store_scan(ancient, None, vec![]);
        let after: Vec<u64> = SCANS.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|s| s.scan_id).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn uninstall_orphans_conservative() {
        let _slot = SCAN_SLOT.lock().unwrap_or_else(|e| e.into_inner());
        let env = TestEnv::new("orph");
        let d = &env.dir;
        let old = SystemTime::now() - Duration::from_secs(200 * 24 * 3600);
        let set_old = |p: &Path| {
            let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
            f.set_modified(old).unwrap();
        };
        set_old(&env.file(r"PF\Old Thing\readme.txt", 1234));
        env.file(r"PF\Fresh Thing\new.txt", 10);
        set_old(&env.file(r"PF\Has Program\bin\tool.exe", 10));
        set_old(&env.file(r"PF\Common Files\x.txt", 10));
        set_old(&env.file(r"PF\Microsoft Something\x.txt", 10));
        set_old(&env.file(r"PF\Live App\data.txt", 10));
        set_old(&env.file(r"PF\Named After Installed\data.txt", 10));
        let un = env.key.child("Uninstall");
        let live = un.child("LiveApp");
        reg::set_string(&live, "DisplayName", "Live App");
        reg::set_string(&live, "InstallLocation", &d.join(r"PF\Live App").to_string_lossy());
        let named = un.child("Named");
        reg::set_string(&named, "DisplayName", "Named After Installed");
        let ghost = un.child("GhostApp");
        reg::set_string(&ghost, "DisplayName", "Ghost App 1.0");
        reg::set_string(&ghost, "UninstallString", &format!("\"{}\" /S", d.join(r"PF\Ghost\uninst.exe").display()));
        let msi = un.child("{AC76BA86-7AD7-1033-7B44-AC0F074E4100}");
        reg::set_string(&msi, "DisplayName", "Some MSI Product");
        reg::set_string(&msi, "UninstallString", "MsiExec.exe /I{AC76BA86-7AD7-1033-7B44-AC0F074E4100}");

        let roots = env.roots();
        let ctx = build_context(&roots).unwrap();
        let items = scan_orphans(&ctx, &roots);
        show(&items);
        let f = find(&items, "folder", r"PF\Old Thing").expect("old folder without programs");
        assert_eq!((f.lo.confidence, f.lo.default_on, f.lo.needs_admin, f.lo.size), (MEDIUM, false, true, 1234));
        assert_eq!(items.iter().filter(|i| i.lo.kind == "folder").count(), 1, "only Old Thing");
        let g = find(&items, "uninstall-entry", r"\GhostApp").expect("ghost entry");
        assert_eq!((g.lo.confidence, g.lo.default_on, g.lo.needs_admin), (HIGH, true, false));
        assert_eq!(items.iter().filter(|i| i.lo.kind == "uninstall-entry").count(), 1, "msiexec entries are never orphans");

        // The folder needs admin: refused unelevated, untouched.
        let scan = store_scan(next_scan_id(), None, items);
        let fid = scan.items.iter().find(|i| i.kind == "folder").unwrap().id.clone();
        let res = remove_items(scan.scan_id, &[fid], &roots, false).unwrap();
        assert_eq!(res.items[0].error.as_deref(), Some(ADMIN));
        assert!(res.backup_folder.is_none());
        assert!(d.join(r"PF\Old Thing\readme.txt").exists());
    }

    #[test]
    fn uninstall_walk_skips_junctions() {
        let env = TestEnv::new("walk");
        env.file(r"root\a.txt", 100);
        env.file(r"root\sub\b.dll", 50);
        env.file(r"outside\big.bin", 10_000);
        let link = env.dir.join(r"root\link");
        let target = env.dir.join("outside");
        let ok = Command::new(system32("cmd.exe"))
            .args(["/c", "mklink", "/J", &link.to_string_lossy(), &target.to_string_lossy()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "mklink /J");
        let st = walk(&env.dir.join("root"), 1000, &|_| false);
        assert_eq!(st.size, 150, "junction must not be followed");
        assert!(st.has_binary);
        assert_eq!(st.files, 2);
        let early = walk(&env.dir.join("root"), 1000, &|s| s.has_binary);
        assert!(early.has_binary);
        // remove_dir on a junction removes only the link.
        let _ = std::fs::remove_dir(&link);
        assert!(target.join("big.bin").exists());
    }

    // ----- Read-only feasibility spikes on this machine -----
    // cargo test uninstall_spike -- --ignored --nocapture --test-threads=1

    #[test]
    #[ignore = "read-only spike: cargo test uninstall_spike_icons -- --ignored --nocapture"]
    fn uninstall_spike_icons() {
        let apps = read_apps(&real_uninstall_roots());
        let t = Instant::now();
        let (mut from_icon, mut from_exe, mut none) = (0, 0, Vec::new());
        for a in &apps {
            match icon_source(a) {
                Some((p, i)) => {
                    let ok = crate::appicon::data_url(&p, i).is_some();
                    if !ok {
                        none.push(format!("{} (no icon in {})", a.display_name, p.display()));
                    } else if a.icon.as_deref() == Some(p.as_path()) {
                        from_icon += 1;
                    } else {
                        from_exe += 1;
                    }
                }
                None => none.push(a.display_name.clone()),
            }
        }
        println!("{} apps: {from_icon} from DisplayIcon, {from_exe} from an exe in the install folder, {} without, in {:?}", apps.len(), none.len(), t.elapsed());
        for n in none {
            println!("  no icon: {n}");
        }
    }

    #[test]
    #[ignore = "read-only spike: cargo test uninstall_spike -- --ignored --nocapture"]
    fn uninstall_spike_registry() {
        use windows_sys::Win32::System::Registry::*;
        // Explicit WOW6432Node path vs KEY_WOW64_32KEY must see the same 32-bit entries.
        let count_view = |path: &str, sam: u32| -> usize {
            let mut h: HKEY = std::ptr::null_mut();
            let p = wide(path);
            if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, p.as_ptr(), 0, KEY_READ | sam, &mut h) } != 0 {
                return 0;
            }
            let mut n = 0u32;
            unsafe {
                RegQueryInfoKeyW(h, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut n, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
                RegCloseKey(h);
            }
            n as usize
        };
        let a = count_view(&format!(r"SOFTWARE\{UNINSTALL_SUBKEY}"), KEY_WOW64_32KEY);
        let b = count_view(&format!(r"SOFTWARE\WOW6432Node\{UNINSTALL_SUBKEY}"), KEY_WOW64_64KEY);
        println!("32-bit view via KEY_WOW64_32KEY: {a} keys, via explicit WOW6432Node: {b} keys");
        assert_eq!(a, b);

        let t = Instant::now();
        let mut raw = 0;
        let (mut with_loc, mut with_size, mut quiet, mut msi, mut no_name) = (0, 0, 0, 0, 0);
        for root in real_uninstall_roots() {
            let Some(k) = reg::open_read(&root.key) else { continue };
            let subs = k.subkeys();
            println!("{}: {} entries", root.key.display(), subs.len());
            for s in subs {
                raw += 1;
                let Some(e) = reg::open_read(&root.key.child(&s)) else { continue };
                if e.string("DisplayName").is_none() {
                    no_name += 1;
                }
                with_loc += e.string("InstallLocation").is_some() as u32;
                with_size += e.dword("EstimatedSize").is_some() as u32;
                quiet += e.string("QuietUninstallString").is_some() as u32;
                msi += e.string("UninstallString").is_some_and(|u| u.to_lowercase().contains("msiexec")) as u32;
            }
        }
        let raw_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let apps = read_apps(&real_uninstall_roots());
        let list_ms = t.elapsed().as_secs_f64() * 1000.0;
        println!("raw {raw} entries ({no_name} without DisplayName), InstallLocation {with_loc}, EstimatedSize {with_size}, QuietUninstallString {quiet}, MsiExec {msi}; raw pass {raw_ms:.1} ms");
        println!(
            "apps_list: {} shown in {list_ms:.1} ms; canUninstall {}, broken {}, with existing installLocation {}",
            apps.len(),
            apps.iter().filter(|a| a.app.can_uninstall).count(),
            apps.iter().filter(|a| a.app.broken).count(),
            apps.iter().filter(|a| a.app.install_location.is_some()).count()
        );
        for a in &apps {
            let t = a.tokens();
            println!(
                "  [{}] {} | {} | can={} broken={} | prog={:?} | tokens={:?} pub={:?}{}",
                a.app.scope,
                a.app.name,
                a.app.publisher,
                a.app.can_uninstall,
                a.app.broken,
                a.cmd.as_ref().map(|c| c.program.as_str()),
                t.names,
                t.publisher,
                if t.publisher_folder_ok { " (pub folder ok)" } else { "" }
            );
        }
    }

    #[test]
    #[ignore = "read-only spike: cargo test uninstall_spike -- --ignored --nocapture"]
    fn uninstall_spike_services() {
        let t = Instant::now();
        let s = list_services();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let with_prog = s.iter().filter(|x| x.program.is_some()).count();
        let non_windows: Vec<_> = s.iter().filter(|x| x.program.as_ref().is_some_and(|p| !in_windows(p))).collect();
        let missing = non_windows.iter().filter(|x| presence(x.program.as_ref().unwrap()) == Presence::Missing).count();
        println!("services: {} Win32 services in {ms:.1} ms; program parsed {with_prog}; outside Windows {}; program missing {missing}", s.len(), non_windows.len());
        for x in &non_windows {
            println!("  {} | {} | {:?}", x.name, x.display, x.program);
        }
        for x in s.iter().filter(|x| x.program.is_none()) {
            println!("  UNPARSED {} | {:?}", x.name, x.image);
        }
        assert!(s.len() > 50);
    }

    #[test]
    #[ignore = "read-only spike: cargo test uninstall_spike -- --ignored --nocapture"]
    fn uninstall_spike_tasks() {
        let t = Instant::now();
        let (tasks, denied) = com::on_mta_thread(tasks::list).unwrap().expect("task scheduler");
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let non_ms: Vec<_> = tasks.iter().filter(|t| !t.is_microsoft()).collect();
        println!("tasks: {} in {ms:.1} ms (elevated={}); outside \\Microsoft\\: {}; denied folders: {}", tasks.len(), is_elevated(), non_ms.len(), denied.len());
        for d in &denied {
            println!("  denied {d}");
        }
        for t in &non_ms {
            let miss = t.programs.iter().any(|p| presence(Path::new(p)) == Presence::Missing);
            println!("  {} | user={} | {:?}{}", t.path, t.user, t.programs, if miss { "  (MISSING)" } else { "" });
        }
        // XML of one task, as the backup would save it.
        if let Some(t) = non_ms.first() {
            let p = t.path.clone();
            let t2 = Instant::now();
            let got = com::on_mta_thread(move || tasks::get(&p)).flatten();
            println!("get_Xml {} chars in {:.1} ms", got.map(|g| g.0.len()).unwrap_or(0), t2.elapsed().as_secs_f64() * 1000.0);
        }
    }

    #[test]
    #[ignore = "read-only spike: cargo test uninstall_spike -- --ignored --nocapture"]
    fn uninstall_spike_startup() {
        let t = Instant::now();
        let s = list_startup(&real_run_keys());
        let a = list_app_paths(&real_app_paths_keys());
        println!("startup values {} + App Paths {} in {:.1} ms", s.len(), a.len(), t.elapsed().as_secs_f64() * 1000.0);
        for v in &s {
            println!("  {} → {} = {:?} → {:?} {:?}", v.key.display(), v.name, v.data, v.program, v.program.as_ref().map(|p| presence(p)));
        }
        for x in a.iter().filter(|x| x.program.as_ref().is_some_and(|p| presence(p) == Presence::Missing)) {
            println!("  App Paths missing: {} → {:?}", x.key.display(), x.program);
        }
    }

    #[test]
    #[ignore = "read-only spike: cargo test uninstall_spike -- --ignored --nocapture"]
    fn uninstall_spike_orphans() {
        let roots = Roots::real();
        let t = Instant::now();
        let ctx = build_context(&roots).unwrap();
        let ctx_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let items = scan_orphans(&ctx, &roots);
        println!(
            "context {ctx_ms:.1} ms ({} apps, {} services, {} tasks, {} startup, {} app paths); orphan scan {:.1} ms, {} items",
            ctx.apps.len(),
            ctx.services.len(),
            ctx.tasks.len(),
            ctx.startup.len(),
            ctx.app_paths.len(),
            t.elapsed().as_secs_f64() * 1000.0,
            items.len()
        );
        show(&items);
    }

    #[test]
    #[ignore = "read-only spike: PLATTER_APP=<name part> cargo test uninstall_spike_app -- --ignored --nocapture"]
    fn uninstall_spike_app_leftovers() {
        let roots = Roots::real();
        let ctx = build_context(&roots).unwrap();
        let filter = std::env::var("PLATTER_APP").unwrap_or_default().to_lowercase();
        for rec in ctx.apps.iter().filter(|a| a.app.name.to_lowercase().contains(&filter)) {
            let t = Instant::now();
            let items = scan_app(rec, &ctx, &roots);
            println!("== {} ({}) — {} items in {:.1} ms", rec.app.name, rec.app.id, items.len(), t.elapsed().as_secs_f64() * 1000.0);
            show(&items);
            // Why an existing install folder isn't offered.
            if let Some(l) = rec.location.as_ref().filter(|l| l.is_dir()) {
                if !items.iter().any(|i| i.lo.kind == "folder" && within(&norm_path(l), &norm_path(Path::new(&i.lo.path)))) {
                    let refs = build_refs(&ctx, &roots);
                    let own = |o: &Owner| matches!(o, Owner::App(id) if *id == rec.app.id);
                    println!("  (install folder {} not offered: refused={:?} blocked by {:?})", l.display(), refuse_path(l, &ctx.protected), blocking_ref(l, &refs, &own).map(|r| (&r.owner, &r.path)));
                }
            }
        }
    }
}
