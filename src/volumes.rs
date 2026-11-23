//! Drive letters, capacity, device properties and the NTFS cluster allocation map.
//! Everything here is plain Win32 so it is fast enough to poll.

use crate::util::{from_wide, system_drive_letter, wide};
use serde::Serialize;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetDriveTypeW, GetLogicalDrives,
    GetVolumeInformationW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
const FSCTL_GET_VOLUME_BITMAP: u32 = 0x0009_006F;
const ERROR_MORE_DATA: u32 = 234;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const SYNCHRONIZE: u32 = 0x0010_0000;

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Volume {
    pub letter: String,
    pub label: String,
    pub file_system: String,
    pub total: u64,
    pub free: u64,
    pub kind: &'static str,
    pub is_system: bool,
    pub serial: String,
    pub cluster_size: u64,
    pub ready: bool,
    /// "ssd" | "hdd" | "unknown"
    pub media: &'static str,
    pub bus: String,
    pub model: String,
    pub read_only: bool,
    pub compressed_supported: bool,
    /// Why the drive is or isn't usable: "ok" | "not-ready" (no media) | "locked" (BitLocker)
    /// | "raw" (unformatted / unrecognised file system) | "unavailable" (offline share, timed out, other error).
    pub status: &'static str,
}

pub fn drive_mask() -> u32 {
    unsafe { GetLogicalDrives() }
}

/// How long one drive may take to answer before it is reported as unavailable.
/// A disconnected mapped share can otherwise block for tens of seconds.
const QUERY_TIMEOUT: Duration = Duration::from_millis(1500);
/// A late answer from a slow drive is reused for this long while a fresh query is still running.
const LATE_RESULT_TTL: Duration = Duration::from_secs(120);

/// One bit per (purpose, letter) for queries still running on a worker thread, so a drive that hangs
/// never gets more than one stuck thread per purpose. Bits 0..26: `list`, bits 32..58: media probes.
static BUSY: AtomicU64 = AtomicU64::new(0);
const PROBE_BIT: u32 = 32;
/// Last completed `query` per letter, including answers that arrived after the deadline.
static LAST: Mutex<Vec<Option<(Instant, Volume)>>> = Mutex::new(Vec::new());

fn claim(bit: u32) -> bool {
    BUSY.fetch_or(1 << bit, Ordering::SeqCst) & (1 << bit) == 0
}

fn release(bit: u32) {
    BUSY.fetch_and(!(1 << bit), Ordering::SeqCst);
}

fn remember(i: usize, v: &Volume) {
    if let Ok(mut last) = LAST.lock() {
        if last.len() < 26 {
            last.resize(26, None);
        }
        last[i] = Some((Instant::now(), v.clone()));
    }
}

fn recent(i: usize, kind: &str) -> Option<Volume> {
    let last = LAST.lock().ok()?;
    let (at, v) = last.get(i)?.as_ref()?;
    (at.elapsed() < LATE_RESULT_TTL && v.kind == kind).then(|| v.clone())
}

fn letters(mask: u32) -> impl Iterator<Item = (usize, String)> {
    (0..26usize).filter(move |i| mask & (1 << i) != 0).map(|i| (i, ((b'A' + i as u8) as char).to_string()))
}

fn root_of(letter: &str) -> Vec<u16> {
    wide(&format!("{letter}:\\"))
}

/// GetDriveTypeW never touches the device, so it is safe to call inline.
fn drive_kind(letter: &str) -> Option<&'static str> {
    let root = root_of(letter);
    Some(match unsafe { GetDriveTypeW(root.as_ptr()) } {
        2 => "removable",
        3 => "fixed",
        4 => "network",
        5 => "optical",
        6 => "ram",
        _ => return None,
    })
}

/// Every drive is queried on its own thread; the list waits at most [`QUERY_TIMEOUT`] in total, and a
/// drive that hasn't answered by then is reported as `ready: false, status: "unavailable"`.
pub fn list() -> Vec<Volume> {
    let sys = system_drive_letter();
    let (tx, rx) = mpsc::channel::<(usize, Volume)>();
    let mut wanted = Vec::new();
    for (i, letter) in letters(drive_mask()) {
        let Some(kind) = drive_kind(&letter) else { continue };
        wanted.push((i, letter.clone(), kind));
        if !claim(i as u32) {
            continue; // an earlier query of this drive is still stuck; don't pile up another
        }
        let (tx, sys) = (tx.clone(), sys.clone());
        let spawned = std::thread::Builder::new().name(format!("platter-vol-{letter}")).spawn(move || {
            let v = query(&letter, kind, &sys);
            remember(i, &v);
            release(i as u32);
            let _ = tx.send((i, v));
        });
        if spawned.is_err() {
            release(i as u32);
        }
    }
    drop(tx);

    let mut got: Vec<Option<Volume>> = vec![None; 26];
    let deadline = Instant::now() + QUERY_TIMEOUT;
    // Ends when every worker has answered (channel disconnected) or the deadline passes.
    while let Ok((i, v)) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        got[i] = Some(v);
    }
    wanted
        .into_iter()
        .map(|(i, letter, kind)| {
            got[i].take().or_else(|| recent(i, kind)).unwrap_or_else(|| unavailable(&letter, kind, &sys))
        })
        .collect()
}

