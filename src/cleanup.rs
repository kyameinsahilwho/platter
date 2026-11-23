//! Clean up: finds caches and leftovers that apps and Windows recreate on their own, and deletes
//! them safely.
//!
//! Safety rules, enforced for every item:
//! * Only files under an item's configured root folders are touched; the roots themselves are never
//!   removed, only emptied.
//! * Deleting never goes back through a path string. Each root is opened once as a handle (without
//!   following a reparse point), and every child is enumerated, opened and deleted *relative to its
//!   parent's open handle* (NtCreateFile + FILE_OPEN_REPARSE_POINT, then
//!   SetFileInformationByHandle(FileDispositionInfoEx)). Each opened handle is re-checked: not a
//!   reparse point, and its final path still inside the root. Swapping a folder for a junction
//!   mid-walk therefore cannot redirect a delete elsewhere.
//! * Reparse points (junctions, symlinks) are never followed or deleted.
//! * Files in use or denied are skipped, never forced.
//! * Roots that resolve to a drive root, a very short path, or a user/system folder — or that
//!   *contain* one — are refused, and the TEMP root must be a dedicated Temp/Tmp folder. Refused roots
//!   are reported by both analyze and run, never silently skipped.
//! * Per-user items are refused when Platter runs elevated as a different account than the one
//!   signed in to the desktop.
//! * "Review" items are only measured — never deleted here.

use crate::util::{env_path, is_elevated, system_drive_letter, CREATE_NO_WINDOW};
use rayon::prelude::*;
use serde::Serialize;
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::MetadataExt;
use std::os::windows::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tauri::Emitter;

const ATTR_READONLY: u32 = 0x1;
const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_REPARSE_POINT: u32 = 0x400;
const ATTR_OFFLINE: u32 = 0x1000;
const ATTR_RECALL_ON_OPEN: u32 = 0x4_0000;
const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
const CLOUD_ATTRS: u32 = ATTR_OFFLINE | ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS;

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const ADMIN_ERROR: &str = "Needs administrator rights.";
const OTHER_ACCOUNT: &str = "Platter is running as a different administrator account than the one signed in, so it won't touch per-user files. Open Platter without \"Run as administrator\" to clean yours.";

static RUNNING: AtomicBool = AtomicBool::new(false);

/// Measure generation. Every `cleanup_analyze` takes a new value when it is invoked, and
/// `cleanup_run` bumps it when it starts and again when it ends. A measure whose value is no longer
/// the newest has been superseded: it does not write the `cleanup` cache (so a measure that started
/// before a clean-up can never overwrite one taken after it), it stops emitting `cleanup-item`, a
/// background one stops walking, and the command returns an error the UI already drops.
static MEASURE_GEN: AtomicU64 = AtomicU64::new(0);
/// Held while checking the generation and writing the `cleanup` cache entry, and while
/// `cleanup_run` bumps the generation: a write that passed the check finishes before a run starts
/// deleting, and two measures never write `cleanup.json(.tmp)` at the same time.
static CACHE_LOCK: Mutex<()> = Mutex::new(());

fn bump_measure_gen() {
    let _g = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    MEASURE_GEN.fetch_add(1, Ordering::SeqCst);
}

/// A cancel check that never fires (deleting, tests, measures nobody can supersede).
fn never() -> bool {
    false
}

// ---------------------------------------------------------------------------------------------
// Public shapes (see CONTRACT.md)

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CleanupItem {
    pub id: String,
    pub group: String,
    pub name: String,
    pub description: String,
    pub size: u64,
    pub files: u64,
    pub needs_admin: bool,
    pub default_on: bool,
    pub kind: String,
    pub review_path: Option<String>,
    pub available: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CleanupProgress {
    pub id: String,
    pub freed: u64,
    pub done: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CleanupItemResult {
    pub id: String,
    pub freed: u64,
    pub skipped: u64,
    pub error: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CleanupRunResult {
    pub freed: u64,
    pub deleted: u64,
    pub skipped: u64,
    pub items: Vec<CleanupItemResult>,
}

// ---------------------------------------------------------------------------------------------
// Item definitions

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Delete,
    RecycleBin,
    Hibernation,
    Review,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Delete => "delete",
            Kind::RecycleBin => "recycle-bin",
            Kind::Hibernation => "hibernation",
            Kind::Review => "review",
        }
    }
}

type NameFilter = fn(&str) -> bool;

struct Root {
    dir: PathBuf,
    recursive: bool,
    /// When set, only top-level files whose name matches are considered (non-recursive roots).
    filter: Option<NameFilter>,
    /// The root comes from %TEMP%: it must be a dedicated Temp/Tmp folder to be emptied.
    temp_dir: bool,
}

struct Def {
    id: &'static str,
    group: &'static str,
    name: &'static str,
    desc: &'static str,
    needs_admin: bool,
    default_on: bool,
    kind: Kind,
    roots: Vec<Root>,
    /// Keep files newer than this.
    min_age: Option<Duration>,
    review_path: Option<PathBuf>,
    /// Set when the item must not be measured or cleaned on this run (shown as its description).
    blocked: Option<&'static str>,
}

fn is_reparse(meta: &std::fs::Metadata) -> bool {
    meta.file_attributes() & ATTR_REPARSE_POINT != 0 || meta.file_type().is_symlink()
}

/// Expands a `\`-separated pattern below `base`; a `*` segment matches every real sub-folder.
/// Returns only existing folders that are not reparse points.
fn glob(base: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut cur = vec![base.to_path_buf()];
    for seg in pattern.split('\\').filter(|s| !s.is_empty()) {
        let mut next = Vec::new();
        for p in cur {
            if seg == "*" {
                if let Ok(rd) = std::fs::read_dir(&p) {
                    for e in rd.flatten() {
                        if let Ok(m) = e.metadata() {
                            if m.is_dir() && !is_reparse(&m) {
                                next.push(e.path());
                            }
                        }
                    }
                }
            } else {
                next.push(p.join(seg));
            }
        }
        cur = next;
    }
    cur.into_iter()
        .filter(|p| std::fs::symlink_metadata(p).map(|m| m.is_dir() && !is_reparse(&m)).unwrap_or(false))
        .collect()
}

/// Recursive roots from patterns below an environment folder. Missing env var → no roots.
fn under(base: &Option<PathBuf>, patterns: &[&str]) -> Vec<Root> {
    let Some(base) = base else { return Vec::new() };
    let mut dirs: Vec<PathBuf> = patterns.iter().flat_map(|p| glob(base, p)).collect();
    dirs.sort();
    dirs.dedup();
    // Drop roots nested inside another root so nothing is counted twice.
    let all = dirs.clone();
    dirs.retain(|d| !all.iter().any(|o| o != d && d.starts_with(o)));
    dirs.into_iter().map(|dir| Root { dir, recursive: true, filter: None, temp_dir: false }).collect()
}

fn files_in(dir: Option<PathBuf>, filter: NameFilter) -> Vec<Root> {
    match dir {
        Some(dir) if dir.is_dir() => vec![Root { dir, recursive: false, filter: Some(filter), temp_dir: false }],
        _ => Vec::new(),
    }
}

fn is_thumb_db(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    (n.starts_with("thumbcache_") || n.starts_with("iconcache_")) && n.ends_with(".db")
}

fn is_memory_dmp(name: &str) -> bool {
    name.eq_ignore_ascii_case("MEMORY.DMP")
}

fn windir() -> Option<PathBuf> {
    env_path("SystemRoot").or_else(|| env_path("windir"))
}

fn sys_root() -> PathBuf {
    PathBuf::from(format!("{}:\\", system_drive_letter()))
}

fn hiberfil_size() -> Option<u64> {
    // std::fs::metadata on hiberfil.sys fails (it's locked); the directory listing has its size.
    std::fs::read_dir(sys_root())
        .ok()?
        .flatten()
        .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case("hiberfil.sys"))
        .and_then(|e| e.metadata().ok())
        .map(|m| m.len())
}

