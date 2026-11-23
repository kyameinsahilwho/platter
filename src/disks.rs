//! Physical disks and their partition layout.
//!
//! Read in-process from the Windows Storage WMI classes (`MSFT_Disk`, `MSFT_PhysicalDisk`,
//! `MSFT_Partition`, and — elevated only — `MSFT_StorageReliabilityCounter`) through WMI's COM API
//! (see [`wmi`]), so no `powershell.exe` is started on the hot path. The old single PowerShell/CIM
//! script is kept as a fallback for when WMI can't be reached in-process; both produce the same raw
//! shapes, and the numeric enums are decoded here in Rust.

use crate::util;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalDisk {
    pub number: u32,
    pub model: String,
    pub size: u64,
    /// "GPT" | "MBR" | "RAW"
    pub partition_style: String,
    pub bus: String,
    /// "ssd" | "hdd" | "unknown"
    pub media: String,
    /// "Healthy" | "Warning" | "Unhealthy" | "Unknown"
    pub health: String,
    pub operational: String,
    pub is_boot: bool,
    pub is_system: bool,
    /// False when the counters need administrator rights or the drive doesn't report them.
    pub reliability_available: bool,
    pub temperature: Option<f64>,
    pub temperature_max: Option<f64>,
    /// Percentage of rated life used.
    pub wear: Option<f64>,
    pub power_on_hours: Option<u64>,
    pub read_errors: Option<u64>,
    pub write_errors: Option<u64>,
    pub partitions: Vec<Partition>,
    pub unallocated: u64,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Partition {
    pub number: u32,
    pub offset: u64,
    pub size: u64,
    pub letter: Option<String>,
    pub kind: String,
    pub is_system: bool,
    pub is_boot: bool,
    pub is_hidden: bool,
}

// ---- raw shapes emitted by the script (PowerShell happily emits nulls, so everything is lenient) ----

#[derive(Deserialize, Default, Debug)]
#[serde(default, rename_all = "camelCase")]
struct RawDisk {
    number: Option<u32>,
    model: Option<String>,
    size: Option<u64>,
    partition_style: Option<u32>,
    disk_bus: Option<u32>,
    pd_bus: Option<u32>,
    media: Option<u32>,
    health: Option<u32>,
    operational: Vec<u32>,
    is_boot: Option<bool>,
    is_system: Option<bool>,
    rc: Option<RawReliability>,
    partitions: Vec<RawPartition>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, rename_all = "camelCase")]
struct RawReliability {
    temperature: Option<f64>,
    temperature_max: Option<f64>,
    wear: Option<f64>,
    power_on_hours: Option<u64>,
    read_errors: Option<u64>,
    write_errors: Option<u64>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, rename_all = "camelCase")]
struct RawPartition {
    number: Option<u32>,
    offset: Option<u64>,
    size: Option<u64>,
    letter: Option<String>,
    gpt: Option<String>,
    mbr: Option<u32>,
    is_system: Option<bool>,
    is_boot: Option<bool>,
    is_hidden: Option<bool>,
}

const SCRIPT: &str = r#"
$ErrorActionPreference='Stop'
$ns='root/Microsoft/Windows/Storage'
$pds=@(Get-CimInstance -Namespace $ns -ClassName MSFT_PhysicalDisk -ErrorAction SilentlyContinue)
$parts=@(Get-CimInstance -Namespace $ns -ClassName MSFT_Partition -ErrorAction SilentlyContinue)
$out=@(foreach($d in @(Get-CimInstance -Namespace $ns -ClassName MSFT_Disk)){
  $pd=$null
  foreach($p in $pds){ if("$($p.DeviceId)" -eq "$($d.Number)"){ $pd=$p; break } }
  $rc=$null
  if($el -and $pd){
    try {
      $c=Get-CimAssociatedInstance -InputObject $pd -ResultClassName MSFT_StorageReliabilityCounter -ErrorAction Stop | Select-Object -First 1
      if($c){ $rc=[ordered]@{temperature=$c.Temperature;temperatureMax=$c.TemperatureMax;wear=$c.Wear;powerOnHours=$c.PowerOnHours;readErrors=$c.ReadErrorsTotal;writeErrors=$c.WriteErrorsTotal} }
    } catch { $rc=$null }
  }
  [ordered]@{
    number=$d.Number; model="$($d.FriendlyName)".Trim(); size=$d.Size; partitionStyle=$d.PartitionStyle;
    diskBus=$d.BusType; pdBus=$(if($pd){$pd.BusType}else{$null}); media=$(if($pd){$pd.MediaType}else{$null});
    health=$d.HealthStatus; operational=@($d.OperationalStatus | ForEach-Object { [int]$_ });
    isBoot=[bool]$d.IsBoot; isSystem=[bool]$d.IsSystem; rc=$rc;
    partitions=@(foreach($p in $parts){ if($p.DiskNumber -eq $d.Number){
      [ordered]@{ number=$p.PartitionNumber; offset=$p.Offset; size=$p.Size;
        letter=$(if($p.DriveLetter -and [int][char]$p.DriveLetter -ne 0){[string]$p.DriveLetter}else{$null});
        gpt="$($p.GptType)"; mbr=$p.MbrType; isSystem=[bool]$p.IsSystem; isBoot=[bool]$p.IsBoot; isHidden=[bool]$p.IsHidden }
    } })
  }
})
ConvertTo-Json -InputObject $out -Depth 6 -Compress
"#;

