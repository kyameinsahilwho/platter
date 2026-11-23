//! Space scanner: a fast parallel directory walk that builds a compact tree of folders.
//!
//! The walk lists each folder with one `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)`
//! query, so every entry's size, attributes, reparse tag and file id come for free from the
//! directory listing — no per-file syscalls. Only folders are stored; per folder we keep the 24
//! largest files plus an aggregate of the rest, so even a drive with millions of files stays small
//! in memory.

use serde::Serialize;
use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::Emitter;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FileIdBothDirectoryInfo, GetCompressedFileSizeW, GetFileInformationByHandleEx,
    GetVolumeInformationByHandleW, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_BOTH_DIR_INFO,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};

const TOP_PER_DIR: usize = 24;
const TOP_GLOBAL: usize = 200;

const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_SPARSE: u32 = 0x200;
const ATTR_REPARSE_POINT: u32 = 0x400;
const ATTR_COMPRESSED: u32 = 0x800;
const ATTR_OFFLINE: u32 = 0x1000;
const ATTR_RECALL_ON_OPEN: u32 = 0x4_0000;
const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
const CLOUD_ATTRS: u32 = ATTR_OFFLINE | ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS;
/// Reparse tags with this bit point somewhere else (junctions, symlinks, mount points).
const NAME_SURROGATE: u32 = 0x2000_0000;

const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_NO_MORE_FILES: u32 = 18;
const FILE_SUPPORTS_HARD_LINKS: u32 = 0x0040_0000;

// ---------------------------------------------------------------------------------------------
// Public shapes (see CONTRACT.md)

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgress {
    /// The id `scan_start` returned for this scan.
    pub scan_id: u64,
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub current: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ScanDone {
    /// The id `scan_start` returned for this scan; ignore a scan-done for any other id.
    pub scan_id: u64,
    pub ok: bool,
    pub cancelled: bool,
    pub error: Option<String>,
    pub root_id: usize,
    pub path: String,
    pub total_bytes: u64,
    pub files: u64,
    pub dirs: u64,
    pub denied_dirs: u64,
    pub elapsed_ms: u64,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NodeView {
    pub id: Option<usize>,
    pub name: String,
    pub path: String,
    pub size: u64,
    pub files: u64,
    pub dirs: u64,
    pub is_dir: bool,
    pub is_other: bool,
    pub cat: u8,
    pub cats: [u64; 8],
    pub parent_id: Option<usize>,
    pub denied: bool,
    pub children: Vec<NodeView>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TopFile {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub cat: u8,
}

// ---------------------------------------------------------------------------------------------
// Stored result

struct FileEntry {
    name: Box<str>,
    size: u64,
    cat: u8,
}

struct DirNode {
    /// Folder name; for the root this is the full scanned path.
    name: Box<str>,
    parent: Option<usize>,
    /// Sub-folder ids: a folder's sub-folders get consecutive ids when it is listed.
    children: std::ops::Range<usize>,
    /// Cumulative (includes the whole subtree).
    size: u64,
    files: u64,
    dirs: u64,
    cats: [u64; 8],
    /// The largest files directly in this folder.
    top: Vec<FileEntry>,
    /// Files directly in this folder that are not in `top`.
    other_count: u64,
    other_bytes: u64,
    denied: bool,
}

pub struct ScanResult {
    /// The scan id this result belongs to (0 outside `scan_start`).
    scan_id: u64,
    nodes: Vec<DirNode>,
    top_files: Vec<TopFile>,
}

struct Running {
    gen: u64,
    cancel: Arc<AtomicBool>,
}

static GEN: AtomicU64 = AtomicU64::new(0);

fn running() -> &'static Mutex<Option<Running>> {
    static R: OnceLock<Mutex<Option<Running>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(None))
}

fn result() -> &'static Mutex<Option<Arc<ScanResult>>> {
    static R: OnceLock<Mutex<Option<Arc<ScanResult>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(None))
}

// ---------------------------------------------------------------------------------------------
// Categories

/// 0 video, 1 images, 2 audio, 3 documents, 4 archives & installers, 5 apps & system,
/// 6 developer, 7 other.
#[cfg(test)]
fn category_of(name: &str) -> u8 {
    category_of_wide(&name.encode_utf16().collect::<Vec<_>>())
}

/// `category_of` for a UTF-16 name straight from a directory listing (no String needed).
fn category_of_wide(name: &[u16]) -> u8 {
    let Some(dot) = name.iter().rposition(|&c| c == b'.' as u16) else { return 7 };
    let ext = &name[dot + 1..];
    if ext.is_empty() || ext.len() > 12 {
        return 7;
    }
    let mut buf = [0u8; 12];
    for (i, &c) in ext.iter().enumerate() {
        if c >= 0x80 {
            return 7; // every known extension is ASCII
        }
        buf[i] = (c as u8).to_ascii_lowercase();
    }
    let Ok(ext) = std::str::from_utf8(&buf[..ext.len()]) else { return 7 };
    match ext {
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "m4v" | "flv" | "mpg" | "mpeg" | "ts"
        | "mts" | "m2ts" | "vob" | "3gp" | "3g2" | "ogv" | "divx" | "f4v" | "asf" | "rm"
        | "rmvb" | "braw" | "r3d" | "prproj" => 0,
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "heic" | "heif" | "avif" | "raw"
        | "cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" | "raf" | "psd" | "psb" | "tif"
        | "tiff" | "svg" | "ico" | "jxl" | "xcf" | "kra" | "ai" | "eps" | "jfif" | "tga"
        | "exr" | "hdr" | "dds" => 1,
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" | "opus" | "aiff" | "aif"
        | "alac" | "ape" | "mid" | "midi" | "amr" | "m4b" | "m4p" => 2,
        "pdf" | "doc" | "docx" | "docm" | "xls" | "xlsx" | "xlsm" | "xlsb" | "ppt" | "pptx"
        | "pptm" | "txt" | "rtf" | "odt" | "ods" | "odp" | "csv" | "md" | "epub" | "mobi"
        | "azw3" | "pages" | "numbers" | "key" | "one" | "pst" | "ost" | "msg" | "eml" | "xps"
        | "oxps" | "tex" | "accdb" | "mdb" | "vsdx" | "pub" => 3,
        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "lz" | "lzma"
        | "iso" | "img" | "msi" | "msp" | "msu" | "cab" | "vhd" | "vhdx" | "vmdk" | "vdi"
        | "qcow2" | "wim" | "esd" | "appx" | "appxbundle" | "msix" | "msixbundle" | "dmg"
        | "apk" | "xapk" | "deb" | "rpm" | "swm" => 4,
        "exe" | "dll" | "sys" | "drv" | "ocx" | "mui" | "dat" | "bin" | "pak" | "etl" | "evtx"
        | "cat" | "manifest" | "cpl" | "scr" | "efi" | "inf" | "pnf" | "ttf" | "otf" | "ttc"
        | "fon" | "nls" | "tlb" | "winmd" | "db" | "edb" | "jrs" | "chk" | "regtrans-ms"
        | "blf" | "log" | "msc" | "mum" | "ini" | "cfg" | "tmp" | "ps1xml" | "sdb" | "vxd" | "com"
        | "bak" | "ldf" | "mdf" | "sqlite" | "pck" | "vpk" | "bundle" | "assets" | "ress"
        | "resource" | "ucas" | "utoc" | "uasset" | "forge" => 5,
        "rs" | "js" | "mjs" | "cjs" | "cts" | "tsx" | "jsx" | "py" | "pyi" | "java" | "kt" | "c"
        | "cc" | "cpp" | "cxx" | "h" | "hh" | "hpp" | "cs" | "go" | "rb" | "php" | "json"
        | "map" | "lock" | "o" | "obj" | "pdb" | "lib" | "rlib" | "rmeta" | "a" | "class"
        | "jar" | "wasm" | "node" | "pyc" | "pyd" | "whl" | "ipynb" | "swift" | "scala"
        | "lua" | "dart" | "vue" | "svelte" | "css" | "scss" | "less" | "html" | "htm" | "xml"
        | "yml" | "yaml" | "toml" | "sln" | "csproj" | "vcxproj" | "nupkg" | "ilk" | "idb"
        | "pch" | "ipch" | "gradle" | "sh" | "bat" | "cmd" | "ps1" | "sql" | "d" | "tlog" => 6,
        _ => 7,
    }
}