fn defs() -> Vec<Def> {
    let local = env_path("LOCALAPPDATA");
    let roaming = env_path("APPDATA");
    let profile = env_path("USERPROFILE");
    let temp = env_path("TEMP").or_else(|| env_path("TMP"));
    let win = windir();
    let pdata = env_path("ProgramData");

    const CHROMIUM: &[&str] = &[
        "*\\Cache",
        "*\\Code Cache",
        "*\\GPUCache",
        "*\\Service Worker\\CacheStorage",
        "*\\Service Worker\\ScriptCache",
        "GrShaderCache",
        "ShaderCache",
        "GraphiteDawnCache",
    ];
    let chromium = |base: &Option<PathBuf>, user_data: &str| -> Vec<Root> {
        let ud = base.as_ref().map(|b| b.join(user_data));
        under(&ud, CHROMIUM)
    };

    let d = |id, group, name, desc, needs_admin, default_on, roots, min_age| Def {
        id,
        group,
        name,
        desc,
        needs_admin,
        default_on,
        kind: Kind::Delete,
        roots,
        min_age,
        review_path: None,
        blocked: None,
    };

    let mut v = vec![
        // ---------------- System
        d(
            "user-temp",
            "System",
            "Temporary files",
            "Leftovers apps were supposed to delete. Files from the last 24 hours are kept.",
            false,
            true,
            under(&temp, &[""]).into_iter().map(|r| Root { temp_dir: true, ..r }).collect(),
            Some(DAY),
        ),
        d(
            "windows-temp",
            "System",
            "Windows temporary files",
            "Leftovers from Windows and installers. Files from the last 24 hours are kept.",
            true,
            true,
            under(&win, &["Temp"]),
            Some(DAY),
        ),
        Def {
            id: "recycle-bin",
            group: "System",
            name: "Recycle Bin",
            desc: "Files you already deleted. Emptying the bin removes them for good.",
            needs_admin: false,
            default_on: true,
            kind: Kind::RecycleBin,
            roots: Vec::new(),
            min_age: None,
            review_path: None,
            blocked: None,
        },
        d(
            "thumbnails",
            "System",
            "Thumbnail cache",
            "Small previews of your pictures and videos. Windows rebuilds them as you browse.",
            false,
            true,
            files_in(local.as_ref().map(|l| l.join("Microsoft\\Windows\\Explorer")), is_thumb_db),
            None,
        ),
        d(
            "crash-dumps",
            "System",
            "Crash reports",
            "Reports saved when apps crashed. They are only useful for troubleshooting.",
            false,
            true,
            under(
                &local,
                &["CrashDumps", "Microsoft\\Windows\\WER\\ReportArchive", "Microsoft\\Windows\\WER\\ReportQueue"],
            ),
            None,
        ),
        d(
            "system-error-reports",
            "System",
            "Windows error reports",
            "Error reports and memory dumps Windows saved after problems or blue screens.",
            true,
            true,
            {
                let mut r = under(&pdata, &["Microsoft\\Windows\\WER\\ReportArchive", "Microsoft\\Windows\\WER\\ReportQueue"]);
                r.extend(under(&win, &["Minidump"]));
                r.extend(files_in(win.clone(), is_memory_dmp));
                r
            },
            None,
        ),
        d(
            "windows-update",
            "System",
            "Windows Update downloads",
            "Update files Windows already installed. It downloads them again if it needs to.",
            true,
            true,
            under(&win, &["SoftwareDistribution\\Download"]),
            None,
        ),
        d(
            "delivery-optimization",
            "System",
            "Delivery Optimization files",
            "Update pieces Windows keeps to share with other PCs. Safe to remove.",
            true,
            true,
            under(
                &win,
                &["ServiceProfiles\\NetworkService\\AppData\\Local\\Microsoft\\Windows\\DeliveryOptimization\\Cache"],
            ),
            None,
        ),
        d(
            "shader-cache",
            "System",
            "Graphics shader cache",
            "Graphics data games and apps prepare ahead of time. It is rebuilt as needed.",
            false,
            true,
            under(&local, &["D3DSCache", "NVIDIA\\DXCache", "NVIDIA\\GLCache", "AMD\\DxCache", "AMD\\DxcCache", "AMD\\VkCache"]),
            None,
        ),
        // ---------------- Browsers
        d(
            "chrome-cache",
            "Browsers",
            "Google Chrome cache",
            "Copies of websites Chrome saved to load pages faster. Logins and history stay.",
            false,
            true,
            chromium(&local, "Google\\Chrome\\User Data"),
            None,
        ),
        d(
            "edge-cache",
            "Browsers",
            "Microsoft Edge cache",
            "Copies of websites Edge saved to load pages faster. Logins and history stay.",
            false,
            true,
            chromium(&local, "Microsoft\\Edge\\User Data"),
            None,
        ),
        d(
            "brave-cache",
            "Browsers",
            "Brave cache",
            "Copies of websites Brave saved to load pages faster. Logins and history stay.",
            false,
            true,
            chromium(&local, "BraveSoftware\\Brave-Browser\\User Data"),
            None,
        ),
        d(
            "firefox-cache",
            "Browsers",
            "Firefox cache",
            "Copies of websites Firefox saved to load pages faster. Logins and history stay.",
            false,
            true,
            under(&local, &["Mozilla\\Firefox\\Profiles\\*\\cache2", "Mozilla\\Firefox\\Profiles\\*\\startupCache"]),
            None,
        ),
        d(
            "opera-cache",
            "Browsers",
            "Opera cache",
            "Copies of websites Opera saved to load pages faster. Logins and history stay.",
            false,
            true,
            {
                let mut r = under(&local, &["Opera Software\\*\\Cache", "Opera Software\\*\\*\\Cache"]);
                r.extend(under(
                    &roaming,
                    &[
                        "Opera Software\\*\\Code Cache",
                        "Opera Software\\*\\GPUCache",
                        "Opera Software\\*\\*\\Code Cache",
                        "Opera Software\\*\\*\\GPUCache",
                    ],
                ));
                r
            },
            None,
        ),
        // ---------------- Apps
        d(
            "teams-cache",
            "Apps",
            "Microsoft Teams cache",
            "Temporary files Teams keeps to start faster. Your chats are stored online.",
            false,
            true,
            {
                let mut r = under(
                    &roaming,
                    &[
                        "Microsoft\\Teams\\Cache",
                        "Microsoft\\Teams\\Code Cache",
                        "Microsoft\\Teams\\GPUCache",
                        "Microsoft\\Teams\\Service Worker\\CacheStorage",
                        "Microsoft\\Teams\\tmp",
                    ],
                );
                let ebw = "Packages\\MSTeams_8wekyb3d8bbwe\\LocalCache\\Microsoft\\MSTeams\\EBWebView";
                r.extend(under(
                    &local,
                    &[
                        &format!("{ebw}\\*\\Cache"),
                        &format!("{ebw}\\*\\Code Cache"),
                        &format!("{ebw}\\*\\GPUCache"),
                        &format!("{ebw}\\*\\Service Worker\\CacheStorage"),
                    ],
                ));
                r
            },
            None,
        ),
        d(
            "discord-cache",
            "Apps",
            "Discord cache",
            "Images and data Discord saved to load faster. Your messages are stored online.",
            false,
            true,
            under(&roaming, &["discord\\Cache", "discord\\Code Cache", "discord\\GPUCache"]),
            None,
        ),
        d(
            "spotify-cache",
            "Apps",
            "Spotify cache",
            "Songs Spotify streamed and kept for later. Downloaded playlists may need to download again.",
            false,
            true,
            under(&local, &["Spotify\\Data", "Packages\\SpotifyAB.SpotifyMusic_zpdnekdrzrea0\\LocalCache\\Spotify\\Data"]),
            None,
        ),
        d(
            "vscode-cache",
            "Apps",
            "Visual Studio Code cache",
            "Caches and old logs from VS Code. Your settings and extensions stay.",
            false,
            true,
            under(&roaming, &["Code\\Cache", "Code\\CachedData", "Code\\Code Cache", "Code\\GPUCache", "Code\\logs"]),
            None,
        ),
        // ---------------- Developer caches
        d(
            "npm-cache",
            "Developer caches",
            "npm cache",
            "Packages npm downloaded before. They download again the next time you install.",
            false,
            false,
            under(&local, &["npm-cache"]),
            None,
        ),
        d(
            "yarn-cache",
            "Developer caches",
            "Yarn cache",
            "Packages Yarn downloaded before. They download again the next time you install.",
            false,
            false,
            under(&local, &["Yarn\\Cache"]),
            None,
        ),
        d(
            "pnpm-store",
            "Developer caches",
            "pnpm store",
            "Packages pnpm keeps for your projects. They download again the next time you install.",
            false,
            false,
            under(&local, &["pnpm\\store"]),
            None,
        ),
        d(
            "pip-cache",
            "Developer caches",
            "pip cache",
            "Python packages pip downloaded before. They download again when needed.",
            false,
            false,
            under(&local, &["pip\\Cache"]),
            None,
        ),
        d(
            "cargo-cache",
            "Developer caches",
            "Cargo registry cache",
            "Rust crates Cargo downloaded before. They download again on the next build.",
            false,
            false,
            under(&profile, &[".cargo\\registry\\cache", ".cargo\\registry\\src"]),
            None,
        ),
        d(
            "nuget-cache",
            "Developer caches",
            "NuGet packages",
            "Packages NuGet downloaded before. They download again on the next restore.",
            false,
            false,
            {
                let mut r = under(&local, &["NuGet\\v3-cache"]);
                r.extend(under(&profile, &[".nuget\\packages"]));
                r
            },
            None,
        ),
        d(
            "gradle-cache",
            "Developer caches",
            "Gradle caches",
            "Libraries and build data Gradle downloaded before. They download again on the next build.",
            false,
            false,
            under(&profile, &[".gradle\\caches"]),
            None,
        ),
    ];

    // ---------------- Review
    let downloads = profile.as_ref().map(|p| p.join("Downloads"));
    v.push(Def {
        id: "downloads",
        group: "Review",
        name: "Old downloads",
        desc: "Files you downloaded more than 3 months ago. Review them in Space before deleting.",
        needs_admin: false,
        default_on: false,
        kind: Kind::Review,
        roots: under(&downloads, &[""]),
        min_age: Some(DAY * 90),
        review_path: downloads,
        blocked: None,
    });
    v.push(Def {
        id: "hibernation",
        group: "System",
        name: "Hibernation file",
        desc: "Turns off hibernation and Fast Startup. You can turn it back on any time.",
        needs_admin: true,
        default_on: false,
        kind: Kind::Hibernation,
        roots: Vec::new(),
        min_age: None,
        review_path: None,
        blocked: None,
    });

    // Elevated as someone else (over-the-shoulder UAC): the environment above is that admin's
    // profile, not the signed-in user's. Don't measure or clean another account's files.
    if other_account() {
        for d in v.iter_mut().filter(|d| !MACHINE_WIDE.contains(&d.id)) {
            d.roots.clear();
            d.review_path = None;
            d.blocked = Some(OTHER_ACCOUNT);
        }
    }
    v
}

/// Items whose data is shared by the whole PC rather than stored in one user's profile.
const MACHINE_WIDE: &[&str] =
    &["windows-temp", "system-error-reports", "windows-update", "delivery-optimization", "hibernation"];

// ---------------------------------------------------------------------------------------------
// Handle-based file access. Once a folder is open, nothing below it is ever reached through a path
// string again, so renaming a folder or swapping it for a junction mid-walk can't redirect anything.

