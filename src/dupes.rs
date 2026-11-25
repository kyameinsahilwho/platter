//! Duplicates (Duplicate Intelligence): identical files by content hash, similar photos and videos
//! by perceptual hash of their Shell thumbnails, folders with identical contents, and safe
//! deletion recommendations. See CONTRACT.md "Duplicates" for the shapes and rules.
//!
//! Pipeline (one background thread per scan, cancellable at every file and every 1 MB buffer):
//! 1. Listing: a parallel, non-recursive walk of the roots (one rayon job per folder, like scan.rs),
//!    listing each folder with one `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)` query.
//!    Every folder records its file count, bytes and an order-independent signature of its file
//!    sizes, summed over its subtree afterwards (duplicate-folder candidates come from these).
//! 2. Exact: same size -> xxh3-128 of the first + last 64 KB -> xxh3-128 of the whole file. Hard
//!    links (same volume serial + 128-bit file id, read from the handle opened for hashing anyway)
//!    count as one file.
//! 3. Folders: candidates with equal (count, bytes, size signature) are confirmed by hashing all of
//!    their files; only the outermost duplicate folders are reported.
//! 4. Similar images/videos: the Shell's cached thumbnail -> 64-bit horizontal + vertical dHash,
//!    multi-index buckets for candidate pairs, union-find components, then groups built around a
//!    keeper so every member is within the threshold of the keeper (no drifting chains).
//!
//! Nothing here deletes anything except `dupes_delete`, which recycles one path at a time through
//! `manage::recycle_sync` after re-checking every path against the stored result.

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::cmp::Ordering as Cmp;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::Emitter;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FileIdBothDirectoryInfo, FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
    GetFinalPathNameByHandleW, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_BOTH_DIR_INFO, FILE_ID_INFO,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RootChoice {
    pub path: String,
    pub label: String,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DupesOptions {
    pub min_size: u64,
    pub similar_images: bool,
    pub similar_videos: bool,
    pub folders: bool,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DupesProgress {
    pub scan_id: u64,
    /// "listing" | "hashing" | "images" | "videos" | "folders"
    pub phase: &'static str,
    pub files: u64,
    pub bytes: u64,
    pub done: u64,
    pub total: u64,
    pub current: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DupesDone {
    pub scan_id: u64,
    pub ok: bool,
    pub cancelled: bool,
    pub error: Option<String>,
    pub elapsed_ms: u64,
}

#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Skipped {
    pub denied: u64,
    pub cloud: u64,
    pub small: u64,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DupFile {
    pub path: String,
    pub name: String,
    pub folder: String,
    pub size: u64,
    pub modified: u64,
    pub keep: bool,
    pub select: bool,
    /// "safe" | "review" | "protected"
    pub risk: &'static str,
    pub note: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DupGroup {
    pub id: String,
    pub cat: u8,
    pub size: u64,
    pub reclaimable: u64,
    pub files: Vec<DupFile>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FolderGroup {
    pub id: String,
    pub size: u64,
    pub files: u64,
    pub reclaimable: u64,
    pub folders: Vec<DupFile>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SimilarFile {
    #[serde(flatten)]
    pub file: DupFile,
    pub width: u32,
    pub height: u32,
    pub duration_ms: Option<u64>,
    pub similarity: u8,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SimilarGroup {
    pub id: String,
    /// "image" | "video"
    pub kind: &'static str,
    pub files: Vec<SimilarFile>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DupesResult {
    pub scan_id: u64,
    pub roots: Vec<String>,
    pub files: u64,
    pub bytes: u64,
    pub skipped: Skipped,
    pub reclaimable: u64,
    pub groups: Vec<DupGroup>,
    pub folders: Vec<FolderGroup>,
    pub images: Vec<SimilarGroup>,
    pub videos: Vec<SimilarGroup>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DeleteFailure {
    pub path: String,
    pub error: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DeleteResult {
    pub removed: u32,
    pub freed: u64,
    pub failed: Vec<DeleteFailure>,
}

// ---------------------------------------------------------------------------------------------
// Constants and tuning (measured on this PC, 16 logical CPUs, NVMe SSD, 2026-11-24)

const ATTR_HIDDEN: u32 = 0x2;
const ATTR_SYSTEM: u32 = 0x4;
const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_REPARSE_POINT: u32 = 0x400;
const CLOUD_ATTRS: u32 = 0x1000 | 0x4_0000 | 0x40_0000;
/// Reparse tags with this bit point somewhere else (junctions, symlinks, mount points).
const NAME_SURROGATE: u32 = 0x2000_0000;

const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_NO_MORE_FILES: u32 = 18;
const FILETIME_UNIX_EPOCH: i64 = 116_444_736_000_000_000;

/// Bytes hashed at each end of a file before the full hash is worth it.
const EDGE: u64 = 64 * 1024;
/// Read buffer for full hashes.
const BUF: usize = 1 << 20;
const MIN_FOLDER_BYTES: u64 = 1 << 20;
const MIN_IMAGE_BYTES: u64 = 16 * 1024;
/// Keep the 5,000 groups that free the most; more is unreadable and only bloats the IPC payload.
const MAX_GROUPS: usize = 5000;

/// Largest dHash Hamming distance (max of the horizontal and vertical 64-bit hashes) for two images
/// to count as similar. Checked by eye on this PC's 1,213 Pictures/Downloads images (2026-11-24,
/// `spike_similar_real`, after the grid/colour/detail checks below): the same picture as
/// jpg/png/webp, resized, re-downloaded or AI-enhanced landed at 0-5; two posters made from one
/// template with different text at 5-6 (shown as similar, never pre-selected); re-generated
/// variants that differ visibly at 7-8.
const IMAGE_MAX_DIST: u32 = 6;
/// Video thumbnails are single frames of possibly different encodes, so they get more slack; the
/// duration check carries most of the weight.
const VIDEO_MAX_DIST: u32 = 10;
/// Mean brightness (0-255) of two similar thumbnails may differ by at most this much: dHash only
/// sees gradients, so a flat white and a flat grey picture would otherwise match.
const MEAN_MAX_DIFF: f32 = 16.0;
/// Width/height ratios of similar pictures differ by at most 12 % (a crop isn't a copy).
const ASPECT_MAX_RATIO: f32 = 1.12;
/// Mean R, G and B of similar pictures differ by at most this much (0-255).
const RGB_MAX_DIFF: u8 = 24;
/// Pictures with fewer informative 16x16 cells (see `Sig::detail`) are left out of similarity.
const MIN_DETAIL: u16 = 12;

const OUT_OF_DATE: &str = "These results are out of date. Scan again.";

const NOTE_PROJECT: &str = "Part of a project folder, so leave it";
const NOTE_APP: &str = "In an app folder, so leave it";
const NOTE_GAME: &str = "In a game folder, so leave it";
const NOTE_WINDOWS: &str = "Part of Windows, so leave it";

const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "bmp", "webp", "heic", "heif", "tif", "tiff"];
const VIDEO_EXTS: &[&str] = &["mp4", "mov", "mkv", "avi", "wmv", "m4v", "webm"];

/// Worker threads for listing and the first+last 64 KB hashes. Both are dominated by opening files
/// and folders (kernel + filter-driver time), which scan.rs measured to stop scaling past ~8
/// threads on this 16-thread PC (full C: 8 threads ~11 s vs 16-32 threads ~14 s).
fn list_threads() -> usize {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    (cpus / 2).clamp(4, 8)
}

/// Threads for full-file hashes. Measured 2026-11-24 over the 40 largest local files in the
/// user's folders (1.55 GB, 1 MB unbuffered reads): 1 thread 315-944 MB/s, 2 threads 1,139 MB/s,
/// 4 threads 1,190 MB/s, 8 threads 1,173 MB/s — the NVMe drive saturates at 2-4 readers. xxh3-128
/// itself is far faster than that in a release build (the debug build manages only ~240 MB/s per
/// thread, so 4 threads also keep debug runs disk-bound). On a spinning disk parallel readers only
/// add seeks, so one reader.
fn full_hash_threads(hdd: bool) -> usize {
    if hdd {
        1
    } else {
        4
    }
}

/// STA threads for Shell thumbnails. Measured 2026-11-24 on 256 cached jpg/png/webp thumbnails
/// (64 px, SIIGBF_THUMBNAILONLY|SIIGBF_BIGGERSIZEOK): STA 2 threads 0.80 ms/file, 4 threads 0.46,
/// 8 threads 0.34; MTA 1.94 / 1.12 / 1.58 (calls get marshalled to an STA). Uncached thumbnails
/// cost ~10-370 ms each (decoding; videos are the slow ones), so a few threads matter, but more than
/// 4 would compete with the rest of the PC for CPU.
const THUMB_THREADS: usize = 4;

// ---------------------------------------------------------------------------------------------
// Global state

struct Running {
    gen: u64,
    cancel: Arc<AtomicBool>,
}

static GEN: AtomicU64 = AtomicU64::new(0);

fn running() -> &'static Mutex<Option<Running>> {
    static R: OnceLock<Mutex<Option<Running>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(None))
}

fn store() -> &'static Mutex<Option<Stored>> {
    static R: OnceLock<Mutex<Option<Stored>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(None))
}

const PHASES: [&str; 5] = ["listing", "hashing", "images", "videos", "folders"];
const PH_LISTING: u8 = 0;
const PH_HASHING: u8 = 1;
const PH_IMAGES: u8 = 2;
const PH_VIDEOS: u8 = 3;
const PH_FOLDERS: u8 = 4;

#[derive(Default)]
struct Progress {
    phase: AtomicU8,
    files: AtomicU64,
    bytes: AtomicU64,
    done: AtomicU64,
    total: AtomicU64,
    current: Mutex<String>,
}

impl Progress {
    fn start_phase(&self, phase: u8, total: u64) {
        self.phase.store(phase, Ordering::Relaxed);
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }
    fn set_current(&self, path: &str) {
        if let Ok(mut c) = self.current.try_lock() {
            c.clear();
            c.push_str(path);
        }
    }
    fn tick(&self, path: &str) {
        self.done.fetch_add(1, Ordering::Relaxed);
        self.set_current(path);
    }
    fn snapshot(&self, scan_id: u64) -> DupesProgress {
        DupesProgress {
            scan_id,
            phase: PHASES[self.phase.load(Ordering::Relaxed) as usize % PHASES.len()],
            files: self.files.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            done: self.done.load(Ordering::Relaxed),
            total: self.total.load(Ordering::Relaxed),
            current: self.current.lock().map(|c| c.clone()).unwrap_or_default(),
        }
    }
}

struct Ctx {
    cancel: Arc<AtomicBool>,
    progress: Progress,
}

impl Ctx {
    fn new(cancel: Arc<AtomicBool>) -> Ctx {
        Ctx { cancel, progress: Progress::default() }
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------------------------
// Small helpers

fn join_path(dir: &str, name: &str) -> String {
    if dir.ends_with('\\') {
        format!("{dir}{name}")
    } else {
        format!("{dir}\\{name}")
    }
}

/// `child` is `parent` or lies inside it (both lower-case, backslashed, no trailing backslash
/// except a drive root).
fn inside(child: &str, parent: &str) -> bool {
    child == parent
        || (child.len() > parent.len()
            && child.starts_with(parent)
            && (parent.ends_with('\\') || child.as_bytes()[parent.len()] == b'\\'))
}

fn lower(s: &str) -> String {
    s.to_lowercase()
}

fn ext_of(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i + 1 < name.len() => name[i + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

fn filetime_ms(ft: i64) -> u64 {
    if ft <= FILETIME_UNIX_EPOCH {
        0
    } else {
        ((ft - FILETIME_UNIX_EPOCH) / 10_000) as u64
    }
}

fn meta_modified_ms(m: &std::fs::Metadata) -> u64 {
    filetime_ms(m.last_write_time() as i64)
}

/// Strong 64-bit mix (splitmix64 finalizer), so summing mixed sizes gives a multiset signature.
fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Explorer/Finder metadata files that differ between otherwise identical folders.
fn is_meta_name(lower_name: &str) -> bool {
    matches!(lower_name, "thumbs.db" | "desktop.ini" | ".ds_store")
}

/// Names Windows and browsers give copies: "x - Copy.jpg", "x - Copy (2).jpg", "x (1).jpg",
/// "Copy of x.jpg".
fn looks_like_copy(name: &str) -> bool {
    let n = name.to_lowercase();
    let stem = match n.rfind('.') {
        Some(i) if i > 0 => &n[..i],
        _ => n.as_str(),
    };
    if stem.starts_with("copy of ") || (stem.starts_with("copy (") && stem.contains(") of ")) {
        return true;
    }
    if stem.ends_with(" - copy") {
        return true;
    }
    // "… (digits)" at the end, optionally after " - copy".
    if let Some(open) = stem.rfind(" (") {
        let inner = &stem[open + 2..];
        if let Some(digits) = inner.strip_suffix(')') {
            if !digits.is_empty() && digits.len() <= 4 && digits.bytes().all(|b| b.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
}

/// Scan category of a file name (same table as scan.rs `category_of_wide`):
/// 0 video, 1 images, 2 audio, 3 documents, 4 archives & installers, 5 apps & system,
/// 6 developer, 7 other.
fn category_of(name: &str) -> u8 {
    match ext_of(name).as_str() {
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "m4v" | "flv" | "mpg" | "mpeg" | "ts" | "mts" | "m2ts"
        | "vob" | "3gp" | "3g2" | "ogv" | "divx" | "f4v" | "asf" | "rm" | "rmvb" | "braw" | "r3d" | "prproj" => 0,
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "heic" | "heif" | "avif" | "raw" | "cr2" | "cr3" | "nef"
        | "arw" | "dng" | "orf" | "rw2" | "raf" | "psd" | "psb" | "tif" | "tiff" | "svg" | "ico" | "jxl" | "xcf"
        | "kra" | "ai" | "eps" | "jfif" | "tga" | "exr" | "hdr" | "dds" => 1,
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" | "opus" | "aiff" | "aif" | "alac" | "ape" | "mid"
        | "midi" | "amr" | "m4b" | "m4p" => 2,
        "pdf" | "doc" | "docx" | "docm" | "xls" | "xlsx" | "xlsm" | "xlsb" | "ppt" | "pptx" | "pptm" | "txt"
        | "rtf" | "odt" | "ods" | "odp" | "csv" | "md" | "epub" | "mobi" | "azw3" | "pages" | "numbers" | "key"
        | "one" | "pst" | "ost" | "msg" | "eml" | "xps" | "oxps" | "tex" | "accdb" | "mdb" | "vsdx" | "pub" => 3,
        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "lz" | "lzma" | "iso" | "img" | "msi"
        | "msp" | "msu" | "cab" | "vhd" | "vhdx" | "vmdk" | "vdi" | "qcow2" | "wim" | "esd" | "appx"
        | "appxbundle" | "msix" | "msixbundle" | "dmg" | "apk" | "xapk" | "deb" | "rpm" | "swm" => 4,
        "exe" | "dll" | "sys" | "drv" | "ocx" | "mui" | "dat" | "bin" | "pak" | "etl" | "evtx" | "cat"
        | "manifest" | "cpl" | "scr" | "efi" | "inf" | "pnf" | "ttf" | "otf" | "ttc" | "fon" | "nls" | "tlb"
        | "winmd" | "db" | "edb" | "jrs" | "chk" | "regtrans-ms" | "blf" | "log" | "msc" | "mum" | "ini" | "cfg"
        | "tmp" | "ps1xml" | "sdb" | "vxd" | "com" | "bak" | "ldf" | "mdf" | "sqlite" | "pck" | "vpk" | "bundle"
        | "assets" | "ress" | "resource" | "ucas" | "utoc" | "uasset" | "forge" => 5,
        "rs" | "js" | "mjs" | "cjs" | "cts" | "tsx" | "jsx" | "py" | "pyi" | "java" | "kt" | "c" | "cc" | "cpp"
        | "cxx" | "h" | "hh" | "hpp" | "cs" | "go" | "rb" | "php" | "json" | "map" | "lock" | "o" | "obj" | "pdb"
        | "lib" | "rlib" | "rmeta" | "a" | "class" | "jar" | "wasm" | "node" | "pyc" | "pyd" | "whl" | "ipynb"
        | "swift" | "scala" | "lua" | "dart" | "vue" | "svelte" | "css" | "scss" | "less" | "html" | "htm"
        | "xml" | "yml" | "yaml" | "toml" | "sln" | "csproj" | "vcxproj" | "nupkg" | "ilk" | "idb" | "pch"
        | "ipch" | "gradle" | "sh" | "bat" | "cmd" | "ps1" | "sql" | "d" | "tlog" => 6,
        _ => 7,
    }
}

fn build_pool(threads: usize, name: &'static str) -> Option<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        // Jobs never recurse, so stack use is flat whatever the folder depth.
        .stack_size(4 * 1024 * 1024)
        .thread_name(move |i| format!("{name}-{i}"))
        .build()
        .ok()
}

#[link(name = "ole32")]
extern "system" {
    fn CoTaskMemFree(pv: *const core::ffi::c_void);
}

/// Where a Known Folder really is (OneDrive folder backup, policy or the user can move it).
fn known_folder(id: &windows_sys::core::GUID) -> Option<String> {
    use windows_sys::Win32::UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY};
    let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
    let hr = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DONT_VERIFY as u32, std::ptr::null_mut(), &mut p) };
    let out = if hr >= 0 && !p.is_null() {
        let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
        let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) });
        (!s.is_empty()).then_some(s)
    } else {
        None
    };
    if !p.is_null() {
        unsafe { CoTaskMemFree(p as *const _) };
    }
    out
}

/// Whether the drive holding `path` is a spinning disk (IOCTL_STORAGE_QUERY_PROPERTY,
/// StorageDeviceSeekPenaltyProperty; works without admin). Unknown counts as not.
fn incurs_seek_penalty(path: &str) -> bool {
    use windows_sys::Win32::System::IO::DeviceIoControl;
    let b = path.as_bytes();
    if b.len() < 2 || b[1] != b':' || !b[0].is_ascii_alphabetic() {
        return false;
    }
    let dev = crate::util::wide(&format!(r"\\.\{}:", b[0] as char));
    // FILE_READ_ATTRIBUTES | SYNCHRONIZE
    let h = unsafe {
        CreateFileW(dev.as_ptr(), 0x80 | 0x0010_0000, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut())
    };
    if h == INVALID_HANDLE_VALUE {
        return false;
    }
    // STORAGE_PROPERTY_QUERY { PropertyId = 7 (seek penalty), QueryType = 0, AdditionalParameters }
    let query: [u32; 3] = [7, 0, 0];
    let mut out = [0u8; 12];
    let mut got = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h,
            0x002D_1400,
            query.as_ptr() as *const _,
            12,
            out.as_mut_ptr() as *mut _,
            12,
            &mut got,
            std::ptr::null_mut(),
        )
    };
    unsafe { CloseHandle(h) };
    ok != 0 && got >= 9 && out[8] != 0
}

// ---------------------------------------------------------------------------------------------
// Where a path is: drives the keeper order, risk and notes.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Place {
    Library(&'static str),
    Desktop,
    Other,
    Downloads,
    Temp,
}

/// Lower-case locations of the user's folders. Built from the real Known Folders in the app;
/// tests build their own over a temp tree.
#[derive(Default, Clone)]
struct Places {
    libraries: Vec<(String, &'static str)>,
    desktop: Vec<String>,
    downloads: Vec<String>,
    temp: Vec<String>,
    /// Folders whose contents are protected as a whole, with the note to show.
    system: Vec<(String, &'static str)>,
    /// Treat any folder named AppData as an app folder (other accounts' profiles on a drive scan).
    appdata_by_name: bool,
}

fn norm_lower(p: &str) -> String {
    let mut s = p.replace('/', "\\").to_lowercase();
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    s
}

impl Places {
    fn real() -> Places {
        use windows_sys::Win32::UI::Shell::{
            FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music, FOLDERID_Pictures,
            FOLDERID_Videos,
        };
        let profile = crate::util::env_path("USERPROFILE").map(|p| p.to_string_lossy().into_owned());
        // Each folder is also registered under its real path (junction, symlink or subst
        // resolved): merge_roots keeps roots by real path, so a Pictures junctioned to D:\Pictures
        // is listed as D:\Pictures\... and must still classify as a library.
        let with_real = |v: &mut Vec<String>| {
            for p in v.clone() {
                if let Some(r) = final_path(&p) {
                    if !v.contains(&r) {
                        v.push(r);
                    }
                }
            }
        };
        let both = |id: &windows_sys::core::GUID, sub: &str| -> Vec<String> {
            let mut v: Vec<String> = known_folder(id).map(|p| norm_lower(&p)).into_iter().collect();
            if let Some(pr) = &profile {
                let d = norm_lower(&join_path(pr, sub));
                if !v.contains(&d) {
                    v.push(d);
                }
            }
            with_real(&mut v);
            v
        };
        let mut libraries = Vec::new();
        for (id, sub) in [
            (&FOLDERID_Pictures, "Pictures"),
            (&FOLDERID_Documents, "Documents"),
            (&FOLDERID_Videos, "Videos"),
            (&FOLDERID_Music, "Music"),
        ] {
            for p in both(id, sub) {
                libraries.push((p, sub));
            }
        }
        let mut temp: Vec<String> = ["TEMP", "TMP"]
            .iter()
            .filter_map(|v| crate::util::env_path(v))
            .map(|p| norm_lower(&p.to_string_lossy()))
            .collect();
        temp.dedup();
        with_real(&mut temp);
        let env = |v: &str| crate::util::env_path(v).map(|p| norm_lower(&p.to_string_lossy()));
        let mut system = Vec::new();
        for (v, note) in [
            ("SystemRoot", NOTE_WINDOWS),
            ("windir", NOTE_WINDOWS),
            ("ProgramFiles", NOTE_APP),
            ("ProgramFiles(x86)", NOTE_APP),
            ("ProgramW6432", NOTE_APP),
            ("ProgramData", NOTE_APP),
            ("LOCALAPPDATA", NOTE_APP),
            ("APPDATA", NOTE_APP),
        ] {
            if let Some(p) = env(v) {
                system.push((p, note));
            }
        }
        if let Some(pr) = &profile {
            system.push((norm_lower(&join_path(pr, "AppData")), NOTE_APP));
        }
        for (p, note) in system.clone() {
            if let Some(r) = final_path(&p) {
                if !system.iter().any(|(s, _)| *s == r) {
                    system.push((r, note));
                }
            }
        }
        Places {
            libraries,
            desktop: both(&FOLDERID_Desktop, "Desktop"),
            downloads: both(&FOLDERID_Downloads, "Downloads"),
            temp,
            system,
            appdata_by_name: true,
        }
    }

    fn classify(&self, path_lower: &str) -> Place {
        if self.downloads.iter().any(|d| inside(path_lower, d)) {
            return Place::Downloads;
        }
        if self.temp.iter().any(|d| inside(path_lower, d)) {
            return Place::Temp;
        }
        if let Some((_, label)) = self.libraries.iter().find(|(d, _)| inside(path_lower, d)) {
            return Place::Library(label);
        }
        if self.desktop.iter().any(|d| inside(path_lower, d)) {
            return Place::Desktop;
        }
        Place::Other
    }

    /// The user's main folders themselves (and drive roots) are never an app or project folder as
    /// a whole: Downloads holding a setup .exe next to a stray .dll must not protect all of
    /// Downloads (it did, on this PC's first real run, 2026-11-24).
    fn markers_ignored(&self, path_lower: &str) -> bool {
        is_drive_root(path_lower)
            || self.downloads.iter().chain(&self.desktop).chain(&self.temp).any(|d| d == path_lower)
            || self.libraries.iter().any(|(d, _)| d == path_lower)
    }

    /// The protection note for a scan root that already lies inside a protected location.
    fn root_note(&self, root_lower: &str) -> Option<&'static str> {
        if let Some((_, note)) = self.system.iter().find(|(d, _)| inside(root_lower, d)) {
            return Some(note);
        }
        if self.appdata_by_name && root_lower.split('\\').any(|c| c == "appdata") {
            return Some(NOTE_APP);
        }
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Directory listing (same query as scan.rs; see filesystem.md "Fast directory walk")

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// NUL-terminated path for `CreateFileW`, adding the `\\?\` prefix for long paths.
fn open_path(dir: &[u16]) -> Vec<u16> {
    let bs = b'\\' as u16;
    let mut v: Vec<u16> = Vec::with_capacity(dir.len() + 10);
    let long = dir.len() > 240;
    let starts_unc = dir.len() > 2 && dir[0] == bs && dir[1] == bs;
    let already = starts_unc && dir.len() > 3 && (dir[2] == b'?' as u16 || dir[2] == b'.' as u16);
    if long && !already {
        if starts_unc {
            v.extend("\\\\?\\UNC".encode_utf16());
            v.extend_from_slice(&dir[1..]);
        } else {
            v.extend("\\\\?\\".encode_utf16());
            v.extend_from_slice(dir);
        }
    } else {
        v.extend_from_slice(dir);
    }
    let bare_drive = v.len() == 2 && v[1] == b':' as u16;
    let share_root = starts_unc && !already && !long && v.iter().filter(|&&c| c == bs).count() == 3;
    if bare_drive || share_root {
        v.push(bs);
    }
    v.push(0);
    v
}

struct Entry<'a> {
    name: &'a [u16],
    attrs: u32,
    /// Reparse tag when `attrs` has the reparse-point bit.
    tag: u32,
    size: u64,
    modified: u64,
}

const LIST_BUF_WORDS: usize = 64 * 1024 / 8;

thread_local! {
    static LIST_BUF: RefCell<Vec<u64>> = RefCell::new(vec![0u64; LIST_BUF_WORDS]);
    static READ_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Lists `dir`, calling `f` for every entry except `.` and `..`.
/// Err(()) when the folder exists but couldn't be read (access denied, ...).
fn list_dir(dir: &[u16], mut f: impl FnMut(&Entry)) -> Result<(), ()> {
    let p = open_path(dir);
    let h = unsafe {
        CreateFileW(
            p.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return match unsafe { GetLastError() } {
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(()),
            _ => Err(()),
        };
    }
    let h = OwnedHandle(h);
    LIST_BUF.with(|b| {
        let mut buf = b.borrow_mut();
        let bytes = buf.len() * 8;
        let fixed = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
        let mut first = true;
        loop {
            let ok = unsafe {
                GetFileInformationByHandleEx(h.0, FileIdBothDirectoryInfo, buf.as_mut_ptr() as *mut core::ffi::c_void, bytes as u32)
            };
            if ok == 0 {
                let err = unsafe { GetLastError() };
                return match err {
                    ERROR_NO_MORE_FILES | ERROR_FILE_NOT_FOUND => Ok(()),
                    _ if first => Err(()),
                    _ => Ok(()),
                };
            }
            first = false;
            let base = buf.as_ptr() as *const u8;
            let mut off = 0usize;
            while off + fixed <= bytes {
                // SAFETY: the call filled the buffer with a chain of entries linked by
                // NextEntryOffset, each followed by its name; reads stay inside `bytes`.
                let (next, info, name) = unsafe {
                    let p = base.add(off) as *const FILE_ID_BOTH_DIR_INFO;
                    let info = std::ptr::read_unaligned(p);
                    let len = (info.FileNameLength as usize / 2).min((bytes - off - fixed) / 2);
                    let name = std::slice::from_raw_parts(base.add(off + fixed) as *const u16, len);
                    (info.NextEntryOffset as usize, info, name)
                };
                let is_dot = name == [b'.' as u16] || name == [b'.' as u16, b'.' as u16];
                if !is_dot {
                    f(&Entry {
                        name,
                        attrs: info.FileAttributes,
                        tag: info.EaSize,
                        size: info.EndOfFile.max(0) as u64,
                        modified: filetime_ms(info.LastWriteTime),
                    });
                }
                if next == 0 {
                    break;
                }
                off += next;
            }
        }
    })
}

fn wide_lower(c: u16) -> u16 {
    if (b'A' as u16..=b'Z' as u16).contains(&c) {
        c + 32
    } else {
        c
    }
}

fn wide_is(name: &[u16], s: &str) -> bool {
    name.len() == s.len() && name.iter().zip(s.bytes()).all(|(&c, b)| wide_lower(c) == b as u16)
}

fn wide_ends(name: &[u16], s: &str) -> bool {
    name.len() >= s.len() && wide_is(&name[name.len() - s.len()..], s)
}

// ---------------------------------------------------------------------------------------------
// Listing

/// This folder (not its sub-folders) holds a project / app / game marker.
const MK_PROJECT: u8 = 1;
const MK_EXE: u8 = 2;
const MK_DLL: u8 = 4;
const MK_GAME: u8 = 8;

struct DirRec {
    /// usize::MAX for a root.
    parent: usize,
    /// Folder name; for a root the full path.
    name: Box<str>,
    modified: u64,
    /// Subtree totals after `assemble` (own files before): every file, whatever its size, except
    /// Explorer metadata files.
    files: u64,
    bytes: u64,
    /// Order-independent signature of the multiset of file sizes (sum of mixed sizes).
    sig: u64,
    /// Own markers (MK_*).
    marks: u8,
    /// Something in the subtree couldn't be seen (denied, cloud-only, a link): never a folder
    /// candidate.
    incomplete: bool,
}

struct FileRec {
    dir: usize,
    name: Box<str>,
    size: u64,
    modified: u64,
}

struct Listing {
    roots: usize,
    dirs: Vec<DirRec>,
    files: Vec<FileRec>,
    skipped: Skipped,
    files_seen: u64,
    bytes_seen: u64,
}

struct Walk<'a> {
    ctx: &'a Ctx,
    min_size: u64,
    windir: Option<String>,
    next_id: AtomicUsize,
    dirs: Mutex<Vec<(usize, DirRec)>>,
    files: Mutex<Vec<FileRec>>,
    denied: AtomicU64,
    cloud: AtomicU64,
    small: AtomicU64,
}

struct Job {
    id: usize,
    parent: usize,
    path: Vec<u16>,
    name: Box<str>,
    modified: u64,
    drive_root: bool,
}

/// Top-level folders skipped when a whole drive is scanned (CONTRACT: Windows, Program Files*,
/// ProgramData, $Recycle.Bin, System Volume Information).
fn skip_at_drive_root(lower_name: &str) -> bool {
    matches!(lower_name, "windows" | "programdata" | "$recycle.bin" | "system volume information" | "recovery" | "$winreagent" | "config.msi")
        || lower_name.starts_with("program files")
}

fn walk_dir<'s>(s: &rayon::Scope<'s>, w: &'s Walk<'s>, job: Job) {
    if w.ctx.cancelled() {
        return;
    }
    let Job { id, parent, path, name, modified, drive_root } = job;
    let mut rec = DirRec { parent, name, modified, files: 0, bytes: 0, sig: 0, marks: 0, incomplete: false };
    let mut subdirs: Vec<(Vec<u16>, Box<str>, u64)> = Vec::new();
    let mut local: Vec<FileRec> = Vec::new();
    let (mut seen, mut seen_bytes, mut small, mut cloud) = (0u64, 0u64, 0u64, 0u64);
    let listed = list_dir(&path, |e| {
        let hidden_system = e.attrs & (ATTR_HIDDEN | ATTR_SYSTEM) == ATTR_HIDDEN | ATTR_SYSTEM;
        let link = e.attrs & ATTR_REPARSE_POINT != 0 && e.tag & NAME_SURROGATE != 0;
        if e.attrs & ATTR_DIRECTORY != 0 {
            if hidden_system {
                // Not looked into, yet it goes to the Recycle Bin with its folder: a folder
                // holding one is never compared as a whole.
                rec.incomplete = true;
                return;
            }
            if link {
                // A junction/symlink points elsewhere: never followed, and its folder can't be
                // compared as a whole.
                rec.incomplete = true;
                return;
            }
            if e.attrs & CLOUD_ATTRS != 0 {
                cloud += 1;
                rec.incomplete = true;
                return;
            }
            let fname = String::from_utf16_lossy(e.name);
            let lname = fname.to_lowercase();
            if drive_root && skip_at_drive_root(&lname) {
                return;
            }
            match lname.as_str() {
                ".git" | "node_modules" => rec.marks |= MK_PROJECT,
                "steamapps" => rec.marks |= MK_GAME,
                _ => {}
            }
            let child = {
                let mut v = path.clone();
                if v.last() != Some(&(b'\\' as u16)) {
                    v.push(b'\\' as u16);
                }
                v.extend_from_slice(e.name);
                v
            };
            if let Some(win) = &w.windir {
                if String::from_utf16_lossy(&child).to_lowercase() == *win {
                    return;
                }
            }
            subdirs.push((child, fname.into_boxed_str(), e.modified));
        } else {
            if link {
                rec.incomplete = true;
                return;
            }
            if e.attrs & CLOUD_ATTRS != 0 {
                cloud += 1;
                rec.incomplete = true;
                return;
            }
            if drive_root && (wide_is(e.name, "pagefile.sys") || wide_is(e.name, "hiberfil.sys") || wide_is(e.name, "swapfile.sys")) {
                return;
            }
            let meta = wide_is(e.name, "thumbs.db") || wide_is(e.name, "desktop.ini") || wide_is(e.name, ".ds_store");
            if hidden_system {
                // Never a duplicate on its own (CONTRACT), but part of what a folder copy holds:
                // two folders differing only in hidden system files are not the same.
                if !meta {
                    rec.files += 1;
                    rec.bytes += e.size;
                    rec.sig = rec.sig.wrapping_add(mix64(e.size));
                }
                return;
            }
            if wide_is(e.name, "package.json") || wide_is(e.name, "cargo.toml") || wide_is(e.name, ".git") || wide_ends(e.name, ".sln") {
                rec.marks |= MK_PROJECT;
            } else if wide_ends(e.name, ".exe") {
                rec.marks |= MK_EXE;
            } else if wide_ends(e.name, ".dll") {
                rec.marks |= MK_DLL;
            }
            seen += 1;
            seen_bytes += e.size;
            if !meta {
                rec.files += 1;
                rec.bytes += e.size;
                rec.sig = rec.sig.wrapping_add(mix64(e.size));
            }
            if e.size < w.min_size {
                small += 1;
            } else if e.size > 0 {
                local.push(FileRec { dir: id, name: String::from_utf16_lossy(e.name).into_boxed_str(), size: e.size, modified: e.modified });
            }
        }
    });
    if listed.is_err() {
        rec.incomplete = true;
        w.denied.fetch_add(1, Ordering::Relaxed);
    }
    let p = &w.ctx.progress;
    let before = p.files.fetch_add(seen, Ordering::Relaxed);
    p.bytes.fetch_add(seen_bytes, Ordering::Relaxed);
    p.done.store(before + seen, Ordering::Relaxed);
    if id % 32 == 0 {
        p.set_current(&String::from_utf16_lossy(&path));
    }
    w.small.fetch_add(small, Ordering::Relaxed);
    w.cloud.fetch_add(cloud, Ordering::Relaxed);
    if !local.is_empty() {
        w.files.lock().unwrap().append(&mut local);
    }
    let first = w.next_id.fetch_add(subdirs.len(), Ordering::Relaxed);
    w.dirs.lock().unwrap().push((id, rec));
    for (i, (cp, cn, cm)) in subdirs.into_iter().enumerate() {
        let job = Job { id: first + i, parent: id, path: cp, name: cn, modified: cm, drive_root: false };
        s.spawn(move |s| walk_dir(s, w, job));
    }
}

fn is_drive_root(p: &str) -> bool {
    let b = p.as_bytes();
    (b.len() == 3 && b[1] == b':' && b[2] == b'\\') || (b.len() == 2 && b[1] == b':')
}

fn list_all(roots: &[String], min_size: u64, ctx: &Ctx) -> Option<Listing> {
    ctx.progress.start_phase(PH_LISTING, 0);
    let w = Walk {
        ctx,
        min_size,
        windir: crate::util::env_path("SystemRoot").map(|p| norm_lower(&p.to_string_lossy())),
        next_id: AtomicUsize::new(roots.len()),
        dirs: Mutex::new(Vec::new()),
        files: Mutex::new(Vec::new()),
        denied: AtomicU64::new(0),
        cloud: AtomicU64::new(0),
        small: AtomicU64::new(0),
    };
    let pool = build_pool(list_threads(), "platter-dupes-list")?;
    let wr = &w;
    pool.scope(|s| {
        for (i, r) in roots.iter().enumerate() {
            let modified = std::fs::metadata(r).map(|m| meta_modified_ms(&m)).unwrap_or(0);
            let job = Job {
                id: i,
                parent: usize::MAX,
                path: r.encode_utf16().collect(),
                name: r.clone().into_boxed_str(),
                modified,
                drive_root: is_drive_root(r),
            };
            s.spawn(move |s| walk_dir(s, wr, job));
        }
    });
    if ctx.cancelled() {
        return None;
    }
    let total = w.next_id.load(Ordering::Relaxed);
    let mut recs = w.dirs.into_inner().ok()?;
    if recs.len() != total {
        return None;
    }
    recs.sort_unstable_by_key(|r| r.0);
    let mut dirs: Vec<DirRec> = recs.into_iter().map(|r| r.1).collect();
    // Children always have larger ids than their parent: one reverse pass folds subtrees up.
    for id in (roots.len()..dirs.len()).rev() {
        let (p, files, bytes, sig, inc) = {
            let d = &dirs[id];
            (d.parent, d.files, d.bytes, d.sig, d.incomplete)
        };
        let pd = &mut dirs[p];
        pd.files += files;
        pd.bytes += bytes;
        pd.sig = pd.sig.wrapping_add(sig);
        pd.incomplete |= inc;
    }
    Some(Listing {
        roots: roots.len(),
        dirs,
        files: w.files.into_inner().ok()?,
        skipped: Skipped {
            denied: w.denied.load(Ordering::Relaxed),
            cloud: w.cloud.load(Ordering::Relaxed),
            small: w.small.load(Ordering::Relaxed),
        },
        files_seen: ctx.progress.files.load(Ordering::Relaxed),
        bytes_seen: ctx.progress.bytes.load(Ordering::Relaxed),
    })
}

impl Listing {
    fn dir_paths(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(self.dirs.len());
        for (i, d) in self.dirs.iter().enumerate() {
            let p = if i < self.roots { d.name.to_string() } else { join_path(&out[d.parent], &d.name) };
            out.push(p);
        }
        out
    }

    /// Per folder: the protection note that applies to it (its own markers, an ancestor's up to
    /// the scan root, or the root's location).
    fn protection(&self, paths: &[String], places: &Places) -> Vec<Option<&'static str>> {
        let mut out: Vec<Option<&'static str>> = Vec::with_capacity(self.dirs.len());
        for (i, d) in self.dirs.iter().enumerate() {
            let inherited = if i < self.roots { places.root_note(&lower(&paths[i])) } else { out[d.parent] };
            let own = if places.markers_ignored(&norm_lower(&paths[i])) {
                None
            } else if d.marks & MK_PROJECT != 0 {
                Some(NOTE_PROJECT)
            } else if d.marks & MK_GAME != 0 {
                Some(NOTE_GAME)
            } else if d.marks & (MK_EXE | MK_DLL) == MK_EXE | MK_DLL {
                Some(NOTE_APP)
            } else if places.appdata_by_name && d.name.eq_ignore_ascii_case("appdata") {
                Some(NOTE_APP)
            } else {
                None
            };
            out.push(inherited.or(own));
        }
        out
    }

    fn file_path(&self, paths: &[String], i: usize) -> String {
        join_path(&paths[self.files[i].dir], &self.files[i].name)
    }

    fn ancestors(&self, dir: usize) -> impl Iterator<Item = usize> + '_ {
        let mut cur = self.dirs[dir].parent;
        std::iter::from_fn(move || {
            if cur == usize::MAX {
                None
            } else {
                let c = cur;
                cur = self.dirs[c].parent;
                Some(c)
            }
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Content hashing

/// Volume serial + 128-bit file id: the same pair = the same file (hard links).
type Ident = (u64, u128);

fn open_read(path: &str) -> std::io::Result<std::fs::File> {
    // FILE_FLAG_SEQUENTIAL_SCAN: the cache manager reads ahead and drops pages behind us.
    std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).custom_flags(0x0800_0000).open(path)
}

fn file_ident(f: &std::fs::File) -> Option<Ident> {
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileInformationByHandleEx(
            f.as_raw_handle() as HANDLE,
            FileIdInfo,
            &mut info as *mut FILE_ID_INFO as *mut core::ffi::c_void,
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    let id = u128::from_le_bytes(info.FileId.Identifier);
    (ok != 0 && id != 0 && id != u128::MAX).then_some((info.VolumeSerialNumber, id))
}

/// A handle for reading attributes of a file or folder. Links on the way, the last part included,
/// are followed: a junction, subst drive or mapped path opens the object it leads to.
fn open_attrs(path: &str) -> Option<OwnedHandle> {
    let w: Vec<u16> = path.encode_utf16().collect();
    let p = open_path(&w);
    let h = unsafe {
        CreateFileW(
            p.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    (h != INVALID_HANDLE_VALUE).then_some(OwnedHandle(h))
}

/// Volume serial + file id of the file or folder `path` leads to. Falls back to the 64-bit file
/// index for file systems without 128-bit ids (FAT, some network shares).
fn path_ident(path: &str) -> Option<Ident> {
    let h = open_attrs(path)?;
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileInformationByHandleEx(h.0, FileIdInfo, &mut info as *mut FILE_ID_INFO as *mut core::ffi::c_void, std::mem::size_of::<FILE_ID_INFO>() as u32)
    };
    let id = u128::from_le_bytes(info.FileId.Identifier);
    if ok != 0 && id != 0 && id != u128::MAX {
        return Some((info.VolumeSerialNumber, id));
    }
    let mut bh: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(h.0, &mut bh) } == 0 {
        return None;
    }
    let id = (bh.nFileIndexHigh as u128) << 32 | bh.nFileIndexLow as u128;
    (id != 0).then_some((bh.dwVolumeSerialNumber as u64, id))
}

/// Where `path` really is, with junctions, symlinks and subst drives resolved (lower-case, like
/// `norm_lower`). None if it can't be opened.
fn final_path(path: &str) -> Option<String> {
    let h = open_attrs(path)?;
    let mut buf = vec![0u16; 1024];
    loop {
        // FILE_NAME_NORMALIZED | VOLUME_NAME_DOS
        let n = unsafe { GetFinalPathNameByHandleW(h.0, buf.as_mut_ptr(), buf.len() as u32, 0) } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            let s = String::from_utf16_lossy(&buf[..n]);
            let s = if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
                format!(r"\\{unc}")
            } else if let Some(local) = s.strip_prefix(r"\\?\") {
                local.to_string()
            } else {
                s
            };
            return Some(norm_lower(&s));
        }
        buf.resize(n + 1, 0);
    }
}

/// `a` and `b` lead to the same file or folder (one seen through a junction, a subst drive, a
/// mapped drive or a hard link). When the ids can't be read, the resolved paths decide.
fn same_object(a: &str, b: &str) -> bool {
    match (path_ident(a), path_ident(b)) {
        (Some(x), Some(y)) => x == y,
        _ => matches!((final_path(a), final_path(b)), (Some(x), Some(y)) if x == y),
    }
}

fn with_buf<R>(f: impl FnOnce(&mut [u8]) -> R) -> R {
    READ_BUF.with(|b| {
        let mut b = b.borrow_mut();
        if b.len() < BUF {
            b.resize(BUF, 0);
        }
        f(&mut b[..])
    })
}

struct Edge {
    ident: Option<Ident>,
    hash: u128,
    /// The file is small enough that `hash` covers all of it.
    whole: bool,
}

/// xxh3-128 of the first and last 64 KB (the whole file when it is at most 128 KB). None if the
/// file can't be read or no longer has the listed size.
fn edge_hash(path: &str, size: u64) -> Option<Edge> {
    let mut f = open_read(path).ok()?;
    if f.metadata().ok()?.len() != size {
        return None;
    }
    let ident = file_ident(&f);
    with_buf(|buf| {
        if size <= 2 * EDGE {
            let n = size as usize;
            f.read_exact(&mut buf[..n]).ok()?;
            Some(Edge { ident, hash: xxhash_rust::xxh3::xxh3_128(&buf[..n]), whole: true })
        } else {
            let e = EDGE as usize;
            f.read_exact(&mut buf[..e]).ok()?;
            f.seek(SeekFrom::Start(size - EDGE)).ok()?;
            f.read_exact(&mut buf[e..2 * e]).ok()?;
            Some(Edge { ident, hash: xxhash_rust::xxh3::xxh3_128(&buf[..2 * e]), whole: false })
        }
    })
}

/// xxh3-128 of the whole file, read sequentially in 1 MB pieces. None if cancelled, unreadable or
/// its length isn't `size` any more.
fn full_hash(path: &str, size: u64, cancel: &AtomicBool) -> Option<u128> {
    let mut f = open_read(path).ok()?;
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    let mut n = 0u64;
    with_buf(|buf| loop {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        match f.read(buf) {
            Ok(0) => break Some(()),
            Ok(k) => {
                h.update(&buf[..k]);
                n += k as u64;
                if n > size {
                    return None;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    })?;
    (n == size).then(|| h.digest128())
}

struct Exact {
    /// Groups of file indices with identical content (one entry per distinct file).
    groups: Vec<Vec<usize>>,
    /// Further hard links of a file already in a group: never duplicates of it.
    linked: HashSet<usize>,
    /// Lower-case path -> (size, full content hash), for the folder phase.
    cache: HashMap<String, (u64, u128)>,
}

fn exact_groups(l: &Listing, paths: &[String], prot: &[Option<&'static str>], ctx: &Ctx, hdd: bool) -> Option<Exact> {
    let mut idx: Vec<usize> = (0..l.files.len()).collect();
    idx.sort_unstable_by_key(|&i| l.files[i].size);
    let mut cands: Vec<usize> = Vec::new();
    for run in idx.chunk_by(|&a, &b| l.files[a].size == l.files[b].size) {
        // A size shared only by protected files can't lead to anything to delete: skip the reads.
        if run.len() >= 2 && run.iter().any(|&i| prot[l.files[i].dir].is_none()) {
            cands.extend_from_slice(run);
        }
    }
    let cpaths: Vec<String> = cands.iter().map(|&i| l.file_path(paths, i)).collect();
    let large = cands.iter().filter(|&&i| l.files[i].size > 2 * EDGE).count();
    ctx.progress.start_phase(PH_HASHING, (cands.len() + large) as u64);

    let pool = build_pool(if hdd { 2 } else { list_threads() }, "platter-dupes-hash")?;
    let edges: Vec<Option<Edge>> = pool.install(|| {
        (0..cands.len())
            .into_par_iter()
            .with_max_len(1)
            .map(|k| {
                if ctx.cancelled() {
                    return None;
                }
                ctx.progress.tick(&cpaths[k]);
                edge_hash(&cpaths[k], l.files[cands[k]].size)
            })
            .collect()
    });
    if ctx.cancelled() {
        return None;
    }

    let mut out = Exact { groups: Vec::new(), linked: HashSet::new(), cache: HashMap::new() };
    let mut by_edge: HashMap<(u64, u128), Vec<usize>> = HashMap::new();
    for (k, e) in edges.iter().enumerate() {
        if let Some(e) = e {
            by_edge.entry((l.files[cands[k]].size, e.hash)).or_default().push(k);
        }
    }
    // Same size + same ends. Drop extra hard links (the first path, in path order, stands for
    // the file), then confirm what's left with full hashes.
    let mut full_cands: Vec<usize> = Vec::new();
    let mut keys: Vec<_> = by_edge.into_iter().filter(|(_, v)| v.len() >= 2).collect();
    keys.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    for (_, mut ks) in keys {
        ks.sort_by(|&a, &b| cpaths[a].cmp(&cpaths[b]));
        let mut ids: HashSet<Ident> = HashSet::new();
        let mut keep: Vec<usize> = Vec::new();
        for k in ks {
            if let Some(id) = edges[k].as_ref().and_then(|e| e.ident) {
                if !ids.insert(id) {
                    out.linked.insert(cands[k]);
                    continue;
                }
            }
            keep.push(k);
        }
        if keep.len() < 2 {
            continue;
        }
        if edges[keep[0]].as_ref().is_some_and(|e| e.whole) {
            for &k in &keep {
                let e = edges[k].as_ref().unwrap();
                out.cache.insert(lower(&cpaths[k]), (l.files[cands[k]].size, e.hash));
            }
            out.groups.push(keep.iter().map(|&k| cands[k]).collect());
        } else {
            full_cands.extend(keep);
        }
    }

    // The bar's total shrinks from "every large candidate" to the ones that still need a full read.
    ctx.progress.total.store((cands.len() + full_cands.len()) as u64, Ordering::Relaxed);
    ctx.progress.done.store(cands.len() as u64, Ordering::Relaxed);
    let pool = build_pool(full_hash_threads(hdd), "platter-dupes-full")?;
    let fulls: Vec<Option<u128>> = pool.install(|| {
        full_cands
            .par_iter()
            .with_max_len(1)
            .map(|&k| {
                if ctx.cancelled() {
                    return None;
                }
                ctx.progress.tick(&cpaths[k]);
                full_hash(&cpaths[k], l.files[cands[k]].size, &ctx.cancel)
            })
            .collect()
    });
    if ctx.cancelled() {
        return None;
    }
    let mut by_full: HashMap<(u64, u128), Vec<usize>> = HashMap::new();
    for (j, h) in fulls.iter().enumerate() {
        if let Some(h) = h {
            let k = full_cands[j];
            let size = l.files[cands[k]].size;
            out.cache.insert(lower(&cpaths[k]), (size, *h));
            by_full.entry((size, *h)).or_default().push(cands[k]);
        }
    }
    let mut more: Vec<Vec<usize>> = by_full.into_values().filter(|g| g.len() >= 2).collect();
    more.sort_unstable();
    out.groups.extend(more);
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Duplicate folders

/// One file of a folder copy: path, size, modified time (ms).
type FolderFile = (String, u64, u64);

/// Every file under `dir`, with the listing's rules (hidden system files count, Explorer metadata
/// files don't). None if anything in it can't be seen (a link, a cloud-only item, a hidden system
/// folder, a denied folder): such folders are never claimed equal.
fn folder_files(dir: &str, cancel: &AtomicBool) -> Option<Vec<FolderFile>> {
    let mut out = Vec::new();
    let mut stack: Vec<Vec<u16>> = vec![dir.encode_utf16().collect()];
    while let Some(d) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let mut bad = false;
        let ds = String::from_utf16_lossy(&d);
        let r = list_dir(&d, |e| {
            let hidden_system = e.attrs & (ATTR_HIDDEN | ATTR_SYSTEM) == ATTR_HIDDEN | ATTR_SYSTEM;
            let is_dir = e.attrs & ATTR_DIRECTORY != 0;
            if (e.attrs & ATTR_REPARSE_POINT != 0 && e.tag & NAME_SURROGATE != 0) || e.attrs & CLOUD_ATTRS != 0 || (is_dir && hidden_system) {
                bad = true;
                return;
            }
            let name = String::from_utf16_lossy(e.name);
            if is_dir {
                stack.push(join_path(&ds, &name).encode_utf16().collect());
            } else if !is_meta_name(&name.to_lowercase()) {
                out.push((join_path(&ds, &name), e.size, e.modified));
            }
        });
        if r.is_err() || bad {
            return None;
        }
    }
    Some(out)
}

/// What a folder copy held when the scan confirmed it: checked again before it is recycled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FolderPrint {
    files: u64,
    bytes: u64,
    /// xxh3-128 over the sorted (relative lower-case path, size, modified) of every file.
    sig: u128,
}

fn folder_print(dir: &str, files: &[FolderFile]) -> FolderPrint {
    let skip = dir.trim_end_matches('\\').len();
    let mut rows: Vec<(String, u64, u64)> =
        files.iter().map(|(p, s, m)| (lower(p.get(skip..).unwrap_or(p)), *s, *m)).collect();
    rows.sort_unstable();
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    for (p, s, m) in &rows {
        h.update(p.as_bytes());
        h.update(&[0]);
        h.update(&s.to_le_bytes());
        h.update(&m.to_le_bytes());
    }
    FolderPrint { files: rows.len() as u64, bytes: rows.iter().map(|r| r.1).sum(), sig: h.digest128() }
}

/// The folder's contents now, in the same form (None if it can't be fully seen any more).
fn folder_print_now(dir: &str) -> Option<FolderPrint> {
    folder_files(dir, &AtomicBool::new(false)).map(|f| folder_print(dir, &f))
}

/// Confirmed duplicate folders: groups of folder ids (outermost copies only) and what each copy
/// held.
struct Folders {
    groups: Vec<Vec<usize>>,
    prints: HashMap<usize, FolderPrint>,
}

/// Groups of folder ids (outermost copies only) whose contents are byte-identical.
fn folder_groups(
    l: &Listing,
    paths: &[String],
    cache: &mut HashMap<String, (u64, u128)>,
    ctx: &Ctx,
    hdd: bool,
) -> Option<Folders> {
    let mut by_key: HashMap<(u64, u64, u64), Vec<usize>> = HashMap::new();
    for (id, d) in l.dirs.iter().enumerate().skip(l.roots) {
        if !d.incomplete && d.files >= 2 && d.bytes >= MIN_FOLDER_BYTES {
            by_key.entry((d.files, d.bytes, d.sig)).or_default().push(id);
        }
    }
    let mut cands: Vec<Vec<usize>> = Vec::new();
    for (_, mut g) in by_key {
        if g.len() < 2 {
            continue;
        }
        // A folder holding nothing but one sub-folder has that sub-folder's contents: the two
        // aren't copies of each other. Keep the outer one.
        let set: HashSet<usize> = g.iter().copied().collect();
        g.retain(|&d| !l.ancestors(d).any(|a| set.contains(&a)));
        if g.len() >= 2 {
            g.sort_unstable();
            cands.push(g);
        }
    }
    // Biggest first: outer folders are confirmed before their sub-folders.
    cands.sort_by(|a, b| l.dirs[b[0]].bytes.cmp(&l.dirs[a[0]].bytes).then(a[0].cmp(&b[0])));
    let total: u64 = cands.iter().map(|g| g.len() as u64 * l.dirs[g[0]].files).sum();
    ctx.progress.start_phase(PH_FOLDERS, total);

    let pool = build_pool(full_hash_threads(hdd), "platter-dupes-folders")?;
    let mut covered: HashSet<usize> = HashSet::new();
    let mut confirmed: Vec<Vec<usize>> = Vec::new();
    let mut prints: HashMap<usize, FolderPrint> = HashMap::new();
    for g in cands {
        if ctx.cancelled() {
            return None;
        }
        // Sub-folders of copies already reported are reported through them.
        let live: Vec<usize> =
            g.into_iter().filter(|&d| !covered.contains(&d) && !l.ancestors(d).any(|a| covered.contains(&a))).collect();
        if live.len() < 2 {
            continue;
        }
        let mut by_sig: HashMap<u128, Vec<usize>> = HashMap::new();
        let mut seen_dirs: HashSet<Ident> = HashSet::new();
        let mut file_ids: HashMap<usize, Vec<Ident>> = HashMap::new();
        let mut print_of: HashMap<usize, FolderPrint> = HashMap::new();
        for &d in &live {
            // One folder reached through two paths (a junction or subst root next to its target,
            // a mapped drive pointing back at this PC) is one folder, never two copies. The
            // alias's sub-folders are the same folders too, so they are skipped with it.
            if let Some(id) = path_ident(&paths[d]) {
                if !seen_dirs.insert(id) {
                    covered.insert(d);
                    continue;
                }
            }
            let Some(files) = folder_files(&paths[d], &ctx.cancel) else { continue };
            if files.len() as u64 != l.dirs[d].files {
                continue; // changed since the listing
            }
            let hashes: Vec<Option<((u64, u128), Option<Ident>)>> = pool.install(|| {
                files
                    .par_iter()
                    .with_max_len(1)
                    .map(|(p, size, _)| {
                        if ctx.cancelled() {
                            return None;
                        }
                        ctx.progress.tick(p);
                        let ident = path_ident(p);
                        if let Some(&(s, h)) = cache.get(&lower(p)) {
                            if s == *size {
                                return Some(((s, h), ident));
                            }
                        }
                        full_hash(p, *size, &ctx.cancel).map(|h| ((*size, h), ident))
                    })
                    .collect()
            });
            if ctx.cancelled() {
                return None;
            }
            if hashes.iter().any(|h| h.is_none()) {
                continue;
            }
            let (mut pairs, ids): (Vec<(u64, u128)>, Vec<Option<Ident>>) = hashes.into_iter().flatten().unzip();
            for ((p, _, _), (s, h)) in files.iter().zip(pairs.iter()) {
                cache.entry(lower(p)).or_insert((*s, *h));
            }
            file_ids.insert(d, ids.into_iter().flatten().collect());
            print_of.insert(d, folder_print(&paths[d], &files));
            // Names may differ: the signature is over the sorted multiset of contents only.
            pairs.sort_unstable();
            let mut hasher = xxhash_rust::xxh3::Xxh3::new();
            for (s, h) in &pairs {
                hasher.update(&s.to_le_bytes());
                hasher.update(&h.to_le_bytes());
            }
            by_sig.entry(hasher.digest128()).or_default().push(d);
        }
        let mut found: Vec<Vec<usize>> = by_sig
            .into_values()
            .map(|v| {
                // A copy whose files are hard links of another copy's files frees nothing and
                // isn't a second copy: the first one (in id order) stands for them.
                let mut ids: HashSet<Ident> = HashSet::new();
                let mut keep: Vec<usize> = Vec::new();
                for d in v {
                    let fids = &file_ids[&d];
                    if fids.iter().any(|i| ids.contains(i)) {
                        continue;
                    }
                    ids.extend(fids.iter().copied());
                    keep.push(d);
                }
                keep
            })
            .filter(|v| v.len() >= 2)
            .collect();
        found.sort_unstable();
        for v in found {
            covered.extend(v.iter().copied());
            for &d in &v {
                prints.insert(d, print_of[&d]);
            }
            confirmed.push(v);
        }
    }
    Some(Folders { groups: confirmed, prints })
}

// ---------------------------------------------------------------------------------------------
// Perceptual hashes (dHash) for similar images and videos

#[derive(Clone, Copy, Debug, PartialEq)]
struct Sig {
    /// Horizontal dHash: 8 rows x 8 left>right comparisons of a 9x8 grey grid.
    h: u64,
    /// Vertical dHash: 8 columns x 8 top>bottom comparisons of an 8x9 grey grid.
    v: u64,
    /// Mean brightness of the grid (0-255).
    mean: f32,
    /// Width / height of the source picture.
    aspect: f32,
    /// 16x16 grey grid: the check that tells apart pictures whose dHash is sparse (mostly white
    /// signatures, icons on transparency, paper textures all hash close to 0).
    grid: [u8; 256],
    /// Mean colour (R, G, B): colour variants of one mock-up share every grey gradient.
    rgb: [u8; 3],
    /// 16x16 cells more than 16 levels away from the median cell: how much there is to compare.
    detail: u16,
}

/// Box-filtered `cols`x`rows` grid of a grey image.
fn grey_grid(grey: &[u8], w: usize, h: usize, cols: usize, rows: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(cols * rows);
    for r in 0..rows {
        let y0 = (r * h / rows).min(h - 1);
        let y1 = ((r + 1) * h / rows).clamp(y0 + 1, h);
        for c in 0..cols {
            let x0 = (c * w / cols).min(w - 1);
            let x1 = ((c + 1) * w / cols).clamp(x0 + 1, w);
            let mut sum = 0u64;
            for y in y0..y1 {
                for x in x0..x1 {
                    sum += grey[y * w + x] as u64;
                }
            }
            out.push(sum as f32 / ((y1 - y0) * (x1 - x0)) as f32);
        }
    }
    out
}

/// dHash signature of a grey image (`w`x`h`, row-major).
fn sig_from_grey(grey: &[u8], w: usize, h: usize) -> Option<Sig> {
    if w == 0 || h == 0 || grey.len() < w * h {
        return None;
    }
    let gh = grey_grid(grey, w, h, 9, 8);
    let gv = grey_grid(grey, w, h, 8, 9);
    let mut hbits = 0u64;
    let mut vbits = 0u64;
    for r in 0..8 {
        for c in 0..8 {
            let bit = r * 8 + c;
            if gh[r * 9 + c] > gh[r * 9 + c + 1] {
                hbits |= 1 << bit;
            }
            if gv[r * 8 + c] > gv[(r + 1) * 8 + c] {
                vbits |= 1 << bit;
            }
        }
    }
    let mean = gh.iter().sum::<f32>() / gh.len() as f32;
    let mut grid = [0u8; 256];
    for (g, x) in grid.iter_mut().zip(grey_grid(grey, w, h, 16, 16)) {
        *g = x.round().clamp(0.0, 255.0) as u8;
    }
    let m = mean.round().clamp(0.0, 255.0) as u8;
    let mut sorted = grid;
    sorted.sort_unstable();
    let median = sorted[128];
    let detail = grid.iter().filter(|&&g| g.abs_diff(median) > 16).count() as u16;
    Some(Sig { h: hbits, v: vbits, mean, aspect: w as f32 / h as f32, grid, rgb: [m, m, m], detail })
}

/// dHash signature of a Shell bitmap, with its real mean colour.
fn sig_from_pixels(p: &shell::Pixels) -> Option<Sig> {
    let mut sig = sig_from_grey(&grey_of(p), p.w as usize, p.h as usize)?;
    let has_alpha = p.bgra.chunks_exact(4).any(|px| px[3] != 0);
    let mut acc = [0u64; 3];
    for px in p.bgra.chunks_exact(4) {
        let white = if has_alpha { 255 - px[3] as u64 } else { 0 };
        for c in 0..3 {
            acc[c] += (px[2 - c] as u64 + white).min(255);
        }
    }
    let n = (p.bgra.len() / 4).max(1) as u64;
    sig.rgb = [(acc[0] / n) as u8, (acc[1] / n) as u8, (acc[2] / n) as u8];
    Some(sig)
}

/// Copies of one picture (resized, re-encoded, lightly edited) stay close in every cell of the
/// 16x16 grid and in mean colour; different pictures differ strongly somewhere. Measured on this
/// PC's pictures (2026-11-24, `spike_similar_real`): with dHash alone, mostly-white signatures,
/// icons and paper textures paired up at distance 5-6, and red/navy variants of one T-shirt
/// mock-up at 0-1.
fn looks_same(a: &Sig, b: &Sig, video: bool) -> bool {
    // Video frames of two encodes differ more (different frame timing, compression).
    let (cell, max_off) = if video { (40, 26) } else { (28, 6) };
    let off = a.grid.iter().zip(b.grid.iter()).filter(|(x, y)| x.abs_diff(**y) > cell).count();
    let rgb_ok = a.rgb.iter().zip(b.rgb.iter()).all(|(x, y)| x.abs_diff(*y) <= RGB_MAX_DIFF);
    off <= max_off && rgb_ok
}

/// Grey levels of a Shell bitmap. The Shell hands out premultiplied BGRA; transparent parts are
/// composited onto white. Bitmaps without any alpha (all 0) are opaque.
fn grey_of(p: &shell::Pixels) -> Vec<u8> {
    let has_alpha = p.bgra.chunks_exact(4).any(|px| px[3] != 0);
    p.bgra
        .chunks_exact(4)
        .map(|px| {
            let a = if has_alpha { px[3] as u32 } else { 255 };
            let lum = (px[2] as u32 * 299 + px[1] as u32 * 587 + px[0] as u32 * 114) / 1000;
            (lum + (255 - a)).min(255) as u8
        })
        .collect()
}

fn dist(a: &Sig, b: &Sig) -> u32 {
    (a.h ^ b.h).count_ones().max((a.v ^ b.v).count_ones())
}

fn similarity(d: u32) -> u8 {
    (100.0 * (1.0 - d as f64 / 64.0)).round().clamp(0.0, 100.0) as u8
}

/// What the similar phase knows about one candidate.
#[derive(Clone, Copy, Debug)]
struct Media {
    sig: Sig,
    duration_ms: Option<u64>,
}

/// Distance between two candidates if they count as similar.
fn accept(a: &Media, b: &Media, video: bool) -> Option<u32> {
    let d = dist(&a.sig, &b.sig);
    // The cheap test first: most pairs sharing a bucket are far apart, and `looks_same` compares
    // 256 cells.
    let most = if video { VIDEO_MAX_DIST } else { IMAGE_MAX_DIST };
    if d > most {
        return None;
    }
    if (a.sig.mean - b.sig.mean).abs() > MEAN_MAX_DIFF {
        return None;
    }
    let ar = a.sig.aspect.max(b.sig.aspect) / a.sig.aspect.min(b.sig.aspect).max(1e-3);
    if ar > ASPECT_MAX_RATIO || !looks_same(&a.sig, &b.sig, video) {
        return None;
    }
    if !video {
        return (d <= IMAGE_MAX_DIST).then_some(d);
    }
    match (a.duration_ms, b.duration_ms) {
        (Some(x), Some(y)) => {
            let tol = 2000u64.max(x.max(y) / 100);
            (x.abs_diff(y) <= tol && d <= VIDEO_MAX_DIST).then_some(d)
        }
        // Without both durations only near-identical frames count.
        _ => (d <= IMAGE_MAX_DIST).then_some(d),
    }
}

/// Bit ranges for multi-index hashing: with at most `t` differing bits, splitting 64 bits into
/// `t + 1` chunks guarantees (pigeonhole) that one chunk is identical, so only items sharing a
/// chunk value need comparing.
fn chunks(t: u32) -> Vec<(u32, u32)> {
    let k = (t + 1).clamp(1, 64);
    let (base, extra) = (64 / k, 64 % k);
    let mut out = Vec::new();
    let mut shift = 0;
    for i in 0..k {
        let bits = base + u32::from(i < extra);
        out.push((shift, bits));
        shift += bits;
    }
    out
}

fn chunk_val(h: u64, (shift, bits): (u32, u32)) -> u64 {
    if bits >= 64 {
        h
    } else {
        (h >> shift) & ((1u64 << bits) - 1)
    }
}

/// Accepted pairs (i < j, distance) among `items`, found through the horizontal hash's buckets.
/// None if cancelled (checked per bucket and every 1,024 rows of a big one).
fn similar_pairs(items: &[Media], video: bool, cancel: &AtomicBool) -> Option<Vec<(usize, usize, u32)>> {
    let t = if video { VIDEO_MAX_DIST } else { IMAGE_MAX_DIST };
    let cs = chunks(t);
    let mut out = Vec::new();
    for (ci, &c) in cs.iter().enumerate() {
        let mut buckets: HashMap<u64, Vec<usize>> = HashMap::new();
        for (i, m) in items.iter().enumerate() {
            buckets.entry(chunk_val(m.sig.h, c)).or_default().push(i);
        }
        for b in buckets.values().filter(|b| b.len() >= 2) {
            for x in 0..b.len() {
                if x % 1024 == 0 && cancel.load(Ordering::Relaxed) {
                    return None;
                }
                for y in x + 1..b.len() {
                    let (i, j) = (b[x], b[y]);
                    // Count each pair once: in the first chunk where the two agree.
                    if cs[..ci].iter().any(|&e| chunk_val(items[i].sig.h, e) == chunk_val(items[j].sig.h, e)) {
                        continue;
                    }
                    if let Some(d) = accept(&items[i], &items[j], video) {
                        out.push((i.min(j), i.max(j), d));
                    }
                }
            }
        }
    }
    out.sort_unstable();
    Some(out)
}

fn uf_find(p: &mut [usize], mut x: usize) -> usize {
    while p[x] != x {
        p[x] = p[p[x]];
        x = p[x];
    }
    x
}

/// Groups (keeper first) from accepted pairs: union-find components, then within each component
/// the best remaining keeper (by `better`) takes every remaining item within the threshold of
/// itself. Every member is therefore within the threshold of its keeper.
fn cluster(n: usize, pairs: &[(usize, usize, u32)], better: impl Fn(usize, usize) -> Cmp) -> Vec<Vec<usize>> {
    let mut parent: Vec<usize> = (0..n).collect();
    let mut adj: HashMap<usize, HashSet<usize>> = HashMap::new();
    for &(i, j, _) in pairs {
        let (a, b) = (uf_find(&mut parent, i), uf_find(&mut parent, j));
        if a != b {
            parent[a] = b;
        }
        adj.entry(i).or_default().insert(j);
        adj.entry(j).or_default().insert(i);
    }
    let mut comps: HashMap<usize, Vec<usize>> = HashMap::new();
    for &i in adj.keys() {
        let r = uf_find(&mut parent, i);
        comps.entry(r).or_default().push(i);
    }
    let mut comps: Vec<Vec<usize>> = comps.into_values().collect();
    for c in comps.iter_mut() {
        c.sort_unstable();
    }
    comps.sort_unstable();
    let mut groups = Vec::new();
    for mut comp in comps {
        comp.sort_by(|&a, &b| better(a, b));
        let mut taken: HashSet<usize> = HashSet::new();
        for &k in &comp {
            if taken.contains(&k) {
                continue;
            }
            let mut g: Vec<usize> = adj[&k].iter().copied().filter(|x| !taken.contains(x)).collect();
            if g.is_empty() {
                continue;
            }
            g.sort_by(|&a, &b| better(a, b));
            taken.insert(k);
            taken.extend(g.iter().copied());
            g.insert(0, k);
            groups.push(g);
        }
    }
    groups
}

/// Runs `f(i)` for `0..n` on `threads` fresh STA threads (Shell thumbnail providers are written
/// for STA; see THUMB_THREADS).
fn on_sta_threads<T: Send>(n: usize, threads: usize, ctx: &Ctx, f: impl Fn(usize) -> Option<T> + Sync) -> Vec<Option<T>> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<T>>> = Mutex::new((0..n).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..threads.min(n).max(1) {
            s.spawn(|| {
                let _com = shell::Apartment::enter(true);
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n || ctx.cancelled() {
                        break;
                    }
                    let r = f(i);
                    out.lock().unwrap()[i] = r;
                }
            });
        }
    });
    out.into_inner().unwrap_or_default()
}

/// Thumbnail signature of one file (64 px, the Shell's cached size when it has one).
fn media_sig(path: &str, video: bool) -> Option<Media> {
    let px = shell::image(path, 64, shell::SIIGBF_THUMBNAILONLY | shell::SIIGBF_BIGGERSIZEOK).ok()?;
    let sig = sig_from_pixels(&px)?;
    if sig.detail < MIN_DETAIL {
        // A near-blank picture (white icon on transparency, a signature, a fade-in frame): every
        // such thumbnail looks like every other, so it says nothing about which picture it is.
        return None;
    }
    let duration_ms = if video { shell::props(path, true).and_then(|p| p.duration_ms) } else { None };
    Some(Media { sig, duration_ms })
}

// ---------------------------------------------------------------------------------------------
// Stored result and recommendations

#[derive(Clone, Debug)]
struct Member {
    path: String,
    lower: String,
    name: String,
    folder: String,
    size: u64,
    modified: u64,
    prot: Option<&'static str>,
    place: Place,
    copy_name: bool,
    width: u32,
    height: u32,
    duration_ms: Option<u64>,
    sig: Option<Sig>,
    /// Folder copies: this is a folder.
    is_dir: bool,
    /// Folder copies: what the folder held when the scan confirmed it.
    print: Option<FolderPrint>,
}

impl Member {
    #[allow(clippy::too_many_arguments)]
    fn new(path: String, name: &str, folder: &str, size: u64, modified: u64, prot: Option<&'static str>, places: &Places, is_dir: bool) -> Member {
        let lower = lower(&path);
        Member {
            place: places.classify(&lower),
            copy_name: looks_like_copy(name),
            path,
            lower,
            name: name.to_string(),
            folder: folder.to_string(),
            size,
            modified,
            prot,
            width: 0,
            height: 0,
            duration_ms: None,
            sig: None,
            is_dir,
            print: None,
        }
    }

    fn rank(&self) -> u8 {
        if self.prot.is_some() {
            return 0;
        }
        match self.place {
            Place::Library(_) => 1,
            Place::Desktop => 2,
            Place::Other => 3,
            Place::Downloads | Place::Temp => 4,
        }
    }

    fn pixels(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    fn where_(&self) -> String {
        match self.place {
            Place::Library(l) => format!("in {l}"),
            Place::Desktop => "on the Desktop".into(),
            Place::Downloads => "in Downloads".into(),
            Place::Temp => "in a temporary folder".into(),
            Place::Other => {
                let f = self.folder.trim_end_matches('\\');
                match f.rsplit('\\').next() {
                    Some(n) if !n.is_empty() && !n.ends_with(':') => format!("in {n}"),
                    _ => format!("on {}", f),
                }
            }
        }
    }
}

/// CONTRACT keeper order: protected > user library > Desktop > other > Downloads/Temp; then a name
/// that doesn't look like a copy; then (similar groups only) the most pixels / bytes; then the
/// oldest; then the shortest path.
fn keeper_cmp(a: &Member, b: &Member, similar: bool) -> Cmp {
    a.rank()
        .cmp(&b.rank())
        .then(a.copy_name.cmp(&b.copy_name))
        .then(if similar { b.pixels().cmp(&a.pixels()).then(b.size.cmp(&a.size)) } else { Cmp::Equal })
        .then(a.modified.cmp(&b.modified))
        .then(a.path.len().cmp(&b.path.len()))
        .then(a.lower.cmp(&b.lower))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Exact,
    Folder,
    Image,
    Video,
}

#[derive(Clone, Debug)]
struct IGroup {
    id: String,
    kind: Kind,
    /// Folder groups: files per copy.
    files: u64,
    members: Vec<Member>,
}

struct Stored {
    scan_id: u64,
    roots: Vec<String>,
    files: u64,
    bytes: u64,
    skipped: Skipped,
    groups: Vec<IGroup>,
    folders: Vec<IGroup>,
    images: Vec<IGroup>,
    videos: Vec<IGroup>,
}

impl Stored {
    fn all_groups(&self) -> impl Iterator<Item = &IGroup> {
        self.groups.iter().chain(&self.folders).chain(&self.images).chain(&self.videos)
    }
    fn find(&self, lower_path: &str) -> Option<&Member> {
        self.all_groups().flat_map(|g| g.members.iter()).find(|m| m.lower == lower_path)
    }
}

/// Duplicate-folder copies (lower-case) the recommendations remove, and the ones they leave.
#[derive(Default)]
struct FolderPicks {
    /// Folder copy (lower-case) -> 2 when recommended for removal, 0 when it stays. A map looked up
    /// by each path's ancestors keeps `tier` O(depth): scanning every folder copy per comparison
    /// was O(groups x folder copies) and froze big drive scans after the folders phase.
    tiers: HashMap<String, u8>,
}

impl FolderPicks {
    fn of(folders: &[IGroup]) -> FolderPicks {
        let mut p = FolderPicks::default();
        for g in folders {
            for (m, f, _) in recommend(g) {
                let t = if f.select { 2 } else { 0 };
                let e = p.tiers.entry(m.lower).or_insert(t);
                *e = (*e).max(t); // the same folder both ways: removal wins, as before
            }
        }
        p
    }

    /// 0: inside a folder copy that stays, 1: outside every folder copy, 2: inside a folder copy
    /// that is recommended for removal (it goes with that folder).
    fn tier(&self, lower_path: &str) -> u8 {
        if self.tiers.is_empty() {
            return 1;
        }
        // Same answer as checking `inside(path, folder)` for every folder copy: any removed
        // ancestor (or the path itself) -> 2, else any kept one -> 0, else 1.
        let mut kept = false;
        let mut s = lower_path;
        loop {
            match self.tiers.get(s) {
                Some(2) => return 2,
                Some(_) => kept = true,
                None => {}
            }
            let Some(i) = s.rfind('\\') else { break };
            if i + 1 == s.len() {
                break; // a drive root ("c:\"): nothing above it
            }
            // "c:\x" -> "c:\" (drive roots keep their backslash, like norm_lower); "c:\x\y" -> "c:\x"
            s = if i == 2 && s.as_bytes().get(1) == Some(&b':') { &s[..3] } else { &s[..i] };
        }
        if kept { 0 } else { 1 }
    }
}

/// Recommendations for one group: members in keeper order with keep/select/risk/note.
fn recommend(g: &IGroup) -> Vec<(Member, DupFile, u8)> {
    recommend_in(g, &FolderPicks::default())
}

/// `recommend` for an exact group that may overlap duplicate-folder groups: each group is ranked
/// on its own, so without this the folder keeper and the file keeper could be different copies
/// and the recommended selection would take every copy of a file (one with its folder, the rest
/// one by one), which `dupes_delete` refuses. The keeper is taken from a folder copy that stays,
/// never from one that goes.
fn recommend_in(g: &IGroup, picks: &FolderPicks) -> Vec<(Member, DupFile, u8)> {
    let similar = matches!(g.kind, Kind::Image | Kind::Video);
    let mut ms = g.members.clone();
    if g.kind == Kind::Exact {
        ms.sort_by(|a, b| picks.tier(&a.lower).cmp(&picks.tier(&b.lower)).then(keeper_cmp(a, b, similar)));
    } else {
        ms.sort_by(|a, b| keeper_cmp(a, b, similar));
    }
    let k = ms[0].clone();
    let others = &ms[1..];
    let mut out = Vec::with_capacity(ms.len());
    for (i, m) in ms.iter().enumerate() {
        let keep = i == 0;
        let same_folder = !keep && m.folder.to_lowercase() == k.folder.to_lowercase();
        let risk = if m.prot.is_some() {
            "protected"
        } else if matches!(m.place, Place::Downloads | Place::Temp) || m.copy_name || same_folder {
            "safe"
        } else {
            "review"
        };
        let select = !similar && !keep && risk != "protected";
        let d = match (&m.sig, &k.sig) {
            (Some(a), Some(b)) => dist(a, b),
            _ => 0,
        };
        let sim = if keep { 100 } else { similarity(d) };
        let note = if let Some(p) = m.prot {
            p.to_string()
        } else if keep {
            let lead = if similar && m.pixels() > 0 && others.iter().all(|o| m.pixels() > o.pixels()) {
                "Highest resolution"
            } else if others.iter().all(|o| m.modified < o.modified) {
                "Oldest copy"
            } else if !m.copy_name && others.iter().any(|o| o.copy_name) {
                "Original name"
            } else {
                "Copy to keep"
            };
            format!("{lead}, {}", m.where_())
        } else if similar {
            if m.pixels() > 0 && m.pixels() < k.pixels() {
                format!("Smaller version, {}", m.where_())
            } else if g.kind == Kind::Video {
                format!("Similar video, {}", m.where_())
            } else {
                format!("Looks alike, {}", m.where_())
            }
        } else if m.place == Place::Downloads {
            "Copy in Downloads".into()
        } else if m.place == Place::Temp {
            "Copy in a temporary folder".into()
        } else if m.copy_name {
            "Named like a copy".into()
        } else if same_folder {
            "Same folder as the copy kept".into()
        } else if m.modified > k.modified {
            format!("Newer copy, {}", m.where_())
        } else {
            format!("Another copy, {}", m.where_())
        };
        let f = DupFile {
            path: m.path.clone(),
            name: m.name.clone(),
            folder: m.folder.clone(),
            size: m.size,
            modified: m.modified,
            keep,
            select,
            risk,
            note,
        };
        out.push((m.clone(), f, sim));
    }
    out
}

fn render(st: &Stored) -> DupesResult {
    let mut selected_folders: Vec<String> = Vec::new();
    let mut folders: Vec<FolderGroup> = st
        .folders
        .iter()
        .map(|g| {
            let rec = recommend(g);
            let size = rec[0].0.size;
            let mut reclaimable = 0;
            for (m, f, _) in &rec {
                if f.select {
                    reclaimable += m.size;
                    selected_folders.push(m.lower.clone());
                }
            }
            FolderGroup { id: g.id.clone(), size, files: g.files, reclaimable, folders: rec.into_iter().map(|r| r.1).collect() }
        })
        .collect();
    let mut total: u64 = folders.iter().map(|f| f.reclaimable).sum();
    let picks = FolderPicks::of(&st.folders);
    let mut groups: Vec<DupGroup> = st
        .groups
        .iter()
        .map(|g| {
            let rec = recommend_in(g, &picks);
            let size = rec[0].0.size;
            let mut reclaimable = 0;
            for (m, f, _) in &rec {
                if f.select {
                    reclaimable += m.size;
                    // Counted once: a file inside a selected folder copy is already in `total`.
                    if !selected_folders.iter().any(|d| inside(&m.lower, d)) {
                        total += m.size;
                    }
                }
            }
            DupGroup { id: g.id.clone(), cat: category_of(&rec[0].0.name), size, reclaimable, files: rec.into_iter().map(|r| r.1).collect() }
        })
        .collect();
    groups.sort_by(|a, b| b.reclaimable.cmp(&a.reclaimable));
    folders.sort_by(|a, b| b.reclaimable.cmp(&a.reclaimable));
    let similar = |gs: &[IGroup], kind: &'static str| -> Vec<SimilarGroup> {
        gs.iter()
            .map(|g| SimilarGroup {
                id: g.id.clone(),
                kind,
                files: recommend(g)
                    .into_iter()
                    .map(|(m, f, sim)| SimilarFile { file: f, width: m.width, height: m.height, duration_ms: m.duration_ms, similarity: sim })
                    .collect(),
            })
            .collect()
    };
    DupesResult {
        scan_id: st.scan_id,
        roots: st.roots.clone(),
        files: st.files,
        bytes: st.bytes,
        skipped: st.skipped.clone(),
        reclaimable: total,
        groups,
        folders,
        images: similar(&st.images, "image"),
        videos: similar(&st.videos, "video"),
    }
}

// ---------------------------------------------------------------------------------------------
// The scan

fn scan(roots: &[String], opts: &DupesOptions, places: &Places, ctx: &Ctx) -> Option<Stored> {
    let l = list_all(roots, opts.min_size, ctx)?;
    let paths = l.dir_paths();
    let prot = l.protection(&paths, places);
    let hdd = roots.iter().any(|r| incurs_seek_penalty(r));

    let mut ex = exact_groups(&l, &paths, &prot, ctx, hdd)?;
    let fgroups = if opts.folders {
        folder_groups(&l, &paths, &mut ex.cache, ctx, hdd)?
    } else {
        Folders { groups: Vec::new(), prints: HashMap::new() }
    };

    let file_member = |i: usize| -> Member {
        let f = &l.files[i];
        let folder = &paths[f.dir];
        Member::new(join_path(folder, &f.name), &f.name, folder, f.size, f.modified, prot[f.dir], places, false)
    };

    // Folder groups (drop ones where every copy is protected: nothing to act on).
    let mut folders: Vec<IGroup> = Vec::new();
    let mut folder_of_dir: HashMap<usize, usize> = HashMap::new();
    for g in &fgroups.groups {
        if g.iter().all(|&d| prot[d].is_some()) {
            continue;
        }
        let gi = folders.len();
        let members = g
            .iter()
            .map(|&d| {
                folder_of_dir.insert(d, gi);
                let dr = &l.dirs[d];
                let parent = if dr.parent == usize::MAX { String::new() } else { paths[dr.parent].clone() };
                let mut m = Member::new(paths[d].clone(), &dr.name, &parent, dr.bytes, dr.modified, prot[d], places, true);
                m.print = fgroups.prints.get(&d).copied();
                m
            })
            .collect();
        folders.push(IGroup { id: format!("f{}", gi + 1), kind: Kind::Folder, files: l.dirs[g[0]].files, members });
    }
    let picks = FolderPicks::of(&folders);

    // Exact groups, minus the ones a duplicate-folder group already covers and the all-protected.
    let mut groups: Vec<IGroup> = Vec::new();
    let mut exact_non_keepers: HashSet<String> = HashSet::new();
    for g in &ex.groups {
        if g.iter().all(|&i| prot[l.files[i].dir].is_some()) {
            continue;
        }
        let ig = IGroup { id: String::new(), kind: Kind::Exact, files: 0, members: g.iter().map(|&i| file_member(i)).collect() };
        // Similar groups take one copy of each identical set (its keeper), so they never pair
        // byte-identical files.
        for (n, (m, _, _)) in recommend_in(&ig, &picks).into_iter().enumerate() {
            if n > 0 {
                exact_non_keepers.insert(m.lower);
            }
        }
        if !folders.is_empty() {
            let mut common: Option<HashSet<usize>> = None;
            for &i in g {
                let d = l.files[i].dir;
                let hits: HashSet<usize> =
                    std::iter::once(d).chain(l.ancestors(d)).filter_map(|a| folder_of_dir.get(&a).copied()).collect();
                common = Some(match common {
                    None => hits,
                    Some(c) => c.intersection(&hits).copied().collect(),
                });
            }
            if common.is_some_and(|c| !c.is_empty()) {
                continue;
            }
        }
        groups.push(ig);
    }
    // Largest savings first; cap the list (see MAX_GROUPS).
    let mut scored: Vec<(u64, IGroup)> =
        groups.into_iter().map(|g| (recommend_in(&g, &picks).iter().filter(|r| r.1.select).map(|r| r.0.size).sum(), g)).collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.truncate(MAX_GROUPS);
    let groups: Vec<IGroup> =
        scored.into_iter().enumerate().map(|(n, (_, mut g))| {
            g.id = format!("g{}", n + 1);
            g
        }).collect();

    // Similar pictures and videos.
    let mut images = Vec::new();
    let mut videos = Vec::new();
    for (video, wanted) in [(false, opts.similar_images), (true, opts.similar_videos)] {
        if !wanted {
            continue;
        }
        let exts = if video { VIDEO_EXTS } else { IMAGE_EXTS };
        let min = if video { opts.min_size.max(1) } else { opts.min_size.max(MIN_IMAGE_BYTES) };
        let cands: Vec<usize> = (0..l.files.len())
            .filter(|&i| {
                let f = &l.files[i];
                f.size >= min && exts.contains(&ext_of(&f.name).as_str()) && !ex.linked.contains(&i)
            })
            .filter(|&i| !exact_non_keepers.contains(&lower(&l.file_path(&paths, i))))
            .collect();
        let gs = similar_groups(&l, &paths, &cands, video, &file_member, ctx)?;
        let prefix = if video { "v" } else { "i" };
        let list: Vec<IGroup> = gs
            .into_iter()
            .enumerate()
            .map(|(n, members)| IGroup { id: format!("{prefix}{}", n + 1), kind: if video { Kind::Video } else { Kind::Image }, files: 0, members })
            .collect();
        if video {
            videos = list;
        } else {
            images = list;
        }
    }

    Some(Stored {
        scan_id: 0,
        roots: roots.to_vec(),
        files: l.files_seen,
        bytes: l.bytes_seen,
        skipped: l.skipped.clone(),
        groups,
        folders,
        images,
        videos,
    })
}

fn similar_groups(
    l: &Listing,
    paths: &[String],
    cands: &[usize],
    video: bool,
    file_member: &dyn Fn(usize) -> Member,
    ctx: &Ctx,
) -> Option<Vec<Vec<Member>>> {
    ctx.progress.start_phase(if video { PH_VIDEOS } else { PH_IMAGES }, cands.len() as u64);
    let cpaths: Vec<String> = cands.iter().map(|&i| l.file_path(paths, i)).collect();
    let media = on_sta_threads(cands.len(), THUMB_THREADS, ctx, |k| {
        ctx.progress.tick(&cpaths[k]);
        media_sig(&cpaths[k], video)
    });
    if ctx.cancelled() {
        return None;
    }
    // Files without a thumbnail are simply left out.
    let have: Vec<usize> = (0..cands.len()).filter(|&k| media[k].is_some()).collect();
    let items: Vec<Media> = have.iter().map(|&k| media[k].unwrap()).collect();
    let pairs = similar_pairs(&items, video, &ctx.cancel)?;
    if pairs.is_empty() {
        return Some(Vec::new());
    }
    // Members (with pixel sizes from the property system) only for items that pair with something.
    let paired: Vec<usize> = {
        let mut s: Vec<usize> = pairs.iter().flat_map(|&(i, j, _)| [i, j]).collect();
        s.sort_unstable();
        s.dedup();
        s
    };
    let props = on_sta_threads(paired.len(), THUMB_THREADS, ctx, |n| shell::props(&cpaths[have[paired[n]]], video));
    if ctx.cancelled() {
        return None;
    }
    let mut members: HashMap<usize, Member> = HashMap::new();
    for (n, &i) in paired.iter().enumerate() {
        let mut m = file_member(cands[have[i]]);
        m.sig = Some(items[i].sig);
        m.duration_ms = items[i].duration_ms;
        if let Some(p) = &props[n] {
            m.width = p.width.unwrap_or(0);
            m.height = p.height.unwrap_or(0);
        }
        members.insert(i, m);
    }
    let groups = cluster(items.len(), &pairs, |a, b| keeper_cmp(&members[&a], &members[&b], true));
    Some(groups.into_iter().map(|g| g.into_iter().map(|i| members[&i].clone()).collect()).collect())
}

// ---------------------------------------------------------------------------------------------
// Roots

fn prepare_roots(raw: &[String]) -> Result<Vec<String>, String> {
    let mut roots: Vec<String> = Vec::new();
    for r in raw {
        let p = crate::scan::normalize_root(r);
        if p.is_empty() {
            continue;
        }
        match std::fs::metadata(&p) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(format!("{p} is a file, not a folder. Choose folders to look in.")),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(format!("Windows denied access to {p}. Restart Platter as administrator and try again."))
            }
            Err(_) => return Err(format!("{p} doesn't exist or isn't ready. Choose another folder.")),
        }
        if !roots.iter().any(|x| x.eq_ignore_ascii_case(&p)) {
            roots.push(p);
        }
    }
    if roots.is_empty() {
        return Err("Choose at least one folder to look in.".into());
    }
    Ok(merge_roots(&roots, &roots.iter().map(|r| final_path(r).unwrap_or_else(|| norm_lower(r))).collect::<Vec<_>>()))
}

/// Roots inside other roots are merged into them, judged by where each one really is (`real`,
/// lower-case): a junction or subst drive leading into another root (C:\Users\me\Videos as a
/// junction to D:\Videos, next to D:\) would otherwise list the same folders twice and show one
/// folder as two copies. Of two roots that are the same place, the one named by its real path
/// stays (else the first).
fn merge_roots(roots: &[String], real: &[String]) -> Vec<String> {
    let direct: Vec<bool> = roots.iter().zip(real).map(|(r, f)| norm_lower(r) == *f).collect();
    let better = |j: usize, i: usize| (direct[j], std::cmp::Reverse(j)) > (direct[i], std::cmp::Reverse(i));
    roots
        .iter()
        .enumerate()
        .filter(|&(i, _)| {
            !(0..roots.len()).any(|j| {
                j != i && inside(&real[i], &real[j]) && (real[i] != real[j] || better(j, i))
            })
        })
        .map(|(_, r)| r.clone())
        .collect()
}

fn default_roots() -> Vec<RootChoice> {
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music, FOLDERID_Pictures, FOLDERID_Videos,
    };
    let mut out: Vec<RootChoice> = Vec::new();
    for (id, label) in [
        (&FOLDERID_Desktop, "Desktop"),
        (&FOLDERID_Documents, "Documents"),
        (&FOLDERID_Downloads, "Downloads"),
        (&FOLDERID_Pictures, "Pictures"),
        (&FOLDERID_Videos, "Videos"),
        (&FOLDERID_Music, "Music"),
    ] {
        if let Some(p) = known_folder(id) {
            if std::path::Path::new(&p).is_dir() && !out.iter().any(|r| r.path.eq_ignore_ascii_case(&p)) {
                out.push(RootChoice { path: p, label: label.into() });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Delete

fn clean_request(p: &str) -> String {
    let mut s = p.trim().replace('/', "\\");
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    s
}

/// A member still is what the scan saw: there, with the same size and modified time (files), or
/// the same files with the same sizes and modified times (folders: a folder copy goes to the
/// Recycle Bin whole, so a file added to it, or one lost from the copy that stays, must stop it).
fn unchanged(m: &Member) -> bool {
    match std::fs::symlink_metadata(&m.path) {
        Ok(meta) if m.is_dir => meta.is_dir() && m.print.is_some() && folder_print_now(&m.path) == m.print,
        Ok(meta) => meta.is_file() && meta.len() == m.size && meta_modified_ms(&meta) == m.modified,
        Err(_) => false,
    }
}

/// One item of a delete request, checked against the stored result.
struct DeleteItem {
    member: Member,
    /// For each exact or folder group holding it: the copies this request leaves in place.
    kept: Vec<Vec<Member>>,
}

/// Checks a request against the stored result (under the store lock): every path is in it and no
/// group loses every copy. Folder copies come first, so files inside a removed folder aren't
/// handled twice.
fn plan_delete(st: &Stored, scan_id: u64, paths: &[String]) -> Result<Vec<DeleteItem>, String> {
    if st.scan_id != scan_id {
        return Err(OUT_OF_DATE.into());
    }
    let mut req: Vec<String> = Vec::new();
    for p in paths {
        let l = lower(&clean_request(p));
        if st.find(&l).is_none() {
            return Err("Some of these items aren't in the latest results. Scan again.".into());
        }
        if !req.contains(&l) {
            req.push(l);
        }
    }
    let reqset: HashSet<&str> = req.iter().map(|s| s.as_str()).collect();
    // Removed by this request: named, or inside a named folder copy.
    let removed = |l: &str| -> bool {
        reqset.contains(l) || l.match_indices('\\').any(|(i, _)| reqset.contains(&l[..i]) || reqset.contains(&l[..=i]))
    };
    // Never every copy: each group touched must keep a copy that still exists.
    for g in st.all_groups() {
        if g.members.iter().any(|m| removed(&m.lower)) && !g.members.iter().any(|m| !removed(&m.lower) && std::fs::symlink_metadata(&m.path).is_ok()) {
            let name = &g.members[0].name;
            return Err(format!("Keep at least one copy of “{name}”. Platter won't remove every copy."));
        }
    }
    let mut items: Vec<DeleteItem> = req
        .iter()
        .map(|l| {
            let member = st.find(l).unwrap().clone();
            let kept = st
                .groups
                .iter()
                .chain(&st.folders)
                .filter(|g| g.members.iter().any(|x| x.lower == *l))
                .map(|g| g.members.iter().filter(|x| x.lower != *l && !removed(&x.lower)).cloned().collect())
                .collect();
            DeleteItem { member, kept }
        })
        .collect();
    items.sort_by_key(|it| !it.member.is_dir);
    Ok(items)
}

/// Recycles the planned items one at a time, re-checking each against the disk first. Runs
/// without the store lock (one SHFileOperation per item can take minutes in all, or wait on a
/// "delete permanently?" prompt). Returns the result and the lower-case paths that went.
fn run_delete(items: &[DeleteItem], recycle: &dyn Fn(&str) -> Result<(), String>) -> (DeleteResult, Vec<String>) {
    let mut res = DeleteResult { removed: 0, freed: 0, failed: Vec::new() };
    let mut gone: Vec<String> = Vec::new();
    for it in items {
        let m = &it.member;
        if gone.iter().any(|g| inside(&m.lower, g)) {
            continue; // went to the Recycle Bin with its folder
        }
        let fail = |res: &mut DeleteResult, e: &str| res.failed.push(DeleteFailure { path: m.path.clone(), error: e.into() });
        if std::fs::symlink_metadata(&m.path).is_err() {
            fail(&mut res, "It's no longer there.");
            continue;
        }
        if !unchanged(m) {
            if m.is_dir {
                fail(&mut res, "This folder changed since the scan, so Platter left it alone.");
            } else {
                fail(&mut res, "It changed since the scan, so Platter left it alone. Scan again.");
            }
            continue;
        }
        // An identical copy must stay behind, still as the scan saw it, and really be another
        // copy: not this same file or folder seen through a junction, subst or mapped drive.
        let mut alias = false;
        let kept_ok = it.kept.iter().all(|others| {
            let live: Vec<&Member> = others.iter().filter(|x| unchanged(x)).collect();
            let distinct = live.iter().any(|x| !same_object(&m.path, &x.path));
            alias |= !distinct && !live.is_empty();
            distinct
        });
        if !kept_ok {
            if alias {
                fail(&mut res, "The copy Platter kept is this same item reached another way, so it was left in place.");
            } else {
                fail(&mut res, "The copy Platter kept is gone or changed, so this one was left in place.");
            }
            continue;
        }
        match recycle(&m.path) {
            Err(e) => fail(&mut res, &e),
            Ok(()) if std::fs::symlink_metadata(&m.path).is_ok() => fail(&mut res, "Windows couldn't move it to the Recycle Bin."),
            Ok(()) => {
                res.removed += 1;
                res.freed += m.size;
                gone.push(m.lower.clone());
            }
        }
    }
    (res, gone)
}

/// Keeps the stored result in step, so a second call (and dupes_result) sees what's left.
fn prune(st: &mut Stored, gone: &[String]) {
    for list in [&mut st.groups, &mut st.folders, &mut st.images, &mut st.videos] {
        for g in list.iter_mut() {
            g.members.retain(|m| !gone.iter().any(|x| inside(&m.lower, x)));
        }
        list.retain(|g| g.members.len() >= 2);
    }
}

/// Plan, run and prune in one go (the command does the same with the store unlocked meanwhile).
#[cfg(test)]
fn delete_with(
    st: &mut Stored,
    scan_id: u64,
    paths: &[String],
    recycle: &dyn Fn(&str) -> Result<(), String>,
) -> Result<DeleteResult, String> {
    let items = plan_delete(st, scan_id, paths)?;
    let (res, gone) = run_delete(&items, recycle);
    prune(st, &gone);
    Ok(res)
}

/// One delete at a time: each checks that the copies it keeps are still there right before it
/// recycles, which only holds if no other delete is removing them meanwhile.
fn delete_lock() -> &'static Mutex<()> {
    static R: OnceLock<Mutex<()>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(()))
}

// ---------------------------------------------------------------------------------------------
// Thumbnails for the UI: a small PNG as a data URL

fn crc32(data: &[u8]) -> u32 {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (n, e) in t.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    !data.iter().fold(!0u32, |c, &b| t[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8))
}

/// A truecolor+alpha PNG with stored (uncompressed) deflate blocks: no encoder needed, ~37 KB for
/// 96x96.
fn png_rgba(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((w as usize * 4 + 1) * h as usize);
    for row in rgba.chunks_exact(w as usize * 4).take(h as usize) {
        raw.push(0); // filter: none
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len()));
        let len = b.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(b);
    }
    if blocks.is_empty() {
        z.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut chunk = |ty: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut c = ty.to_vec();
        c.extend_from_slice(data);
        out.extend_from_slice(&c);
        out.extend_from_slice(&crc32(&c).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    out
}

fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        s.push(A[(n >> 18) as usize & 63] as char);
        s.push(A[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { A[n as usize & 63] as char } else { '=' });
    }
    s
}

/// Shell bitmap -> straight-alpha RGBA fitted inside `max`x`max` (box filter).
fn fit_rgba(p: &shell::Pixels, max: u32) -> (u32, u32, Vec<u8>) {
    let scale = (max as f64 / p.w.max(p.h) as f64).min(1.0);
    let nw = ((p.w as f64 * scale).round() as u32).max(1);
    let nh = ((p.h as f64 * scale).round() as u32).max(1);
    let has_alpha = p.bgra.chunks_exact(4).any(|px| px[3] != 0);
    let (w, h) = (p.w as usize, p.h as usize);
    let mut out = Vec::with_capacity((nw * nh * 4) as usize);
    for y in 0..nh as usize {
        let y0 = (y * h / nh as usize).min(h - 1);
        let y1 = ((y + 1) * h / nh as usize).clamp(y0 + 1, h);
        for x in 0..nw as usize {
            let x0 = (x * w / nw as usize).min(w - 1);
            let x1 = ((x + 1) * w / nw as usize).clamp(x0 + 1, w);
            let mut acc = [0u32; 4];
            for yy in y0..y1 {
                for xx in x0..x1 {
                    let i = (yy * w + xx) * 4;
                    for c in 0..4 {
                        acc[c] += p.bgra[i + c] as u32;
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let (b, g, r) = (acc[0] / n, acc[1] / n, acc[2] / n);
            let a = if has_alpha { acc[3] / n } else { 255 };
            // Premultiplied -> straight alpha.
            let un = |c: u32| if a == 0 { 0 } else { (c * 255 / a).min(255) as u8 };
            out.extend_from_slice(&[un(r), un(g), un(b), a as u8]);
        }
    }
    (nw, nh, out)
}

/// Premultiplied BGRA pixels (top-down) as a straight-alpha PNG data URL fitted inside `max`x`max`.
/// Shared with appicon.rs (app icons in Uninstall apps).
pub(crate) fn png_data_url_premultiplied(w: u32, h: u32, bgra: Vec<u8>, max: u32) -> String {
    let (w, h, rgba) = fit_rgba(&shell::Pixels { w, h, bgra }, max);
    format!("data:image/png;base64,{}", base64(&png_rgba(w, h, &rgba)))
}

fn thumb_data_url(path: String) -> Result<String, String> {
    let px = std::thread::Builder::new()
        .name("platter-dupes-thumb".into())
        .spawn(move || {
            let _com = shell::Apartment::enter(true);
            shell::image(&path, 96, shell::SIIGBF_THUMBNAILONLY | shell::SIIGBF_BIGGERSIZEOK)
                // No real thumbnail (folders, unknown formats): the Shell's icon is still a useful preview.
                .or_else(|_| shell::image(&path, 96, 0))
                .ok()
        })
        .map_err(|_| "Platter couldn't make a preview right now. Try again.".to_string())?
        .join()
        .ok()
        .flatten()
        .ok_or_else(|| "Windows couldn't make a preview of this file.".to_string())?;
    let (w, h, rgba) = fit_rgba(&px, 96);
    Ok(format!("data:image/png;base64,{}", base64(&png_rgba(w, h, &rgba))))
}

// ---------------------------------------------------------------------------------------------
// Commands

fn blocking_err(e: impl std::fmt::Display) -> String {
    let _ = e;
    "Platter couldn't finish that. Try again.".into()
}

#[tauri::command]
pub async fn dupes_default_roots() -> Result<Vec<RootChoice>, String> {
    tauri::async_runtime::spawn_blocking(default_roots).await.map_err(blocking_err)
}

#[tauri::command]
pub async fn dupes_start(app: tauri::AppHandle, roots: Vec<String>, options: DupesOptions) -> Result<u64, String> {
    let roots = tauri::async_runtime::spawn_blocking(move || prepare_roots(&roots)).await.map_err(blocking_err)??;
    let cancel = Arc::new(AtomicBool::new(false));
    // The id is taken under the lock, so the scan installed last is always the newest one.
    let gen = {
        let mut r = running().lock().unwrap();
        let gen = GEN.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(old) = r.take() {
            old.cancel.store(true, Ordering::SeqCst);
        }
        *r = Some(Running { gen, cancel: cancel.clone() });
        gen
    };
    let spawned = std::thread::Builder::new().name("platter-dupes".into()).spawn(move || {
        let started = Instant::now();
        let ctx = Arc::new(Ctx::new(cancel.clone()));
        let done = Arc::new(AtomicBool::new(false));
        {
            let (ctx, done, app) = (ctx.clone(), done.clone(), app.clone());
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_millis(150));
                if done.load(Ordering::SeqCst) || ctx.cancelled() || GEN.load(Ordering::SeqCst) != gen {
                    break;
                }
                let _ = app.emit("dupes-progress", ctx.progress.snapshot(gen));
            });
        }
        let places = Places::real();
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan(&roots, &options, &places, &ctx)));
        done.store(true, Ordering::SeqCst);
        let mut payload = DupesDone { scan_id: gen, ok: false, cancelled: false, error: None, elapsed_ms: started.elapsed().as_millis() as u64 };
        let mut st = store().lock().unwrap();
        if GEN.load(Ordering::SeqCst) != gen {
            return; // superseded: the newer scan reports
        }
        match res {
            Ok(Some(mut s)) => {
                s.scan_id = gen;
                *st = Some(s);
                payload.ok = true;
            }
            Ok(None) if cancel.load(Ordering::SeqCst) => payload.cancelled = true,
            _ => payload.error = Some("The duplicate scan couldn't be completed. Try again.".into()),
        }
        {
            let mut r = running().lock().unwrap();
            if r.as_ref().map(|x| x.gen) == Some(gen) {
                *r = None;
            }
        }
        drop(st);
        let _ = app.emit("dupes-done", payload);
    });
    if spawned.is_err() {
        let mut r = running().lock().unwrap();
        if r.as_ref().map(|x| x.gen) == Some(gen) {
            *r = None;
        }
        return Err("Couldn't start looking for duplicates. Try again.".into());
    }
    Ok(gen)
}

#[tauri::command]
pub async fn dupes_cancel() -> Result<(), String> {
    if let Some(r) = running().lock().unwrap().as_ref() {
        r.cancel.store(true, Ordering::SeqCst);
    }
    Ok(())
}

#[tauri::command]
pub async fn dupes_result(scan_id: u64) -> Result<DupesResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let st = store().lock().unwrap();
        match st.as_ref() {
            Some(s) if s.scan_id == scan_id => Ok(render(s)),
            _ => Err(OUT_OF_DATE.to_string()),
        }
    })
    .await
    .map_err(blocking_err)?
}