fn is_dev_dir(name: &str) -> bool {
    ["node_modules", "target", ".git", "__pycache__", ".gradle"]
        .iter()
        .any(|d| d.eq_ignore_ascii_case(name))
}

fn dominant(cats: &[u64; 8]) -> u8 {
    let mut best = 7usize;
    let mut best_v = 0u64;
    for (i, &v) in cats.iter().enumerate() {
        if v > best_v {
            best = i;
            best_v = v;
        }
    }
    best as u8
}

// ---------------------------------------------------------------------------------------------
// Walk
//
// The walk does not recurse. Every folder is its own rayon job spawned into one scope, so stack
// use stays flat however deep a tree goes. Each job lists its folder, records a `DirNode` holding
// only that folder's *own* files under an id handed out up front (a folder's subfolders get
// consecutive ids, all larger than its own), then spawns jobs for its subfolders. When the scope
// drains, the records are collected in id order and subtree totals are summed in a single
// reverse pass over the ids.
//
// Listing uses `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)`, the same directory query
// `FindFirstFileExW` makes, but it also returns each entry's file id and allocation size at no
// extra cost. The file id lets a file with several hard links (WinSxS <-> System32, pnpm stores,
// ...) be counted once; see `LinkSet`.

/// Number of link-set shards (far more than the scan pool's threads, so little contention).
const SHARDS: usize = 64;
/// Listing buffer per call (what `FIND_FIRST_EX_LARGE_FETCH` uses too).
const LIST_BUF_WORDS: usize = 64 * 1024 / 8;

struct Ctx {
    cancel: Arc<AtomicBool>,
    files: AtomicU64,
    dirs: AtomicU64,
    bytes: AtomicU64,
    denied: AtomicU64,
    /// Bytes not counted again because they belong to a hard link already counted elsewhere.
    linked_bytes: AtomicU64,
    /// File ids the hard-link set held when the walk finished (for diagnostics).
    tracked_ids: AtomicU64,
    /// Files whose on-disk size had to be looked up one by one (CompactOS / WOF).
    size_lookups: AtomicU64,
    current: Mutex<String>,
    top_min: AtomicU64,
    top: Mutex<BinaryHeap<Reverse<(u64, String, u8)>>>,
    next_id: AtomicUsize,
    /// Finished folders, by id.
    nodes: NodeStore,
    /// Count each hard-linked file only once (the volume supports hard links).
    dedupe: AtomicBool,
    links: LinkSet,
    /// While set, `WinSxS` folders are parked in `deferred` and walked after everything else.
    defer_sxs: AtomicBool,
    deferred: Mutex<Vec<Job>>,
}

impl Ctx {
    fn new(cancel: Arc<AtomicBool>) -> Self {
        Ctx {
            cancel,
            files: AtomicU64::new(0),
            dirs: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            denied: AtomicU64::new(0),
            linked_bytes: AtomicU64::new(0),
            tracked_ids: AtomicU64::new(0),
            size_lookups: AtomicU64::new(0),
            current: Mutex::new(String::new()),
            top_min: AtomicU64::new(0),
            top: Mutex::new(BinaryHeap::with_capacity(TOP_GLOBAL + 1)),
            next_id: AtomicUsize::new(0),
            nodes: NodeStore::default(),
            dedupe: AtomicBool::new(false),
            links: LinkSet::new(),
            defer_sxs: AtomicBool::new(false),
            deferred: Mutex::new(Vec::new()),
        }
    }

    fn offer_top(&self, size: u64, dir: &[u16], name: &[u16], cat: u8) {
        if size == 0 || size <= self.top_min.load(Ordering::Relaxed) {
            return;
        }
        let mut heap = self.top.lock().unwrap();
        if heap.len() >= TOP_GLOBAL {
            if let Some(Reverse((min, _, _))) = heap.peek() {
                if size <= *min {
                    return;
                }
            }
        }
        heap.push(Reverse((size, String::from_utf16_lossy(&child_path(dir, name)), cat)));
        if heap.len() > TOP_GLOBAL {
            heap.pop();
        }
        if heap.len() >= TOP_GLOBAL {
            if let Some(Reverse((min, _, _))) = heap.peek() {
                self.top_min.store(*min, Ordering::Relaxed);
            }
        }
    }
}

/// Folders per `NodeStore` chunk.
const NODE_CHUNK: usize = 4096;

/// Finished folders stored by id, in fixed-size chunks allocated as ids get used. Assembling the
/// result moves them out chunk by chunk and frees each chunk as it goes, so the folder records
/// never exist twice in full (the old per-thread vectors plus the id-ordered copy, both with
/// growth slack, were most of a full-drive scan's ~300 MB peak).
#[derive(Default)]
struct NodeStore {
    chunks: Mutex<Vec<Option<Vec<Option<DirNode>>>>>,
}

