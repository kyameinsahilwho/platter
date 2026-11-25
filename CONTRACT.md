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
- `system_info() -> { elevated: bool, askAdminAtStart: bool | null, computerName: String, systemDrive: "C", userProfile: String, downloads: String }` — `askAdminAtStart` is `null` when Platter runs elevated as a different account than the desktop user (over-the-shoulder credentials, Administrator protection): the setting lives in the desktop user's profile, so the UI hides it there.
- `relaunch_elevated() -> bool` — async; UAC relaunch (one prompt at a time); app exits if accepted. The elevated copy gets `--wait-for-pid <pid>`, takes over the single-instance mutex at once, then waits for this instance to exit before starting WebView2, and uses its own WebView2 data folder (`%LOCALAPPDATA%\app.platter.disks\elevated`).
- `set_ask_admin(enabled: bool) -> ()` — async; turns the startup administrator prompt on/off (cache key `askAdmin`, bool; missing = on). Errors if it couldn't be saved, or when elevated as a different account.
- Startup prompt: a standard launch that becomes the single instance asks for administrator rights (the same `runas` relaunch with `--wait-for-pid`) before any window or WebView exists. Accepted: the elevated copy takes over and this process exits. Declined: Platter runs in standard mode. Skipped when already elevated, when started with `--wait-for-pid` (no relaunch loop), with `--no-elevate` (used by bench/launch.ps1), when `askAdmin` is false, when UAC is off (`EnableLUA` = 0), and in debug builds unless `PLATTER_ASK_ADMIN=1`. The named event `Local\app.platter.disks.elevating` (NULL DACL) exists from the prompt until the mutex is safe: declined, at once; accepted, when the elevated copy sets it after opening the mutex (wait capped at 10 s or the copy's exit), so the mutex is never free during the handoff. A second launch keeps waiting while it exists and never prompts itself.
- Registered window message `app.platter.disks.close` (let through UIPI): the main window closes as if its X was clicked. Sent by the unelevated per-user installer/uninstaller (`installer/hooks.nsh`, `NSIS_HOOK_PREINSTALL`/`PREUNINSTALL`), which can't end an elevated Platter otherwise.
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

## Uninstall apps — `uninstall.rs`  (Deep Uninstaller)
Lists desktop (Win32) apps from the Uninstall registry keys, runs an app's own uninstaller, then finds what it left behind.
Store (MSIX) apps aren't listed. Platter normally runs elevated, but every command must also work unelevated: items
that need admin come back with `needsAdmin: true`, and removing them returns the admin sentence.
- `apps_list() -> InstalledApp[]` — async. Reads `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall` (64- and 32-bit views) and the same key under HKCU. Skips entries without `DisplayName`, `SystemComponent = 1`, entries with `ParentKeyName` or `ReleaseType` Update/Hotfix/Security Update, and duplicates (same name + version in both views). Sorted by name. The backend remembers the last list (id → app snapshot) so `app_leftovers` still works after the entry is gone.
  ```
  InstalledApp { id: String /* opaque, stable across calls: hive + view + key name */, name, publisher: String /* "" if unknown */,
    version: String, installDate: Option<String> /* "2025-03-14" (InstallDate yyyymmdd) */, size: u64 /* EstimatedSize KB × 1024; 0 = unknown */,
    installLocation: Option<String> /* only if the folder exists */, scope: "machine" | "user",
    canUninstall: bool /* has an UninstallString whose program exists */, broken: bool /* uninstaller program missing */ }
  ```
- `app_icon(id) -> Option<String>` — async. The app's icon as a ~64 px `data:image/png;base64,…` URL, or null when it has none (the UI shows its initial). Source: `DisplayIcon` ("path,index", local files only), else Windows Installer's cached `ProductIcon` for MSI products, else an .exe at the top of the install folder named after the app (never an uninstaller/updater/setup). Cached per id for the session.
- `app_uninstall(id) -> { removed: bool, exitCode: Option<i32>, stoppedWaiting: bool }` — async. Runs the app's normal (interactive) `UninstallString` so the user sees the vendor's uninstaller. `MsiExec.exe /I{code}` is rewritten to `%SystemRoot%\System32\msiexec.exe /X{code}`. Launched with ShellExecuteExW (so uninstallers that need admin get their own UAC prompt), working dir = the program's folder. Waits for the process to exit, then polls every 1 s for up to 10 min until the Uninstall entry disappears (many uninstallers hand off to a copy of themselves and exit at once). No polling when it exits with 1602 or 1223 (the user cancelled inside it): it returns at once with `removed: false`. msiexec never hands off to a copy of itself, so after it exits Platter waits only a few seconds after success and not at all after a failure; exit 1618 with the entry still there → "Another installation is running. Wait for it to finish, then try again." An entry that is already gone returns `removed: true` without running anything. `removed` = the entry is gone; a cancelled uninstaller is `Ok` with `removed: false`, not an error. One at a time: "Another uninstaller is already running." UAC declined → "The uninstaller was cancelled, so nothing was removed." Missing program → "This app's uninstaller is missing. Look for leftovers to remove what's left of it." Access denied → "Windows denied access to the uninstaller. Restart Platter as administrator and try again."
- `app_uninstall_stop_waiting() -> ()` — makes a waiting `app_uninstall` return now (`stoppedWaiting: true`, `removed` = whether the entry is gone at that moment).
- `app_leftovers(id) -> LeftoverScan` — async. What this app left (or would leave) behind. Works for an app in the last `apps_list` even after its entry is gone. It doesn't check whether the app is still installed: for an installed app, its live install folder ("The app's install folder.") and the services, tasks, startup and App Paths entries that run from it come back `high` (so `defaultOn`). Only its Uninstall entry is marked for that case: `low`, "The app still looks installed. Remove this only if its uninstaller won't run." (`high` when its uninstaller and install folder are gone). The UI therefore selects nothing for an app that is still listed and not `broken` (see Clean up UI).
- `orphan_leftovers() -> LeftoverScan` — async. Leftovers of apps that are no longer installed, not tied to one app: uninstall entries whose uninstaller and install folder (and icon) are gone, services whose program file is missing, scheduled tasks (outside `\Microsoft\`) whose program is missing, startup entries (Run/RunOnce, HKCU + HKLM, both views) and App Paths entries pointing at missing files, and folders under Program Files (both), `%LOCALAPPDATA%\Programs` and `%ProgramData%` that contain no programs (.exe/.dll), no files changed in the last 90 days, and that no installed app, service or task refers to.
  ```
  LeftoverScan { scanId: u64, app: Option<String> /* app name, None for orphans */, items: Leftover[] }
  Leftover { id: String /* opaque, valid for this scanId only */,
    kind: "folder" | "file" | "registry" | "service" | "task" | "startup" | "uninstall-entry",
    path: String /* shown to the user: C:\…, HKCU\Software\Vendor\App, service key name, \Vendor\Task, HKCU\…\Run → value */,
    label: String /* short: folder name, service display name, task name, value name */,
    reason: String /* one plain sentence: "In the app's install folder.", "Named after the app.", "Its program file is gone." */,
    size: u64 /* folders/files: on-disk bytes; 0 otherwise */, confidence: "high" | "medium" | "low",
    needsAdmin: bool, defaultOn: bool /* confidence == high; always false for orphan folders. Anything another app, service or task still refers to isn't listed at all */ }
  ```
- `leftovers_remove(scanId, ids: String[]) -> { removed: u32, failed: u32, freed: u64, backupFolder: Option<String>, items: { id, ok: bool, error: Option<String> }[] }` — async. Re-checks every item against the same rules first (still exists, still unreferenced, not protected). Folders and files go to the **Recycle Bin**. Registry keys/values, services and tasks are **backed up first** into `%LOCALAPPDATA%\app.platter.disks\backups\<yyyy-mm-dd HHmmss> <app or "Orphans">\` (`reg.exe export` per key, task XML, the service's registry key). An item whose backup fails is not removed. Services are stopped, then deleted (DeleteService). Items are removed in dependency order (services, tasks, startup values, registry keys, then folders deepest first) so a leftover service is stopped before its folder is recycled; result `items` keep the order the ids were sent. A service that won't stop is reported as "Windows will finish removing it after a restart." Scan ids are taken when a scan starts and the last 4 scans stay valid; an unknown or older scanId → "These results are out of date. Look for leftovers again."

Safety rules (enforced by the backend, not the UI):
- Never touch anything in or containing: the Windows folder, the `Program Files`/`Program Files (x86)`/`Common Files` roots themselves, the `ProgramData` root, the user's profile root and main folders, the `AppData\Local|Roaming|LocalLow` roots, drive roots — or any folder that is (or contains) another installed app's install location or uninstaller, or a service/task program still in use.
- Registry only below these parents, at depth ≥ 1 below them: `HK{CU,LM}\Software\<x>` and `HKLM\Software\WOW6432Node\<x>` (never `Microsoft`, `Windows`, `Classes`, `Policies`, `Wow6432Node`, `Clients`, `RegisteredApplications` themselves), the Uninstall keys (one entry), `App Paths` (one entry), and single values under the Run/RunOnce keys.
- Name matching ignores tokens shorter than 4 characters and generic words (Microsoft, Windows, Common Files, Update, Setup, Installer, Driver, Runtime, Redistributable, Tools, Program(s), Data, Temp, Cache, Packages, and Google/Intel/NVIDIA/AMD alone as publisher folders…). A publisher folder is offered only when every child of it is also a leftover of this app.
- Services: only Win32 services (no drivers), never ones whose program lives in the Windows folder. Tasks: never under `\Microsoft\`.

## Duplicates — `dupes.rs`  (Duplicate Intelligence)
Finds identical files by content (size → first+last 64 KB hash → full 128-bit xxh3 hash), similar photos and videos by
perceptual hash of their Windows Shell thumbnails, and folders with identical contents. Never follows reparse points.
Skips cloud placeholders (attrs 0x1000/0x40000/0x400000) and system+hidden files. Hard links of one file are not duplicates (dedupe by volume serial + file id).
- `dupes_default_roots() -> { path, label }[]` — the user's Desktop, Documents, Downloads, Pictures, Videos and Music (real Known Folder paths that exist; OneDrive-redirected paths are fine).
- `dupes_start(roots: String[], options: { minSize: u64, similarImages: bool, similarVideos: bool, folders: bool }) -> u64` — async. First checks every root (a metadata call each, so a sleeping disk or an unreachable share can take seconds), then starts a background scan (cancels a running one) and returns its scanId. A bad root fails the call before any scan exists: "<path> doesn't exist or isn't ready. Choose another folder.", "<path> is a file, not a folder. …", the admin sentence, or "Choose at least one folder to look in." Roots inside other roots are merged, judged by where each really is (a junction or subst drive into another root counts as inside it). A root that is a drive root is allowed (whole-drive scan), but the Windows folder, Program Files, ProgramData, `$Recycle.Bin` and `System Volume Information` are skipped inside it. Emits (both carry `scanId`):
  - `dupes-progress` `{ scanId, phase: "listing" | "hashing" | "images" | "videos" | "folders", files: u64, bytes: u64, done: u64, total: u64, current: String }` every ~150 ms (`done/total` = items in the current phase; `current` = path being read)
  - `dupes-done` `{ scanId, ok: bool, cancelled: bool, error: Option<String>, elapsedMs: u64 }`
  The stored result is replaced only when a scan finishes (`ok: true`). A stopped or failed scan leaves the previous finished result as the latest, still under its own scanId: the UI deletes against `DupesResult.scanId` of the results on screen, not the id of the last scan it started.
- `dupes_cancel() -> ()` — stops the running scan (`dupes-done` with `cancelled: true`). Does nothing when no scan is running, including while `dupes_start` is still checking roots; the UI remembers a Stop clicked then and calls `dupes_cancel` again once `dupes_start` returns.
- `dupes_result(scanId) -> DupesResult` — error "These results are out of date. Scan again." if it isn't the latest finished scan.
  ```
  DupesResult { scanId, roots: String[], files: u64 /* files looked at */, bytes: u64,
    skipped: { denied: u64 /* folders */, cloud: u64, small: u64 /* below minSize */ },
    reclaimable: u64 /* bytes freed by deleting every recommended (select=true) copy, counted once */,
    groups: DupGroup[] /* identical files, sorted by reclaimable desc */, folders: FolderGroup[], images: SimilarGroup[], videos: SimilarGroup[] }
  DupGroup { id: String, cat: u8 /* scan.rs category index */, size: u64 /* each copy */, reclaimable: u64, files: DupFile[] /* keeper first */ }
  DupFile { path, name, folder: String /* parent path */, size, modified: u64 /* ms epoch */,
    keep: bool /* recommended copy to keep: exactly one per group */, select: bool /* recommended for deletion */,
    risk: "safe" | "review" | "protected", note: String /* one plain line, e.g. "Oldest copy, in Pictures", "Copy in Downloads", "Part of an app's folder, so leave it" */ }
  FolderGroup { id, size /* each copy */, files: u64 /* per copy */, reclaimable, folders: DupFile[] /* path/name = the folder */ }
  SimilarGroup { id, kind: "image" | "video", files: SimilarFile[] /* keeper first */ }
  SimilarFile = DupFile fields + { width: u32, height: u32, durationMs: Option<u64> /* videos */, similarity: u8 /* 0–100 vs the keeper; keeper 100 */ }
  ```
  Exact-file groups whose every copy lies inside the copies of one duplicate-folder group are left out of `groups` (the folder group covers them). Similar groups never include a pair that is byte-identical (that's an exact group). Only the outermost duplicate folders are reported, not their sub-folders.
- Recommendations: `risk = "protected"` (never `select`) for anything in an app, game, system or project folder: under Windows, Program Files*, ProgramData or `AppData`, or a folder (or an ancestor up to the root) containing `.git`, `package.json`, `Cargo.toml`, `*.sln`, `node_modules`, `.exe`+`.dll` together, or `steamapps`. `"safe"` for copies in Downloads or Temp, copies named like a copy (`" - Copy"`, `" (1)"`, `"Copy of "`), or copies in the same folder as the keeper. `"review"` otherwise. Keeper order: protected copy > user library (Pictures/Documents/Videos/Music) > Desktop > other > Downloads/Temp; then a name that doesn't look like a copy; then the oldest modified (a folder copy: the folder's own modified time); then the shortest path. Before that order, an exact group ranks its copies by the duplicate folders they sit in: inside a folder copy that stays, then outside every folder copy, then inside a folder copy recommended for removal (that copy goes with its folder). So the file and folder recommendations keep the same copy; the similar-photo/video candidates are these exact keepers. `select = !keep && risk != "protected"` for exact groups and folder groups. Always `false` for similar images/videos (similar isn't identical, so the user decides).
- `dupes_thumb(path) -> String` — a `data:image/…;base64,` thumbnail (~96 px) of a file in the latest result (other paths are refused).
- `dupes_delete(scanId, paths: String[]) -> { removed: u32, freed: u64, failed: { path, error }[] }` — async. **Recycle Bin only** (same refusal rules as `recycle_paths`). A path counts as removed when it is named or lies inside a named folder copy (files inside go with their folder and aren't counted again). The whole request is refused, nothing moved, when any path isn't in this scan's result ("Some of these items aren't in the latest results. Scan again."), when the scanId isn't the latest finished scan (`dupes_result`'s error), or when any group of any kind (exact, folder, similar) would be left with no existing copy ("Keep at least one copy of “name”. Platter won't remove every copy."); the UI leaves such picks out before sending. Per item, skips (into `failed`) one that is gone, a file whose size or modified time changed since the scan, a folder copy whose files (names, sizes, modified times) changed, or one whose kept copy is gone or changed.

## Clean up UI
Clean up has three sub-views: `#view=cleanup&sub=caches|apps|dupes` (route `{ view: "cleanup", sub }`, remembered).
Apps leftovers: for an app still in the list and not `broken` (its uninstaller was cancelled, is still open, or was never run), nothing is pre-selected, the title is "What belongs to <app>" with a still-installed warning, and the confirm is a danger button. While an uninstaller runs, the orphan "Look for leftovers" is disabled; a finished uninstall never replaces an open leftovers screen (it leaves a notice instead).
Mock-only hash hooks: `&leftovers=1` (Apps: open the leftovers panel for the first app), `&orphans=1`, `&dupes=done|running` (Duplicates: show a finished / running scan).
