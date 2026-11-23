# Platter — backend ↔ UI contract

Tauri v2 app. Rust backend in `src/`, static UI in `ui/` (vanilla ES modules, `window.__TAURI__`).
JS calls `invoke("command_name", { camelCaseArgs })`; Tauri maps camelCase arg keys to snake_case Rust params.
All JSON field names below are **camelCase** (use `#[serde(rename_all = "camelCase")]`).
All sizes are bytes (`u64`). Errors are returned as `Err(String)` with a plain-English, user-facing
sentence ("Windows denied access. Restart Platter as administrator and try again."), never raw debug output.

Toolchain: Rust stable **x86_64-pc-windows-gnu** (`~/.cargo/bin/cargo`). No MSVC. Win32 via `windows-sys 0.59`
(features already in Cargo.toml). Helpers in `src/util.rs`: `wide()`, `from_wide()`, `powershell(script) -> Result<String,String>`,
`clean_ps_error()`, `is_elevated()`, `env_path()`, `system_drive_letter()`, `CREATE_NO_WINDOW`.
Spawn every child process with `.creation_flags(CREATE_NO_WINDOW)` so no console flashes.
Heavy commands are `async` and run their work in `tauri::async_runtime::spawn_blocking`.

## System — `main.rs` / `util.rs`  (done)
- `system_info() -> { elevated: bool, computerName: String, systemDrive: "C", userProfile: String, downloads: String }`
- `relaunch_elevated() -> bool` — async; UAC relaunch (one prompt at a time); app exits if accepted. The elevated copy gets `--wait-for-pid <pid>`, waits for this instance to exit, and uses its own WebView2 data folder (`%LOCALAPPDATA%app.platter.diskselevated`).
- event `drives-changed` (no payload) when the set of drive letters changes, or media is inserted/removed/swapped in a removable or optical drive (polled every 2s).

## Cache & startup — `cache.rs`, `perf.rs`, `main.rs`
- `cache_get_many(keys: String[]) -> { [key]: { savedAt: ms-epoch, version, data } }` — last-known results for instant first paint. Keys: `disks`, `history`, `schedule`, `cleanup` (CleanupItem[]). Writers: disks.rs/optimize.rs/cleanup.rs call `cache::write(key, &value)` after each successful fresh result.
- `app_ready() -> ()` — UI calls it after its first paint; shows + focuses the main window.
- `set_theme_pref(theme: "light"|"dark"|"system") -> ()` — persists the theme (cache key `theme`) so the next launch paints the window and WebView background in the right colours before any page loads.
- Cache key `window`: `{x, y, width, height, maximized}` — restored before the window is shown; ignored if off-screen.
- Startup: the window is created hidden with the theme's background, shown right after it is built, and focused on `app_ready` (fallback 1.5 s). A second launch focuses the running window (class `PlatterMainWindow`) and exits.
- `cleanup_analyze(background?: bool)` — `true`: 2 low-priority threads (THREAD_MODE_BACKGROUND, lower I/O priority), cancellable when superseded; absent/false: full speed. `cleanup_run` deletes the `cleanup` cache entry before deleting anything.
- `perf_mark(name)`, `perf_enabled() -> bool` — timeline log when `PLATTER_PERF=<file>` is set (bench/launch.ps1).

## Volumes — `volumes.rs`  (done)
- `list_volumes() -> Volume[]` — `{ letter:"C", label, fileSystem, total, free, kind:"fixed|removable|network|optical|ram", isSystem, serial, clusterSize, ready, media:"ssd|hdd|unknown", bus:"NVMe|SATA|USB|…", model, readOnly, compressedSupported, status:"ok|not-ready|locked|raw|unavailable" }`
- `allocation_map(letter, cells) -> { cells: u8[] (0 free … 255 full), clusters, clusterSize, bytesPerCell, freeRuns, largestFreeRun }`

## Disks — `disks.rs`
- `list_disks() -> PhysicalDisk[]` (PowerShell `Get-Disk`/`Get-PhysicalDisk`/`Get-Partition`/`Get-StorageReliabilityCounter`, one script, JSON)
  ```
  PhysicalDisk { number: u32, model: String, size: u64, partitionStyle: "GPT|MBR|RAW",
    bus: String, media: "ssd|hdd|unknown", health: "Healthy|Warning|Unhealthy|Unknown", operational: String,
    isBoot: bool, isSystem: bool,
    reliabilityAvailable: bool,          // false when counters need admin / unsupported
    temperature: Option<f64>, temperatureMax: Option<f64>, wear: Option<f64> /* % used life */,
    powerOnHours: Option<u64>, readErrors: Option<u64>, writeErrors: Option<u64>,
    partitions: Partition[], unallocated: u64 }
  Partition { number: u32, offset: u64, size: u64, letter: Option<String> /* "C" */,
    kind: String /* human: "EFI system", "Recovery", "Microsoft reserved", "Basic data", … */,
    isSystem: bool, isBoot: bool, isHidden: bool }
  ```
  Partitions sorted by offset.
- `rescan_disks() -> ()` — `Update-HostStorageCache` (admin). Non-admin: Ok(()) silently.