impl NodeStore {
    fn put(&self, id: usize, node: DirNode) {
        let (c, i) = (id / NODE_CHUNK, id % NODE_CHUNK);
        let mut chunks = self.chunks.lock().unwrap();
        if chunks.get(c).map_or(true, |x| x.is_none()) {
            // Allocate a new chunk without holding up the other threads.
            drop(chunks);
            let fresh: Vec<Option<DirNode>> = (0..NODE_CHUNK).map(|_| None).collect();
            chunks = self.chunks.lock().unwrap();
            if chunks.len() <= c {
                chunks.resize_with(c + 1, || None);
            }
            chunks[c].get_or_insert(fresh);
        }
        chunks[c].as_mut().unwrap()[i] = Some(node);
    }

    /// All folders with ids below `total`, in id order. None if any is missing.
    fn take(&self, total: usize) -> Option<Vec<DirNode>> {
        let chunks = std::mem::take(&mut *self.chunks.lock().unwrap());
        let mut nodes = Vec::with_capacity(total);
        for chunk in chunks {
            if nodes.len() == total {
                break;
            }
            for slot in chunk? {
                if nodes.len() == total {
                    break;
                }
                nodes.push(slot?);
            }
        }
        (nodes.len() == total).then_some(nodes)
    }
}

/// Cheap hasher for file ids.
#[derive(Default)]
struct IdHasher(u64);
impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
    }
    fn write_u64(&mut self, x: u64) {
        let m = x.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = m ^ (m >> 29);
    }
}

/// File ids seen so far in this scan. The first directory entry seen for a file counts its bytes;
/// any further hard link to the same file counts 0. Only files with a non-zero size are tracked.
struct LinkSet {
    shards: Vec<Mutex<HashSet<u64, BuildHasherDefault<IdHasher>>>>,
}

impl LinkSet {
    fn new() -> Self {
        LinkSet { shards: (0..SHARDS).map(|_| Mutex::new(HashSet::default())).collect() }
    }

    /// True the first time `id` is offered. Ids of 0 / -1 (not reported) always count.
    fn first(&self, id: i64) -> bool {
        if id == 0 || id == -1 {
            return true;
        }
        let k = id as u64;
        let shard = (k.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) as usize % SHARDS;
        self.shards[shard].lock().unwrap().insert(k)
    }

    /// Frees the set once the walk is done. Returns how many ids it held.
    fn release(&self) -> usize {
        self.shards.iter().map(|s| std::mem::take(&mut *s.lock().unwrap()).len()).sum()
    }
}

struct Job {
    id: usize,
    parent: Option<usize>,
    path: Vec<u16>,
    name: Box<str>,
    dev: bool,
}

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
    // A bare drive ("C:") or share root ("\\server\share") needs its trailing backslash to open
    // the root folder rather than the current directory / the share itself.
    let bare_drive = v.len() == 2 && v[1] == b':' as u16;
    let share_root = starts_unc && !already && !long && v.iter().filter(|&&c| c == bs).count() == 3;
    if bare_drive || share_root {
        v.push(bs);
    }
    v.push(0);
    v
}

fn open_dir(dir: &[u16], access: u32) -> Result<OwnedHandle, u32> {
    let p = open_path(dir);
    let h = unsafe {
        CreateFileW(
            p.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        Err(unsafe { GetLastError() })
    } else {
        Ok(OwnedHandle(h))
    }
}

fn child_path(dir: &[u16], name: &[u16]) -> Vec<u16> {
    let mut v = Vec::with_capacity(dir.len() + name.len() + 1);
    v.extend_from_slice(dir);
    if v.last() != Some(&(b'\\' as u16)) {
        v.push(b'\\' as u16);
    }
    v.extend_from_slice(name);
    v
}

fn join_path(dir: &str, name: &str) -> String {
    if dir.ends_with('\\') {
        format!("{dir}{name}")
    } else {
        format!("{dir}\\{name}")
    }
}

/// One directory entry, as the listing reports it.
struct Entry<'a> {
    name: &'a [u16],
    attrs: u32,
    /// Reparse tag when `attrs` has the reparse-point bit.
    tag: u32,
    end_of_file: u64,
    allocation: u64,
    file_id: i64,
}

thread_local! {
    static LIST_BUF: RefCell<Vec<u64>> = RefCell::new(vec![0u64; LIST_BUF_WORDS]);
}