mod fsh {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};
    use std::ptr::{addr_of, null, null_mut};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::NtCreateFile;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, UNICODE_STRING};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FileAttributeTagInfo, FileBasicInfo, FileDispositionInfo, FileDispositionInfoEx,
        FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo, FileIdInfo, FileStandardInfo,
        GetFileInformationByHandleEx, GetFinalPathNameByHandleW, SetFileInformationByHandle, FILE_ATTRIBUTE_TAG_INFO,
        FILE_BASIC_INFO, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX, FILE_ID_BOTH_DIR_INFO, FILE_ID_INFO,
        FILE_INFO_BY_HANDLE_CLASS, FILE_STANDARD_INFO,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    pub const LIST: u32 = 0x0001; // FILE_LIST_DIRECTORY
    pub const READ_ATTRIBUTES: u32 = 0x0080;
    pub const WRITE_ATTRIBUTES: u32 = 0x0100;
    pub const DELETE: u32 = 0x0001_0000;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    /// Others may read/write but not rename or delete a folder while we hold it.
    pub const SHARE_RW: u32 = 0x1 | 0x2;
    pub const SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;

    const OPEN_EXISTING: u32 = 3;
    const FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const NT_FILE_OPEN: u32 = 1;
    const NT_DIRECTORY_FILE: u32 = 0x1;
    const NT_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
    const NT_NON_DIRECTORY_FILE: u32 = 0x40;
    const NT_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    const ERROR_INVALID_FUNCTION: u32 = 1;
    const ERROR_FILE_NOT_FOUND: u32 = 2;
    const ERROR_NO_MORE_FILES: u32 = 18;
    const ERROR_NOT_SUPPORTED: u32 = 50;
    const ERROR_INVALID_PARAMETER: u32 = 87;

    const DISPOSITION_DELETE: u32 = 0x1;
    const DISPOSITION_POSIX: u32 = 0x2;
    const DISPOSITION_IGNORE_READONLY: u32 = 0x10;

    pub struct Handle(HANDLE);

    // SAFETY: a kernel handle may be used and closed from any thread.
    unsafe impl Send for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Opens a folder by path without following a reparse point at the last component.
    pub fn open_dir_path(path: &Path, access: u32, share: u32) -> Option<Handle> {
        let w: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                access | SYNCHRONIZE,
                share,
                null(),
                OPEN_EXISTING,
                FLAG_BACKUP_SEMANTICS | FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };
        (h != INVALID_HANDLE_VALUE && !h.is_null()).then_some(Handle(h))
    }

    /// Opens `name` relative to the open folder `parent` (never by path), without following a
    /// reparse point. `dir` demands a folder; otherwise a non-folder.
    pub fn open_child(parent: &Handle, name: &[u16], access: u32, share: u32, dir: bool) -> Option<Handle> {
        let bytes = u16::try_from(name.len() * 2).ok()?;
        let us = UNICODE_STRING { Length: bytes, MaximumLength: bytes, Buffer: name.as_ptr() as *mut u16 };
        let oa = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent.0,
            ObjectName: &us,
            Attributes: 0, // exact (case-sensitive) name, as enumerated
            SecurityDescriptor: null(),
            SecurityQualityOfService: null(),
        };
        let mut iosb: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
        let mut h: HANDLE = null_mut();
        let kind = if dir { NT_DIRECTORY_FILE } else { NT_NON_DIRECTORY_FILE };
        let st = unsafe {
            NtCreateFile(
                &mut h,
                access | SYNCHRONIZE,
                &oa,
                &mut iosb,
                null(),
                0,
                share,
                NT_FILE_OPEN,
                NT_OPEN_REPARSE_POINT | NT_SYNCHRONOUS_IO_NONALERT | kind,
                null(),
                0,
            )
        };
        (st >= 0 && !h.is_null()).then_some(Handle(h))
    }

    /// Tests only: creates folder `name` inside the open folder `parent` and returns it opened.
    #[cfg(test)]
    pub fn create_child_dir(parent: &Handle, name: &[u16]) -> Option<Handle> {
        const NT_FILE_CREATE: u32 = 2;
        let bytes = u16::try_from(name.len() * 2).ok()?;
        let us = UNICODE_STRING { Length: bytes, MaximumLength: bytes, Buffer: name.as_ptr() as *mut u16 };
        let oa = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent.0,
            ObjectName: &us,
            Attributes: 0,
            SecurityDescriptor: null(),
            SecurityQualityOfService: null(),
        };
        let mut iosb: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
        let mut h: HANDLE = null_mut();
        let st = unsafe {
            NtCreateFile(
                &mut h,
                LIST | READ_ATTRIBUTES | SYNCHRONIZE,
                &oa,
                &mut iosb,
                null(),
                0,
                SHARE_ALL,
                NT_FILE_CREATE,
                NT_DIRECTORY_FILE | NT_SYNCHRONOUS_IO_NONALERT,
                null(),
                0,
            )
        };
        (st >= 0 && !h.is_null()).then_some(Handle(h))
    }

    /// Hard-link count of `name` inside the open folder `parent`, without opening it
    /// (NtQueryInformationByName / FileStatInformation; Windows 10 1709+). None if unsupported.
    pub fn stat_links(parent: &Handle, name: &[u16]) -> Option<u32> {
        use windows_sys::Wdk::Storage::FileSystem::{NtQueryInformationByName, FileStatInformation, FILE_STAT_INFORMATION};
        let bytes = u16::try_from(name.len() * 2).ok()?;
        let us = UNICODE_STRING { Length: bytes, MaximumLength: bytes, Buffer: name.as_ptr() as *mut u16 };
        let oa = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent.0,
            ObjectName: &us,
            Attributes: 0,
            SecurityDescriptor: null(),
            SecurityQualityOfService: null(),
        };
        let mut iosb: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
        let mut info: FILE_STAT_INFORMATION = unsafe { std::mem::zeroed() };
        let st = unsafe {
            NtQueryInformationByName(
                &oa,
                &mut iosb,
                &mut info as *mut _ as *mut _,
                std::mem::size_of::<FILE_STAT_INFORMATION>() as u32,
                FileStatInformation,
            )
        };
        (st >= 0).then_some(info.NumberOfLinks)
    }

    fn query<T>(h: &Handle, class: FILE_INFO_BY_HANDLE_CLASS) -> Option<T> {
        let mut v: T = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            GetFileInformationByHandleEx(h.0, class, &mut v as *mut T as *mut _, std::mem::size_of::<T>() as u32)
        };
        (ok != 0).then_some(v)
    }

    fn set<T>(h: &Handle, class: FILE_INFO_BY_HANDLE_CLASS, v: &T) -> bool {
        unsafe { SetFileInformationByHandle(h.0, class, v as *const T as *const _, std::mem::size_of::<T>() as u32) != 0 }
    }

    /// Current attributes of the open object itself.
    pub fn attrs(h: &Handle) -> Option<u32> {
        query::<FILE_ATTRIBUTE_TAG_INFO>(h, FileAttributeTagInfo).map(|i| i.FileAttributes)
    }

    /// (size, hard-link count)
    pub fn standard(h: &Handle) -> Option<(u64, u32)> {
        query::<FILE_STANDARD_INFO>(h, FileStandardInfo).map(|i| (i.EndOfFile.max(0) as u64, i.NumberOfLinks))
    }

    /// (volume serial, 128-bit file id): identifies a file across all of its hard links.
    pub fn file_id(h: &Handle) -> Option<(u64, [u8; 16])> {
        query::<FILE_ID_INFO>(h, FileIdInfo).map(|i| (i.VolumeSerialNumber, i.FileId.Identifier))
    }

    /// Where the open object really is now (`\\?\C:\...`), resolved by the file system.
    pub fn final_path(h: &Handle) -> Option<PathBuf> {
        let mut buf = vec![0u16; 512];
        loop {
            let n = unsafe { GetFinalPathNameByHandleW(h.0, buf.as_mut_ptr(), buf.len() as u32, 0) } as usize;
            if n == 0 {
                return None;
            }
            if n < buf.len() {
                return Some(PathBuf::from(OsString::from_wide(&buf[..n])));
            }
            buf.resize(n + 1, 0);
        }
    }

    pub struct Entry {
        pub name: Vec<u16>,
        pub attrs: u32,
        pub size: u64,
        /// Newer of creation and last-write time.
        pub newest: Option<SystemTime>,
    }

    fn filetime(t: i64) -> Option<SystemTime> {
        const UNIX_EPOCH_FT: i64 = 116_444_736_000_000_000;
        if t <= 0 {
            return None;
        }
        let d = |ticks: u64| Duration::new(ticks / 10_000_000, ((ticks % 10_000_000) * 100) as u32);
        if t >= UNIX_EPOCH_FT {
            UNIX_EPOCH.checked_add(d((t - UNIX_EPOCH_FT) as u64))
        } else {
            UNIX_EPOCH.checked_sub(d((UNIX_EPOCH_FT - t) as u64))
        }
    }

    /// Lists the children of the open folder `h` (no `.`/`..`), reading through the handle.
    pub fn list(h: &Handle) -> Option<Vec<Entry>> {
        let mut buf = vec![0u64; 8192]; // 64 KB, 8-byte aligned
        let mut out = Vec::new();
        let mut class = FileIdBothDirectoryRestartInfo;
        loop {
            let ok = unsafe {
                GetFileInformationByHandleEx(h.0, class, buf.as_mut_ptr() as *mut _, (buf.len() * 8) as u32)
            };
            if ok == 0 {
                let e = unsafe { GetLastError() };
                if e == ERROR_NO_MORE_FILES || e == ERROR_FILE_NOT_FOUND {
                    return Some(out);
                }
                return if class == FileIdBothDirectoryRestartInfo { None } else { Some(out) };
            }
            class = FileIdBothDirectoryInfo;
            let base = buf.as_ptr() as *const u8;
            let mut off = 0usize;
            loop {
                // SAFETY: the file system wrote a chain of FILE_ID_BOTH_DIR_INFO records into `buf`.
                let (info, name) = unsafe {
                    let p = base.add(off) as *const FILE_ID_BOTH_DIR_INFO;
                    let info = std::ptr::read_unaligned(p);
                    let np = addr_of!((*p).FileName) as *const u16;
                    (info, std::slice::from_raw_parts(np, info.FileNameLength as usize / 2).to_vec())
                };
                let dot = name.len() <= 2 && name.iter().all(|&c| c == b'.' as u16);
                if !dot && !name.is_empty() {
                    let newest = match (filetime(info.CreationTime), filetime(info.LastWriteTime)) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (a, b) => a.or(b),
                    };
                    out.push(Entry { name, attrs: info.FileAttributes, size: info.EndOfFile.max(0) as u64, newest });
                }
                if info.NextEntryOffset == 0 {
                    break;
                }
                off += info.NextEntryOffset as usize;
                if off >= buf.len() * 8 {
                    break;
                }
            }
        }
    }

    /// Deletes the open object through its handle. Folders are only removed when empty.
    /// `attrs` are its current attributes; `can_write_attrs` says the handle may clear read-only.
    pub fn delete(h: &Handle, attrs: u32, can_write_attrs: bool) -> bool {
        let ex = |flags| set(h, FileDispositionInfoEx, &FILE_DISPOSITION_INFO_EX { Flags: flags });
        if ex(DISPOSITION_DELETE | DISPOSITION_POSIX | DISPOSITION_IGNORE_READONLY) {
            return true;
        }
        let e = unsafe { GetLastError() };
        if !matches!(e, ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED | ERROR_INVALID_PARAMETER) {
            return false; // in use, denied, or a folder that isn't empty
        }
        // Older Windows or a file system without POSIX deletes (FAT, some network shares).
        if ex(DISPOSITION_DELETE | DISPOSITION_IGNORE_READONLY) {
            return true;
        }
        if attrs & super::ATTR_READONLY != 0 && can_write_attrs {
            let cleared = attrs & !super::ATTR_READONLY;
            let basic = FILE_BASIC_INFO {
                CreationTime: 0,
                LastAccessTime: 0,
                LastWriteTime: 0,
                ChangeTime: 0,
                FileAttributes: if cleared == 0 { 0x80 } else { cleared }, // 0x80 = NORMAL
            };
            if !set(h, FileBasicInfo, &basic) {
                return false;
            }
        }
        set(h, FileDispositionInfo, &FILE_DISPOSITION_INFO { DeleteFile: 1 })
    }
}