## Optimize — `optimize.rs`  (uses `defrag.exe`, admin required — return the admin sentence if not elevated)
- `optimize_start(letter, mode: "trim"|"defrag"|"analyze") -> ()` — spawns `defrag X: /L|/D|/A /U /V`, streams stdout
  (split on `\r` and `\n`), emits:
  - `optimize-progress` `{ letter, mode, percent: Option<f32>, stage: String /* "Retrim", "Defragmentation", "Analysis" */, line: String }`
  - `optimize-done` `{ letter, mode, ok: bool, message: String /* one friendly sentence */, fragmentedPercent: Option<f32>, report: String /* full output */ }`
  Only one run at a time (Err if busy).
- `optimize_cancel() -> ()` — asks defrag to cancel (Ctrl+C), terminates it after 15 s; returns immediately; the outcome arrives via `optimize-done` (the message says if Windows may finish the pass in the background).
- `optimize_history() -> OptimizeEvent[]` — from Application log, provider `Microsoft-Windows-Defrag`, newest first, max ~40:
  `{ letter: Option<String>, action: "Trim|Defragment|Analyze|Boot optimization|Slab consolidation|Other", time: String /* ISO 8601 */, ok: bool, message: String }`
- `optimize_schedule() -> { enabled: bool, lastRun: Option<String>, nextRun: Option<String> }` (task `\Microsoft\Windows\Defrag\ScheduledDefrag`)

## Manage — `manage.rs`
- `set_label(letter, label) -> ()` (`SetVolumeLabelW`; max 32 chars NTFS)
- `free_letters() -> String[]` — unused letters D–Z (letters used by mapped network drives count as used)
- `change_letter(letter, newLetter) -> ()` — PowerShell `Set-Partition`; refuse for system drive
- `check_volume(letter) -> { status: "NoErrorsFound"|"ErrorsFound"|…, message: String }` — `Repair-Volume -Scan` (online, read-only)
- `open_path(path) -> ()` — explorer
- `reveal_path(path) -> ()` — `explorer /select,"path"`
- `recycle_paths(paths: String[]) -> ()` — `SHFileOperationW` FO_DELETE + FOF_ALLOWUNDO|FOF_NOCONFIRMATION|FOF_SILENT|FOF_NOERRORUI
- `open_recycle_bin() -> ()` — `explorer shell:RecycleBinFolder`

## Space scanner — `scan.rs`
Parallel (rayon) directory walk. Never follow reparse points (junctions/symlinks). Cloud placeholders
(attributes OFFLINE 0x1000 / RECALL_ON_DATA_ACCESS 0x400000 / RECALL_ON_OPEN 0x40000) count 0 bytes.
Categories by extension, index into `cats: [u64; 8]`:
`0 video, 1 images, 2 audio, 3 documents, 4 archives & installers, 5 apps & system (exe dll sys …), 6 developer (source, build output, node_modules contents), 7 other`.
- `scan_start(path) -> u64` — starts background scan (cancels any running one) and returns its scan id. Emits (both carry `scanId`):
  - `scan-progress` `{ files, dirs, bytes, current: String /* path being read */ }` every ~120ms
  - `scan-done` `{ ok, cancelled, error: Option<String>, rootId: usize, path, totalBytes, files, dirs, deniedDirs: u64, elapsedMs }`
- `scan_cancel() -> ()`
- `scan_node(id, depth, scanId?) -> NodeView`
  ```
  NodeView { id: Option<usize> /* dirs only */, name, path, size, files: u64, dirs: u64, isDir: bool,
    isOther: bool /* aggregate "smaller files" bucket */, cat: u8 /* dominant category */, cats: [u64;8],
    parentId: Option<usize>, denied: bool, children: NodeView[] /* depth-1 levels, sorted size desc; [] at depth 0 */ }
  ```
  Children = sub-dirs + that dir's 24 largest files + one `isOther` bucket ("312 smaller files") for the rest.
- `scan_top_files(limit, scanId?) -> { path, name, size, cat }[]` — largest files in the whole scan.

Sizes: hard-linked files count once; sparse, compressed and WOF-compressed files count their on-disk size.

## Clean up — `cleanup.rs`
- `cleanup_analyze() -> CleanupItem[]` and emits `cleanup-item` (CleanupItem) as each one finishes.
  ```
  CleanupItem { id, group: "System|Browsers|Apps|Developer caches|Review", name, description /* one plain line */,
    size, files, needsAdmin: bool, defaultOn: bool, kind: "delete|recycle-bin|hibernation|review",
    reviewPath: Option<String>, available: bool /* found on this PC */ }
  ```
- Items whose folder Platter refuses to empty (e.g. TEMP pointing at a profile folder), or per-user items when elevated as a different account, come back with size 0 and the reason appended to `description`.
- `cleanup_run(ids: String[]) -> { freed, deleted, skipped, items: { id, freed, skipped, error: Option<String> }[] }`
  emits `cleanup-progress` `{ id, freed, done: bool }`. `error` is also set when some folders were refused even if `freed > 0`.

Deletion is handle-based: every folder and file is opened with `FILE_FLAG_OPEN_REPARSE_POINT`, its final path re-checked against the root, and deleted through that handle. Junctions and symlinks inside a root are never followed.