/// Lists `dir`, calling `f` for every entry except `.` and `..`.
/// Err(()) when the folder exists but couldn't be read (access denied, path too long, ...).
fn list_dir(dir: &[u16], mut f: impl FnMut(&Entry)) -> Result<(), ()> {
    let h = match open_dir(dir, FILE_LIST_DIRECTORY) {
        Ok(h) => h,
        Err(ERROR_FILE_NOT_FOUND) | Err(ERROR_PATH_NOT_FOUND) => return Ok(()),
        Err(_) => return Err(()),
    };
    LIST_BUF.with(|b| {
        let mut buf = b.borrow_mut();
        let bytes = buf.len() * 8;
        let fixed = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
        let mut first = true;
        loop {
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    h.0,
                    FileIdBothDirectoryInfo,
                    buf.as_mut_ptr() as *mut core::ffi::c_void,
                    bytes as u32,
                )
            };
            if ok == 0 {
                let err = unsafe { GetLastError() };
                return match err {
                    ERROR_NO_MORE_FILES | ERROR_FILE_NOT_FOUND => Ok(()),
                    _ if first => Err(()),
                    // A failure part-way through keeps what was read, like FindNextFileW does.
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
                        end_of_file: info.EndOfFile.max(0) as u64,
                        allocation: info.AllocationSize.max(0) as u64,
                        file_id: info.FileId,
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

/// Bytes a file entry is counted as.
///
/// Normally the logical size (what Explorer shows as "Size"). Cloud-only placeholders count 0.
/// Sparse and NTFS-compressed files count what's actually allocated when that's smaller, so a
/// 100 GB sparse VM disk holding 3 GB, or a compressed log folder, isn't overstated (the listing's
/// allocation size is exact for those). CompactOS / WOF-compressed files are handled by
/// `maybe_wof` + `compressed_size`, since the listing can't tell their real size.
fn file_bytes(e: &Entry) -> u64 {
    if e.attrs & CLOUD_ATTRS != 0 {
        return 0;
    }
    if e.attrs & (ATTR_SPARSE | ATTR_COMPRESSED) != 0 {
        return e.end_of_file.min(e.allocation);
    }
    e.end_of_file
}

/// A plain-looking file whose main stream has (almost) nothing allocated: how CompactOS / WOF
/// compressed files show up in a listing (WOF hides their reparse point and reports 0 allocated).
/// Ordinary files always have at least their size allocated, so this needs a per-file lookup only
/// for WOF files and the odd file whose directory entry is stale.
fn maybe_wof(e: &Entry) -> bool {
    e.attrs & (ATTR_SPARSE | ATTR_COMPRESSED | CLOUD_ATTRS) == 0
        && e.end_of_file > 4096
        && e.allocation + 4096 <= e.end_of_file
}

/// On-disk size of a file via `GetCompressedFileSizeW` (WOF reports the compressed size here).
fn compressed_size(dir: &[u16], name: &[u16]) -> Option<u64> {
    let p = open_path(&child_path(dir, name));
    let mut hi = 0u32;
    let lo = unsafe { GetCompressedFileSizeW(p.as_ptr(), &mut hi) };
    if lo == u32::MAX && unsafe { GetLastError() } != 0 {
        return None;
    }
    Some(((hi as u64) << 32) | lo as u64)
}

fn walk_dir<'s>(s: &rayon::Scope<'s>, ctx: &'s Ctx, job: Job) {
    if ctx.cancel.load(Ordering::Relaxed) {
        return;
    }
    let Job { id, parent, path, name, dev } = job;
    let mut node = DirNode {
        name,
        parent,
        children: 0..0,
        size: 0,
        files: 0,
        dirs: 0,
        cats: [0; 8],
        top: Vec::new(),
        other_count: 0,
        other_bytes: 0,
        denied: false,
    };
    let n = ctx.dirs.fetch_add(1, Ordering::Relaxed);
    if n % 64 == 0 {
        if let Ok(mut cur) = ctx.current.try_lock() {
            *cur = String::from_utf16_lossy(&path);
        }
    }

    let dedupe = ctx.dedupe.load(Ordering::Relaxed);
    let mut files: Vec<FileEntry> = Vec::new();
    // Once a folder has more files than it keeps, the smaller ones are folded into "other" as
    // they come: a file no bigger than `keep_min` can't make the folder's top list any more, so
    // its name is never copied out of the listing.
    let mut keep_min: Option<u64> = None;
    let mut subdirs: Vec<(Vec<u16>, Box<str>, bool)> = Vec::new();
    let mut local_bytes = 0u64;
    let listed = list_dir(&path, |e| {
        if e.attrs & ATTR_DIRECTORY != 0 {
            let reparse_link = e.attrs & ATTR_REPARSE_POINT != 0 && e.tag & NAME_SURROGATE != 0;
            // Junctions/symlinks point elsewhere; cloud-only folders hold nothing local.
            let cloud_only = e.attrs & CLOUD_ATTRS != 0;
            if !reparse_link && !cloud_only {
                let fname = String::from_utf16_lossy(e.name);
                let d = dev || is_dev_dir(&fname);
                subdirs.push((child_path(&path, e.name), fname.into_boxed_str(), d));
            } else {
                node.dirs += 1;
            }
        } else {
            let mut size = file_bytes(e);
            if size > 0 && dedupe && !ctx.links.first(e.file_id) {
                // Another hard link to this same file was already counted.
                ctx.linked_bytes.fetch_add(size, Ordering::Relaxed);
                size = 0;
            } else if size > 0 && maybe_wof(e) {
                ctx.size_lookups.fetch_add(1, Ordering::Relaxed);
                if let Some(c) = compressed_size(&path, e.name) {
                    size = size.min(c);
                }
            }
            let cat = if dev { 6 } else { category_of_wide(e.name) };
            node.cats[cat as usize] += size;
            node.size += size;
            node.files += 1;
            local_bytes += size;
            ctx.offer_top(size, &path, e.name, cat);
            if keep_min.is_some_and(|m| size <= m) {
                node.other_count += 1;
                node.other_bytes += size;
            } else {
                files.push(FileEntry { name: String::from_utf16_lossy(e.name).into_boxed_str(), size, cat });
                if files.len() >= 2 * TOP_PER_DIR {
                    keep_largest(&mut files, &mut node);
                    keep_min = files.iter().map(|f| f.size).min();
                }
            }
        }
    });
    if listed.is_err() {
        node.denied = true;
        ctx.denied.fetch_add(1, Ordering::Relaxed);
    }

    ctx.files.fetch_add(node.files, Ordering::Relaxed);
    ctx.bytes.fetch_add(local_bytes, Ordering::Relaxed);

    keep_largest(&mut files, &mut node);
    files.shrink_to_fit();
    node.top = files;

    let defer = ctx.defer_sxs.load(Ordering::Relaxed);
    let first = ctx.next_id.fetch_add(subdirs.len(), Ordering::Relaxed);
    node.children = first..first + subdirs.len();
    for (i, (p, n, d)) in subdirs.into_iter().enumerate() {
        let job = Job { id: first + i, parent: Some(id), path: p, name: n, dev: d };
        if defer && job.name.eq_ignore_ascii_case("WinSxS") {
            ctx.deferred.lock().unwrap().push(job);
        } else {
            s.spawn(move |s| walk_dir(s, ctx, job));
        }
    }
    ctx.nodes.put(id, node);
}

/// Keeps the `TOP_PER_DIR` largest of `files` and folds the rest into the folder's "other" bucket.
fn keep_largest(files: &mut Vec<FileEntry>, node: &mut DirNode) {
    if files.len() > TOP_PER_DIR {
        files.select_nth_unstable_by(TOP_PER_DIR - 1, |a, b| b.size.cmp(&a.size));
        for f in files.drain(TOP_PER_DIR..) {
            node.other_count += 1;
            node.other_bytes += f.size;
        }
    }
}

/// Whether the volume holding `root` has hard links (NTFS, ReFS) worth de-duplicating.
fn volume_has_hard_links(root: &[u16]) -> bool {
    let Ok(h) = open_dir(root, FILE_READ_ATTRIBUTES) else { return false };
    let mut flags = 0u32;
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            h.0,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags,
            std::ptr::null_mut(),
            0,
        )
    };
    ok != 0 && flags & FILE_SUPPORTS_HARD_LINKS != 0
}

/// Collects the per-folder records in id order and sums subtree totals. None if any is missing.
fn assemble(ctx: &Ctx) -> Option<Vec<DirNode>> {
    let mut nodes = ctx.nodes.take(ctx.next_id.load(Ordering::Relaxed))?;
    // Every child id is larger than its parent's, so one reverse pass folds whole subtrees up.
    for id in (1..nodes.len()).rev() {
        let (p, size, files, dirs, cats) = {
            let c = &nodes[id];
            (c.parent?, c.size, c.files, c.dirs, c.cats)
        };
        let pn = &mut nodes[p];
        pn.size += size;
        pn.files += files;
        pn.dirs += dirs + 1;
        for i in 0..8 {
            pn.cats[i] += cats[i];
        }
    }
    Some(nodes)
}

/// Normalizes "C:" / "c:/foo/" style input into a Windows path.
pub fn normalize_root(path: &str) -> String {
    let mut p = path.trim().trim_matches('"').replace('/', "\\");
    let b = p.as_bytes();
    if b.len() == 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        p.push('\\');
    }
    while p.len() > 3 && p.ends_with('\\') && !p.ends_with(":\\") {
        p.pop();
    }
    if p.len() >= 2 && p.as_bytes()[1] == b':' {
        let first = p[..1].to_ascii_uppercase();
        p.replace_range(..1, &first);
    }
    p
}