// ---------------------------------------------------------------------------------------------
// Safety

/// Case-insensitive component list, for comparing paths the file system resolved.
fn path_key(p: &Path) -> Vec<String> {
    p.components()
        .filter(|c| !matches!(c, Component::CurDir))
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect()
}

/// `path` is `root` or somewhere below it.
fn within(path: &Path, root: &Path) -> bool {
    let (p, r) = (path_key(path), path_key(root));
    !r.is_empty() && p.len() >= r.len() && p[..r.len()] == r[..]
}

fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;
    let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
    let hr = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut p) };
    let out = (hr >= 0 && !p.is_null()).then(|| unsafe {
        let len = (0..).take_while(|&i| *p.add(i) != 0).count();
        PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(p, len)))
    });
    if !p.is_null() {
        unsafe { CoTaskMemFree(p as *const _) };
    }
    out
}

/// Folders that must never be emptied — neither as a root nor as anything inside a root.
fn protected() -> Vec<PathBuf> {
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
        "PUBLIC",
        "ALLUSERSPROFILE",
        "OneDrive",
        "OneDriveConsumer",
        "OneDriveCommercial",
    ]
    .iter()
    .filter_map(|v| env_path(v))
    .collect();
    if let Some(p) = env_path("USERPROFILE") {
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
            "Saved Games",
            "AppData",
            "AppData\\Local",
            "AppData\\LocalLow",
            "AppData\\Roaming",
        ] {
            v.push(p.join(s));
        }
        if let Some(users) = p.parent() {
            v.push(users.to_path_buf());
        }
    }
    v.push(sys_root().join("Users"));
    if let Some(w) = windir() {
        v.push(w.join("System32"));
        v.push(w.join("SysWOW64"));
    }
    // The real known-folder locations, which may have been moved away from the profile.
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
        &FOLDERID_RoamingAppData,
        &FOLDERID_UserProfiles,
        &FOLDERID_Public,
        &FOLDERID_ProgramData,
        &FOLDERID_ProgramFiles,
        &FOLDERID_ProgramFilesX86,
        &FOLDERID_Windows,
        &FOLDERID_System,
    ] {
        v.extend(known_folder(id));
    }
    v.iter().filter_map(|p| std::fs::canonicalize(p).ok()).collect()
}

/// A dedicated temp folder: named Temp/Tmp (e.g. %LOCALAPPDATA%\Temp, %SystemRoot%\Temp), or a
/// numbered per-session folder inside one (%LOCALAPPDATA%\Temp\2).
fn is_dedicated_temp(c: &Path) -> bool {
    let names: Vec<String> = c
        .components()
        .filter_map(|x| match x {
            Component::Normal(n) => Some(n.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect();
    let is_temp = |s: &str| s == "temp" || s == "tmp";
    match names.as_slice() {
        [.., last] if is_temp(last) => true,
        [.., parent, last] => is_temp(parent) && last.chars().all(|ch| ch.is_ascii_digit()),
        _ => false,
    }
}

/// Whether the canonical root `c` must not be emptied by a delete item.
fn root_refused(c: &Path, root: &Root, protected: &[PathBuf]) -> bool {
    if root.recursive {
        let normal = c.components().filter(|x| matches!(x, Component::Normal(_))).count();
        // Refuse drive roots, very short paths, protected folders and anything that contains one.
        if normal < 2 || protected.iter().any(|p| within(p, c)) {
            return true;
        }
        if root.temp_dir && !is_dedicated_temp(c) {
            return true;
        }
        false
    } else {
        root.filter.is_none()
    }
}

enum RootErr {
    /// Not there (or not a plain folder) — nothing to report.
    Missing,
    /// There, but refused for safety; reported to the user.
    Unsafe(PathBuf),
}

/// Opens a root folder as a handle and checks the folder the handle really points at.
/// `strict` applies the delete rules (used for Delete items whether measuring or deleting, so
/// analyze and run agree).
fn open_root(root: &Root, protected: &[PathBuf], strict: bool, delete: bool) -> Result<(fsh::Handle, PathBuf), RootErr> {
    let access = fsh::LIST | fsh::READ_ATTRIBUTES;
    let h = if delete {
        fsh::open_dir_path(&root.dir, access, fsh::SHARE_RW)
            .or_else(|| fsh::open_dir_path(&root.dir, access, fsh::SHARE_ALL))
    } else {
        fsh::open_dir_path(&root.dir, access, fsh::SHARE_ALL)
    }
    .ok_or(RootErr::Missing)?;
    let a = fsh::attrs(&h).ok_or(RootErr::Missing)?;
    if a & ATTR_DIRECTORY == 0 || a & ATTR_REPARSE_POINT != 0 {
        return Err(RootErr::Missing);
    }
    let c = fsh::final_path(&h).ok_or(RootErr::Missing)?;
    if strict && root_refused(&c, root, protected) {
        return Err(RootErr::Unsafe(c));
    }
    Ok((h, c))
}

/// `\\?\C:\x` → `C:\x` for messages.
fn display_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{rest}")
    } else {
        s.strip_prefix("\\\\?\\").unwrap_or(&s).to_string()
    }
}

fn refused_message(refused: &[PathBuf]) -> String {
    let first = display_path(&refused[0]);
    match refused.len() {
        1 => format!("Left {first} alone: it isn't a folder Platter can safely empty."),
        n => format!("Left {first} and {} more alone: they aren't folders Platter can safely empty.", n - 1),
    }
}

// ---------------------------------------------------------------------------------------------
// Walking

#[derive(Default)]
struct Acc {
    bytes: u64,
    files: u64,
    skipped: u64,
    /// Roots refused for safety.
    refused: Vec<PathBuf>,
    /// Measuring (a sequential `walk` with `delete: false`): what was found so far.
    tally: Tally,
    sampler: Sampler,
}

/// What measuring found in (part of) one item.
#[derive(Default)]
struct Tally {
    bytes: u64,
    files: u64,
    /// Hard-linked files seen so far → (links seen, total links, size). Space only comes back once
    /// every link is gone, so such a file counts when all of its links are inside the item.
    links: HashMap<(u64, [u8; 16]), (u32, u32, u64)>,
}

impl Tally {
    fn merge(&mut self, o: Tally) {
        self.bytes += o.bytes;
        self.files += o.files;
        for (id, (seen, total, size)) in o.links {
            self.links.entry(id).or_insert((0, total, size)).0 += seen;
        }
    }

    /// (bytes, files), counting each hard-linked file whose links were all seen once.
    fn finish(self) -> (u64, u64) {
        let linked: u64 = self.links.values().filter(|(seen, total, _)| seen >= total).map(|l| l.2).sum();
        (self.bytes + linked, self.files)
    }
}

/// Hard-link sampling state shared by everything measuring one item (see `measure_entry`).
#[derive(Default)]
struct Sampler {
    /// Non-empty files seen so far.
    sized: AtomicU64,
    /// A sampled file had several hard links, so every file of this item is now checked.
    exact: AtomicBool,
}

fn old_enough(newest: Option<SystemTime>, cutoff: Option<SystemTime>) -> bool {
    match cutoff {
        None => true,
        Some(c) => newest.map(|t| t < c).unwrap_or(false),
    }
}

struct Walk<'a> {
    canon_root: &'a Path,
    recursive: bool,
    filter: Option<NameFilter>,
    cutoff: Option<SystemTime>,
    delete: bool,
    /// Measuring only (deleting always passes `never`): stop early, the result is thrown away.
    cancel: &'a (dyn Fn() -> bool + Sync),
}

/// Opens a sub-folder relative to its parent; folders held for deleting can't be renamed meanwhile.
/// Returns the handle and whether it was opened with DELETE access.
fn open_sub(parent: &fsh::Handle, name: &[u16], want_delete: bool, deleting: bool) -> Option<(fsh::Handle, bool)> {
    let base = fsh::LIST | fsh::READ_ATTRIBUTES;
    let shares: &[u32] = if deleting { &[fsh::SHARE_RW, fsh::SHARE_ALL] } else { &[fsh::SHARE_ALL] };
    let accesses: &[(u32, bool)] = if want_delete { &[(base | fsh::DELETE, true), (base, false)] } else { &[(base, false)] };
    for &(access, del) in accesses {
        for &share in shares {
            if let Some(h) = fsh::open_child(parent, name, access, share, true) {
                return Some((h, del));
            }
        }
    }
    None
}

/// One open folder on the walk stack.
struct Frame {
    dir: fsh::Handle,
    entries: std::vec::IntoIter<fsh::Entry>,
    /// Remove this folder (through `dir`) once it has been emptied.
    remove: bool,
}

/// Checks and lists an open folder. None if it failed its checks or couldn't be listed.
fn enter(w: &Walk, dir: fsh::Handle, remove: bool) -> Option<Frame> {
    if w.delete {
        // Belt and braces: the folder this handle points at must really be inside the root.
        match fsh::final_path(&dir) {
            Some(p) if within(&p, w.canon_root) => {}
            _ => return None,
        }
    }
    let entries = fsh::list(&dir)?.into_iter();
    Some(Frame { dir, entries, remove })
}