/// Anything smaller than this left between partitions is alignment slack, not usable space.
const UNALLOCATED_MIN: u64 = 16 << 20;

pub fn list() -> Result<Vec<PhysicalDisk>, String> {
    let elevated = util::is_elevated();
    let disks = match wmi::raw_disks(elevated) {
        Ok(raw) => finish(raw),
        Err(hr) => match wmi::unreachable_reason(hr) {
            Some(why) => return Err(format!("Windows didn't report the disk layout: {why}.")),
            // In-process route problems (thread, apartment, generic failures): try the script.
            None => list_powershell(elevated)?,
        },
    };
    crate::perf::mark("rust:disks-done");
    crate::cache::write("disks", &disks);
    Ok(disks)
}

/// The original single-script CIM query through PowerShell; used only when in-process WMI fails.
fn list_powershell(elevated: bool) -> Result<Vec<PhysicalDisk>, String> {
    let el = if elevated { "$true" } else { "$false" };
    let out = util::powershell(&format!("$el={el};{SCRIPT}"))
        .map_err(|e| format!("Windows didn't report the disk layout: {}.", e.trim().trim_end_matches('.')))?;
    parse(&out)
}

fn parse(json: &str) -> Result<Vec<PhysicalDisk>, String> {
    let json = json.trim();
    if json.is_empty() {
        return Ok(vec![]);
    }
    let raw: Vec<RawDisk> =
        serde_json::from_str(json).map_err(|_| "Windows returned disk information Platter couldn't read.".to_string())?;
    Ok(finish(raw))
}

fn finish(raw: Vec<RawDisk>) -> Vec<PhysicalDisk> {
    let mut disks: Vec<PhysicalDisk> = raw.into_iter().filter_map(convert).collect();
    disks.sort_by_key(|d| d.number);
    disks
}

fn convert(d: RawDisk) -> Option<PhysicalDisk> {
    let number = d.number?;
    let size = d.size.unwrap_or(0);
    let style = match d.partition_style {
        Some(1) => "MBR",
        Some(2) => "GPT",
        _ => "RAW",
    };
    let offsets_known = d.partitions.iter().all(|p| p.offset.is_some());
    let mut partitions: Vec<Partition> = d
        .partitions
        .into_iter()
        .map(|p| Partition {
            number: p.number.unwrap_or(0),
            offset: p.offset.unwrap_or(0),
            size: p.size.unwrap_or(0),
            letter: p
                .letter
                .map(|l| l.trim().trim_end_matches(':').to_ascii_uppercase())
                .filter(|l| l.len() == 1 && l.chars().all(|c| c.is_ascii_alphabetic())),
            kind: partition_kind(p.gpt.as_deref(), p.mbr),
            is_system: p.is_system.unwrap_or(false),
            is_boot: p.is_boot.unwrap_or(false),
            is_hidden: p.is_hidden.unwrap_or(false),
        })
        .collect();
    partitions.sort_by_key(|p| p.offset);

    let unallocated = size.saturating_sub(allocated(&partitions, size, offsets_known));
    let unallocated = if unallocated < UNALLOCATED_MIN { 0 } else { unallocated };

    let rc = d.rc.unwrap_or_default();
    // 0 °C means "not reported", not a frozen drive.
    let temperature = rc.temperature.filter(|&t| t > 0.0);
    let temperature_max = rc.temperature_max.filter(|&t| t > 0.0);
    let reliability_available = temperature.is_some()
        || rc.wear.is_some()
        || rc.power_on_hours.is_some()
        || rc.read_errors.is_some()
        || rc.write_errors.is_some();

    Some(PhysicalDisk {
        number,
        model: d.model.map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).unwrap_or_else(|| format!("Disk {number}")),
        size,
        partition_style: style.into(),
        bus: bus_name(d.pd_bus.or(d.disk_bus)).into(),
        media: match d.media {
            Some(3) => "hdd",
            Some(4) | Some(5) => "ssd", // 5 = SCM (storage-class memory)
            _ => "unknown",
        }
        .into(),
        health: match d.health {
            Some(0) => "Healthy",
            Some(1) => "Warning",
            Some(2) => "Unhealthy",
            _ => "Unknown",
        }
        .into(),
        operational: operational_text(&d.operational),
        is_boot: d.is_boot.unwrap_or(false),
        is_system: d.is_system.unwrap_or(false),
        reliability_available,
        temperature,
        temperature_max,
        wear: rc.wear,
        power_on_hours: rc.power_on_hours,
        read_errors: rc.read_errors,
        write_errors: rc.write_errors,
        partitions,
        unallocated,
    })
}

/// Bytes of the disk covered by partitions. On MBR disks Windows lists the extended container *and*
/// the logical partitions inside it, so plain summing would count the logical ones twice. Measure the
/// union of the partitions' byte ranges instead (the container covers its logicals and any free space
/// inside it, which Windows shows as "Free space", not "Unallocated").
fn allocated(partitions: &[Partition], disk_size: u64, offsets_known: bool) -> u64 {
    if !offsets_known {
        // Without offsets, fall back to the sizes, leaving out the containers.
        return partitions.iter().filter(|p| p.kind != "Extended").map(|p| p.size).sum();
    }
    let mut ranges: Vec<(u64, u64)> = partitions
        .iter()
        .filter(|p| p.size > 0)
        .map(|p| (p.offset.min(disk_size), p.offset.saturating_add(p.size).min(disk_size)))
        .filter(|(s, e)| e > s)
        .collect();
    ranges.sort_unstable();
    let mut total = 0u64;
    let mut cur: Option<(u64, u64)> = None;
    for (s, e) in ranges {
        cur = match cur {
            Some((cs, ce)) if s <= ce => Some((cs, ce.max(e))),
            Some((cs, ce)) => {
                total += ce - cs;
                Some((s, e))
            }
            None => Some((s, e)),
        };
    }
    if let Some((cs, ce)) = cur {
        total += ce - cs;
    }
    total
}