/// Runs a whole scan synchronously. Returns None if cancelled.
/// Worker threads for a scan. Opening and listing a folder is almost all kernel time, and past
/// ~8 threads the file system mostly contends with itself: on a 16-thread PC, a full C:\ scan
/// (450 000 folders) took ~11 s and ~75 s of CPU with 8 threads, but ~14 s and ~90-120 s of CPU
/// with 16-32.
fn scan_threads() -> usize {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    (cpus / 2).clamp(4, 8)
}

fn run_scan(root: &str, ctx: &Ctx) -> Option<ScanResult> {
    let wroot: Vec<u16> = root.encode_utf16().collect();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(scan_threads())
        // The walk doesn't recurse, so stack use is flat whatever the folder depth.
        .stack_size(4 * 1024 * 1024)
        .thread_name(|i| format!("platter-scan-{i}"))
        .build()
        .ok()?;

    // Hard links are counted once. Component-store folders (WinSxS) are walked last, so the
    // files they share with System32 & co. are counted where Windows uses them — the same split
    // `DISM /AnalyzeComponentStore` reports as "shared with Windows" — rather than at random.
    let dedupe = volume_has_hard_links(&wroot);
    ctx.dedupe.store(dedupe, Ordering::Relaxed);
    ctx.defer_sxs.store(dedupe, Ordering::Relaxed);
    ctx.next_id.store(1, Ordering::Relaxed);
    let root_job = Job { id: 0, parent: None, path: wroot, name: root.into(), dev: false };
    pool.scope(|s| walk_dir(s, ctx, root_job));
    ctx.defer_sxs.store(false, Ordering::Relaxed);
    loop {
        let later = std::mem::take(&mut *ctx.deferred.lock().unwrap());
        if later.is_empty() {
            break;
        }
        pool.scope(|s| {
            for job in later {
                s.spawn(move |s| walk_dir(s, ctx, job));
            }
        });
    }
    // Hard-link ids are only needed during the walk; free them before the result is assembled.
    ctx.tracked_ids.store(ctx.links.release() as u64, Ordering::Relaxed);
    if ctx.cancel.load(Ordering::Relaxed) {
        return None;
    }
    let nodes = assemble(ctx)?;
    let mut top: Vec<TopFile> = ctx
        .top
        .lock()
        .unwrap()
        .drain()
        .map(|Reverse((size, path, cat))| {
            let name = path.rsplit('\\').next().unwrap_or(&path).to_string();
            TopFile { path, name, size, cat }
        })
        .collect();
    top.sort_by(|a, b| b.size.cmp(&a.size));
    Some(ScanResult { scan_id: 0, nodes, top_files: top })
}

// ---------------------------------------------------------------------------------------------
// Views

fn node_path(nodes: &[DirNode], id: usize) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let mut cur = Some(id);
    while let Some(i) = cur {
        parts.push(&nodes[i].name);
        cur = nodes[i].parent;
    }
    let mut out = String::new();
    for p in parts.iter().rev() {
        if out.is_empty() {
            out.push_str(p);
        } else {
            out = join_path(&out, p);
        }
    }
    out
}

fn view(nodes: &[DirNode], id: usize, depth: usize, path: String) -> NodeView {
    let n = &nodes[id];
    let mut children = Vec::new();
    if depth > 0 {
        let mut direct_cats = n.cats;
        for c in n.children.clone() {
            let cn = &nodes[c];
            for i in 0..8 {
                direct_cats[i] = direct_cats[i].saturating_sub(cn.cats[i]);
            }
            let cpath = join_path(&path, &cn.name);
            children.push(view(nodes, c, depth - 1, cpath));
        }
        for f in &n.top {
            direct_cats[f.cat as usize] = direct_cats[f.cat as usize].saturating_sub(f.size);
            let mut cats = [0u64; 8];
            cats[f.cat as usize] = f.size;
            children.push(NodeView {
                id: None,
                name: f.name.to_string(),
                path: join_path(&path, &f.name),
                size: f.size,
                files: 1,
                dirs: 0,
                is_dir: false,
                is_other: false,
                cat: f.cat,
                cats,
                parent_id: Some(id),
                denied: false,
                children: Vec::new(),
            });
        }
        if n.other_count > 0 {
            let name = if n.other_count == 1 {
                "1 smaller file".to_string()
            } else {
                format!("{} smaller files", n.other_count)
            };
            children.push(NodeView {
                id: None,
                name,
                path: path.clone(),
                size: n.other_bytes,
                files: n.other_count,
                dirs: 0,
                is_dir: false,
                is_other: true,
                cat: dominant(&direct_cats),
                cats: direct_cats,
                parent_id: Some(id),
                denied: false,
                children: Vec::new(),
            });
        }
        children.sort_by(|a, b| b.size.cmp(&a.size));
    }
    NodeView {
        id: Some(id),
        name: n.name.to_string(),
        path,
        size: n.size,
        files: n.files,
        dirs: n.dirs,
        is_dir: true,
        is_other: false,
        cat: dominant(&n.cats),
        cats: n.cats,
        parent_id: n.parent,
        denied: n.denied,
        children,
    }
}

impl ScanResult {
    pub fn node(&self, id: usize, depth: usize) -> Option<NodeView> {
        if id >= self.nodes.len() {
            return None;
        }
        Some(view(&self.nodes, id, depth, node_path(&self.nodes, id)))
    }
}

// ---------------------------------------------------------------------------------------------
// Commands