/// Walks the open folder `root` depth-first with an explicit stack (folder trees can be far deeper
/// than a thread's stack allows). Every folder below `root` is opened relative to its parent's
/// handle, which stays open until the folder is done. Returns false if `root` failed its checks.
fn walk(w: &Walk, root: fsh::Handle, acc: &mut Acc, progress: &mut dyn FnMut(u64)) -> bool {
    let Some(first) = enter(w, root, false) else { return false };
    let mut stack = vec![first];
    while let Some(top) = stack.last_mut() {
        // A superseded measure stops here (one atomic load per entry). Never for a delete: only
        // measures get a real cancel check, and stopping would leave nothing half-done anyway.
        if !w.delete && (w.cancel)() {
            break;
        }
        let Some(e) = top.entries.next() else {
            let done = stack.pop().unwrap();
            if done.remove {
                // Only succeeds if it is empty now.
                let _ = fsh::delete(&done.dir, 0, false);
            }
            continue;
        };
        if e.attrs & ATTR_REPARSE_POINT != 0 {
            continue; // never follow or delete junctions / symlinks
        }
        if e.attrs & ATTR_DIRECTORY != 0 {
            if !w.recursive {
                continue;
            }
            let remove = w.delete && old_enough(e.newest, w.cutoff);
            let Some((sub, can_delete)) = open_sub(&top.dir, &e.name, remove, w.delete) else { continue };
            // Re-check on the handle itself: still a plain folder, not a junction swapped in.
            match fsh::attrs(&sub) {
                Some(a) if a & ATTR_DIRECTORY != 0 && a & ATTR_REPARSE_POINT == 0 => {}
                _ => continue,
            }
            if let Some(frame) = enter(w, sub, remove && can_delete) {
                stack.push(frame);
            }
        } else {
            if let Some(f) = w.filter {
                if !f(&String::from_utf16_lossy(&e.name)) {
                    continue;
                }
            }
            if !old_enough(e.newest, w.cutoff) {
                continue;
            }
            if w.delete {
                delete_entry(w, &top.dir, &e, acc);
                progress(acc.bytes);
            } else {
                measure_entry(&top.dir, &e, &acc.sampler, &mut acc.tally);
            }
        }
    }
    true
}

fn delete_entry(w: &Walk, dir: &fsh::Handle, e: &fsh::Entry, acc: &mut Acc) {
    let base = fsh::DELETE | fsh::READ_ATTRIBUTES;
    let opened = if e.attrs & ATTR_READONLY != 0 {
        fsh::open_child(dir, &e.name, base | fsh::WRITE_ATTRIBUTES, fsh::SHARE_ALL, false).map(|h| (h, true))
    } else {
        None
    }
    .or_else(|| fsh::open_child(dir, &e.name, base, fsh::SHARE_ALL, false).map(|h| (h, false)));
    let Some((h, can_write)) = opened else {
        acc.skipped += 1; // in use or denied
        return;
    };
    let Some(a) = fsh::attrs(&h) else {
        acc.skipped += 1;
        return;
    };
    if a & (ATTR_REPARSE_POINT | ATTR_DIRECTORY) != 0 {
        return; // changed since it was listed; leave it alone
    }
    match fsh::final_path(&h) {
        Some(p) if within(&p, w.canon_root) => {}
        _ => {
            acc.skipped += 1;
            return;
        }
    }
    let (size, links) = fsh::standard(&h).unwrap_or((e.size, 1));
    // Deleting one of several hard links frees nothing; the last link frees the data.
    let freed = if a & CLOUD_ATTRS != 0 || links > 1 { 0 } else { size };
    if fsh::delete(&h, a, can_write) {
        acc.bytes += freed;
        acc.files += 1;
    } else {
        acc.skipped += 1;
    }
}

/// Hard-link sampling while measuring. Asking a file for its link count costs a kernel open (plus
/// every file-system filter, e.g. the antivirus, seeing that open); for the ~550 000 files of a
/// typical temp folder + npm cache that was about a third of the CPU a measure took. Directory
/// listings don't carry the link count, so instead the first `PROBE_FIRST` non-empty files of an
/// item and then every `PROBE_EVERY`-th are checked; as soon as one has several links (a pnpm
/// store, a hard-linked build cache), the item switches to checking every file exactly, as before.
///
/// Trade-off: a hard link the samples miss counts its full size (the space would only come back
/// once its links outside the item are gone too), so a mostly-unlinked item can be overstated by
/// its few unsampled hard-linked files. Items made of hard links are caught by the first samples,
/// so an item that would free nothing is not shown as freeing space. `cleanup_run` always reads
/// each file's link count before deleting, so the "freed" it reports stays exact.
const PROBE_FIRST: u64 = 32;
const PROBE_EVERY: u64 = 64;

fn measure_entry(dir: &fsh::Handle, e: &fsh::Entry, sampler: &Sampler, t: &mut Tally) {
    t.files += 1;
    let size = if e.attrs & CLOUD_ATTRS != 0 { 0 } else { e.size };
    if size == 0 {
        return;
    }
    if !sampler.exact.load(Ordering::Relaxed) {
        let n = sampler.sized.fetch_add(1, Ordering::Relaxed);
        if n >= PROBE_FIRST && n % PROBE_EVERY != 0 {
            t.bytes += size;
            return;
        }
    }
    // One cheap query by name first; only hard-linked files (rare) are opened for their file id.
    if fsh::stat_links(dir, &e.name) == Some(1) {
        t.bytes += size;
        return;
    }
    let linked = fsh::open_child(dir, &e.name, fsh::READ_ATTRIBUTES, fsh::SHARE_ALL, false).and_then(|h| {
        let (_, links) = fsh::standard(&h)?;
        if links > 1 {
            Some((fsh::file_id(&h)?, links))
        } else {
            None
        }
    });
    match linked {
        None => t.bytes += size,
        Some((id, total)) => {
            sampler.exact.store(true, Ordering::Relaxed);
            // Counted by `Tally::finish` once every link turned up inside this item.
            t.links.entry(id).or_insert((0, total, size)).0 += 1;
        }
    }
}

/// A folder handle that child tasks open their sub-folders relative to.
struct SharedDir(fsh::Handle);

// SAFETY: shared only to open children relative to it (NtCreateFile with RootDirectory), which is
// thread-safe; the folder is listed by its own task before the handle is shared.
unsafe impl Sync for SharedDir {}

/// What one root's measure shares between its tasks.
struct Measure<'a> {
    recursive: bool,
    filter: Option<NameFilter>,
    cutoff: Option<SystemTime>,
    /// Per item (all of its roots).
    sampler: &'a Sampler,
    out: &'a Mutex<Tally>,
}

/// Measures the open folder `root` (already checked by `open_root`), every sub-folder as its own
/// task on the current rayon pool, so one huge item (a temp folder with 100 000+ folders) is
/// spread over all of the pool's threads. Same rules as `walk` measuring: reparse points are
/// never followed, sub-folders are opened relative to their parent's handle and re-checked on the
/// handle itself. Nothing is deleted. The walk does not recurse, so deep trees are fine.
fn measure_tree(root: fsh::Handle, m: &Measure) {
    rayon::scope(|s| measure_dir(s, m, root));
}

fn measure_dir<'s>(s: &rayon::Scope<'s>, m: &'s Measure<'_>, dir: fsh::Handle) {
    let Some(entries) = fsh::list(&dir) else { return };
    let mut t = Tally::default();
    let mut subdirs = Vec::new();
    for e in entries {
        if e.attrs & ATTR_REPARSE_POINT != 0 {
            continue; // never follow junctions / symlinks
        }
        if e.attrs & ATTR_DIRECTORY != 0 {
            if m.recursive {
                subdirs.push(e.name);
            }
            continue;
        }
        if let Some(f) = m.filter {
            if !f(&String::from_utf16_lossy(&e.name)) {
                continue;
            }
        }
        if !old_enough(e.newest, m.cutoff) {
            continue;
        }
        measure_entry(&dir, &e, m.sampler, &mut t);
    }
    if t.files > 0 {
        m.out.lock().unwrap().merge(t);
    }
    if subdirs.is_empty() {
        return;
    }
    let parent = Arc::new(SharedDir(dir));
    for name in subdirs {
        let parent = parent.clone();
        s.spawn(move |s| {
            let opened = open_sub(&parent.0, &name, false, false);
            drop(parent);
            let Some((sub, _)) = opened else { return };
            // Re-check on the handle itself: still a plain folder, not a junction swapped in.
            match fsh::attrs(&sub) {
                Some(a) if a & ATTR_DIRECTORY != 0 && a & ATTR_REPARSE_POINT == 0 => measure_dir(s, m, sub),
                _ => {}
            }
        });
    }
}

fn process(def: &Def, delete: bool, progress: &mut dyn FnMut(u64)) -> Acc {
    process_with(def, delete, &protected(), true, &never, progress)
}

/// `process` with the protected-folder list already worked out (analyze shares one between
/// items). `parallel` (measuring only) spreads each folder tree over the current rayon pool;
/// otherwise each item is walked on one thread, which costs less CPU in total because threads in
/// the same tree contend inside the file system. `cancel` stops a one-thread measure early (the
/// result is then partial and must be thrown away); it is ignored when deleting.
fn process_with(
    def: &Def,
    delete: bool,
    protected: &[PathBuf],
    parallel: bool,
    cancel: &(dyn Fn() -> bool + Sync),
    progress: &mut dyn FnMut(u64),
) -> Acc {
    let cancel: &(dyn Fn() -> bool + Sync) = if delete { &never } else { cancel };
    let mut acc = Acc::default();
    if def.blocked.is_some() {
        return acc;
    }
    // Delete items follow the delete rules even when only measuring, so analyze shows what a run
    // would really free. Only Delete items may ever delete.
    let strict = def.kind == Kind::Delete;
    if delete && !strict {
        return acc;
    }
    let cutoff = def.min_age.and_then(|a| SystemTime::now().checked_sub(a));
    // Measuring in parallel: what every task found. (`acc.sampler` spans all of the item's roots.)
    let tally = Mutex::new(Tally::default());
    for root in &def.roots {
        let (h, c) = match open_root(root, protected, strict, delete) {
            Ok(x) => x,
            Err(RootErr::Missing) => continue,
            Err(RootErr::Unsafe(c)) => {
                acc.refused.push(c);
                continue;
            }
        };
        if delete || !parallel {
            let w = Walk { canon_root: &c, recursive: root.recursive, filter: root.filter, cutoff, delete, cancel };
            walk(&w, h, &mut acc, progress);
        } else {
            let m = Measure { recursive: root.recursive, filter: root.filter, cutoff, sampler: &acc.sampler, out: &tally };
            measure_tree(h, &m);
        }
    }
    if !delete {
        let mut t = tally.into_inner().unwrap_or_default();
        t.merge(std::mem::take(&mut acc.tally));
        (acc.bytes, acc.files) = t.finish();
    }
    acc
}