fn unavailable(letter: &str, kind: &'static str, sys: &str) -> Volume {
    Volume {
        letter: letter.to_string(),
        label: String::new(),
        file_system: String::new(),
        total: 0,
        free: 0,
        kind,
        is_system: letter.eq_ignore_ascii_case(sys),
        serial: String::new(),
        cluster_size: 0,
        ready: false,
        media: "unknown",
        bus: String::new(),
        model: String::new(),
        read_only: false,
        compressed_supported: false,
        status: "unavailable",
    }
}

/// Bytes per cluster of one drive, without touching any other drive.
pub fn cluster_size(letter: &str) -> u64 {
    let root = root_of(letter);
    let (mut spc, mut bps, mut nf, mut tc) = (0u32, 0u32, 0u32, 0u32);
    if unsafe { GetDiskFreeSpaceW(root.as_ptr(), &mut spc, &mut bps, &mut nf, &mut tc) } == 0 {
        return 0;
    }
    spc as u64 * bps as u64
}

/// A mapped drive whose share is remembered but not connected. Asking it anything else would make
/// Windows try to reconnect, which can block for the SMB timeout.
fn is_disconnected_share(letter: &str) -> bool {
    #[link(name = "mpr")]
    extern "system" {
        fn WNetGetConnectionW(local: *const u16, remote: *mut u16, len: *mut u32) -> u32;
    }
    const ERROR_CONNECTION_UNAVAIL: u32 = 1201;
    let local = wide(&format!("{letter}:"));
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    unsafe { WNetGetConnectionW(local.as_ptr(), buf.as_mut_ptr(), &mut len) == ERROR_CONNECTION_UNAVAIL }
}

/// Map a GetVolumeInformationW failure to a `status`.
fn failure_status(err: u32, kind: &str) -> &'static str {
    const ERROR_ACCESS_DENIED: u32 = 5;
    const ERROR_NOT_READY: u32 = 21;
    const ERROR_UNRECOGNIZED_VOLUME: u32 = 1005;
    const ERROR_NO_MEDIA_IN_DRIVE: u32 = 1112;
    const ERROR_UNRECOGNIZED_MEDIA: u32 = 1785;
    match err {
        ERROR_NOT_READY | ERROR_NO_MEDIA_IN_DRIVE => "not-ready",
        ERROR_UNRECOGNIZED_VOLUME | ERROR_UNRECOGNIZED_MEDIA => "raw",
        // FVE_E_* (BitLocker, facility 0x31), e.g. FVE_E_LOCKED_VOLUME 0x80310000.
        e if e & 0xFFFF_0000 == 0x8031_0000 => "locked",
        ERROR_ACCESS_DENIED if kind == "fixed" || kind == "removable" => "locked",
        _ if kind == "removable" || kind == "optical" => "not-ready",
        _ => "unavailable",
    }
}

fn query(letter: &str, kind: &'static str, sys: &str) -> Volume {
    if kind == "network" && is_disconnected_share(letter) {
        return unavailable(letter, kind, sys);
    }
    let root = root_of(letter);

    let mut name = [0u16; 261];
    let mut fs = [0u16; 261];
    let mut serial = 0u32;
    let mut flags = 0u32;
    let ready = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            name.as_mut_ptr(),
            name.len() as u32,
            &mut serial,
            std::ptr::null_mut(),
            &mut flags,
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    } != 0;
    let file_system = from_wide(&fs);
    let status = if !ready {
        failure_status(unsafe { GetLastError() }, kind)
    } else if file_system.eq_ignore_ascii_case("RAW") {
        "raw"
    } else {
        "ok"
    };

    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    if ready {
        unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut avail, &mut total, &mut free) };
    }
    let (mut spc, mut bps, mut _nf, mut _tc) = (0u32, 0u32, 0u32, 0u32);
    if ready {
        unsafe { GetDiskFreeSpaceW(root.as_ptr(), &mut spc, &mut bps, &mut _nf, &mut _tc) };
    }

    let dev = if kind == "fixed" || kind == "removable" { device_props(letter) } else { DeviceProps::default() };

    Volume {
        letter: letter.to_string(),
        label: from_wide(&name),
        file_system,
        total,
        free,
        kind,
        is_system: letter.eq_ignore_ascii_case(sys),
        serial: if ready { format!("{:04X}-{:04X}", serial >> 16, serial & 0xFFFF) } else { String::new() },
        cluster_size: spc as u64 * bps as u64,
        ready,
        media: if dev.media.is_empty() { "unknown" } else { dev.media },
        bus: dev.bus,
        model: dev.model,
        read_only: flags & 0x0008_0000 != 0,
        compressed_supported: flags & 0x10 != 0,
        status,
    }
}