fn bus_name(code: Option<u32>) -> &'static str {
    match code {
        Some(1) => "SCSI",
        Some(2) => "ATAPI",
        Some(3) => "ATA",
        Some(4) => "IEEE 1394",
        Some(5) => "SSA",
        Some(6) => "Fibre Channel",
        Some(7) => "USB",
        Some(8) => "RAID",
        Some(9) => "iSCSI",
        Some(10) => "SAS",
        Some(11) => "SATA",
        Some(12) => "SD card",
        Some(13) => "MMC",
        Some(14) | Some(15) => "Virtual",
        Some(16) => "Storage Spaces",
        Some(17) => "NVMe",
        Some(18) => "SCM",
        Some(19) => "UFS",
        _ => "Unknown",
    }
}

/// MSFT_Disk.OperationalStatus codes → the words `Get-Disk` would show.
fn operational_text(codes: &[u32]) -> String {
    let words: Vec<&str> = codes
        .iter()
        .map(|c| match c {
            0 => "Unknown",
            1 => "Other",
            2 => "OK",
            3 => "Degraded",
            4 => "Stressed",
            5 => "Predictive failure",
            6 => "Error",
            7 => "Non-recoverable error",
            8 => "Starting",
            9 => "Stopping",
            10 => "Stopped",
            11 => "In service",
            12 => "No contact",
            13 => "Lost communication",
            14 => "Aborted",
            15 => "Dormant",
            16 => "Supporting entity in error",
            17 => "Completed",
            18 => "Power mode",
            0xD010 => "Online",
            0xD011 => "Not ready",
            0xD012 => "No media",
            0xD013 => "Offline",
            0xD014 => "Failed",
            _ => "Unknown",
        })
        .collect();
    if words.is_empty() {
        "Unknown".into()
    } else {
        words.join(", ")
    }
}

fn partition_kind(gpt: Option<&str>, mbr: Option<u32>) -> String {
    let guid = gpt.unwrap_or("").trim().trim_matches(|c| c == '{' || c == '}').to_ascii_lowercase();
    if !guid.is_empty() {
        return match guid.as_str() {
            "c12a7328-f81f-11d2-ba4b-00a0c93ec93b" => "EFI system",
            "e3c9e316-0b5c-4db8-817d-f92df00215ae" => "Microsoft reserved",
            "de94bba4-06d1-4d40-a16a-bfd50179d6ac" => "Recovery",
            "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7" => "Basic data",
            "5808c8aa-7e8f-42e0-85d2-e1e90434cfb3" => "Dynamic (metadata)",
            "af9b60a0-1431-4f62-bc68-3311714a69ad" => "Dynamic (data)",
            "e75caf8f-f680-4cee-afa3-b001e56efc2d" => "Storage Spaces",
            "0fc63daf-8483-4772-8e79-3d69d8477de4" => "Linux",
            "0657fd6d-a4ab-43c4-84e5-0933c84b4f4f" => "Linux swap",
            "e6d6d379-f507-44c2-a23c-238f2a3df928" => "Linux LVM",
            "48465300-0000-11aa-aa11-00306543ecac" => "Apple HFS+",
            "7c3457ef-0000-11aa-aa11-00306543ecac" => "Apple APFS",
            "21686148-6449-6e6f-744e-656564454649" => "BIOS boot",
            _ => "Other",
        }
        .into();
    }
    match mbr {
        Some(0x07) => "Basic data",
        Some(0x01) | Some(0x04) | Some(0x06) | Some(0x0E) => "FAT",
        Some(0x0B) | Some(0x0C) => "FAT32",
        Some(0x05) | Some(0x0F) | Some(0x85) => "Extended",
        Some(0x27) => "Recovery",
        Some(0xEF) => "EFI system",
        Some(0x42) => "Dynamic (data)",
        Some(0x83) => "Linux",
        Some(0x82) => "Linux swap",
        Some(0x8E) => "Linux LVM",
        Some(0x11) | Some(0x14) | Some(0x16) | Some(0x17) | Some(0x1B) | Some(0x1C) | Some(0x1E) => "Hidden",
        Some(0xEE) => "GPT protective",
        Some(_) => "Other",
        None => "Unknown",
    }
    .into()
}

// ---------------------------------------------------------------------------------------------
// Minimal COM plumbing (raw vtables; windows-sys has no interface wrappers). Shared with optimize.rs.
// ---------------------------------------------------------------------------------------------