#[tauri::command]
/// Starts a background scan and returns its scan id. Every `scan-progress` / `scan-done` event
/// carries the id of the scan that sent it; ids only ever increase.
pub async fn scan_start(app: tauri::AppHandle, path: String) -> Result<u64, String> {
    let root = normalize_root(&path);
    if root.is_empty() {
        return Err("Choose a drive or folder to scan.".into());
    }
    match std::fs::metadata(&root) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err("That's a file, not a folder. Choose a drive or folder to scan.".into()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err("Windows denied access to that folder. Restart Platter as administrator and try again.".into())
        }
        Err(_) => return Err("That drive or folder doesn't exist or isn't ready.".into()),
    }

    let cancel = Arc::new(AtomicBool::new(false));
    // The id is taken while holding the lock, so the scan installed last is always the newest
    // one: two overlapping scan_start calls can't leave an older scan running and a newer one
    // cancelled.
    let gen = {
        let mut r = running().lock().unwrap();
        let gen = GEN.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(old) = r.take() {
            old.cancel.store(true, Ordering::SeqCst);
        }
        *r = Some(Running { gen, cancel: cancel.clone() });
        gen
    };

    let spawned = std::thread::Builder::new()
        .name("platter-scan".into())
        .spawn(move || {
            let started = Instant::now();
            let ctx = Arc::new(Ctx::new(cancel.clone()));
            let done = Arc::new(AtomicBool::new(false));

            // Progress ticker.
            {
                let ctx = ctx.clone();
                let done = done.clone();
                let app = app.clone();
                std::thread::spawn(move || loop {
                    std::thread::sleep(Duration::from_millis(120));
                    if done.load(Ordering::SeqCst) || ctx.cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    if GEN.load(Ordering::SeqCst) != gen {
                        break;
                    }
                    let current = ctx.current.lock().map(|c| c.clone()).unwrap_or_default();
                    let _ = app.emit(
                        "scan-progress",
                        ScanProgress {
                            scan_id: gen,
                            files: ctx.files.load(Ordering::Relaxed),
                            dirs: ctx.dirs.load(Ordering::Relaxed),
                            bytes: ctx.bytes.load(Ordering::Relaxed),
                            current,
                        },
                    );
                });
            }

            release_old_result(gen);
            let res = run_scan(&root, &ctx);
            done.store(true, Ordering::SeqCst);
            let elapsed_ms = started.elapsed().as_millis() as u64;

            let mut payload = ScanDone {
                scan_id: gen,
                ok: false,
                cancelled: false,
                error: None,
                root_id: 0,
                path: root.clone(),
                total_bytes: ctx.bytes.load(Ordering::Relaxed),
                files: ctx.files.load(Ordering::Relaxed),
                dirs: ctx.dirs.load(Ordering::Relaxed),
                denied_dirs: ctx.denied.load(Ordering::Relaxed),
                elapsed_ms,
            };

            // Only the newest scan may publish its result or announce completion.
            let mut store = result().lock().unwrap();
            if GEN.load(Ordering::SeqCst) != gen {
                return;
            }
            match res {
                Some(mut r) => {
                    r.scan_id = gen;
                    let root_node = &r.nodes[0];
                    payload.ok = true;
                    payload.total_bytes = root_node.size;
                    payload.files = root_node.files;
                    payload.dirs = root_node.dirs;
                    *store = Some(Arc::new(r));
                }
                None => {
                    payload.cancelled = cancel.load(Ordering::SeqCst);
                    if !payload.cancelled {
                        payload.error = Some("The scan couldn't be completed.".into());
                    }
                }
            }
            {
                let mut r = running().lock().unwrap();
                if r.as_ref().map(|x| x.gen) == Some(gen) {
                    *r = None;
                }
            }
            let _ = app.emit("scan-done", payload);
            drop(store);
        });
    if spawned.is_err() {
        let mut r = running().lock().unwrap();
        if r.as_ref().map(|x| x.gen) == Some(gen) {
            *r = None;
        }
        return Err("Couldn't start the scan.".into());
    }
    Ok(gen)
}

/// Frees the previous scan's folders as soon as scan `gen` starts (if it is still the newest):
/// the UI drops its view of them at that point, and keeping them would hold two scans' worth of
/// memory (~150 MB for a full drive) until this one finishes.
fn release_old_result(gen: u64) {
    let old = {
        let mut store = result().lock().unwrap();
        if GEN.load(Ordering::SeqCst) == gen {
            store.take()
        } else {
            None
        }
    };
    drop(old); // outside the lock: dropping a big result takes a moment
}

#[tauri::command]
pub async fn scan_cancel() -> Result<(), String> {
    if let Some(r) = running().lock().unwrap().as_ref() {
        r.cancel.store(true, Ordering::SeqCst);
    }
    Ok(())
}

/// The published result. With `scan_id`, only if it's that scan's result: node ids from one scan
/// must never be resolved against another scan's folders.
fn current_result(scan_id: Option<u64>) -> Result<Arc<ScanResult>, String> {
    let r = result()
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "There are no scan results yet. Scan a drive or folder first.".to_string())?;
    match scan_id {
        Some(id) if id != r.scan_id => {
            Err("Those scan results were replaced by a newer scan. Scan again.".into())
        }
        _ => Ok(r),
    }
}