/// Per-letter media fingerprint for removable and optical drives, used by the drive watcher to notice
/// a card or disc being inserted, removed or swapped (the drive-letter mask doesn't change then).
/// `Some(0)` = no readable media, `Some(1<<32 | serial)` = mounted volume, `None` = not a removable or
/// optical drive. A drive that doesn't answer within [`QUERY_TIMEOUT`] keeps its value from `prev`.
pub type MediaSignature = [Option<u64>; 26];

pub fn media_signature(mask: u32, prev: &MediaSignature) -> MediaSignature {
    let (tx, rx) = mpsc::channel::<(usize, u64)>();
    let mut out: MediaSignature = [None; 26];
    for (i, letter) in letters(mask) {
        if !matches!(drive_kind(&letter), Some("removable" | "optical")) {
            continue;
        }
        out[i] = prev[i];
        let bit = PROBE_BIT + i as u32;
        if !claim(bit) {
            continue;
        }
        let tx = tx.clone();
        let spawned = std::thread::Builder::new().name(format!("platter-probe-{letter}")).spawn(move || {
            let root = root_of(&letter);
            let mut serial = 0u32;
            let ok = unsafe {
                GetVolumeInformationW(
                    root.as_ptr(),
                    std::ptr::null_mut(),
                    0,
                    &mut serial,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0,
                )
            } != 0;
            release(bit);
            let _ = tx.send((i, if ok { 1u64 << 32 | serial as u64 } else { 0 }));
        });
        if spawned.is_err() {
            release(bit);
        }
    }
    drop(tx);
    let deadline = Instant::now() + QUERY_TIMEOUT;
    while let Ok((i, sig)) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        out[i] = Some(sig);
    }
    out
}

#[derive(Default)]
struct DeviceProps {
    media: &'static str,
    bus: String,
    model: String,
}

fn open_volume(letter: &str) -> Option<HANDLE> {
    open_volume_with(letter, FILE_READ_ATTRIBUTES | SYNCHRONIZE)
}

/// NTFS only answers FSCTL_GET_VOLUME_BITMAP on a handle opened for reading
/// (it returns ERROR_INVALID_FUNCTION otherwise), and Windows grants that only to administrators.
fn open_volume_with(letter: &str, access: u32) -> Option<HANDLE> {
    let path = wide(&format!("\\\\.\\{letter}:"));
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        None
    } else {
        Some(h)
    }
}

#[repr(C)]
struct StoragePropertyQuery {
    property_id: u32,
    query_type: u32,
    additional: [u8; 4],
}

fn storage_query(h: HANDLE, property_id: u32, out: &mut [u8]) -> bool {
    let q = StoragePropertyQuery { property_id, query_type: 0, additional: [0; 4] };
    let mut ret = 0u32;
    unsafe {
        DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &q as *const _ as *const c_void,
            std::mem::size_of::<StoragePropertyQuery>() as u32,
            out.as_mut_ptr() as *mut c_void,
            out.len() as u32,
            &mut ret,
            std::ptr::null_mut(),
        ) != 0
    }
}