pub(crate) mod com {
    use std::ffi::c_void;
    use windows_sys::core::{BSTR, GUID, HRESULT};
    use windows_sys::Win32::Foundation::{SysAllocString, SysFreeString};
    use windows_sys::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoSetProxyBlanket, CoUninitialize};

    #[link(name = "oleaut32")]
    extern "system" {
        fn VariantClear(v: *mut c_void) -> HRESULT;
    }

    const CLSCTX_INPROC_SERVER: u32 = 0x1;
    const COINIT_MULTITHREADED: u32 = 0x0;

    const VT_EMPTY: u16 = 0;
    const VT_NULL: u16 = 1;
    const VT_I2: u16 = 2;
    const VT_I4: u16 = 3;
    const VT_R4: u16 = 4;
    const VT_R8: u16 = 5;
    const VT_BSTR: u16 = 8;
    const VT_BOOL: u16 = 11;
    const VT_I1: u16 = 16;
    const VT_UI1: u16 = 17;
    const VT_UI2: u16 = 18;
    const VT_UI4: u16 = 19;
    const VT_I8: u16 = 20;
    const VT_UI8: u16 = 21;
    const VT_INT: u16 = 22;
    const VT_UINT: u16 = 23;
    const VT_ARRAY: u16 = 0x2000;

    /// Runs `f` on a fresh thread joined to the multithreaded apartment, so the apartment state of
    /// pooled worker threads (which other modules may have set) never matters.
    pub fn on_mta_thread<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> Option<R> {
        std::thread::Builder::new()
            .name("platter-com".into())
            .spawn(move || in_mta(f))
            .ok()?
            .join()
            .ok()
    }

    /// Runs `f` with this thread in the multithreaded apartment. Only for threads Platter created
    /// itself (fresh or scoped), never pooled ones.
    pub fn in_mta<R>(f: impl FnOnce() -> R) -> R {
        let hr = unsafe { CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED) };
        let r = f();
        if hr >= 0 {
            unsafe { CoUninitialize() };
        }
        r
    }

    /// Some(reason) when `hr` says the Windows service behind a COM server is disabled, stopped, missing
    /// or unreachable. That is system-wide: a PowerShell fallback talks to the same service and would
    /// fail the same way after a 0.3-2 s powershell.exe spawn, so callers report it directly. Everything
    /// else (apartment errors, a panicked thread, generic failures) returns None and may fall back.
    pub fn service_unreachable(hr: HRESULT) -> Option<String> {
        let u = hr as u32;
        if u >> 16 != 0x8007 {
            return None; // not HRESULT_FROM_WIN32
        }
        let code = u & 0xffff;
        // ERROR_SERVICE_REQUEST_TIMEOUT, ERROR_SERVICE_DISABLED, ERROR_SERVICE_DOES_NOT_EXIST,
        // ERROR_SERVICE_NOT_ACTIVE, RPC_S_SERVER_UNAVAILABLE
        if !matches!(code, 1053 | 1058 | 1060 | 1062 | 1722) {
            return None;
        }
        let msg = std::io::Error::from_raw_os_error(code as i32).to_string();
        let msg = msg.split(" (os error").next().unwrap_or(&msg).trim().trim_end_matches('.');
        let mut c = msg.chars();
        Some(match c.next() {
            Some(f) => f.to_lowercase().chain(c).collect(),
            None => format!("error {code}"),
        })
    }

    /// The `idx`-th entry of a COM object's vtable, as the function-pointer type `F`.
    ///
    /// # Safety
    /// `obj` must be a live interface pointer whose vtable has a method of type `F` at `idx`.
    pub unsafe fn method<F: Copy>(obj: *mut c_void, idx: usize) -> F {
        let vtbl = *(obj as *const *const *const c_void);
        std::mem::transmute_copy(&*vtbl.add(idx))
    }

    /// An owned interface pointer, released on drop.
    pub struct Unk(pub *mut c_void);
    impl Drop for Unk {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { method::<unsafe extern "system" fn(*mut c_void) -> u32>(self.0, 2)(self.0) };
            }
        }
    }

    pub fn create(clsid: &GUID, iid: &GUID) -> Result<Unk, HRESULT> {
        let mut p = std::ptr::null_mut();
        let hr = unsafe { CoCreateInstance(clsid, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, iid, &mut p) };
        if hr < 0 || p.is_null() {
            return Err(hr);
        }
        Ok(Unk(p))
    }

    /// Calls through this proxy impersonate the caller (WMI refuses the default "identify" level).
    pub fn set_blanket(p: &Unk) {
        const RPC_C_AUTHN_WINNT: u32 = 10;
        const RPC_C_AUTHZ_NONE: u32 = 0;
        const RPC_C_AUTHN_LEVEL_CALL: u32 = 3;
        const RPC_C_IMP_LEVEL_IMPERSONATE: u32 = 3;
        unsafe {
            CoSetProxyBlanket(
                p.0,
                RPC_C_AUTHN_WINNT,
                RPC_C_AUTHZ_NONE,
                std::ptr::null(),
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                std::ptr::null(),
                0,
            )
        };
    }

    pub struct Bstr(pub BSTR);
    impl Bstr {
        pub fn new(s: &str) -> Bstr {
            Bstr(unsafe { SysAllocString(crate::util::wide(s).as_ptr()) })
        }
    }
    impl Drop for Bstr {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { SysFreeString(self.0) };
            }
        }
    }

    /// VARIANT as an out-parameter (24 bytes on x64), cleared on drop.
    #[repr(C)]
    pub struct Variant {
        vt: u16,
        _reserved: [u16; 3],
        data: [u64; 2],
    }

    /// An empty VARIANT passed *by value* (e.g. the optional arguments of `ITaskService::Connect`).
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct EmptyVariant([u64; 3]);
    pub const EMPTY: EmptyVariant = EmptyVariant([0; 3]);

    #[derive(Debug, Clone, PartialEq)]
    pub enum Val {
        Null,
        Int(i64),
        Float(f64),
        Bool(bool),
        Str(String),
        Ints(Vec<i64>),
    }

    #[repr(C)]
    struct SafeArray {
        dims: u16,
        features: u16,
        element_size: u32,
        locks: u32,
        data: *mut c_void,
        count: u32,
        lower_bound: i32,
    }

    unsafe fn bstr_string(b: *const u16) -> String {
        if b.is_null() {
            return String::new();
        }
        let len = *(b as *const u32).offset(-1) as usize / 2;
        String::from_utf16_lossy(std::slice::from_raw_parts(b, len))
    }

    impl Variant {
        pub fn new() -> Variant {
            Variant { vt: VT_EMPTY, _reserved: [0; 3], data: [0; 2] }
        }

        pub fn value(&self) -> Val {
            let d = self.data[0];
            unsafe {
                match self.vt {
                    VT_EMPTY | VT_NULL => Val::Null,
                    VT_I1 => Val::Int(d as u8 as i8 as i64),
                    VT_UI1 => Val::Int(d as u8 as i64),
                    VT_I2 => Val::Int(d as u16 as i16 as i64),
                    VT_UI2 => Val::Int(d as u16 as i64),
                    VT_I4 | VT_INT => Val::Int(d as u32 as i32 as i64),
                    VT_UI4 | VT_UINT => Val::Int(d as u32 as i64),
                    VT_I8 | VT_UI8 => Val::Int(d as i64),
                    VT_R4 => Val::Float(f32::from_bits(d as u32) as f64),
                    VT_R8 => Val::Float(f64::from_bits(d)),
                    VT_BOOL => Val::Bool(d as u16 != 0),
                    VT_BSTR => Val::Str(bstr_string(d as usize as *const u16)),
                    vt if vt & VT_ARRAY != 0 => {
                        let sa = d as usize as *const SafeArray;
                        if sa.is_null() || (*sa).dims != 1 || (*sa).data.is_null() {
                            return Val::Ints(vec![]);
                        }
                        let (n, p) = ((*sa).count as usize, (*sa).data as *const u8);
                        let elem = vt & !VT_ARRAY;
                        let read = |i: usize| -> Option<i64> {
                            Some(match elem {
                                VT_I1 => *(p.add(i) as *const i8) as i64,
                                VT_UI1 => *p.add(i) as i64,
                                VT_I2 => (p.add(i * 2) as *const i16).read_unaligned() as i64,
                                VT_UI2 => (p.add(i * 2) as *const u16).read_unaligned() as i64,
                                VT_I4 | VT_INT => (p.add(i * 4) as *const i32).read_unaligned() as i64,
                                VT_UI4 | VT_UINT => (p.add(i * 4) as *const u32).read_unaligned() as i64,
                                _ => return None,
                            })
                        };
                        Val::Ints((0..n).filter_map(read).collect())
                    }
                    _ => Val::Null,
                }
            }
        }
    }

    impl Drop for Variant {
        fn drop(&mut self) {
            unsafe { VariantClear(self as *mut Variant as *mut c_void) };
        }
    }

    impl Val {
        pub fn int(&self) -> Option<i64> {
            match self {
                Val::Int(i) => Some(*i),
                Val::Float(f) => Some(*f as i64),
                // WMI hands 64-bit integers over as strings.
                Val::Str(s) => s.trim().parse().ok(),
                Val::Bool(b) => Some(*b as i64),
                _ => None,
            }
        }
        pub fn u64(&self) -> Option<u64> {
            match self {
                Val::Str(s) => s.trim().parse().ok(),
                _ => self.int().map(|i| i as u64),
            }
        }
        pub fn u32(&self) -> Option<u32> {
            self.int().map(|i| i as u32)
        }
        pub fn f64(&self) -> Option<f64> {
            match self {
                Val::Float(f) => Some(*f),
                Val::Str(s) => s.trim().parse().ok(),
                _ => self.int().map(|i| i as f64),
            }
        }
        pub fn bool(&self) -> Option<bool> {
            match self {
                Val::Bool(b) => Some(*b),
                Val::Int(i) => Some(*i != 0),
                _ => None,
            }
        }
        pub fn string(&self) -> Option<String> {
            match self {
                Val::Str(s) => Some(s.clone()),
                Val::Int(i) => Some(i.to_string()),
                _ => None,
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// In-process WMI (root\Microsoft\Windows\Storage): the same data the PowerShell script reads via CIM.
// ---------------------------------------------------------------------------------------------

mod wmi {
    use super::com::{self, Bstr, Unk, Val, Variant};
    use super::{RawDisk, RawPartition, RawReliability};
    use std::ffi::c_void;
    use windows_sys::core::{GUID, HRESULT};

    const CLSID_WBEM_LOCATOR: GUID = GUID::from_u128(0x4590f811_1d3a_11d0_891f_00aa004b2e24);
    const IID_IWBEM_LOCATOR: GUID = GUID::from_u128(0xdc12a687_737f_11cf_884d_00aa004b2e24);
    const WBEM_FLAG_RETURN_IMMEDIATELY: i32 = 0x10;
    const WBEM_FLAG_FORWARD_ONLY: i32 = 0x20;
    const WBEM_INFINITE: i32 = -1;

    // IWbemLocator::ConnectServer (3), IWbemServices::ExecQuery (20), IEnumWbemClassObject::Next (4),
    // IWbemClassObject::Get (4).
    type ConnectServer = unsafe extern "system" fn(
        *mut c_void,
        *const u16,
        *const u16,
        *const u16,
        *const u16,
        i32,
        *const u16,
        *mut c_void,
        *mut *mut c_void,
    ) -> HRESULT;
    type ExecQuery =
        unsafe extern "system" fn(*mut c_void, *const u16, *const u16, i32, *mut c_void, *mut *mut c_void) -> HRESULT;
    type EnumNext = unsafe extern "system" fn(*mut c_void, i32, u32, *mut *mut c_void, *mut u32) -> HRESULT;
    type ClassGet = unsafe extern "system" fn(*mut c_void, *const u16, i32, *mut Variant, *mut i32, *mut i32) -> HRESULT;

    struct Wmi(Unk);

    impl Wmi {
        fn connect(namespace: &str) -> Result<Wmi, HRESULT> {
            let loc = com::create(&CLSID_WBEM_LOCATOR, &IID_IWBEM_LOCATOR)?;
            let ns = Bstr::new(namespace);
            let mut svc = std::ptr::null_mut();
            let z = std::ptr::null();
            let hr = unsafe {
                com::method::<ConnectServer>(loc.0, 3)(loc.0, ns.0, z, z, z, 0, z, std::ptr::null_mut(), &mut svc)
            };
            if hr < 0 || svc.is_null() {
                return Err(hr);
            }
            let svc = Unk(svc);
            com::set_blanket(&svc);
            Ok(Wmi(svc))
        }

        /// Runs a WQL query and returns every matching object.
        fn query(&self, wql: &str) -> Result<Vec<Obj>, HRESULT> {
            let (lang, q) = (Bstr::new("WQL"), Bstr::new(wql));
            let mut en = std::ptr::null_mut();
            let flags = WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY;
            let svc = self.0 .0;
            let hr = unsafe { com::method::<ExecQuery>(svc, 20)(svc, lang.0, q.0, flags, std::ptr::null_mut(), &mut en) };
            if hr < 0 || en.is_null() {
                return Err(hr);
            }
            let en = Unk(en);
            com::set_blanket(&en);
            let mut out = Vec::new();
            loop {
                let mut batch = [std::ptr::null_mut::<c_void>(); 32];
                let mut n = 0u32;
                let hr = unsafe {
                    com::method::<EnumNext>(en.0, 4)(en.0, WBEM_INFINITE, batch.len() as u32, batch.as_mut_ptr(), &mut n)
                };
                out.extend(batch[..(n as usize).min(batch.len())].iter().map(|&p| Obj(Unk(p))));
                if hr < 0 {
                    return Err(hr);
                }
                // WBEM_S_FALSE (1): fewer objects than asked for, i.e. the end of the results.
                if hr != 0 || n == 0 {
                    return Ok(out);
                }
            }
        }
    }

    struct Obj(Unk);

    impl Obj {
        fn get(&self, name: &str) -> Val {
            let w = crate::util::wide(name);
            let mut v = Variant::new();
            let (o, p) = (self.0 .0, std::ptr::null_mut());
            let hr = unsafe { com::method::<ClassGet>(o, 4)(o, w.as_ptr(), 0, &mut v, p, p) };
            if hr < 0 {
                Val::Null
            } else {
                v.value()
            }
        }
    }

    /// Some(reason) when WMI or the Storage provider is unavailable system-wide, so the CIM script
    /// (same service, same provider, same classes) can't succeed either. Generic WBEM_E_FAILED,
    /// WBEM_E_PROVIDER_FAILURE and query errors return None and still fall back.
    pub fn unreachable_reason(hr: HRESULT) -> Option<String> {
        if let Some(why) = com::service_unreachable(hr) {
            return Some(why);
        }
        let why = match hr as u32 {
            0x8004_100E => "the Storage management namespace isn't available", // WBEM_E_INVALID_NAMESPACE
            0x8004_1010 => "the Storage management provider has no disk class", // WBEM_E_INVALID_CLASS
            // WBEM_E_PROVIDER_NOT_FOUND, WBEM_E_PROVIDER_LOAD_FAILURE, WBEM_E_INITIALIZATION_FAILURE
            0x8004_1011 | 0x8004_1013 | 0x8004_1014 => "the Storage management provider couldn't start",
            0x8004_1033 => "Windows Management Instrumentation is shutting down", // WBEM_E_SHUTTING_DOWN
            _ => return None,
        };
        Some(why.into())
    }

    /// The same raw records the PowerShell script produces, read in-process.
    pub fn raw_disks(elevated: bool) -> Result<Vec<RawDisk>, HRESULT> {
        com::on_mta_thread(move || read(elevated)).unwrap_or(Err(-1))
    }

    fn read(elevated: bool) -> Result<Vec<RawDisk>, HRESULT> {
        const NS: &str = r"ROOT\Microsoft\Windows\Storage";
        // Each Storage-provider query costs ~50-130 ms of provider work, so the three run side by side,
        // each on its own MTA thread and connection. Like the script, failing physical-disk or partition
        // queries leave those parts empty; only a failing disk query is an error.
        std::thread::scope(|s| {
            let pds = s.spawn(|| {
                com::in_mta(|| {
                    let w = Wmi::connect(NS)?;
                    let rows = w.query("SELECT ObjectId, DeviceId, BusType, MediaType FROM MSFT_PhysicalDisk")?;
                    Ok::<_, HRESULT>(
                        rows.iter()
                            .map(|p| PhysRow {
                                device_id: p.get("DeviceId").string().unwrap_or_default(),
                                bus: p.get("BusType").u32(),
                                media: p.get("MediaType").u32(),
                                relpath: p.get("__RELPATH").string(),
                            })
                            .collect::<Vec<_>>(),
                    )
                })
            });
            let parts = s.spawn(|| {
                com::in_mta(|| {
                    let w = Wmi::connect(NS)?;
                    let rows = w.query(
                        "SELECT DiskNumber, PartitionNumber, Offset, Size, DriveLetter, GptType, MbrType, IsSystem, \
                         IsBoot, IsHidden FROM MSFT_Partition",
                    )?;
                    Ok::<_, HRESULT>(rows.iter().map(partition_row).collect::<Vec<_>>())
                })
            });

            let w = Wmi::connect(NS)?;
            let disks = w.query(
                "SELECT Number, FriendlyName, Size, PartitionStyle, BusType, HealthStatus, OperationalStatus, \
                 IsBoot, IsSystem FROM MSFT_Disk",
            )?;
            let pds: Vec<PhysRow> = pds.join().ok().and_then(Result::ok).unwrap_or_default();
            let mut parts: Vec<Option<(Option<u32>, RawPartition)>> =
                parts.join().ok().and_then(Result::ok).unwrap_or_default().into_iter().map(Some).collect();

            let mut out = Vec::with_capacity(disks.len());
            for d in &disks {
                let number = d.get("Number").u32();
                let key = number.map(|n| n.to_string()).unwrap_or_default();
                let pd = pds.iter().find(|p| p.device_id == key);
                let rc = match (elevated, pd.and_then(|p| p.relpath.as_deref())) {
                    (true, Some(path)) => reliability(&w, path),
                    _ => None,
                };
                let partitions = parts
                    .iter_mut()
                    .filter(|p| matches!(p, Some((n, _)) if *n == number))
                    .filter_map(|p| p.take().map(|(_, rp)| rp))
                    .collect();
                out.push(RawDisk {
                    number,
                    model: Some(d.get("FriendlyName").string().unwrap_or_default().trim().to_string()),
                    size: d.get("Size").u64(),
                    partition_style: d.get("PartitionStyle").u32(),
                    disk_bus: d.get("BusType").u32(),
                    pd_bus: pd.and_then(|p| p.bus),
                    media: pd.and_then(|p| p.media),
                    health: d.get("HealthStatus").u32(),
                    operational: match d.get("OperationalStatus") {
                        Val::Ints(v) => v.into_iter().map(|c| c as u32).collect(),
                        v => v.u32().into_iter().collect(),
                    },
                    is_boot: Some(d.get("IsBoot").bool().unwrap_or(false)),
                    is_system: Some(d.get("IsSystem").bool().unwrap_or(false)),
                    rc,
                    partitions,
                });
            }
            Ok(out)
        })
    }

    struct PhysRow {
        device_id: String,
        bus: Option<u32>,
        media: Option<u32>,
        /// Needs the key (ObjectId) in the SELECT list, or WMI leaves it empty.
        relpath: Option<String>,
    }

    fn partition_row(p: &Obj) -> (Option<u32>, RawPartition) {
        let letter = p.get("DriveLetter").int().filter(|&c| c != 0).and_then(|c| char::from_u32(c as u32));
        (
            p.get("DiskNumber").u32(),
            RawPartition {
                number: p.get("PartitionNumber").u32(),
                offset: p.get("Offset").u64(),
                size: p.get("Size").u64(),
                letter: letter.map(|c| c.to_string()),
                gpt: Some(p.get("GptType").string().unwrap_or_default()),
                mbr: p.get("MbrType").u32(),
                is_system: Some(p.get("IsSystem").bool().unwrap_or(false)),
                is_boot: Some(p.get("IsBoot").bool().unwrap_or(false)),
                is_hidden: Some(p.get("IsHidden").bool().unwrap_or(false)),
            },
        )
    }

    /// `Get-CimAssociatedInstance -ResultClassName MSFT_StorageReliabilityCounter`, in WQL.
    fn reliability(w: &Wmi, pd_relpath: &str) -> Option<RawReliability> {
        let found = w
            .query(&format!("ASSOCIATORS OF {{{pd_relpath}}} WHERE ResultClass = MSFT_StorageReliabilityCounter"))
            .ok()?;
        let c = found.first()?;
        Some(RawReliability {
            temperature: c.get("Temperature").f64(),
            temperature_max: c.get("TemperatureMax").f64(),
            wear: c.get("Wear").f64(),
            power_on_hours: c.get("PowerOnHours").u64(),
            read_errors: c.get("ReadErrorsTotal").u64(),
            write_errors: c.get("WriteErrorsTotal").u64(),
        })
    }
}

#[tauri::command]
pub async fn list_disks() -> Result<Vec<PhysicalDisk>, String> {
    tauri::async_runtime::spawn_blocking(list)
        .await
        .map_err(|_| "Reading the disk layout stopped unexpectedly.".to_string())?
}

#[tauri::command]
pub async fn rescan_disks() -> Result<(), String> {
    if !util::is_elevated() {
        return Ok(());
    }
    tauri::async_runtime::spawn_blocking(|| {
        util::powershell("Update-HostStorageCache -ErrorAction Stop")
            .map(|_| ())
            .map_err(|_| "Windows couldn't rescan the disks. Try again in a moment.".to_string())
    })
    .await
    .map_err(|_| "Rescanning the disks stopped unexpectedly.".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disks_list_real() {
        let t = std::time::Instant::now();
        let disks = list().expect("list_disks");
        println!("list_disks took {} ms", t.elapsed().as_millis());
        println!("{}", serde_json::to_string_pretty(&disks).unwrap());
        assert!(!disks.is_empty());
        for d in &disks {
            assert!(d.partitions.windows(2).all(|w| w[0].offset <= w[1].offset));
        }
    }

    /// In-process WMI must report exactly what the PowerShell/CIM script reports.
    #[test]
    fn disks_native_matches_powershell() {
        let el = util::is_elevated();
        let t = std::time::Instant::now();
        let native = finish(wmi::raw_disks(el).expect("native WMI"));
        let native_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = std::time::Instant::now();
        let ps = list_powershell(el).expect("powershell");
        let ps_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = std::time::Instant::now();
        let again = finish(wmi::raw_disks(el).expect("native WMI"));
        let warm_ms = t.elapsed().as_secs_f64() * 1000.0;
        println!("list_disks native {native_ms:.1} ms (again {warm_ms:.1} ms), powershell {ps_ms:.1} ms");
        let a = serde_json::to_string_pretty(&native).unwrap();
        assert_eq!(a, serde_json::to_string_pretty(&ps).unwrap());
        assert_eq!(a, serde_json::to_string_pretty(&again).unwrap());
    }

    /// Only service-level / provider-level failures skip the PowerShell fallback.
    #[test]
    fn disks_unreachable_classification() {
        let h = |u: u32| u as i32;
        let disabled = wmi::unreachable_reason(h(0x8007_0422)).expect("service disabled");
        assert!(!disabled.is_empty() && !disabled.contains("os error") && !disabled.ends_with('.'), "{disabled}");
        println!("ERROR_SERVICE_DISABLED -> {disabled}");
        assert!(wmi::unreachable_reason(h(0x8007_0426)).is_some()); // ERROR_SERVICE_NOT_ACTIVE
        assert!(wmi::unreachable_reason(h(0x8007_06BA)).is_some()); // RPC_S_SERVER_UNAVAILABLE
        assert!(wmi::unreachable_reason(h(0x8004_100E)).is_some()); // WBEM_E_INVALID_NAMESPACE
        assert!(wmi::unreachable_reason(h(0x8004_1013)).is_some()); // WBEM_E_PROVIDER_LOAD_FAILURE
        // In-process route problems and generic failures still fall back.
        for u in [0xFFFF_FFFF, 0x8004_01F0, 0x8001_0106, 0x8004_1001, 0x8004_1004, 0x8004_1017, 0x8007_0005] {
            assert!(wmi::unreachable_reason(h(u)).is_none(), "{u:#x}");
        }
    }

    #[test]
    fn disks_parse_single_and_nulls() {
        let j = r#"[{"number":1,"model":null,"size":1000000000,"partitionStyle":0,"diskBus":7,"pdBus":null,
            "media":null,"health":5,"operational":[53264],"isBoot":false,"isSystem":false,"rc":null,"partitions":[]}]"#;
        let d = parse(j).unwrap();
        assert_eq!(d[0].partition_style, "RAW");
        assert_eq!(d[0].bus, "USB");
        assert_eq!(d[0].model, "Disk 1");
        assert_eq!(d[0].unallocated, 1000000000);
        assert!(!d[0].reliability_available);
        assert_eq!(partition_kind(Some("{C12A7328-F81F-11D2-BA4B-00A0C93EC93B}"), None), "EFI system");
        assert_eq!(partition_kind(Some(""), Some(7)), "Basic data");
    }

    #[test]
    fn disks_extended_container_not_counted_twice() {
        const GB: u64 = 1_000_000_000;
        // 1 TB MBR disk: 100 GB primary, then a 500 GB extended container holding 300 GB of logicals,
        // then 400 GB never allocated.
        let part = |n: u32, off: u64, size: u64, mbr: u32| {
            format!(r#"{{"number":{n},"offset":{off},"size":{size},"letter":null,"gpt":"","mbr":{mbr},"isSystem":false,"isBoot":false,"isHidden":false}}"#)
        };
        let parts = [
            part(1, 1 << 20, 100 * GB, 7),
            part(2, (1 << 20) + 100 * GB, 500 * GB, 0x0F),
            part(5, (2 << 20) + 100 * GB, 200 * GB, 7),
            part(6, (3 << 20) + 300 * GB, 100 * GB, 7),
        ]
        .join(",");
        let j = format!(
            r#"[{{"number":2,"model":"X","size":{},"partitionStyle":1,"diskBus":11,"health":0,"operational":[2],"partitions":[{parts}]}}]"#,
            1000 * GB
        );
        let d = parse(&j).unwrap();
        assert_eq!(d[0].partitions.iter().filter(|p| p.kind == "Extended").count(), 1);
        // 1000 GB - 100 GB primary - 500 GB container (the logicals sit inside it)
        assert_eq!(d[0].unallocated, 400 * GB);

        // No offsets reported: sizes without the container.
        let j = r#"[{"number":3,"size":1000000000000,"partitionStyle":1,"partitions":[
            {"number":1,"size":100000000000,"mbr":7},{"number":2,"size":500000000000,"mbr":5},{"number":5,"size":300000000000,"mbr":7}]}]"#;
        assert_eq!(parse(j).unwrap()[0].unallocated, 600 * GB);
    }
}