/// `scan_id` (optional, JS `scanId`): the id `scan_start` returned; a mismatch is an error.
#[tauri::command]
pub async fn scan_node(id: usize, depth: usize, scan_id: Option<u64>) -> Result<NodeView, String> {
    let r = current_result(scan_id)?;
    tauri::async_runtime::spawn_blocking(move || {
        r.node(id, depth.min(4))
            .ok_or_else(|| "That folder is no longer in the scan results. Scan again.".to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn scan_top_files(limit: usize, scan_id: Option<u64>) -> Result<Vec<TopFile>, String> {
    let r = current_result(scan_id)?;
    Ok(r.top_files.iter().take(limit).cloned().collect())
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize() {
        assert_eq!(normalize_root("c:"), "C:\\");
        assert_eq!(normalize_root("C:\\"), "C:\\");
        assert_eq!(normalize_root("c:/Users/"), "C:\\Users");
    }

    #[test]
    fn categories() {
        assert_eq!(category_of("Movie.MKV"), 0);
        assert_eq!(category_of("a.heic"), 1);
        assert_eq!(category_of("pagefile.sys"), 5);
        assert_eq!(category_of("main.rs"), 6);
        assert_eq!(category_of("README"), 7);
        assert_eq!(category_of("photo.JPEG"), 1);
        assert_eq!(category_of("caf\u{e9}.mp\u{e9}"), 7);
        assert_eq!(category_of("archive.tar.gz"), 4);
        assert_eq!(category_of("trailing."), 7);
    }

    /// Scans %LOCALAPPDATA% (or the temp dir) and prints totals. Run with
    /// `cargo test scan_real -- --ignored --nocapture` (set PLATTER_SCAN_TEST to pick a folder).
    #[test]
    #[ignore = "slow; scans a real folder: cargo test scan_real -- --ignored --nocapture"]
    fn scan_real_folder() {
        let root = std::env::var("PLATTER_SCAN_TEST")
            .ok()
            .or_else(|| std::env::var("LOCALAPPDATA").ok())
            .unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned());
        let root = normalize_root(&root);
        let ctx = Ctx::new(Arc::new(AtomicBool::new(false)));
        let t = Instant::now();
        let r = run_scan(&root, &ctx).expect("scan");
        let el = t.elapsed();
        let v = r.node(0, 2).unwrap();
        println!(
            "{root}: {} bytes ({:.2} GB), {} files, {} dirs, {} denied, {} nodes, {:?}",
            v.size,
            v.size as f64 / 1e9,
            v.files,
            v.dirs,
            ctx.denied.load(Ordering::Relaxed),
            r.nodes.len(),
            el
        );
        println!(
            "  hard links: dedupe={} {:.2} GB not double-counted, {} file ids tracked; {} WOF size lookups",
            ctx.dedupe.load(Ordering::Relaxed),
            ctx.linked_bytes.load(Ordering::Relaxed) as f64 / 1e9,
            ctx.tracked_ids.load(Ordering::Relaxed),
            ctx.size_lookups.load(Ordering::Relaxed)
        );
        assert_eq!(v.files, ctx.files.load(Ordering::Relaxed));
        assert_eq!(v.cats.iter().sum::<u64>(), v.size);
        let child_sum: u64 = v.children.iter().map(|c| c.size).sum();
        assert_eq!(child_sum, v.size, "children must add up to the parent");
        for c in v.children.iter().take(8) {
            println!("  {:>14} {:5} {} {}", c.size, c.cat, if c.is_dir { "D" } else { " " }, c.name);
        }
        for f in r.top_files.iter().take(5) {
            println!("  top {:>14} {}", f.size, f.path);
        }
        if r.top_files.len() > 1 {
            assert!(r.top_files[0].size >= r.top_files[1].size);
        }
    }

    /// Scans the same folder three times in a row the way `scan_start` does (the old result is
    /// released when the next scan starts), so the harness's peak working set shows whether a
    /// replaced result is freed. PLATTER_KEEP_OLD=1 keeps it until the new one is published.
    #[test]
    #[ignore = "slow; scans a real folder three times: cargo test scan_replace -- --ignored --nocapture"]
    fn scan_replace_releases_memory() {
        let root = normalize_root(&std::env::var("PLATTER_SCAN_TEST").unwrap_or_else(|_| "C:\\".into()));
        for i in 0..3 {
            let gen = GEN.fetch_add(1, Ordering::SeqCst) + 1;
            if std::env::var_os("PLATTER_KEEP_OLD").is_none() {
                release_old_result(gen);
            }
            let ctx = Ctx::new(Arc::new(AtomicBool::new(false)));
            let t = Instant::now();
            let mut r = run_scan(&root, &ctx).expect("scan");
            r.scan_id = gen;
            println!("scan {i}: {} folders in {:?}", r.nodes.len(), t.elapsed());
            *result().lock().unwrap() = Some(Arc::new(r));
        }
        assert!(current_result(None).is_ok());
    }

    #[test]
    fn keeps_the_largest_files_of_big_folders() {
        // 300 files in scrambled size order: the 24 largest stay listed, the rest fold into one
        // bucket, and nothing is lost on the way.
        let t = TempTree::new("top");
        let mut total = 0u64;
        for i in 0..300u64 {
            let size = (i * 7919) % 300 + 1; // a permutation of 1..=300
            total += size;
            std::fs::write(t.0.join(format!("f{i:03}.txt")), vec![b'x'; size as usize]).unwrap();
        }
        let ctx = Ctx::new(Arc::new(AtomicBool::new(false)));
        let r = run_scan(&normalize_root(&t.0.to_string_lossy()), &ctx).expect("scan");
        check_tree(&r);
        let n = &r.nodes[0];
        let mut kept: Vec<u64> = n.top.iter().map(|f| f.size).collect();
        kept.sort_unstable();
        assert_eq!(kept, (277..=300).collect::<Vec<u64>>());
        assert_eq!((n.other_count, n.files, n.size), (276, 300, total));
        assert_eq!(n.other_bytes, (1..=276).sum::<u64>());
        assert_eq!(n.cats[3], total, "all .txt files are documents");
    }

    /// A throwaway folder under %TEMP%, removed on drop.
    struct TempTree(std::path::PathBuf);
    impl TempTree {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "platter-scan-test-{tag}-{}-{}",
                std::process::id(),
                Instant::now().elapsed().as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempTree(p)
        }
    }
    impl Drop for TempTree {
        fn drop(&mut self) {
            assert!(self.0.starts_with(std::env::temp_dir()));
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn check_tree(r: &ScanResult) {
        for (id, n) in r.nodes.iter().enumerate() {
            if let Some(p) = n.parent {
                assert!(p < id, "parent ids come first");
                assert!(r.nodes[p].children.contains(&id));
                assert!(n.children.start > id || n.children.is_empty(), "child ids come after their parent");
            }
            let own: u64 = n.top.iter().map(|f| f.size).sum::<u64>() + n.other_bytes;
            let kids: u64 = n.children.clone().map(|c| r.nodes[c].size).sum();
            assert_eq!(n.size, own + kids, "folder size = own files + subfolders");
            assert_eq!(n.cats.iter().sum::<u64>(), n.size);
            let kid_dirs: u64 = n.children.clone().map(|c| r.nodes[c].dirs + 1).sum();
            assert!(n.dirs >= kid_dirs);
        }
    }

    #[test]
    fn hard_links_counted_once() {
        let t = TempTree::new("links");
        let a = t.0.join("a");
        let b = t.0.join("b").join("WinSxS");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("big.dll"), vec![7u8; 300_000]).unwrap();
        std::fs::write(a.join("other.txt"), vec![1u8; 1_000]).unwrap();
        std::fs::hard_link(a.join("big.dll"), b.join("big.dll")).unwrap();
        std::fs::hard_link(a.join("big.dll"), a.join("big-copy.dll")).unwrap();

        let root = normalize_root(&t.0.to_string_lossy());
        let ctx = Ctx::new(Arc::new(AtomicBool::new(false)));
        let r = run_scan(&root, &ctx).expect("scan");
        check_tree(&r);
        let v = r.node(0, 1).unwrap();
        if ctx.dedupe.load(Ordering::Relaxed) {
            assert_eq!(v.size, 301_000, "three links to one file count once");
            assert_eq!(ctx.linked_bytes.load(Ordering::Relaxed), 600_000);
            // The WinSxS link is walked last, so the bytes land in `a`.
            let a_view = v.children.iter().find(|c| c.name == "a").unwrap();
            assert_eq!(a_view.size, 301_000);
            assert_eq!(r.top_files.iter().filter(|f| f.size == 300_000).count(), 1);
        } else {
            assert_eq!(v.size, 901_000);
        }
        assert_eq!(v.files, 4);
        assert_eq!(v.dirs, 3);
    }

    #[test]
    fn deep_tree_does_not_overflow() {
        // 3 000 nested folders (~9 000-character path). The old recursive walk needed a stack
        // frame plus rayon join frames per level; this walk's stack use doesn't depend on depth.
        // (Deeper trees work the same way but make the test slow to set up.)
        let t = TempTree::new("deep");
        let levels = 3_000;
        let mut long = format!(r"\\?\{}", t.0.to_string_lossy());
        let mut dirs = Vec::with_capacity(levels);
        for _ in 0..levels {
            long.push_str(r"\dd");
            std::fs::create_dir(&long).unwrap();
            dirs.push(long.clone());
        }
        let leaf = format!(r"{long}\leaf.bin");
        std::fs::write(&leaf, vec![0u8; 4096]).unwrap();

        let root = normalize_root(&t.0.to_string_lossy());
        let ctx = Ctx::new(Arc::new(AtomicBool::new(false)));
        let r = run_scan(&root, &ctx).expect("scan");
        check_tree(&r);
        assert_eq!(r.nodes.len(), levels + 1);
        assert_eq!(r.nodes[0].size, 4096);
        assert_eq!(r.nodes[0].dirs, levels as u64);
        assert_eq!(ctx.denied.load(Ordering::Relaxed), 0);
        // Rendering the deepest folder walks the parent chain without recursion.
        let deepest = r.node(levels, 1).unwrap();
        assert!(deepest.path.ends_with("dd"));
        assert_eq!(deepest.children.len(), 1);
        // The arena is flat, so dropping it needs no deep stack either.
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || drop(r))
            .unwrap()
            .join()
            .unwrap();
        // Clean up deepest-first (only our own temp folder).
        std::fs::remove_file(&leaf).unwrap();
        for d in dirs.iter().rev() {
            std::fs::remove_dir(d).unwrap();
        }
    }

    #[test]
    fn compressed_and_sparse_sizes() {
        // Files in our own temp folder: plain, NTFS-compressed, WOF (CompactOS-style) and sparse.
        let t = TempTree::new("sizes");
        let mut data = vec![0u8; 4 << 20];
        for b in data.iter_mut().step_by(7) {
            *b = 65;
        }
        for n in ["plain.bin", "ntfs.bin", "wof.bin"] {
            std::fs::write(t.0.join(n), &data).unwrap();
        }
        let run = |args: &[&str]| {
            std::process::Command::new(args[0])
                .args(&args[1..])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        let ntfs = t.0.join("ntfs.bin").to_string_lossy().into_owned();
        let wof = t.0.join("wof.bin").to_string_lossy().into_owned();
        let sparse = t.0.join("sparse.bin").to_string_lossy().into_owned();
        let ntfs_ok = run(&["compact", "/c", "/q", &ntfs]);
        let wof_ok = run(&["compact", "/c", "/q", "/exe:xpress4k", &wof]);
        let sparse_ok = run(&["fsutil", "file", "createnew", &sparse, "0"])
            && run(&["fsutil", "sparse", "setflag", &sparse])
            && run(&["fsutil", "file", "seteof", &sparse, "104857600"]);

        let ctx = Ctx::new(Arc::new(AtomicBool::new(false)));
        let r = run_scan(&normalize_root(&t.0.to_string_lossy()), &ctx).expect("scan");
        check_tree(&r);
        let size = |n: &str| r.nodes[0].top.iter().find(|f| &*f.name == n).map(|f| f.size);
        println!(
            "plain={:?} ntfs={:?} wof={:?} sparse={:?} (compact ntfs={ntfs_ok} wof={wof_ok}, sparse={sparse_ok})",
            size("plain.bin"),
            size("ntfs.bin"),
            size("wof.bin"),
            size("sparse.bin")
        );
        assert_eq!(size("plain.bin"), Some(4 << 20));
        if ntfs_ok {
            assert!(size("ntfs.bin").unwrap() < 1 << 20, "NTFS-compressed counts its on-disk size");
        }
        if wof_ok {
            let w = size("wof.bin").unwrap();
            assert!(w > 0 && w < 1 << 20, "WOF-compressed counts its compressed size, not 0");
        }
        if sparse_ok {
            assert!(size("sparse.bin").unwrap_or(0) < 1 << 20, "empty sparse file isn't 100 MB");
        }
    }

    #[test]
    fn cancelled_scan_returns_none() {
        let t = TempTree::new("cancel");
        std::fs::create_dir_all(t.0.join("x").join("y")).unwrap();
        let ctx = Ctx::new(Arc::new(AtomicBool::new(true)));
        assert!(run_scan(&normalize_root(&t.0.to_string_lossy()), &ctx).is_none());
    }

    #[test]
    fn open_paths() {
        let w = |s: &str| {
            let v = open_path(&s.encode_utf16().collect::<Vec<_>>());
            assert_eq!(v.last(), Some(&0));
            String::from_utf16_lossy(&v[..v.len() - 1])
        };
        assert_eq!(w(r"C:\"), r"C:\");
        assert_eq!(w("C:"), r"C:\");
        assert_eq!(w(r"C:\Users"), r"C:\Users");
        assert_eq!(w(r"\\srv\share"), r"\\srv\share\");
        assert_eq!(w(r"\\srv\share\dir"), r"\\srv\share\dir");
        let long = format!(r"C:\{}", "a".repeat(300));
        assert_eq!(w(&long), format!(r"\\?\{long}"));
        let unc = format!(r"\\srv\share\{}", "a".repeat(300));
        assert_eq!(w(&unc), format!(r"\\?\UNC\srv\share\{}", "a".repeat(300)));
    }
}