fn device_props(letter: &str) -> DeviceProps {
    let Some(h) = open_volume(letter) else { return DeviceProps { media: "unknown", ..Default::default() } };
    let mut props = DeviceProps { media: "unknown", ..Default::default() };

    // StorageDeviceSeekPenaltyProperty (7): DEVICE_SEEK_PENALTY_DESCRIPTOR { u32, u32, BOOLEAN }
    let mut buf = [0u8; 12];
    if storage_query(h, 7, &mut buf) {
        props.media = if buf[8] != 0 { "hdd" } else { "ssd" };
    }

    // StorageDeviceProperty (0): STORAGE_DEVICE_DESCRIPTOR
    let mut buf = vec![0u8; 1024];
    if storage_query(h, 0, &mut buf) {
        let rd = |o: usize| u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]) as usize;
        let cstr = |o: usize| -> String {
            if o == 0 || o >= buf.len() {
                return String::new();
            }
            let end = buf[o..].iter().position(|&b| b == 0).map(|p| o + p).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[o..end]).trim().to_string()
        };
        // Layout: Version, Size, DeviceType(u8), DeviceTypeModifier(u8), RemovableMedia, CommandQueueing,
        // VendorIdOffset(u32 @12), ProductIdOffset(@16), ProductRevisionOffset(@20), SerialNumberOffset(@24), BusType(u32 @28)
        let vendor = cstr(rd(12));
        let product = cstr(rd(16));
        props.model = if vendor.is_empty() || product.starts_with(&vendor) { product } else { format!("{vendor} {product}") };
        props.bus = match rd(28) {
            1 => "SCSI",
            2 => "ATAPI",
            3 => "ATA",
            4 => "IEEE 1394",
            5 => "SSA",
            6 => "Fibre Channel",
            7 => "USB",
            8 => "RAID",
            9 => "iSCSI",
            10 => "SAS",
            11 => "SATA",
            12 => "SD card",
            13 => "MMC",
            14 => "Virtual",
            15 => "Virtual",
            16 => "Storage Spaces",
            17 => "NVMe",
            18 => "SCM",
            19 => "UFS",
            _ => "",
        }
        .to_string();
    }
    unsafe { CloseHandle(h) };
    props
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AllocationMap {
    /// One byte per cell: 0 = every cluster free, 255 = every cluster used.
    pub cells: Vec<u8>,
    pub clusters: u64,
    pub cluster_size: u64,
    pub bytes_per_cell: u64,
    /// Count of separate free regions — a rough measure of free-space fragmentation.
    pub free_runs: u64,
    pub largest_free_run: u64,
}