#[tauri::command]
pub async fn dupes_thumb(path: String) -> Result<String, String> {
    // The store lock is taken on a blocking thread, never on an async runtime worker.
    tauri::async_runtime::spawn_blocking(move || {
        let path = {
            let st = store().lock().unwrap();
            let l = lower(&clean_request(&path));
            st.as_ref()
                .and_then(|s| s.find(&l))
                .map(|m| m.path.clone())
                .ok_or_else(|| "Platter only shows previews of files in the latest results.".to_string())?
        };
        thumb_data_url(path)
    })
    .await
    .map_err(blocking_err)?
}

#[tauri::command]
pub async fn dupes_delete(scan_id: u64, paths: Vec<String>) -> Result<DeleteResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _one = delete_lock().lock().unwrap_or_else(|e| e.into_inner());
        // The store is locked only to check the request and, afterwards, to update the result:
        // previews and results stay available while the Recycle Bin works.
        let items = {
            let st = store().lock().unwrap();
            plan_delete(st.as_ref().ok_or_else(|| OUT_OF_DATE.to_string())?, scan_id, &paths)?
        };
        let (res, gone) = run_delete(&items, &|p| crate::manage::recycle_sync(&[p.to_string()]));
        if let Some(s) = store().lock().unwrap().as_mut().filter(|s| s.scan_id == scan_id) {
            prune(s, &gone);
        }
        Ok(res)
    })
    .await
    .map_err(blocking_err)?
}