// ---------------------------------------------------------------------------------------------
// Who is signed in

/// The user SID of a token, kept in its TOKEN_USER buffer.
struct TokenSid(Vec<u64>);

impl TokenSid {
    fn of_process(process: windows_sys::Win32::Foundation::HANDLE) -> Option<TokenSid> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY};
        use windows_sys::Win32::System::Threading::OpenProcessToken;
        let mut tok = std::ptr::null_mut();
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut tok) } == 0 {
            return None;
        }
        let mut buf = vec![0u64; 64];
        let mut len = 0u32;
        let ok = unsafe { GetTokenInformation(tok, TokenUser, buf.as_mut_ptr() as *mut _, (buf.len() * 8) as u32, &mut len) };
        unsafe { CloseHandle(tok) };
        (ok != 0).then_some(TokenSid(buf))
    }

    fn same_as(&self, other: &TokenSid) -> bool {
        use windows_sys::Win32::Security::{EqualSid, TOKEN_USER};
        unsafe {
            let a = (*(self.0.as_ptr() as *const TOKEN_USER)).User.Sid;
            let b = (*(other.0.as_ptr() as *const TOKEN_USER)).User.Sid;
            EqualSid(a, b) != 0
        }
    }
}

/// True when Platter runs elevated as a different account than the one that owns the desktop
/// (over-the-shoulder UAC). When that can't be confirmed either way while elevated, it says true,
/// so per-user items are left alone rather than cleaning someone else's profile.
fn other_account() -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};
    if !is_elevated() {
        return false;
    }
    let Some(me) = TokenSid::of_process(unsafe { GetCurrentProcess() }) else { return true };
    let shell = unsafe { GetShellWindow() };
    if shell.is_null() {
        return true;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(shell, &mut pid) };
    if pid == 0 {
        return true;
    }
    let hp = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if hp.is_null() {
        return true;
    }
    let desktop_user = TokenSid::of_process(hp);
    unsafe { CloseHandle(hp) };
    match desktop_user {
        Some(u) => !u.same_as(&me),
        None => true,
    }
}

// ---------------------------------------------------------------------------------------------
// Recycle bin & hibernation

fn recycle_query() -> Option<(u64, u64)> {
    use windows_sys::Win32::UI::Shell::{SHQueryRecycleBinW, SHQUERYRBINFO};
    let mut info: SHQUERYRBINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHQUERYRBINFO>() as u32;
    let hr = unsafe { SHQueryRecycleBinW(std::ptr::null(), &mut info) };
    (hr >= 0).then(|| (info.i64Size.max(0) as u64, info.i64NumItems.max(0) as u64))
}

fn recycle_empty() -> i32 {
    use windows_sys::Win32::UI::Shell::SHEmptyRecycleBinW;
    // SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND
    unsafe { SHEmptyRecycleBinW(std::ptr::null_mut(), std::ptr::null(), 0x7) }
}

/// `<System32>\name`, from GetSystemDirectoryW (not the environment, not the search path).
fn system32_exe(name: &str) -> Option<PathBuf> {
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = [0u16; 512];
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    (n > 0 && n < buf.len()).then(|| PathBuf::from(OsString::from_wide(&buf[..n])).join(name))
}

fn hibernate_off() -> Result<(), String> {
    let exe = system32_exe("powercfg.exe").ok_or_else(|| "Couldn't find powercfg to turn off hibernation.".to_string())?;
    let out = std::process::Command::new(exe)
        .args(["/hibernate", "off"])
        .stdin(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|_| "Couldn't run powercfg to turn off hibernation.".to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err("Windows couldn't turn off hibernation.".into())
    }
}

// ---------------------------------------------------------------------------------------------
// Analyze

#[cfg(test)]
fn analyze_one(def: &Def) -> CleanupItem {
    analyze_item(def, &protected(), true, &never)
}

fn analyze_item(def: &Def, protected: &[PathBuf], parallel: bool, cancel: &(dyn Fn() -> bool + Sync)) -> CleanupItem {
    let mut description = def.desc.to_string();
    let (size, files, available) = match (def.blocked, def.kind) {
        // Shown with its reason and a size of 0 (so it can't be selected). A blocked review item
        // has nothing to open, so it is hidden instead.
        (Some(note), kind) => {
            description = note.into();
            (0, 0, kind != Kind::Review)
        }
        (None, Kind::RecycleBin) => {
            let (s, n) = recycle_query().unwrap_or((0, 0));
            (s, n, true)
        }
        (None, Kind::Hibernation) => match hiberfil_size() {
            Some(s) => (s, 1, true),
            None => (0, 0, false),
        },
        (None, Kind::Delete | Kind::Review) => {
            let available = !def.roots.is_empty();
            let acc = process_with(def, false, protected, parallel, cancel, &mut |_| {});
            if !acc.refused.is_empty() {
                // Same rules as a run: refused folders aren't counted, and the reason is shown.
                let note = refused_message(&acc.refused);
                description = if acc.bytes > 0 { format!("{} {note}", def.desc) } else { note };
            }
            (acc.bytes, acc.files, available)
        }
    };
    CleanupItem {
        id: def.id.into(),
        group: def.group.into(),
        name: def.name.into(),
        description,
        size,
        files,
        needs_admin: def.needs_admin,
        default_on: def.default_on,
        kind: def.kind.as_str().into(),
        review_path: def.review_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        available,
    }
}

/// Worker threads for a background measure: few, so the PC stays responsive. Each walks whole
/// items, the cheapest way in CPU.
const BACKGROUND_THREADS: usize = 2;
/// Worker threads when the user asked to measure. Folder trees are split between them, so the two
/// or three huge items (temp, npm cache) don't leave the rest idle. More threads finish a little
/// sooner but mostly contend inside the file system: on a 16-thread PC, 4 threads measured in
/// ~7 s for ~23 s of CPU, 8 threads in ~5 s for ~35 s of CPU.
const FOREGROUND_THREADS: usize = 4;

/// Lowers the calling thread's CPU, I/O and memory priority (THREAD_MODE_BACKGROUND_BEGIN).
fn enter_background_mode() {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
}

/// Background mode on a thread Platter doesn't own (a pooled tokio blocking worker): restored on
/// drop, even on panic, so later jobs on that thread don't inherit the low priority.
struct BackgroundGuard;

impl BackgroundGuard {
    fn enter() -> Self {
        enter_background_mode();
        BackgroundGuard
    }
}

impl Drop for BackgroundGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_END};
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END) };
    }
}

/// Measures every item, calling `on_item` as each one finishes. `background` runs on two
/// background-priority threads (the refresh at launch: lowest CPU, I/O and memory priority);
/// otherwise on `FOREGROUND_THREADS` at normal priority (the user asked to measure). `cancel` is
/// polled by a background measure while it walks; once it returns true the remaining work is
/// skipped and the result is partial (the caller must throw it away).
pub fn analyze(
    background: bool,
    cancel: impl Fn() -> bool + Sync,
    on_item: impl Fn(&CleanupItem) + Sync,
) -> Vec<CleanupItem> {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let threads = if background { BACKGROUND_THREADS } else { FOREGROUND_THREADS.min(cpus).max(2) };
    let mut builder = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("platter-cleanup-{i}"));
    if background {
        builder = builder.start_handler(|_| enter_background_mode());
    }
    let work = || {
        let defs = defs();
        let protected = protected();
        defs.par_iter()
            // One item per task: a few items (temp, npm cache) are far bigger than the rest.
            .with_max_len(1)
            .map(|d| {
                // The foreground measure splits trees over the pool and isn't cancelled (the UI
                // never supersedes it); a background one walks each item on one thread and stops
                // early once superseded.
                let stop: &(dyn Fn() -> bool + Sync) = if background { &cancel } else { &never };
                let item = analyze_item(d, &protected, !background, stop);
                on_item(&item);
                item
            })
            .collect()
    };
    match builder.build() {
        Ok(pool) => pool.install(work),
        Err(_) => {
            let _guard = background.then(BackgroundGuard::enter);
            work()
        }
    }
}