/// Read the NTFS allocation bitmap and fold it into `cells` buckets in physical order.
pub fn allocation_map(letter: &str, cells: usize, cluster_size: u64) -> Result<AllocationMap, String> {
    let cells = cells.clamp(64, 20_000);
    const GENERIC_READ: u32 = 0x8000_0000;
    let Some(h) = open_volume_with(letter, GENERIC_READ | SYNCHRONIZE) else {
        return Err(if crate::util::is_elevated() {
            "Windows didn't let Platter read this drive's allocation map.".into()
        } else {
            "Windows only shows a drive's allocation map to administrators.".into()
        });
    };

    const CHUNK: usize = 4 << 20; // 4 MB of bitmap = 32M clusters per call
    let mut out = vec![0u8; CHUNK + 16];
    let mut start: i64 = 0;
    let mut total_clusters: u64 = 0;
    let mut used = vec![0u64; cells];
    let mut free_runs = 0u64;
    let mut largest_free = 0u64;
    let mut cur_free = 0u64;
    let mut prev_used = true;

    loop {
        let mut ret = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                h,
                FSCTL_GET_VOLUME_BITMAP,
                &start as *const i64 as *const c_void,
                8,
                out.as_mut_ptr() as *mut c_void,
                out.len() as u32,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        let err = if ok == 0 { unsafe { GetLastError() } } else { 0 };
        if ok == 0 && err != ERROR_MORE_DATA {
            unsafe { CloseHandle(h) };
            return Err(match err {
                5 => "Reading the allocation map needs administrator rights on this drive.".into(),
                1 | 50 => "This drive's file system doesn't expose an allocation map (it isn't NTFS).".into(),
                e => format!("Couldn't read the allocation map (Windows error {e})."),
            });
        }
        let starting_lcn = i64::from_le_bytes(out[0..8].try_into().unwrap()) as u64;
        let bitmap_size = i64::from_le_bytes(out[8..16].try_into().unwrap()) as u64;
        if total_clusters == 0 {
            total_clusters = starting_lcn + bitmap_size;
            if total_clusters == 0 {
                break;
            }
        }
        let bytes = (ret as usize).saturating_sub(16);
        let bits = (bitmap_size).min(bytes as u64 * 8);
        let data = &out[16..16 + bytes];
        let tc = total_clusters as u128;
        let cell_of = |lcn: u64| ((lcn as u128 * cells as u128) / tc) as usize;

        let mut lcn = starting_lcn;
        let end = starting_lcn + bits;
        for &b in data {
            if lcn >= end {
                break;
            }
            let n = (end - lcn).min(8);
            if n == 8 && cell_of(lcn) == cell_of(lcn + 7) {
                used[cell_of(lcn)] += b.count_ones() as u64;
            } else {
                for k in 0..n {
                    if b & (1 << k) != 0 {
                        used[cell_of(lcn + k)] += 1;
                    }
                }
            }
            // free-run tracking
            if b == 0 && n == 8 {
                if prev_used {
                    free_runs += 1;
                }
                cur_free += 8;
                prev_used = false;
            } else if b == 0xFF && n == 8 {
                if !prev_used {
                    largest_free = largest_free.max(cur_free);
                    cur_free = 0;
                }
                prev_used = true;
            } else {
                for k in 0..n {
                    let u = b & (1 << k) != 0;
                    if u {
                        if !prev_used {
                            largest_free = largest_free.max(cur_free);
                            cur_free = 0;
                        }
                    } else {
                        if prev_used {
                            free_runs += 1;
                        }
                        cur_free += 1;
                    }
                    prev_used = u;
                }
            }
            lcn += n;
        }
        if ok != 0 || lcn >= total_clusters {
            break;
        }
        start = lcn as i64;
    }
    largest_free = largest_free.max(cur_free);
    unsafe { CloseHandle(h) };

    if total_clusters == 0 {
        return Err("The drive reported an empty allocation map.".into());
    }
    let tc = total_clusters as u128;
    let cells_out = (0..cells)
        .map(|i| {
            let lo = (i as u128 * tc / cells as u128) as u64;
            let hi = ((i as u128 + 1) * tc / cells as u128) as u64;
            let span = (hi - lo).max(1);
            ((used[i].min(span) * 255) / span) as u8
        })
        .collect();

    Ok(AllocationMap {
        cells: cells_out,
        clusters: total_clusters,
        cluster_size,
        bytes_per_cell: total_clusters * cluster_size / cells as u64,
        free_runs,
        largest_free_run: largest_free * cluster_size,
    })
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_statuses() {
        assert_eq!(failure_status(21, "removable"), "not-ready");
        assert_eq!(failure_status(21, "fixed"), "not-ready");
        assert_eq!(failure_status(1112, "optical"), "not-ready");
        assert_eq!(failure_status(1005, "fixed"), "raw");
        assert_eq!(failure_status(0x8031_0000, "fixed"), "locked");
        assert_eq!(failure_status(5, "fixed"), "locked");
        assert_eq!(failure_status(5, "network"), "unavailable");
        assert_eq!(failure_status(53, "network"), "unavailable");
        assert_eq!(failure_status(31, "optical"), "not-ready");
        assert_eq!(failure_status(31, "fixed"), "unavailable");
    }

    #[test]
    fn list_reports_system_drive_within_the_timeout() {
        let t = Instant::now();
        let vols = list();
        // Each drive is bounded by QUERY_TIMEOUT; allow slack for a busy machine.
        assert!(t.elapsed() < QUERY_TIMEOUT + Duration::from_secs(2), "list took {:?}", t.elapsed());
        let sys = system_drive_letter();
        let c = vols.iter().find(|v| v.letter == sys).expect("system drive listed");
        assert!(c.ready && c.status == "ok" && c.total > 0 && c.is_system, "{c:?}");
        for v in &vols {
            assert!(v.ready == (v.status == "ok") || v.status == "raw", "{v:?}");
            assert!(matches!(v.media, "ssd" | "hdd" | "unknown"), "{v:?}");
        }
        assert!(cluster_size(&sys) > 0);
        assert_eq!(cluster_size(&sys), c.cluster_size);
    }

    /// `cargo test --release list_timing -- --ignored --nocapture`: what one 15 s UI poll costs.
    #[test]
    #[ignore]
    fn list_timing() {
        let _ = list();
        let n = 200;
        let t = Instant::now();
        for _ in 0..n {
            let _ = list();
        }
        let per = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        let t = Instant::now();
        for _ in 0..n {
            std::thread::spawn(|| {}).join().unwrap();
        }
        let spawn = t.elapsed().as_secs_f64() * 1e6 / n as f64;
        let drives = letters(drive_mask()).count();
        println!("list(): {per:.3} ms per call over {drives} drives; spawn+join of one thread: {spawn:.1} µs");
    }

    #[test]
    fn media_signature_only_covers_removable_and_optical() {
        let mask = drive_mask();
        let sig = media_signature(mask, &[None; 26]);
        let sys = (system_drive_letter().as_bytes()[0] - b'A') as usize;
        assert_eq!(sig[sys], None);
        for (i, letter) in letters(mask) {
            // A probed drive that doesn't answer in time keeps its previous value (None here).
            if !matches!(drive_kind(&letter), Some("removable" | "optical")) {
                assert_eq!(sig[i], None, "{letter}");
            }
        }
    }
}
