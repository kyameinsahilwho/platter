# Platter

See, clean, optimize and manage the disks on a Windows PC. Rust (Tauri v2) backend, plain HTML/CSS/JS UI.

## What it does

- **Drive overview**: free space, a real **cluster allocation map** read from NTFS (`FSCTL_GET_VOLUME_BITMAP`),
  suggestions ("only 8% free", "not trimmed in 6 weeks", "10 GB of temp files can go"), details and disk health.
- **What's using space**: fast parallel scan of a drive or folder, squarified treemap by file type,
  drill-down, largest files, show in Explorer, move to Recycle Bin. Skips junctions and OneDrive placeholders.
- **Clean up**: measures temp files, Recycle Bin, browser and app caches, Windows Update leftovers, crash dumps,
  shader caches, developer caches (off by default) and the hibernation file, then removes what you select.
- **Trim / Optimize**: trims SSDs and defragments hard disks via `defrag.exe` with live progress,
  plus history from Windows' own log and the automatic schedule.
- **Manage**: rename a drive, change its letter, check it for errors.
- **This PC**: every physical disk with its partition layout, health, temperature, wear and power-on hours.
- Notices drives being plugged in or removed; refreshes free space as it changes.

Platter runs in standard mode. Trim, defragment, error checks, letter changes and some clean-up items need
administrator rights; Platter explains this and offers **Restart as administrator** when you reach for one.

## Performance

Measured with `bench/launch.ps1` (5 launches each, same PC, 15 s window, CPU summed over Platter + WebView2):

| | Before | After |
| --- | --- | --- |
| Window on screen | ~421 ms, empty | ~357 ms, in the theme's colours |
| UI painted with data | 537 ms | 477 ms |
| Disk info ready | 1,357 ms (PowerShell) | 557 ms (native WMI) |
| Optimize history ready | 1,594 ms (PowerShell) | 530 ms (Event Log API) |
| Clean-up sizes shown | 9.8 s (measured every launch) | 0.48 s (cached; re-measured in the background when older than 12 h) |
| CPU used in first 15 s | 20.7 s | 1.9 s |
| Memory (whole process tree) | 477 MB | 398 MB |
| Full C: scan, 1.8M files | 11.8 s | 8.5–9.0 s |

Run it: `powershell -File bench\launch.ps1 -Runs 5` (add `-NoCache` to time a first launch).
Set `PLATTER_PERF=<file>` to log a startup timeline.

## Build

Needs Rust stable. This machine uses the GNU toolchain (no Visual Studio needed) plus a MinGW `windres`
on `PATH` (Strawberry Perl or WinLibs both provide one).

```sh
cargo build --release
./target/release/platter.exe
```

`cargo run` works for development. The UI lives in `ui/` and is embedded at compile time.

## Layout

| Path | What |
| --- | --- |
| `src/main.rs` | Tauri setup, command registration, drive-change watcher |
| `src/volumes.rs` | Drive letters, capacity, SSD/HDD detection, allocation map |
| `src/disks.rs` | Physical disks, partitions, reliability counters |
| `src/scan.rs` | Parallel space scanner |
| `src/cleanup.rs` | Clean-up targets, measuring and deleting |
| `src/optimize.rs` | `defrag.exe` runs, optimization history and schedule |
| `src/manage.rs` | Label, drive letter, error check, Explorer, Recycle Bin |
| `ui/` | `index.html`, `styles.css`, `app.js`, `icons.js` (Lucide subset), Archivo font |
| `ui/mock.js` | Fake backend so the UI can be previewed in a plain browser |
| `CONTRACT.md` | Every command, event and JSON shape between backend and UI |

Preview the UI without Rust: open `ui/index.html` in Chrome started with `--allow-file-access-from-files`.
Hash options drive the mock: `#theme=dark&view=cleanup`, `#tab=space&scan=1`, `#view=pc&admin=1`.