// ---------------------------------------------------------------------------------------------
// Shell thumbnails and media properties

mod shell {
    use crate::disks::com::{method, Unk};
    use std::ffi::c_void;
    use windows_sys::core::{GUID, HRESULT};
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
        BI_RGB, DIB_RGB_COLORS,
    };
    use windows_sys::Win32::System::Com::{CoInitializeEx, CoUninitialize};
    use windows_sys::Win32::UI::Shell::PropertiesSystem::SHGetPropertyStoreFromParsingName;
    use windows_sys::Win32::UI::Shell::SHCreateItemFromParsingName;

    pub const SIIGBF_BIGGERSIZEOK: i32 = 0x1;
    pub const SIIGBF_THUMBNAILONLY: i32 = 0x8;

    const IID_ISHELLITEMIMAGEFACTORY: GUID = GUID::from_u128(0xbcc18b79_ba16_442f_80c4_8a59c30c463b);
    const IID_IPROPERTYSTORE: GUID = GUID::from_u128(0x886d8eeb_8cf2_4446_8d02_cdba1dbdcf99);

    #[repr(C)]
    struct PropKey {
        fmtid: GUID,
        pid: u32,
    }
    /// PKEY_Media_Duration (VT_UI8, 100 ns units).
    const PKEY_MEDIA_DURATION: PropKey = PropKey { fmtid: GUID::from_u128(0x64440490_4c8b_11d1_8b70_080036b11a03), pid: 3 };
    const FMTID_IMAGE: GUID = GUID::from_u128(0x6444048f_4c8b_11d1_8b70_080036b11a03);
    const FMTID_VIDEO: GUID = GUID::from_u128(0x64440491_4c8b_11d1_8b70_080036b11a03);
    /// PKEY_Image_HorizontalSize / VerticalSize (VT_UI4).
    const PKEY_IMAGE_W: PropKey = PropKey { fmtid: FMTID_IMAGE, pid: 3 };
    const PKEY_IMAGE_H: PropKey = PropKey { fmtid: FMTID_IMAGE, pid: 4 };
    /// PKEY_Video_FrameWidth / FrameHeight (VT_UI4).
    const PKEY_VIDEO_W: PropKey = PropKey { fmtid: FMTID_VIDEO, pid: 3 };
    const PKEY_VIDEO_H: PropKey = PropKey { fmtid: FMTID_VIDEO, pid: 4 };

    #[link(name = "ole32")]
    extern "system" {
        fn PropVariantClear(pv: *mut c_void) -> HRESULT;
    }

    /// PROPVARIANT (24 bytes on x64), cleared on drop.
    #[repr(C)]
    struct PropVariant {
        vt: u16,
        _r: [u16; 3],
        data: [u64; 2],
    }
    impl Drop for PropVariant {
        fn drop(&mut self) {
            unsafe { PropVariantClear(self as *mut PropVariant as *mut c_void) };
        }
    }
    impl PropVariant {
        fn uint(&self) -> Option<u64> {
            match self.vt {
                19 | 23 => Some(self.data[0] as u32 as u64), // VT_UI4, VT_UINT
                21 => Some(self.data[0]),                     // VT_UI8
                18 => Some(self.data[0] as u16 as u64),       // VT_UI2
                _ => None,
            }
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Size {
        cx: i32,
        cy: i32,
    }

    /// COM for this thread for as long as the guard lives. Only on threads Platter created.
    pub struct Apartment(bool);
    impl Apartment {
        /// `sta`: single-threaded apartment (what Explorer's thumbnail providers are written for).
        pub fn enter(sta: bool) -> Apartment {
            // COINIT_APARTMENTTHREADED 0x2 | COINIT_DISABLE_OLE1DDE 0x4; COINIT_MULTITHREADED 0x0
            let flags = if sta { 0x2 | 0x4 } else { 0x0 };
            Apartment(unsafe { CoInitializeEx(std::ptr::null(), flags) } >= 0)
        }
    }
    impl Drop for Apartment {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    /// A decoded bitmap: top-down rows of BGRA pixels (alpha premultiplied, as the Shell hands it).
    pub struct Pixels {
        pub w: u32,
        pub h: u32,
        pub bgra: Vec<u8>,
    }

    /// The Shell's thumbnail of `path` fitted in `size`×`size`. `flags`: SIIGBF_* bits. With
    /// SIIGBF_THUMBNAILONLY a file without a real thumbnail fails instead of returning its icon
    /// (icons would make unrelated files look alike).
    pub fn image(path: &str, size: i32, flags: i32) -> Result<Pixels, HRESULT> {
        let w = crate::util::wide(path);
        let mut p = std::ptr::null_mut();
        let hr = unsafe { SHCreateItemFromParsingName(w.as_ptr(), std::ptr::null_mut(), &IID_ISHELLITEMIMAGEFACTORY, &mut p) };
        if hr < 0 || p.is_null() {
            return Err(hr);
        }
        let factory = Unk(p);
        type GetImage = unsafe extern "system" fn(*mut c_void, Size, i32, *mut *mut c_void) -> HRESULT;
        let mut hbm: *mut c_void = std::ptr::null_mut();
        let hr = unsafe { method::<GetImage>(factory.0, 3)(factory.0, Size { cx: size, cy: size }, flags, &mut hbm) };
        if hr < 0 || hbm.is_null() {
            return Err(if hr < 0 { hr } else { -1 });
        }
        let out = bitmap_pixels(hbm);
        unsafe { DeleteObject(hbm) };
        out.ok_or(-1)
    }

    fn bitmap_pixels(hbm: *mut c_void) -> Option<Pixels> {
        let mut bm: BITMAP = unsafe { std::mem::zeroed() };
        let n = unsafe { GetObjectW(hbm, std::mem::size_of::<BITMAP>() as i32, &mut bm as *mut BITMAP as *mut c_void) };
        if n == 0 || bm.bmWidth <= 0 || bm.bmHeight == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth as u32, bm.bmHeight.unsigned_abs());
        if w > 4096 || h > 4096 {
            return None;
        }
        let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        let dc = unsafe { CreateCompatibleDC(std::ptr::null_mut()) };
        if dc.is_null() {
            return None;
        }
        let lines = unsafe { GetDIBits(dc, hbm, 0, h, bgra.as_mut_ptr() as *mut c_void, &mut info, DIB_RGB_COLORS) };
        unsafe { DeleteDC(dc) };
        (lines as u32 == h).then_some(Pixels { w, h, bgra })
    }

    /// Duration (ms) and pixel size from the Shell property system (same data as Explorer's
    /// Details pane). Missing values come back as None.
    pub struct Props {
        pub duration_ms: Option<u64>,
        pub width: Option<u32>,
        pub height: Option<u32>,
    }

    pub fn props(path: &str, video: bool) -> Option<Props> {
        let w = crate::util::wide(path);
        let mut p = std::ptr::null_mut();
        // GPS_DEFAULT (0): GPS_FASTPROPERTIESONLY skips the media property handlers that know durations.
        let hr = unsafe { SHGetPropertyStoreFromParsingName(w.as_ptr(), std::ptr::null_mut(), 0, &IID_IPROPERTYSTORE, &mut p) };
        if hr < 0 || p.is_null() {
            return None;
        }
        let store = Unk(p);
        type GetValue = unsafe extern "system" fn(*mut c_void, *const PropKey, *mut PropVariant) -> HRESULT;
        let get = |key: &PropKey| -> Option<u64> {
            let mut v = PropVariant { vt: 0, _r: [0; 3], data: [0; 2] };
            let hr = unsafe { method::<GetValue>(store.0, 5)(store.0, key, &mut v) };
            if hr < 0 {
                return None;
            }
            v.uint()
        };
        let (kw, kh) = if video { (&PKEY_VIDEO_W, &PKEY_VIDEO_H) } else { (&PKEY_IMAGE_W, &PKEY_IMAGE_H) };
        Some(Props {
            duration_ms: if video { get(&PKEY_MEDIA_DURATION).map(|d| d / 10_000) } else { None },
            width: get(kw).map(|x| x as u32).filter(|&x| x > 0),
            height: get(kh).map(|x| x as u32).filter(|&x| x > 0),
        })
    }
}

#[cfg(test)]
mod spike {
    use super::shell;
    use std::path::PathBuf;
    use std::time::Instant;

    pub fn sample_files(exts: &[&str], per_ext: usize) -> Vec<PathBuf> {
        let home = PathBuf::from(std::env::var("USERPROFILE").unwrap());
        let mut out = Vec::new();
        let mut counts = vec![0usize; exts.len()];
        let mut stack: Vec<(PathBuf, u32)> =
            ["Pictures", "Videos", "Downloads", "Documents"].iter().map(|d| (home.join(d), 0)).collect();
        while let Some((d, depth)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_dir() && !ft.is_symlink() && depth < 4 {
                    stack.push((e.path(), depth + 1));
                } else if ft.is_file() {
                    let p = e.path();
                    let ext = p.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
                    if let Some(i) = exts.iter().position(|x| *x == ext) {
                        if counts[i] < per_ext {
                            counts[i] += 1;
                            out.push(p);
                        }
                    }
                }
            }
        }
        out
    }

    fn short(f: &std::path::Path) -> String {
        f.file_name().unwrap().to_string_lossy().chars().take(36).collect()
    }

    fn run_thumbs(sta: bool, files: &[PathBuf]) {
        let _a = shell::Apartment::enter(sta);
        let label = if sta { "STA" } else { "MTA" };
        for f in files {
            let s = f.to_string_lossy();
            let mut line = format!("{label} {:>36}", short(f));
            for (size, flags) in [
                (64, shell::SIIGBF_THUMBNAILONLY),
                (64, shell::SIIGBF_THUMBNAILONLY),
                (64, shell::SIIGBF_THUMBNAILONLY | shell::SIIGBF_BIGGERSIZEOK),
                (128, shell::SIIGBF_THUMBNAILONLY),
                (256, shell::SIIGBF_THUMBNAILONLY | shell::SIIGBF_BIGGERSIZEOK),
            ] {
                let t = Instant::now();
                let r = shell::image(&s, size, flags);
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                match r {
                    Ok(p) => line += &format!(" | {size}/{flags}: {}x{} {ms:.1}ms", p.w, p.h),
                    Err(hr) => line += &format!(" | {size}/{flags}: ERR {:#x} {ms:.1}ms", hr as u32),
                }
            }
            println!("{line}");
        }
    }

    #[test]
    #[ignore = "spike: cargo test spike_thumbs -- --ignored --nocapture"]
    fn spike_thumbs() {
        let files = sample_files(&["jpg", "png", "webp", "gif", "heic", "mp4", "mov", "mkv"], 4);
        println!("{} files", files.len());
        let f2 = files.clone();
        std::thread::spawn(move || run_thumbs(true, &f2)).join().unwrap();
        std::thread::spawn(move || run_thumbs(false, &files)).join().unwrap();
    }

    #[test]
    #[ignore = "spike: cargo test spike_props -- --ignored --nocapture"]
    fn spike_props() {
        let files = sample_files(&["jpg", "png", "mp4", "mov", "mkv", "webp"], 5);
        std::thread::spawn(move || {
            let _a = shell::Apartment::enter(true);
            for f in &files {
                let ext = f.extension().unwrap().to_string_lossy().to_lowercase();
                let video = matches!(ext.as_str(), "mp4" | "mov" | "mkv");
                let t = Instant::now();
                let p = shell::props(&f.to_string_lossy(), video);
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                match p {
                    Some(p) => println!("{:>36} dur={:?} {:?}x{:?} {ms:.1}ms", short(f), p.duration_ms, p.width, p.height),
                    None => println!("{:>36} no store {ms:.1}ms", short(f)),
                }
            }
        })
        .join()
        .unwrap();
    }

    /// Read throughput (1 MB buffers) vs threads. Unbuffered (FILE_FLAG_NO_BUFFERING) = what a cold
    /// scan sees from the device; buffered + xxh3 = warm cache + hashing CPU (debug build!).
    #[test]
    #[ignore = "spike: cargo test spike_read -- --ignored --nocapture"]
    fn spike_read() {
        use std::io::Read;
        use std::os::windows::fs::OpenOptionsExt;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let mut files: Vec<(PathBuf, u64)> = Vec::new();
        let home = PathBuf::from(std::env::var("USERPROFILE").unwrap());
        let mut stack = vec![home.join("Downloads"), home.join("Documents"), home.join("Videos"), home.join("Pictures")];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_dir() && !ft.is_symlink() {
                    stack.push(e.path());
                } else if ft.is_file() {
                    let Ok(m) = e.metadata() else { continue };
                    // skip cloud placeholders (attrs 0x1000 | 0x40000 | 0x400000)
                    use std::os::windows::fs::MetadataExt;
                    if m.file_attributes() & 0x0044_1000 == 0 && m.len() > 8 << 20 {
                        files.push((e.path(), m.len()));
                    }
                }
            }
        }
        files.sort_by(|a, b| b.1.cmp(&a.1));
        files.truncate(40);
        let total: u64 = files.iter().map(|f| f.1).sum();
        println!("{} files, {:.0} MB", files.len(), total as f64 / 1e6);
        let files = Arc::new(files);
        for (label, unbuffered, hash) in [("unbuffered", true, false), ("buffered+xxh3", false, true), ("unbuffered", true, false)] {
            for threads in [1usize, 2, 4, 8] {
                let next = Arc::new(AtomicUsize::new(0));
                let t = Instant::now();
                let hs: Vec<_> = (0..threads)
                    .map(|_| {
                        let (files, next) = (files.clone(), next.clone());
                        std::thread::spawn(move || {
                            // 1 MB, sector-aligned for FILE_FLAG_NO_BUFFERING.
                            let mut raw = vec![0u8; (1 << 20) + 4096];
                            let off = raw.as_ptr().align_offset(4096);
                            let buf = &mut raw[off..off + (1 << 20)];
                            loop {
                                let i = next.fetch_add(1, Ordering::Relaxed);
                                let Some((p, _)) = files.get(i) else { break };
                                let mut o = std::fs::OpenOptions::new();
                                o.read(true).share_mode(7);
                                o.custom_flags(if unbuffered { 0x2000_0000 } else { 0x0800_0000 });
                                let Ok(mut f) = o.open(p) else { continue };
                                let mut h = xxhash_rust::xxh3::Xxh3::new();
                                loop {
                                    match f.read(buf) {
                                        Ok(0) | Err(_) => break,
                                        Ok(n) => {
                                            if hash {
                                                h.update(&buf[..n]);
                                            }
                                        }
                                    }
                                }
                                std::hint::black_box(h.digest128());
                            }
                        })
                    })
                    .collect();
                for h in hs {
                    h.join().unwrap();
                }
                let s = t.elapsed().as_secs_f64();
                println!("{label:>14} threads={threads}: {s:.2} s, {:.0} MB/s", total as f64 / 1e6 / s);
            }
        }
        // Pure hashing speed of this (debug) build, in memory.
        let data = vec![7u8; 64 << 20];
        let t = Instant::now();
        std::hint::black_box(xxhash_rust::xxh3::xxh3_128(&data));
        println!("xxh3_128 in memory: {:.0} MB/s", 64.0 * 1.048576 / t.elapsed().as_secs_f64());
    }

    /// Thumbnails of many images on N threads: throughput per apartment.
    #[test]
    #[ignore = "spike: cargo test spike_thumb_threads -- --ignored --nocapture"]
    fn spike_thumb_threads() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let files = Arc::new(sample_files(&["jpg", "png", "webp"], 120));
        for sta in [true, false] {
            for threads in [1usize, 2, 4, 8] {
                let next = Arc::new(AtomicUsize::new(0));
                let ok = Arc::new(AtomicUsize::new(0));
                let t = Instant::now();
                let hs: Vec<_> = (0..threads)
                    .map(|_| {
                        let (files, next, ok) = (files.clone(), next.clone(), ok.clone());
                        std::thread::spawn(move || {
                            let _a = shell::Apartment::enter(sta);
                            loop {
                                let i = next.fetch_add(1, Ordering::Relaxed);
                                let Some(f) = files.get(i) else { break };
                                let flags = shell::SIIGBF_THUMBNAILONLY | shell::SIIGBF_BIGGERSIZEOK;
                                if shell::image(&f.to_string_lossy(), 64, flags).is_ok() {
                                    ok.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        })
                    })
                    .collect();
                for h in hs {
                    h.join().unwrap();
                }
                let el = t.elapsed();
                println!(
                    "{} threads={threads}: {} files, {} ok, {:?} ({:.2} ms/file)",
                    if sta { "STA" } else { "MTA" },
                    files.len(),
                    ok.load(Ordering::Relaxed),
                    el,
                    el.as_secs_f64() * 1000.0 / files.len() as f64
                );
            }
        }
    }

    /// Thresholds: dHash distances between the user's real pictures (read-only). Prints every pair
    /// within 12 so the near/far boundary can be checked by eye.
    #[test]
    #[ignore = "spike: cargo test spike_similar_real -- --ignored --nocapture"]
    fn spike_similar_real() {
        let files = sample_files(&["jpg", "jpeg", "png", "webp", "gif", "heic"], 2000);
        let t = Instant::now();
        let ctx = super::Ctx::new(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let paths: Vec<String> = files.iter().map(|f| f.to_string_lossy().into_owned()).collect();
        let media = super::on_sta_threads(paths.len(), super::THUMB_THREADS, &ctx, |i| super::media_sig(&paths[i], false));
        let ok: Vec<usize> = (0..paths.len()).filter(|&i| media[i].is_some()).collect();
        println!("{} images, {} with thumbnails, {:?}", paths.len(), ok.len(), t.elapsed());
        let mut near = Vec::new();
        for a in 0..ok.len() {
            for b in a + 1..ok.len() {
                let (x, y) = (media[ok[a]].unwrap(), media[ok[b]].unwrap());
                let d = super::dist(&x.sig, &y.sig);
                if d <= 12 {
                    near.push((d, ok[a], ok[b], (x.sig.mean - y.sig.mean).abs(), super::accept(&x, &y, false).is_some()));
                }
            }
        }
        near.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        let band: u32 = std::env::var("PLATTER_BAND").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
        for (d, a, b, dm, acc) in near.iter().filter(|n| n.0 >= band && (n.4 || std::env::var("PLATTER_ALL").is_ok())).take(150) {
            println!("d={d:2} dmean={dm:5.1} accept={acc} | {} | {}", paths[*a], paths[*b]);
        }
        let mut hist = [0usize; 13];
        for n in &near {
            hist[n.0 as usize] += 1;
        }
        println!("pairs by distance 0..=12: {hist:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_tiers_by_ancestor() {
        let mut p = FolderPicks::default();
        assert_eq!(p.tier(r"c:\a\b.txt"), 1);
        p.tiers.insert(r"d:\keep".into(), 0);
        p.tiers.insert(r"d:\go".into(), 2);
        p.tiers.insert(r"d:\keep\inner".into(), 2);
        p.tiers.insert(r"e:\".into(), 0);
        assert_eq!(p.tier(r"d:\keep\x\y.jpg"), 0);
        assert_eq!(p.tier(r"d:\go\y.jpg"), 2);
        assert_eq!(p.tier(r"d:\keep\inner\y.jpg"), 2, "a removed copy inside a kept one still goes");
        assert_eq!(p.tier(r"d:\keeper\y.jpg"), 1, "a sibling with a longer name isn't inside");
        assert_eq!(p.tier(r"e:\z.jpg"), 0, "drive-root copies are found");
        assert_eq!(p.tier(r"\srv\share\z.jpg"), 1);
    }
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A throwaway folder under %TEMP%, removed on drop.
    struct Tree(PathBuf);
    impl Tree {
        fn new(tag: &str) -> Tree {
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let p = std::env::temp_dir().join(format!("platter-dupes-{tag}-{}-{nanos}", std::process::id()));
            assert!(p.starts_with(std::env::temp_dir()));
            std::fs::create_dir_all(&p).unwrap();
            Tree(p)
        }
        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
        fn s(&self, rel: &str) -> String {
            let p = if rel.is_empty() { self.0.clone() } else { self.path(rel) };
            p.to_string_lossy().into_owned()
        }
        fn file(&self, rel: &str, data: &[u8]) -> PathBuf {
            let p = self.path(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, data).unwrap();
            p
        }
        /// Sets the modified time to `days` days after 2020-01-01.
        fn age(&self, rel: &str, days: u64) {
            let f = std::fs::OpenOptions::new().write(true).open(self.path(rel)).unwrap();
            f.set_modified(UNIX_EPOCH + Duration::from_secs(1_577_836_800 + days * 86_400)).unwrap();
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            assert!(self.0.starts_with(std::env::temp_dir()));
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn bytes(seed: u64, len: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    /// Test places: <tree>\Pictures is a library, <tree>\Downloads is Downloads; nothing is
    /// protected by location (the tree itself lives in AppData\Local\Temp).
    fn places(t: &Tree) -> Places {
        Places {
            libraries: vec![(norm_lower(&t.s("Pictures")), "Pictures")],
            downloads: vec![norm_lower(&t.s("Downloads"))],
            ..Places::default()
        }
    }

    fn opts(min_size: u64, folders: bool) -> DupesOptions {
        DupesOptions { min_size, similar_images: false, similar_videos: false, folders }
    }

    fn ctx() -> Ctx {
        Ctx::new(Arc::new(AtomicBool::new(false)))
    }

    fn run(t: &Tree, o: &DupesOptions) -> Stored {
        let mut s = scan(&[t.s("")], o, &places(t), &ctx()).expect("scan");
        s.scan_id = 7;
        s
    }

    /// Sorted lower-case file names of each exact group.
    fn names(r: &DupesResult) -> Vec<Vec<String>> {
        let mut v: Vec<Vec<String>> = r
            .groups
            .iter()
            .map(|g| {
                let mut n: Vec<String> = g.files.iter().map(|f| f.name.to_lowercase()).collect();
                n.sort();
                n
            })
            .collect();
        v.sort();
        v
    }

    fn find<'a>(r: &'a DupesResult, name: &str) -> &'a DupFile {
        r.groups.iter().flat_map(|g| g.files.iter()).find(|f| f.name == name).unwrap_or_else(|| panic!("{name} not in groups"))
    }

    #[test]
    fn exact_groups_need_identical_content() {
        let t = Tree::new("exact");
        let a = bytes(1, 300_000);
        t.file("a.bin", &a);
        t.file("sub\\b.bin", &a);
        // Same size, same first and last 64 KB, one byte different in the middle.
        let mut c = a.clone();
        c[150_000] ^= 0x55;
        t.file("c.bin", &c);
        // Small files (≤ 128 KB) are hashed whole.
        let e = bytes(2, 100_000);
        t.file("e1.bin", &e);
        t.file("deep\\er\\e2.bin", &e);
        let mut e3 = e.clone();
        e3[50_000] ^= 1;
        t.file("e3.bin", &e3);
        // Below the minimum size: never grouped, counted as skipped.
        t.file("tiny1.txt", b"same");
        t.file("tiny2.txt", b"same");
        let st = run(&t, &opts(1024, false));
        let r = render(&st);
        assert_eq!(names(&r), vec![vec!["a.bin", "b.bin"], vec!["e1.bin", "e2.bin"]]);
        assert_eq!(r.skipped.small, 2);
        assert_eq!(r.files, 8);
        assert_eq!(r.groups[0].size, 300_000, "sorted by reclaimable");
        assert_eq!(r.reclaimable, 400_000);
        assert!(r.groups.iter().all(|g| g.files.iter().filter(|f| f.keep).count() == 1));
    }

    #[test]
    fn hard_links_are_one_file() {
        let t = Tree::new("links");
        let a = bytes(3, 200_000);
        t.file("a.bin", &a);
        std::fs::hard_link(t.path("a.bin"), t.path("link-to-a.bin")).unwrap();
        t.file("other\\copy.bin", &a);
        let x = bytes(4, 50_000);
        t.file("x.bin", &x);
        std::fs::hard_link(t.path("x.bin"), t.path("x-link.bin")).unwrap();
        let r = render(&run(&t, &opts(1, false)));
        assert_eq!(r.groups.len(), 1, "{:?}", names(&r));
        let g = &names(&r)[0];
        assert_eq!(g.len(), 2);
        assert!(g.contains(&"copy.bin".to_string()));
        assert_eq!(r.reclaimable, 200_000);
    }

    #[test]
    fn recommendations_follow_the_rules() {
        let t = Tree::new("recs");
        // Pictures copy (older) vs Downloads copy.
        let a = bytes(10, 60_000);
        t.file("Pictures\\holiday.jpg", &a);
        t.file("Downloads\\holiday.jpg", &a);
        t.age("Pictures\\holiday.jpg", 10);
        t.age("Downloads\\holiday.jpg", 20);
        // Copy-named file next to its original.
        let b = bytes(11, 40_000);
        t.file("Other\\report.pdf", &b);
        t.file("Other\\report - Copy.pdf", &b);
        // Two copies elsewhere: the newer one is for review.
        let c = bytes(12, 30_000);
        t.file("Work\\plan.docx", &c);
        t.file("Archive\\plan.docx", &c);
        t.age("Work\\plan.docx", 5);
        t.age("Archive\\plan.docx", 50);
        // Same folder, different names.
        let f = bytes(13, 20_000);
        t.file("Misc\\f1.dat", &f);
        t.file("Misc\\f2.dat", &f);
        t.age("Misc\\f1.dat", 1);
        t.age("Misc\\f2.dat", 2);
        // A project folder: its copy is protected and kept.
        let d = bytes(14, 25_000);
        t.file("proj\\package.json", b"{\"name\":\"proj\"}");
        t.file("proj\\assets\\logo.png", &d);
        t.file("Downloads\\logo.png", &d);
        // A setup .exe next to a stray .dll in Downloads itself must not make Downloads an app folder.
        t.file("Downloads\\setup.exe", b"setup program");
        t.file("Downloads\\helper.dll", b"helper library");
        // Only protected copies: nothing to act on, so not listed.
        let e = bytes(15, 25_000);
        t.file("proj\\lib\\e.bin", &e);
        t.file("app\\x.exe", b"exe");
        t.file("app\\x.dll", b"dll");
        t.file("app\\e.bin", &e);

        let r = render(&run(&t, &opts(1, false)));
        assert_eq!(r.groups.len(), 5, "{:?}", names(&r));
        assert!(!names(&r).iter().any(|g| g.contains(&"e.bin".to_string())));

        let g = r.groups.iter().find(|g| g.files[0].name == "holiday.jpg").unwrap();
        assert!(g.files[0].keep && g.files[0].path.contains("Pictures"));
        assert_eq!(g.files[0].note, "Oldest copy, in Pictures");
        assert!(!g.files[0].select);
        assert_eq!((g.files[1].risk, g.files[1].select, g.files[1].note.as_str()), ("safe", true, "Copy in Downloads"));

        let k = find(&r, "report.pdf");
        assert!(k.keep);
        let c = find(&r, "report - Copy.pdf");
        assert_eq!((c.keep, c.risk, c.select, c.note.as_str()), (false, "safe", true, "Named like a copy"));

        let w = find(&r, "plan.docx");
        let g = r.groups.iter().find(|g| g.files[0].name == "plan.docx").unwrap();
        assert!(w.keep && g.files[0].path.contains("Work"));
        assert_eq!((g.files[1].risk, g.files[1].select), ("review", true));
        assert_eq!(g.files[1].note, "Newer copy, in Archive");

        let f2 = find(&r, "f2.dat");
        assert_eq!((f2.keep, f2.risk, f2.note.as_str()), (false, "safe", "Same folder as the copy kept"));

        let g = r.groups.iter().find(|g| g.files.iter().any(|f| f.name == "logo.png")).unwrap();
        assert!(g.files[0].keep && g.files[0].path.contains("proj"));
        assert_eq!((g.files[0].risk, g.files[0].select), ("protected", false));
        assert_eq!(g.files[0].note, "Part of a project folder, so leave it");
        assert_eq!((g.files[1].risk, g.files[1].select), ("safe", true));

        // Every select is counted once.
        let sum: u64 = r.groups.iter().map(|g| g.reclaimable).sum();
        assert_eq!(r.reclaimable, sum);
        assert_eq!(sum, 60_000 + 40_000 + 30_000 + 20_000 + 25_000);
    }

    #[test]
    fn copy_names() {
        for n in ["a - Copy.jpg", "a - Copy (2).jpg", "a (1).jpg", "Copy of a.jpg", "A - COPY.PNG", "x (12)"] {
            assert!(looks_like_copy(n), "{n}");
        }
        for n in ["a.jpg", "copy.jpg", "a (b).jpg", "a(1)x.jpg", "photocopy.png", "a (2019 trip).jpg"] {
            assert!(!looks_like_copy(n), "{n}");
        }
    }

    #[test]
    fn duplicate_folders_outermost_only() {
        let t = Tree::new("folders");
        let (p, q, r1, s1) = (bytes(20, 400_000), bytes(21, 400_000), bytes(22, 600_000), bytes(23, 600_000));
        for (dir, names) in [
            ("Docs\\Album", ["p.bin", "q.bin", "sub\\r.bin", "sub\\s.bin", "notes.txt"]),
            ("Backup\\Album copy", ["pp.bin", "qq.bin", "deeper\\rr.bin", "deeper\\ss.bin", "n.txt"]),
        ] {
            let data: [&[u8]; 5] = [&p, &q, &r1, &s1, b"tiny"];
            for (n, d) in names.iter().zip(data) {
                t.file(&format!("{dir}\\{n}"), d);
            }
        }
        // The parents differ (a parent holding nothing but a duplicate folder would itself be the
        // outermost duplicate).
        t.file("Docs\\readme.txt", b"docs");
        t.file("Backup\\log.txt", b"backup log");
        // A third copy of p outside the folders: its exact group stays.
        t.file("Loose\\p-extra.bin", &p);
        // Nested duplicates whose parents differ: the inner folders are the outermost duplicates.
        let (u, v) = (bytes(30, 700_000), bytes(31, 700_000));
        t.file("X\\inner\\u.bin", &u);
        t.file("X\\inner\\v.bin", &v);
        t.file("X\\only-x.bin", &bytes(32, 1000));
        t.file("Y\\inner2\\u2.bin", &u);
        t.file("Y\\inner2\\v2.bin", &v);
        t.file("Y\\only-y.bin", &bytes(33, 1000));

        let st = run(&t, &opts(1024, true));
        let r = render(&st);
        let mut fg: Vec<Vec<String>> = r
            .folders
            .iter()
            .map(|g| {
                let mut v: Vec<String> = g.folders.iter().map(|f| f.name.clone()).collect();
                v.sort();
                v
            })
            .collect();
        fg.sort();
        assert_eq!(fg, vec![vec!["Album".to_string(), "Album copy".to_string()], vec!["inner".to_string(), "inner2".to_string()]]);
        let album = r.folders.iter().find(|g| g.folders.iter().any(|f| f.name == "Album")).unwrap();
        assert_eq!(album.files, 5);
        assert_eq!(album.size, 2_000_004);
        assert_eq!(album.folders.iter().filter(|f| f.keep).count(), 1);
        assert_eq!(album.reclaimable, 2_000_004);
        // q/r/s groups are covered by the folder group; p has a copy outside it.
        assert_eq!(names(&r), vec![vec!["p-extra.bin", "p.bin", "pp.bin"]]);
        // Totals: folder copies + p copies not inside a selected folder copy.
        let selected_dirs: Vec<String> =
            r.folders.iter().flat_map(|g| g.folders.iter()).filter(|f| f.select).map(|f| lower(&f.path)).collect();
        let loose: u64 = r.groups[0]
            .files
            .iter()
            .filter(|f| f.select && !selected_dirs.iter().any(|d| inside(&lower(&f.path), d)))
            .map(|f| f.size)
            .sum();
        let folders_total: u64 = r.folders.iter().map(|g| g.reclaimable).sum();
        assert_eq!(r.reclaimable, folders_total + loose);
        assert!(r.reclaimable < folders_total + r.groups[0].reclaimable || r.groups[0].reclaimable == loose);
    }

    #[test]
    fn delete_rules() {
        let t = Tree::new("delete");
        let a = bytes(40, 70_000);
        t.file("Pictures\\a.bin", &a);
        t.file("Downloads\\a.bin", &a);
        t.file("Downloads\\a (1).bin", &a);
        let b = bytes(41, 50_000);
        t.file("Pictures\\b.bin", &b);
        t.file("Downloads\\b.bin", &b);
        let c = bytes(42, 30_000);
        t.file("Pictures\\c.bin", &c);
        t.file("Downloads\\c.bin", &c);
        t.file("Pictures\\unique.bin", &bytes(43, 10_000));
        let mut st = run(&t, &opts(1, false));
        // The "recycle" here deletes the test's own temp files.
        let rm = |p: &str| -> Result<(), String> {
            assert!(PathBuf::from(p).starts_with(std::env::temp_dir()));
            std::fs::remove_file(p).map_err(|e| e.to_string())
        };
        let s = |rel: &str| t.s(rel);

        assert!(delete_with(&mut st, 8, &[s("Downloads\\a.bin")], &rm).unwrap_err().contains("out of date"));
        let e = delete_with(&mut st, 7, &[s("Pictures\\a.bin"), s("Downloads\\a.bin"), s("Downloads\\a (1).bin")], &rm).unwrap_err();
        assert!(e.contains("at least one copy"), "{e}");
        let e = delete_with(&mut st, 7, &[s("Pictures\\unique.bin")], &rm).unwrap_err();
        assert!(e.contains("aren't in the latest results"), "{e}");
        assert!(t.path("Pictures\\a.bin").exists() && t.path("Pictures\\unique.bin").exists());

        let r = delete_with(&mut st, 7, &[s("Downloads\\a.bin")], &rm).unwrap();
        assert_eq!((r.removed, r.freed, r.failed.len()), (1, 70_000, 0));
        assert!(!t.path("Downloads\\a.bin").exists());
        // The stored result follows: that path is gone, the group keeps its other two copies.
        let g = render(&st).groups.into_iter().find(|g| g.size == 70_000).unwrap();
        assert_eq!(g.files.len(), 2);
        assert!(delete_with(&mut st, 7, &[s("Downloads\\a.bin")], &rm).unwrap_err().contains("aren't in the latest results"));
        let r = delete_with(&mut st, 7, &[s("Downloads\\a (1).bin")], &rm).unwrap();
        assert_eq!(r.removed, 1);
        assert!(!render(&st).groups.iter().any(|g| g.size == 70_000), "a group with one copy left is dropped");
        assert!(t.path("Pictures\\a.bin").exists());

        // A file that changed since the scan is left alone.
        std::thread::sleep(Duration::from_millis(20));
        let mut b2 = b.clone();
        b2[0] ^= 1;
        std::fs::write(t.path("Downloads\\b.bin"), &b2).unwrap();
        let r = delete_with(&mut st, 7, &[s("Downloads\\b.bin")], &rm).unwrap();
        assert_eq!(r.removed, 0);
        assert!(r.failed[0].error.contains("changed since the scan"), "{:?}", r.failed);
        assert!(t.path("Downloads\\b.bin").exists());

        // The kept copy changed: the other copy stays.
        std::thread::sleep(Duration::from_millis(20));
        let mut c2 = c.clone();
        c2[0] ^= 1;
        std::fs::write(t.path("Pictures\\c.bin"), &c2).unwrap();
        let r = delete_with(&mut st, 7, &[s("Downloads\\c.bin")], &rm).unwrap();
        assert_eq!(r.removed, 0);
        assert!(r.failed[0].error.contains("kept is gone or changed"), "{:?}", r.failed);
        assert!(t.path("Downloads\\c.bin").exists());
    }

    #[test]
    fn delete_folder_copy_counts_once() {
        let t = Tree::new("delfolder");
        let (p, q) = (bytes(50, 600_000), bytes(51, 600_000));
        t.file("A\\p.bin", &p);
        t.file("A\\q.bin", &q);
        t.file("Downloads\\A\\p.bin", &p);
        t.file("Downloads\\A\\q.bin", &q);
        t.file("Loose\\p.bin", &p);
        t.file("Downloads\\other.txt", b"something else");
        let mut st = run(&t, &opts(1, true));
        let r0 = render(&st);
        assert_eq!(r0.folders.len(), 1);
        let rm = |p: &str| -> Result<(), String> {
            assert!(PathBuf::from(p).starts_with(std::env::temp_dir()));
            std::fs::remove_dir_all(p).or_else(|_| std::fs::remove_file(p)).map_err(|e| e.to_string())
        };
        // Folder copy + a file inside it: removed once, freed once.
        let r = delete_with(&mut st, 7, &[t.s("Downloads\\A\\p.bin"), t.s("Downloads\\A")], &rm).unwrap();
        assert_eq!((r.removed, r.freed), (1, 1_200_000), "{:?}", r.failed);
        let r1 = render(&st);
        assert!(r1.folders.is_empty());
        // p still has A\p.bin and Loose\p.bin.
        assert_eq!(names(&r1), vec![vec!["p.bin", "p.bin"]]);
    }

    #[test]
    fn real_recycle_of_a_temp_copy() {
        let t = Tree::new("recycle");
        let a = bytes(60, 5_000);
        t.file("keep\\x.bin", &a);
        t.file("Downloads\\x.bin", &a);
        let mut st = run(&t, &opts(1, false));
        let r = delete_with(&mut st, 7, &[t.s("Downloads\\x.bin")], &|p| crate::manage::recycle_sync(&[p.to_string()])).unwrap();
        assert_eq!((r.removed, r.freed), (1, 5_000), "{:?}", r.failed);
        assert!(!t.path("Downloads\\x.bin").exists());
        assert!(t.path("keep\\x.bin").exists());
    }

    fn media(h: u64, v: u64) -> Media {
        Media { sig: Sig { h, v, mean: 100.0, aspect: 1.5, grid: [100; 256], rgb: [100; 3], detail: 100 }, duration_ms: None }
    }

    #[test]
    fn dhash_of_synthetic_pixels() {
        // A 9x9 gradient with a bright block, a noisy copy, and a mirrored (different) picture.
        let base: Vec<u8> = (0..81).map(|i| ((i % 9) * 20 + (i / 9) * 7 + if (30..34).contains(&i) { 60 } else { 0 }) as u8).collect();
        let noisy: Vec<u8> = base.iter().enumerate().map(|(i, &x)| x.saturating_add((i % 3) as u8)).collect();
        let mirrored: Vec<u8> = (0..81).map(|i| base[(i / 9) * 9 + 8 - i % 9]).collect();
        let s0 = sig_from_grey(&base, 9, 9).unwrap();
        let s1 = sig_from_grey(&noisy, 9, 9).unwrap();
        let s2 = sig_from_grey(&mirrored, 9, 9).unwrap();
        assert!(dist(&s0, &s1) <= 2, "{}", dist(&s0, &s1));
        assert!(dist(&s0, &s2) > 40, "{}", dist(&s0, &s2));
        assert_eq!(similarity(0), 100);
        assert_eq!(similarity(6), 91);
        // A bigger, scaled-up rendering of the same picture hashes (almost) the same.
        let big: Vec<u8> = (0..90 * 72).map(|i| base[((i / 90) / 8) * 9 + (i % 90) / 10]).collect();
        let s3 = sig_from_grey(&big, 90, 72).unwrap();
        assert!(dist(&s0, &s3) <= 6, "{}", dist(&s0, &s3));
        // Flat pictures of different brightness never match.
        let white = sig_from_grey(&[250u8; 81], 9, 9).unwrap();
        let grey = sig_from_grey(&[128u8; 81], 9, 9).unwrap();
        assert_eq!(dist(&white, &grey), 0);
        let (w, g) = (Media { sig: white, duration_ms: None }, Media { sig: grey, duration_ms: None });
        assert!(accept(&w, &g, false).is_none());
    }

    #[test]
    fn grouping_never_drifts_from_the_keeper() {
        // a~b (4 bits), b~c (4 bits), a-c 8 bits apart: c can't join a's group.
        let items = [media(0, 0), media(0xF, 0xF), media(0xFF, 0xFF)];
        let pairs = similar_pairs(&items, false, &AtomicBool::new(false)).unwrap();
        assert_eq!(pairs, vec![(0, 1, 4), (1, 2, 4)]);
        let g = cluster(3, &pairs, |a, b| a.cmp(&b));
        assert_eq!(g, vec![vec![0, 1]]);
        // With b as the best keeper, everything is within 4 of it.
        let g = cluster(3, &pairs, |a, b| (a != 1).cmp(&(b != 1)).then(a.cmp(&b)));
        assert_eq!(g, vec![vec![1, 0, 2]]);
        // Videos also need matching durations.
        let mut x = media(0, 0);
        let mut y = media(0x3FF, 0x3FF);
        x.duration_ms = Some(60_000);
        y.duration_ms = Some(61_500);
        assert_eq!(accept(&x, &y, true), Some(10));
        y.duration_ms = Some(64_000);
        assert_eq!(accept(&x, &y, true), None);
        // Different shapes don't match.
        let mut z = media(0, 0);
        z.sig.aspect = 0.75;
        assert!(accept(&media(0, 0), &z, false).is_none());
    }

    #[test]
    fn buckets_find_every_close_pair() {
        let mut seed = 0x1234_5678_9ABC_DEFFu64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut items: Vec<Media> = Vec::new();
        for _ in 0..200 {
            let h = rnd();
            items.push(media(h, h));
            // A near copy with up to 7 flipped bits.
            let mut h2 = h;
            for _ in 0..(rnd() % 8) {
                h2 ^= 1 << (rnd() % 64);
            }
            items.push(media(h2, h2));
        }
        for video in [false, true] {
            let fast = similar_pairs(&items, video, &AtomicBool::new(false)).unwrap();
            let mut slow = Vec::new();
            for i in 0..items.len() {
                for j in i + 1..items.len() {
                    if let Some(d) = accept(&items[i], &items[j], video) {
                        slow.push((i, j, d));
                    }
                }
            }
            assert_eq!(fast, slow);
            assert!(fast.len() >= 150);
        }
    }

    #[test]
    fn cancel_stops_a_scan() {
        let t = Tree::new("cancel");
        let a = bytes(70, 300_000);
        for i in 0..20 {
            t.file(&format!("d{i}\\a.bin"), &a);
        }
        let c = Ctx::new(Arc::new(AtomicBool::new(true)));
        assert!(scan(&[t.s("")], &opts(1, true), &places(&t), &c).is_none());
        // Cancelled between listing and hashing.
        let c = ctx();
        let l = list_all(&[t.s("")], 1, &c).unwrap();
        assert_eq!(l.files.len(), 20);
        c.cancel.store(true, Ordering::SeqCst);
        let paths = l.dir_paths();
        let prot = l.protection(&paths, &places(&t));
        assert!(exact_groups(&l, &paths, &prot, &c, false).is_none());
        assert!(full_hash(&t.s("d0\\a.bin"), 300_000, &c.cancel).is_none());
        assert!(full_hash(&t.s("d0\\a.bin"), 300_000, &AtomicBool::new(false)).is_some());
    }

    #[test]
    fn roots_are_merged_and_checked() {
        let t = Tree::new("roots");
        std::fs::create_dir_all(t.path("a\\b")).unwrap();
        std::fs::create_dir_all(t.path("ab")).unwrap();
        let r = prepare_roots(&[t.s("a\\b"), t.s("a"), t.s("ab"), t.s("a\\")]).unwrap();
        assert_eq!(r, vec![t.s("a"), t.s("ab")]);
        assert!(prepare_roots(&[t.s("missing")]).unwrap_err().contains("doesn't exist"));
        assert!(prepare_roots(&[]).is_err());
        t.file("f.txt", b"x");
        assert!(prepare_roots(&[t.s("f.txt")]).unwrap_err().contains("not a folder"));
        assert!(inside(r"c:\a\b", r"c:\a") && inside(r"c:\a", r"c:\a") && inside(r"c:\x", r"c:\"));
        assert!(!inside(r"c:\ab", r"c:\a"));
    }

    /// A directory junction `link` -> `target` (no admin needed), inside the test's own tree.
    fn junction(t: &Tree, link: &str, target: &str) {
        let (l, g) = (t.path(link), t.path(target));
        assert!(l.starts_with(std::env::temp_dir()) && g.starts_with(std::env::temp_dir()));
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&l)
            .arg(&g)
            .stdout(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "mklink /J failed");
    }

    /// Removes a junction made by `junction` (the link only, never what it points to).
    fn unjunction(t: &Tree, link: &str) {
        let l = t.path(link);
        assert!(l.starts_with(std::env::temp_dir()));
        let _ = std::fs::remove_dir(&l);
    }

    /// Sets a folder's modified time to `days` days after 2020-01-01.
    fn age_dir(t: &Tree, rel: &str, days: u64) {
        let f = std::fs::OpenOptions::new()
            .access_mode(0x100) // FILE_WRITE_ATTRIBUTES
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(t.path(rel))
            .unwrap();
        f.set_modified(UNIX_EPOCH + Duration::from_secs(1_577_836_800 + days * 86_400)).unwrap();
    }

    fn hide_system(t: &Tree, rel: &str) {
        let w = crate::util::wide(&t.s(rel));
        assert!(unsafe { windows_sys::Win32::Storage::FileSystem::SetFileAttributesW(w.as_ptr(), ATTR_HIDDEN | ATTR_SYSTEM) } != 0);
    }

    fn folder_names(r: &DupesResult) -> Vec<Vec<String>> {
        let mut fg: Vec<Vec<String>> = r
            .folders
            .iter()
            .map(|g| {
                let mut v: Vec<String> = g.folders.iter().map(|f| f.path.clone()).collect();
                v.sort();
                v
            })
            .collect();
        fg.sort();
        fg
    }

    #[test]
    fn roots_through_a_junction_are_merged() {
        let t = Tree::new("rootlink");
        t.file("real\\Trip\\a.bin", b"a");
        junction(&t, "link", "real");
        // The junction and its target are one place: the one named by its real path stays.
        assert_eq!(prepare_roots(&[t.s("link"), t.s("real")]).unwrap(), vec![t.s("real")]);
        // A junction leading inside another root is merged into it, and the other way round.
        assert_eq!(prepare_roots(&[t.s("link"), t.s("")]).unwrap(), vec![t.s("")]);
        assert_eq!(prepare_roots(&[t.s("real\\Trip"), t.s("link")]).unwrap(), vec![t.s("link")]);
        assert_eq!(final_path(&t.s("link\\Trip")), Some(norm_lower(&t.s("real\\Trip"))));
        assert!(same_object(&t.s("link\\Trip"), &t.s("real\\Trip")));
        assert!(!same_object(&t.s("real\\Trip"), &t.s("real")));
        // Unresolvable paths keep their own text.
        let r = merge_roots(&[r"X:\a".into(), r"X:\a\b".into(), r"X:\c".into()], &[r"x:\a".into(), r"x:\a\b".into(), r"x:\a".into()]);
        assert_eq!(r, vec![r"X:\a".to_string()]);
        unjunction(&t, "link");
    }

    #[test]
    fn one_folder_through_two_roots_is_not_two_copies() {
        let t = Tree::new("aliasroots");
        let (a, b) = (bytes(80, 600_000), bytes(81, 600_000));
        t.file("real\\Trip\\a.bin", &a);
        t.file("real\\Trip\\b.bin", &b);
        t.file("real\\other.txt", b"other");
        junction(&t, "link", "real");
        // Straight to the scan (prepare_roots would merge them): every folder is listed twice.
        let mut st = scan(&[t.s("real"), t.s("link")], &opts(1, true), &places(&t), &ctx()).expect("scan");
        st.scan_id = 7;
        let r = render(&st);
        assert!(r.folders.is_empty(), "{:?}", folder_names(&r));
        assert!(r.groups.is_empty(), "{:?}", names(&r));
        unjunction(&t, "link");
    }

    #[test]
    fn delete_never_counts_an_alias_as_the_kept_copy() {
        let t = Tree::new("aliasdel");
        let (a, b) = (bytes(82, 600_000), bytes(83, 600_000));
        for dir in ["Real\\Trip", "Copy\\Trip"] {
            t.file(&format!("{dir}\\a.bin"), &a);
            t.file(&format!("{dir}\\b.bin"), &b);
            t.age(&format!("{dir}\\a.bin"), 30);
            t.age(&format!("{dir}\\b.bin"), 31);
        }
        t.file("Real\\r.txt", b"real");
        t.file("Copy\\c.txt", b"copy");
        let mut st = run(&t, &opts(1, true));
        assert_eq!(st.folders.len(), 1);
        // The stored copy becomes the same folder seen through a junction (what a mapped drive
        // pointing back at this PC looks like): identical contents, but not another copy.
        junction(&t, "Link", "Real");
        let alias = t.s("Link\\Trip");
        for m in st.folders[0].members.iter_mut() {
            if m.lower == lower(&t.s("Copy\\Trip")) {
                m.path = alias.clone();
                m.lower = lower(&alias);
            }
        }
        let never = |p: &str| -> Result<(), String> { panic!("recycled {p}") };
        let r = delete_with(&mut st, 7, &[t.s("Real\\Trip")], &never).unwrap();
        assert_eq!(r.removed, 0);
        assert!(r.failed[0].error.contains("same item"), "{:?}", r.failed);
        assert!(t.path("Real\\Trip\\a.bin").exists());
        unjunction(&t, "Link");
    }

    #[test]
    fn folder_copies_are_rechecked_before_recycling() {
        let t = Tree::new("folderchk");
        let (a, b) = (bytes(84, 600_000), bytes(85, 600_000));
        for dir in ["Work\\Report", "Old\\Report"] {
            t.file(&format!("{dir}\\a.bin"), &a);
            t.file(&format!("{dir}\\b.bin"), &b);
        }
        t.file("Work\\w.txt", b"work");
        t.file("Old\\o.txt", b"old");
        let mut st = run(&t, &opts(1, true));
        let r = render(&st);
        assert_eq!(r.folders.len(), 1, "{:?}", folder_names(&r));
        let sel = r.folders[0].folders.iter().find(|f| f.select).unwrap().path.clone();
        let keep = r.folders[0].folders.iter().find(|f| f.keep).unwrap().path.clone();
        let never = |p: &str| -> Result<(), String> { panic!("recycled {p}") };

        // A file saved into the selected copy after the scan: the folder stays.
        let new_file = PathBuf::from(&sel).join("new photo.jpg");
        std::fs::write(&new_file, b"only here").unwrap();
        let r = delete_with(&mut st, 7, &[sel.clone()], &never).unwrap();
        assert_eq!(r.removed, 0);
        assert!(r.failed[0].error.contains("This folder changed since the scan"), "{:?}", r.failed);
        std::fs::remove_file(&new_file).unwrap();

        // A same-size edit inside the selected copy is a change too.
        std::thread::sleep(Duration::from_millis(20));
        let mut a2 = a.clone();
        a2[0] ^= 1;
        std::fs::write(PathBuf::from(&sel).join("a.bin"), &a2).unwrap();
        let r = delete_with(&mut st, 7, &[sel.clone()], &never).unwrap();
        assert!(r.failed[0].error.contains("This folder changed since the scan"), "{:?}", r.failed);
        std::fs::write(PathBuf::from(&sel).join("a.bin"), &a).unwrap();

        // The kept copy lost a file: the other copy no longer has a twin, so it stays.
        let mut st = run(&t, &opts(1, true));
        std::fs::remove_file(PathBuf::from(&keep).join("b.bin")).unwrap();
        let r = delete_with(&mut st, 7, &[sel.clone()], &never).unwrap();
        assert_eq!(r.removed, 0);
        assert!(r.failed[0].error.contains("kept is gone or changed"), "{:?}", r.failed);
        assert!(PathBuf::from(&sel).join("b.bin").exists());
    }

    #[test]
    fn hidden_system_content_is_part_of_a_folder_copy() {
        let t = Tree::new("hiddensys");
        let (p, q) = (bytes(86, 600_000), bytes(87, 600_000));
        for dir in ["A", "B", "C", "D"] {
            t.file(&format!("{dir}\\p.bin"), &p);
            t.file(&format!("{dir}\\q.bin"), &q);
        }
        // B also holds a hidden system file, C a hidden system folder: neither is a copy of A.
        t.file("B\\AlbumArt_Large.jpg", b"album art only in B");
        hide_system(&t, "B\\AlbumArt_Large.jpg");
        t.file("C\\private\\s.txt", b"secret");
        hide_system(&t, "C\\private");
        let r = render(&run(&t, &opts(1, true)));
        assert_eq!(folder_names(&r), vec![vec![t.s("A"), t.s("D")]]);
        // A hidden system file is still never a duplicate on its own.
        assert!(!names(&r).iter().flatten().any(|n| n.contains("albumart")));
    }

    #[test]
    fn recommended_selection_never_takes_every_copy() {
        let t = Tree::new("overlap");
        let (x, y) = (bytes(88, 600_000), bytes(89, 600_000));
        // The original folder (deeper path, older folder) and an Explorer copy (shorter path, new
        // folder time, same file times), plus a loose third copy of x.
        for dir in ["Pictures\\2019\\Italy trip", "Pictures\\Italy"] {
            t.file(&format!("{dir}\\x.bin"), &x);
            t.file(&format!("{dir}\\y.bin"), &y);
            t.age(&format!("{dir}\\x.bin"), 100);
            t.age(&format!("{dir}\\y.bin"), 100);
        }
        t.file("Pictures\\2019\\notes.txt", b"2019 notes");
        t.file("Loose\\x.bin", &x);
        t.age("Loose\\x.bin", 100);
        age_dir(&t, "Pictures\\2019\\Italy trip", 10);
        age_dir(&t, "Pictures\\Italy", 20);

        let mut st = run(&t, &opts(1, true));
        let r = render(&st);
        assert_eq!(folder_names(&r), vec![vec![t.s("Pictures\\2019\\Italy trip"), t.s("Pictures\\Italy")]]);
        let fk = r.folders[0].folders.iter().find(|f| f.keep).unwrap();
        assert_eq!(fk.path, t.s("Pictures\\2019\\Italy trip"));
        // The file keeper sits in the folder copy that stays.
        assert_eq!(r.groups.len(), 1, "{:?}", names(&r));
        assert_eq!(r.groups[0].files[0].path, t.s("Pictures\\2019\\Italy trip\\x.bin"));
        assert!(r.groups[0].files[0].keep);

        // The whole recommended selection goes through.
        let sel: Vec<String> = r
            .folders
            .iter()
            .flat_map(|g| g.folders.iter())
            .chain(r.groups.iter().flat_map(|g| g.files.iter()))
            .filter(|f| f.select)
            .map(|f| f.path.clone())
            .collect();
        let rm = |p: &str| -> Result<(), String> {
            assert!(PathBuf::from(p).starts_with(std::env::temp_dir()));
            std::fs::remove_dir_all(p).or_else(|_| std::fs::remove_file(p)).map_err(|e| e.to_string())
        };
        let res = delete_with(&mut st, 7, &sel, &rm).expect("the recommended selection is deletable");
        assert!(res.failed.is_empty(), "{:?}", res.failed);
        assert_eq!(res.removed, 2);
        assert!(t.path("Pictures\\2019\\Italy trip\\x.bin").exists());
        assert!(!t.path("Pictures\\Italy").exists() && !t.path("Loose\\x.bin").exists());
    }

    #[test]
    fn similar_pairs_can_be_cancelled() {
        let items = [media(0, 0), media(1, 1), media(3, 3)];
        assert!(similar_pairs(&items, false, &AtomicBool::new(true)).is_none());
        assert_eq!(similar_pairs(&items, false, &AtomicBool::new(false)).unwrap().len(), 3);
        // Far-apart hashes are refused before the grid comparison.
        let mut far = media(u64::MAX, u64::MAX);
        far.sig.grid = [0; 256];
        assert!(accept(&media(0, 0), &far, true).is_none());
    }

    #[test]
    fn png_and_base64() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(base64(b""), "");
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        let png = png_rgba(2, 2, &[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 128]);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        // zlib stream: header, one final stored block with the 2 rows (1 filter byte + 8 bytes each).
        let idat = 8 + 25;
        assert_eq!(&png[idat + 4..idat + 8], b"IDAT");
        let z = &png[idat + 8..];
        assert_eq!(&z[..3], &[0x78, 0x01, 0x01]);
        assert_eq!(u16::from_le_bytes([z[3], z[4]]), 18);
    }

    #[test]
    fn premultiplied_pixels_become_straight_alpha() {
        // One opaque red pixel and one half-transparent premultiplied white pixel (BGRA).
        let p = shell::Pixels { w: 2, h: 1, bgra: vec![0, 0, 255, 255, 128, 128, 128, 128] };
        let (w, h, rgba) = fit_rgba(&p, 96);
        assert_eq!((w, h), (2, 1));
        assert_eq!(rgba, vec![255, 0, 0, 255, 255, 255, 255, 128]);
        // No alpha at all = opaque.
        let p = shell::Pixels { w: 1, h: 1, bgra: vec![10, 20, 30, 0] };
        assert_eq!(fit_rgba(&p, 96).2, vec![30, 20, 10, 255]);
        let grey = grey_of(&p);
        assert_eq!(grey, vec![((30 * 299 + 20 * 587 + 10 * 114) / 1000) as u8]);
    }

    /// The real thing over the default roots (read-only). Prints timings and counts.
    #[test]
    #[ignore = "real run: cargo test dupes_real_default_roots -- --ignored --nocapture"]
    fn dupes_real_default_roots() {
        let roots: Vec<String> = default_roots().into_iter().map(|r| r.path).collect();
        let roots = prepare_roots(&roots).unwrap();
        let similar = std::env::var("PLATTER_DUPES_SIMILAR").is_ok();
        let o = DupesOptions { min_size: 1, similar_images: similar, similar_videos: similar, folders: true };
        let c = Arc::new(ctx());
        let done = Arc::new(AtomicBool::new(false));
        let (c2, d2) = (c.clone(), done.clone());
        let ticker = std::thread::spawn(move || {
            let t = Instant::now();
            let mut last = u8::MAX;
            while !d2.load(Ordering::SeqCst) {
                let p = c2.progress.snapshot(0);
                let ph = c2.progress.phase.load(Ordering::Relaxed);
                if ph != last {
                    println!("{:>7.2} s  phase {}", t.elapsed().as_secs_f64(), p.phase);
                    last = ph;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let t = Instant::now();
        let st = scan(&roots, &o, &Places::real(), &c).unwrap();
        let el = t.elapsed();
        done.store(true, Ordering::SeqCst);
        ticker.join().unwrap();
        let r = render(&st);
        println!("roots: {:?}", r.roots);
        println!(
            "{:.2} s: {} files, {:.2} GB looked at, skipped {:?}; {} exact groups, {} folder groups, {} image groups, {} video groups; reclaimable {:.1} MB",
            el.as_secs_f64(),
            r.files,
            r.bytes as f64 / 1e9,
            r.skipped,
            r.groups.len(),
            r.folders.len(),
            r.images.len(),
            r.videos.len(),
            r.reclaimable as f64 / 1e6
        );
        for g in r.groups.iter().take(8) {
            println!("  group {} cat {} {} x {} B, reclaim {}", g.id, g.cat, g.files.len(), g.size, g.reclaimable);
            for f in &g.files {
                println!("     keep={} select={} {:9} {} — {}", f.keep, f.select, f.risk, f.path, f.note);
            }
        }
        for g in r.folders.iter().take(5) {
            println!("  folders {} {} files {} B", g.id, g.files, g.size);
            for f in &g.folders {
                println!("     keep={} select={} {:9} {} — {}", f.keep, f.select, f.risk, f.path, f.note);
            }
        }
        for g in r.images.iter().chain(&r.videos).take(10) {
            println!("  similar {} {}", g.id, g.kind);
            for f in &g.files {
                println!("     {}% {}x{} {:?} keep={} {} — {}", f.similarity, f.width, f.height, f.duration_ms, f.file.keep, f.file.path, f.file.note);
            }
        }
    }
}