/// `background` (optional, JS `background`): true for the automatic refresh at launch, which runs
/// on two background-priority threads; false/absent when the user asked, which runs at full speed.
/// A successful result is also saved as the `cleanup` cache entry.
///
/// Superseded (another `cleanup_analyze` or a `cleanup_run` was invoked after this one): rejects
/// with `SUPERSEDED` and writes nothing to the cache; a background measure also stops walking.
#[tauri::command]
pub async fn cleanup_analyze(app: tauri::AppHandle, background: Option<bool>) -> Result<Vec<CleanupItem>, String> {
    let background = background.unwrap_or(false);
    // Taken in invoke order, before any thread hop.
    let my_gen = MEASURE_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let current = move || MEASURE_GEN.load(Ordering::SeqCst) == my_gen;
    tauri::async_runtime::spawn_blocking(move || {
        crate::perf::mark(if background { "rust:cleanup-start-bg" } else { "rust:cleanup-start" });
        let items = analyze(background, || !current(), |item| {
            if current() {
                let _ = app.emit("cleanup-item", item.clone());
            }
        });
        let _g = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if !current() {
            crate::perf::mark("rust:cleanup-superseded");
            return Err(SUPERSEDED.to_string());
        }
        crate::cache::write("cleanup", &items);
        crate::perf::mark("rust:cleanup-done");
        Ok(items)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// `cleanup_analyze`'s error when a newer measure or a clean-up was started after it.
pub const SUPERSEDED: &str = "This measure was replaced by a newer one.";

// ---------------------------------------------------------------------------------------------
// Run

fn run_one(def: &Def, elevated: bool, emit: &dyn Fn(u64, bool)) -> (CleanupItemResult, u64) {
    let mut res = CleanupItemResult { id: def.id.into(), freed: 0, skipped: 0, error: None };
    let mut deleted = 0u64;
    if def.kind == Kind::Review {
        res.error = Some("Review items are never deleted automatically. Open them in Space instead.".into());
        return (res, 0);
    }
    if let Some(note) = def.blocked {
        res.error = Some(note.into());
        return (res, 0);
    }
    if def.needs_admin && !elevated {
        res.error = Some(ADMIN_ERROR.into());
        return (res, 0);
    }
    match def.kind {
        Kind::RecycleBin => {
            let before = recycle_query().unwrap_or((0, 0));
            let hr = recycle_empty();
            let after = recycle_query().unwrap_or((0, 0));
            res.freed = before.0.saturating_sub(after.0);
            deleted = before.1.saturating_sub(after.1);
            res.skipped = after.1;
            if hr < 0 && before.1 > 0 && res.freed == 0 {
                res.error = Some("Windows couldn't empty the Recycle Bin.".into());
            }
        }
        Kind::Hibernation => {
            let before = hiberfil_size();
            if before.is_none() {
                res.error = Some("Hibernation is already off.".into());
            } else if let Err(e) = hibernate_off() {
                res.error = Some(e);
            } else {
                let after = hiberfil_size().unwrap_or(0);
                res.freed = before.unwrap_or(0).saturating_sub(after);
                deleted = 1;
            }
        }
        Kind::Delete => {
            let mut last = Instant::now();
            let acc = process(def, true, &mut |freed| {
                if last.elapsed() >= Duration::from_millis(150) {
                    last = Instant::now();
                    emit(freed, false);
                }
            });
            res.freed = acc.bytes;
            res.skipped = acc.skipped;
            deleted = acc.files;
            if !acc.refused.is_empty() {
                res.error = Some(refused_message(&acc.refused));
            }
        }
        Kind::Review => unreachable!(),
    }
    (res, deleted)
}

pub fn run(ids: &[String], emit: impl Fn(&str, u64, bool)) -> CleanupRunResult {
    let elevated = is_elevated();
    let defs = defs();
    let mut out = CleanupRunResult { freed: 0, deleted: 0, skipped: 0, items: Vec::new() };
    for id in ids {
        if !defs.iter().any(|d| d.id == id) {
            out.items.push(CleanupItemResult {
                id: id.clone(),
                freed: 0,
                skipped: 0,
                error: Some("Platter doesn't know this item.".into()),
            });
        }
    }
    for def in defs.iter().filter(|d| ids.iter().any(|i| i == d.id)) {
        let (r, deleted) = run_one(def, elevated, &|freed, done| emit(def.id, freed, done));
        emit(def.id, r.freed, true);
        out.freed += r.freed;
        out.skipped += r.skipped;
        out.deleted += deleted;
        out.items.push(r);
    }
    out
}

#[tauri::command]
pub async fn cleanup_run(app: tauri::AppHandle, ids: Vec<String>) -> Result<CleanupRunResult, String> {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Err("A clean up is already running.".into());
    }
    // Any measure started before (or during) this run counted files that may now be gone: it must
    // not write the `cleanup` cache, and a background one stops. Bumped under CACHE_LOCK, so a
    // cache write that already passed its check finishes before anything is deleted.
    bump_measure_gen();
    // The cached sizes describe files this run is about to delete; drop them for every instance
    // (standard and elevated share the cache), so the next launch re-measures instead.
    {
        let _g = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::cache::remove("cleanup");
    }
    let res = tauri::async_runtime::spawn_blocking(move || {
        run(&ids, |id, freed, done| {
            let _ = app.emit("cleanup-progress", CleanupProgress { id: id.into(), freed, done });
        })
    })
    .await;
    bump_measure_gen();
    RUNNING.store(false, Ordering::SeqCst);
    res.map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway folder under %TEMP% for one test.
    fn scratch(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("platter-cleanup-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn def_for(roots: Vec<Root>, min_age: Option<Duration>) -> Def {
        Def {
            id: "t",
            group: "System",
            name: "t",
            desc: "t",
            needs_admin: false,
            default_on: true,
            kind: Kind::Delete,
            roots,
            min_age,
            review_path: None,
            blocked: None,
        }
    }

    /// Measures `def` both ways (tree split over the pool, and one thread per item) and checks
    /// they agree. (bytes, files)
    fn measure(def: &Def) -> (u64, u64) {
        let p = protected();
        let a = process_with(def, false, &p, true, &never, &mut |_| {});
        let b = process_with(def, false, &p, false, &never, &mut |_| {});
        assert_eq!((a.bytes, a.files), (b.bytes, b.files), "parallel and one-thread measures agree");
        (a.bytes, a.files)
    }

    fn rroot(dir: &Path) -> Root {
        Root { dir: dir.to_path_buf(), recursive: true, filter: None, temp_dir: false }
    }

    fn junction(link: &Path, target: &Path) {
        let st = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(st.success(), "mklink /J failed");
    }

    /// CPU time (kernel + user) the calling thread has used so far.
    fn thread_cpu() -> Duration {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
        let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut a, mut b, mut k, mut u) = (z, z, z, z);
        unsafe { GetThreadTimes(GetCurrentThread(), &mut a, &mut b, &mut k, &mut u) };
        let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) * 100;
        Duration::from_nanos(t(k) + t(u))
    }

    #[test]
    #[ignore = "slow; times each item on one thread: cargo test cleanup_profile -- --ignored --nocapture"]
    fn cleanup_profile_items() {
        let t0 = Instant::now();
        let defs = defs();
        println!("defs() {:?}", t0.elapsed());
        let t0 = Instant::now();
        let protected = protected();
        println!("protected() {:?}", t0.elapsed());
        for d in &defs {
            let (t, c) = (Instant::now(), thread_cpu());
            let item = analyze_item(d, &protected, false, &never);
            println!("{:24} {:>9} files {:>8.0} ms wall {:>8.0} ms cpu", d.id, item.files, t.elapsed().as_secs_f64() * 1e3, (thread_cpu() - c).as_secs_f64() * 1e3);
        }
    }

    #[test]
    #[ignore = "slow; measures this PC: cargo test cleanup_analyze -- --ignored --nocapture"]
    fn cleanup_analyze_prints() {
        let background = std::env::var_os("PLATTER_CLEANUP_BG").is_some();
        let t = Instant::now();
        let items = analyze(background, never, |_| {});
        for i in &items {
            println!(
                "{:24} {:18} {:>14} bytes {:>8} files admin={} on={} avail={} {}",
                i.id, i.group, i.size, i.files, i.needs_admin, i.default_on, i.available, i.description
            );
        }
        println!("analyzed {} items in {:?}", items.len(), t.elapsed());
        assert_eq!(items.len(), defs().len());
        assert_eq!(items[0].id, "user-temp");
    }

    #[test]
    #[ignore = "slow; measures this PC: cargo test cleanup_cancel_latency -- --ignored --nocapture"]
    fn cleanup_cancel_latency() {
        // A background measure superseded 1 s in: how long until it gives the threads back.
        let t = Instant::now();
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_secs(1));
                stop.store(true, Ordering::SeqCst);
            });
            analyze(true, || stop.load(Ordering::SeqCst), |_| {});
        });
        println!("cancelled background measure returned after {:?} (cancel at 1 s)", t.elapsed());
    }

    #[test]
    fn cleanup_deletes_only_inside_root() {
        let base = scratch("test");
        let root = base.join("cache");
        let keep = base.join("keep.txt");
        std::fs::create_dir_all(root.join("a\\b")).unwrap();
        std::fs::write(root.join("x.bin"), vec![0u8; 1000]).unwrap();
        std::fs::write(root.join("a\\b\\y.bin"), vec![0u8; 500]).unwrap();
        std::fs::write(&keep, b"keep").unwrap();

        let def = def_for(vec![rroot(&root)], None);
        let m = measure(&def);
        assert_eq!(m, (1500, 2));
        let r = process(&def, true, &mut |_| {});
        assert_eq!((r.bytes, r.files, r.skipped), (1500, 2, 0));
        assert!(root.is_dir(), "root folder itself must stay");
        assert!(!root.join("a").exists(), "empty sub-folders are removed");
        assert!(keep.exists(), "files outside the root are untouched");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn cleanup_cancel_stops_measure_but_never_a_delete() {
        let base = scratch("cancel");
        let root = base.join("cache");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("x.bin"), vec![0u8; 1000]).unwrap();
        std::fs::write(root.join("a\\y.bin"), vec![0u8; 500]).unwrap();
        let def = def_for(vec![rroot(&root)], None);
        let p = protected();
        let stop = || true;
        let m = process_with(&def, false, &p, false, &stop, &mut |_| {});
        assert_eq!((m.bytes, m.files), (0, 0), "a cancelled one-thread measure stops at once");
        let d = process_with(&def, true, &p, false, &stop, &mut |_| {});
        assert_eq!((d.bytes, d.files, d.skipped), (1500, 2, 0), "deleting ignores cancel");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn cleanup_refuses_risky_roots() {
        // Checked without deleting anything.
        let protected = protected();
        let mut risky: Vec<PathBuf> =
            ["C:\\", "C:\\Windows", "C:\\Windows\\System32", "C:\\Users"].iter().map(PathBuf::from).collect();
        risky.extend(env_path("USERPROFILE"));
        risky.extend(env_path("LOCALAPPDATA"));
        // Ancestors of protected folders (e.g. TEMP=%USERPROFILE%\AppData) are refused too.
        risky.extend(env_path("USERPROFILE").map(|p| p.join("AppData")));
        for dir in risky {
            let r = rroot(&dir);
            let c = std::fs::canonicalize(&dir).unwrap();
            assert!(root_refused(&c, &r, &protected), "{dir:?} must be refused");
            assert!(matches!(open_root(&r, &protected, true, false), Err(RootErr::Unsafe(_))), "{dir:?}");
        }
        // A cache folder deep inside LOCALAPPDATA is fine.
        let base = scratch("roots");
        let c = std::fs::canonicalize(&base).unwrap();
        assert!(!root_refused(&c, &rroot(&base), &protected));

        // TEMP must be a dedicated temp folder.
        assert!(is_dedicated_temp(Path::new("C:\\Users\\u\\AppData\\Local\\Temp")));
        assert!(is_dedicated_temp(Path::new("\\\\?\\C:\\Windows\\TEMP")));
        assert!(is_dedicated_temp(Path::new("C:\\Users\\u\\AppData\\Local\\Temp\\2")));
        assert!(is_dedicated_temp(Path::new("D:\\scratch\\tmp")));
        assert!(!is_dedicated_temp(Path::new("C:\\Users\\u\\AppData")));
        assert!(!is_dedicated_temp(Path::new("D:\\Users\\u")));
        assert!(!is_dedicated_temp(Path::new("D:\\stuff\\2")));
        let not_temp = base.join("stuff");
        let temp = base.join("Temp");
        std::fs::create_dir_all(&not_temp).unwrap();
        std::fs::create_dir_all(&temp).unwrap();
        let tr = |d: &Path| Root { temp_dir: true, ..rroot(d) };
        assert!(matches!(open_root(&tr(&not_temp), &protected, true, false), Err(RootErr::Unsafe(_))));
        assert!(open_root(&tr(&temp), &protected, true, false).is_ok());
        // Same folder but not from TEMP: allowed.
        assert!(open_root(&rroot(&not_temp), &protected, true, false).is_ok());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn cleanup_reports_refused_roots_in_analyze_and_run() {
        let base = scratch("refused");
        let bad = base.join("NotATempFolder");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("old.bin"), vec![0u8; 4000]).unwrap();
        let def = def_for(vec![Root { temp_dir: true, ..rroot(&bad) }], None);

        let item = analyze_one(&def);
        assert_eq!((item.size, item.files), (0, 0), "a refused root isn't measured");
        assert!(item.description.contains("Left"), "{}", item.description);
        let (res, deleted) = run_one(&def, false, &|_, _| {});
        assert_eq!((res.freed, deleted), (0, 0));
        assert!(res.error.is_some(), "a refused root is reported, not silently skipped");
        assert!(bad.join("old.bin").exists());

        // Blocked (another account) items report their note and touch nothing.
        let mut blocked = def_for(vec![rroot(&bad)], None);
        blocked.blocked = Some(OTHER_ACCOUNT);
        let item = analyze_one(&blocked);
        assert_eq!((item.size, item.description.as_str(), item.available), (0, OTHER_ACCOUNT, true));
        let (res, _) = run_one(&blocked, true, &|_, _| {});
        assert_eq!(res.error.as_deref(), Some(OTHER_ACCOUNT));
        assert!(bad.join("old.bin").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn cleanup_never_follows_junctions() {
        // %TEMP%\platter-cleanup-junction-N\
        //   outside\secret.txt            <- must survive
        //   cache\x.bin                   <- deleted
        //   cache\link  -> outside        (junction inside the root)
        //   cache\sub\deeplink -> outside (junction deeper down)
        let base = scratch("junction");
        let outside = base.join("outside");
        let root = base.join("cache");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(outside.join("secret.txt"), vec![7u8; 300]).unwrap();
        std::fs::write(root.join("x.bin"), vec![0u8; 100]).unwrap();
        junction(&root.join("link"), &outside);
        junction(&root.join("sub\\deeplink"), &outside);

        let def = def_for(vec![rroot(&root)], None);
        let m = measure(&def);
        assert_eq!(m, (100, 1), "junction targets aren't measured");
        let r = process(&def, true, &mut |_| {});
        assert_eq!((r.bytes, r.files), (100, 1));
        assert!(outside.join("secret.txt").exists(), "file behind a junction must survive");
        assert!(!root.join("x.bin").exists());
        assert!(std::fs::symlink_metadata(root.join("link")).is_ok(), "junctions themselves are left alone");

        // A root that is itself a junction is not walked.
        let jroot = base.join("jroot");
        junction(&jroot, &outside);
        let r = process(&def_for(vec![rroot(&jroot)], None), true, &mut |_| {});
        assert_eq!(r.files, 0);
        assert!(outside.join("secret.txt").exists());

        // A folder handle that now resolves outside the root (renamed or swapped away after the
        // root was checked) is refused by the walker before anything in it is deleted.
        let canon_root = std::fs::canonicalize(&root).unwrap();
        let h = fsh::open_dir_path(&outside, fsh::LIST | fsh::READ_ATTRIBUTES, fsh::SHARE_ALL).unwrap();
        let w = Walk { canon_root: &canon_root, recursive: true, filter: None, cutoff: None, delete: true, cancel: &never };
        let mut acc = Acc::default();
        assert!(!walk(&w, h, &mut acc, &mut |_| {}));
        assert_eq!(acc.files, 0);
        assert!(outside.join("secret.txt").exists());

        std::fs::remove_dir(&jroot).unwrap();
        std::fs::remove_dir(root.join("link")).unwrap();
        std::fs::remove_dir(root.join("sub\\deeplink")).unwrap();
        assert!(outside.join("secret.txt").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn cleanup_handles_very_deep_trees() {
        // Deeper than a recursive walk could manage on a worker thread's stack.
        let base = scratch("deep");
        let root = base.join("cache");
        std::fs::create_dir_all(&root).unwrap();
        let canon_root = std::fs::canonicalize(&root).unwrap();
        let mut dir = fsh::open_dir_path(&root, fsh::LIST | fsh::READ_ATTRIBUTES, fsh::SHARE_ALL).unwrap();
        let mut depth = 0;
        // Built relative to handles, so the total path length can exceed MAX_PATH.
        let name: Vec<u16> = "d".encode_utf16().collect();
        while depth < 1200 {
            let Some(next) = fsh::create_child_dir(&dir, &name) else { break };
            dir = next;
            depth += 1;
        }
        assert!(depth >= 1200, "built only {depth} levels");
        drop(dir);
        let def = def_for(vec![rroot(&root)], None);
        // The parallel measure doesn't recurse either.
        let m = measure(&def);
        assert_eq!(m, (0, 0));
        let w = Walk { canon_root: &canon_root, recursive: true, filter: None, cutoff: None, delete: false, cancel: &never };
        let mut acc = Acc::default();
        let h = fsh::open_dir_path(&root, fsh::LIST | fsh::READ_ATTRIBUTES, fsh::SHARE_ALL).unwrap();
        // Run on a small-stack thread, like a rayon worker.
        std::thread::scope(|s| {
            std::thread::Builder::new()
                .stack_size(256 * 1024)
                .spawn_scoped(s, || assert!(walk(&w, h, &mut acc, &mut |_| {})))
                .unwrap()
                .join()
                .unwrap();
        });
        let r = process(&def, true, &mut |_| {});
        assert_eq!(r.files, 0);
        assert!(root.is_dir());
        assert!(fsh::list(&fsh::open_dir_path(&root, fsh::LIST, fsh::SHARE_ALL).unwrap()).unwrap().is_empty(), "empty folders removed");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn cleanup_counts_hard_links_once() {
        let base = scratch("links");
        let root = base.join("store");
        std::fs::create_dir_all(&root).unwrap();
        // a.bin is also linked from outside the root: deleting it frees nothing.
        std::fs::write(root.join("a.bin"), vec![1u8; 1000]).unwrap();
        std::fs::hard_link(root.join("a.bin"), base.join("project-a.bin")).unwrap();
        // b.bin has two links, both inside the root: its space comes back once.
        std::fs::write(root.join("b.bin"), vec![2u8; 500]).unwrap();
        std::fs::hard_link(root.join("b.bin"), root.join("b2.bin")).unwrap();

        let def = def_for(vec![rroot(&root)], None);
        let m = measure(&def);
        assert_eq!(m, (500, 3));
        let r = process(&def, true, &mut |_| {});
        assert_eq!((r.bytes, r.files, r.skipped), (500, 3, 0));
        assert_eq!(std::fs::read(base.join("project-a.bin")).unwrap(), vec![1u8; 1000]);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn cleanup_sampling_catches_hard_linked_stores() {
        // A store made of hard links (like pnpm's): far more files than the first samples, every
        // one also linked from outside the item, so measuring must find that it frees nothing.
        let base = scratch("store");
        let root = base.join("store");
        let outside = base.join("project");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        for i in 0..300 {
            let f = if i % 2 == 0 { root.join(format!("f{i}.bin")) } else { root.join("sub").join(format!("f{i}.bin")) };
            std::fs::write(&f, vec![1u8; 100]).unwrap();
            std::fs::hard_link(&f, outside.join(format!("f{i}.bin"))).unwrap();
        }
        let def = def_for(vec![rroot(&root)], None);
        let m = measure(&def);
        assert_eq!(m, (0, 300));
        // Plain files are counted in full.
        let plain = base.join("plain");
        std::fs::create_dir_all(plain.join("a").join("b")).unwrap();
        for i in 0..200 {
            let dir = if i % 3 == 0 { plain.join("a").join("b") } else { plain.join("a") };
            std::fs::write(dir.join(format!("p{i}")), vec![0u8; 10]).unwrap();
        }
        let m = measure(&def_for(vec![rroot(&plain)], None));
        assert_eq!(m, (2000, 200));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn cleanup_deletes_read_only_files() {
        let base = scratch("ro");
        let root = base.join("cache");
        std::fs::create_dir_all(&root).unwrap();
        let f = root.join("ro.bin");
        std::fs::write(&f, vec![0u8; 10]).unwrap();
        let mut p = std::fs::metadata(&f).unwrap().permissions();
        p.set_readonly(true);
        std::fs::set_permissions(&f, p).unwrap();
        let r = process(&def_for(vec![rroot(&root)], None), true, &mut |_| {});
        assert_eq!((r.bytes, r.files), (10, 1));
        assert!(!f.exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn cleanup_age_filter_keeps_new_files() {
        let base = scratch("age");
        std::fs::write(base.join("new.tmp"), b"hello").unwrap();
        let def = def_for(vec![rroot(&base)], Some(DAY));
        assert_eq!(process(&def, true, &mut |_| {}).files, 0);
        assert!(base.join("new.tmp").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn cleanup_powercfg_from_system32() {
        let exe = system32_exe("powercfg.exe").unwrap();
        assert!(exe.is_absolute());
        assert!(path_key(&exe).ends_with(&["system32".to_string(), "powercfg.exe".to_string()]));
        assert!(exe.exists());
    }

    #[test]
    fn cleanup_same_account_when_not_elevated() {
        if !is_elevated() {
            assert!(!other_account());
        }
    }
}
