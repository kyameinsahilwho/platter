<p align="center">
  <img src="icons/128x128.png" width="96" height="96" alt="Platter app icon">
</p>

# Platter

**Platter is a free Windows disk utility, written in Rust with Tauri v2, that shows what is using space on your drives, cleans caches and app leftovers, finds duplicate files, trims SSDs and defragments hard disks, and checks disk health in one app.**

<p align="center">
  <img alt="Version 1.2.0" src="https://img.shields.io/badge/version-1.2.0-2f6f4f">
  <img alt="Platform: Windows" src="https://img.shields.io/badge/platform-Windows%20x64-0078D6?logo=windows&logoColor=white">
  <img alt="Built with Rust" src="https://img.shields.io/badge/Rust-stable-000000?logo=rust&logoColor=white">
  <img alt="Tauri v2" src="https://img.shields.io/badge/Tauri-v2-24C8DB?logo=tauri&logoColor=white">
  <img alt="UI: vanilla HTML, CSS and JS" src="https://img.shields.io/badge/UI-vanilla%20HTML%2FCSS%2FJS-f0db4f">
</p>

## Why Platter

- **Full C: scan of 1.8 million files in 8.5 to 9.0 s.** A parallel Rust scanner feeds a squarified treemap by file type, skips junctions and counts OneDrive placeholders as 0 bytes (timed by hand on the development PC).
- **A real NTFS cluster map.** The drive overview reads the volume's allocation bitmap straight from Windows (`FSCTL_GET_VOLUME_BITMAP`), not a simulation.
- **27 clean-up targets.** Including temp files, Windows Update downloads, crash dumps, shader caches, the thumbnail cache, the Recycle Bin, 5 browsers, 4 apps (Teams, Discord, Spotify, VS Code), 7 developer caches (off by default), the hibernation file and old downloads (listed for review, not deleted).
- **Careful removal.** Duplicates, the uninstaller and the Space tab send files and folders to the Recycle Bin, registry entries, services and scheduled tasks are backed up (`.reg` / task XML) first, and Duplicates never removes the last copy of a file. Cache clean-up deletes for good, but only what you select.
- **Opens fast and stays quiet.** UI painted with data in ~477 ms, clean-up sizes shown in 0.48 s from cache, and 1.9 s of CPU in the first 15 s (down from 20.7 s), all from the repo's own launch benchmark.
- **Nothing leaves your PC.** The app has no network code, no accounts and no telemetry. Its cache and backups stay in `%LOCALAPPDATA%\app.platter.disks`.

## Features

### Drive overview
- Free space, a cluster allocation map read from NTFS, drive details and disk health.
- Plain suggestions such as "only 8% free", "not trimmed in 6 weeks" or "10 GB of temp files can go".
- Notices drives being plugged in or removed (and media swapped in removable or optical drives) and refreshes free space as it changes.

### What's using space
- Fast parallel scan of a whole drive or any folder, with live progress.
- Squarified treemap colored by 8 file categories: video, images, audio, documents, archives and installers, apps and system, developer files, other.
- Drill down, list the largest files, show any item in Explorer or move it to the Recycle Bin.
- Hard-linked files count once; sparse, compressed and WOF-compressed files count their real on-disk size.

### Clean up
- **Caches:** measures every target first, then removes only what you select. Deletion is handle-based: each file is opened without following reparse points and its final path is re-checked, so a junction or symlink inside a cache folder is never followed. Temp files from the last 24 hours are kept.
- **Uninstall apps (Deep Uninstaller):** lists desktop apps, runs the app's own uninstaller, then finds what it left behind: folders, app data, registry entries, services, scheduled tasks and startup entries, each with a high, medium or low confidence rating. **Look for leftovers** also finds the remains of apps removed earlier.
- **Duplicates:** finds identical files by content (size, then a first-and-last 64 KB hash, then a full 128-bit xxh3 hash), similar photos and videos (perceptual hash of Windows' own thumbnails) and folders with identical contents. It recommends which copy to keep and leaves app, game and project folders (`.git`, `package.json`, `Cargo.toml`, `node_modules`, `steamapps` and more) alone.

### Trim and optimize
- Trims SSDs and defragments or analyzes hard disks through Windows' own `defrag.exe`, with live progress and cancel.
- Shows optimization history from the Windows Event Log and the state of the automatic schedule.

### Manage
- Rename a drive, change its letter (the system drive is protected) and check it for errors with a read-only online scan.

### This PC
- Every physical disk with its partition layout (EFI, recovery, reserved, data), health, temperature, wear, power-on hours and read/write errors (temperature, wear and error counters need administrator rights).

### Comfort
- Light, dark and system themes, painted before the first frame so there is no white flash.
- Remembers window position and size, runs as a single instance, and switches tabs with the keys 1 to 4.

## How it works

Platter is a Tauri v2 app: a Rust backend in `src/` talks to Windows directly through `windows-sys` (Win32 and NT APIs, native WMI over COM, the Event Log API; a few actions, like the error check and letter changes, still run through PowerShell) and a plain HTML, CSS and JavaScript UI in `ui/` is embedded in the executable at compile time. Every command, event and JSON shape between the two is documented in [`CONTRACT.md`](CONTRACT.md).

### Administrator rights

Each time it opens, Platter asks Windows for administrator rights, because trim, defragment, error checks, letter changes and some clean-up items need them. Say no and it opens in standard mode, offering **Restart as administrator** when you reach for one of those tools. Untick **Ask at startup** in the sidebar to always open in standard mode, or start it with `--no-elevate` to skip the prompt once.

### Safety rules

- Clean-up never follows junctions or symlinks and refuses folders that point somewhere unexpected (for example a `TEMP` variable set to a profile folder).
- The uninstaller never touches the Windows folder, the roots of Program Files, ProgramData, AppData or your profile, drive roots, or anything another installed app, service or task still uses. Registry changes are limited to app-specific keys below `Software`, single Uninstall and App Paths entries, and single Run/RunOnce values.
- Leftover registry keys, services and tasks are exported to `%LOCALAPPDATA%\app.platter.disks\backups\` first; an item whose backup fails is not removed.
- Duplicates only uses the Recycle Bin, skips any file that changed since the scan, and refuses a request that would leave a group with no copy.

## Performance

Measured with `bench/launch.ps1` (5 launches each, same PC, 15 s window, CPU summed over Platter and WebView2), except the scan row:

| | Before | After |
| --- | --- | --- |
| Window on screen | ~421 ms, empty | ~357 ms, in the theme's colors |
| UI painted with data | 537 ms | 477 ms |
| Disk info ready | 1,357 ms (PowerShell) | 557 ms (native WMI) |
| Optimize history ready | 1,594 ms (PowerShell) | 530 ms (Event Log API) |
| Clean-up sizes shown | 9.8 s (measured every launch) | 0.48 s (cached; re-measured in the background when older than 12 h) |
| CPU used in first 15 s | 20.7 s | 1.9 s |
| Memory (whole process tree) | 477 MB | 398 MB |
| Full C: scan, 1.8M files* | 11.8 s | 8.5 to 9.0 s |

\* Timed manually, not by `launch.ps1`.

Run it with `powershell -File bench\launch.ps1 -Runs 5` (add `-NoCache` to time a first launch). Set `PLATTER_PERF=<file>` to log a startup timeline.

## Quick start

### Requirements

- Windows x64 with the WebView2 runtime (the installer downloads it if it is missing).
- Rust stable. The project builds on the GNU toolchain (`x86_64-pc-windows-gnu`, no Visual Studio needed) with a MinGW `windres` on `PATH` (Strawberry Perl or WinLibs both provide one).
- Node.js, only to build the installer.

### Build and run from source

```sh
git clone https://github.com/kyameinsahilwho/platter.git
cd platter
cargo run                 # development build
cargo build --release     # optimized build (fat LTO, stripped)
```

If the project path contains a space, set `target-dir` in `.cargo/config.toml` to a folder outside it, because MinGW `windres` can't handle a build folder whose path contains a space. `platter.exe` lands in `<target-dir>/release/`.

### Build the installer

```sh
npx -y @tauri-apps/cli@2 build --bundles nsis
```

The NSIS installer (per-user, no admin needed to install) lands in `<target-dir>/release/bundle/nsis/`. Its hooks ask a running Platter to close before files are replaced or removed.

### Preview the UI without Rust

`ui/mock.js` is a fake backend, so the UI can be previewed in a plain browser: open `ui/index.html` in Chrome started with `--allow-file-access-from-files`. Hash options drive the mock, for example `#theme=dark&view=cleanup`, `#tab=space&scan=1`, `#view=pc&admin=1` or `#view=cleanup&sub=dupes&dupes=done`.

## Tech stack

| Layer | What |
| --- | --- |
| Backend | Rust (2021 edition), Tauri v2, `windows-sys` 0.59, `rayon` for parallel scanning, `xxhash-rust` (xxh3) for duplicate hashing |
| UI | Vanilla ES modules, HTML and CSS, a Lucide icon subset, the Archivo font (bundled locally) |
| Windows APIs | `FSCTL_GET_VOLUME_BITMAP`, native WMI (COM), Event Log API, Shell thumbnails, `SHFileOperationW` (Recycle Bin), Service Control Manager, registry |
| Packaging | NSIS installer via the Tauri CLI, with custom install and uninstall hooks |

### Project layout

| Path | What |
| --- | --- |
| `src/main.rs` | Tauri setup, command registration, startup elevation, drive-change watcher |
| `src/volumes.rs` | Drive letters, capacity, SSD/HDD detection, allocation map |
| `src/disks.rs` | Physical disks, partitions, reliability counters |
| `src/scan.rs` | Parallel space scanner |
| `src/cleanup.rs` | Clean-up targets, measuring and handle-based deleting |
| `src/uninstall.rs` | Installed apps, running uninstallers, leftover and orphan detection, backups |
| `src/dupes.rs` | Exact, similar and folder duplicate detection |
| `src/appicon.rs` | App icons for the uninstaller list |
| `src/optimize.rs` | `defrag.exe` runs, optimization history and schedule |
| `src/manage.rs` | Label, drive letter, error check, Explorer, Recycle Bin |
| `src/util.rs` | Elevation and UAC relaunch, single instance, shared helpers |
| `src/cache.rs`, `src/perf.rs` | Last-known results for instant first paint, startup timeline |
| `ui/` | `index.html`, `styles.css`, `app.js`, `icons.js`, `mock.js`, fonts |
| `installer/` | NSIS hooks and installer artwork |
| `bench/launch.ps1` | Launch benchmark (timeline, CPU and memory) |
| `CONTRACT.md` | Every command, event and JSON shape between backend and UI |

## FAQ

### Is Platter free?
Yes. Platter has no paid tiers, accounts or ads, and the full source is in this repository so you can build it yourself with `cargo build --release`.

### Does Platter upload my files or need the internet?
No. The app contains no network code and sends no telemetry; scans, caches and backups stay on your PC under `%LOCALAPPDATA%\app.platter.disks`. The only download is the WebView2 runtime, fetched by the installer if Windows doesn't already have it.

### How do I find what is taking up space on my C: drive?
Open the drive in the sidebar and choose the **What's using space** tab. Platter scans the drive in parallel, draws a treemap by file type and lists the largest files, which you can open in Explorer or move to the Recycle Bin.

### Is it safe to delete what Platter suggests?
Platter only deletes what you select, keeps temp files from the last 24 hours, never follows junctions or symlinks, and sends files and folders from Duplicates and Uninstall apps to the Recycle Bin. Registry entries, services and tasks are backed up before removal, and app, game and project folders are never recommended for deletion.

### Does Platter need administrator rights?
Only for some tools: trim, defragment, error checks, drive letter changes, disk reliability counters and a few clean-up items. It asks at startup, and if you decline it runs in standard mode with everything else available.

### How is Platter different from WinDirStat?
Both show a treemap of what is using space. Platter also cleans caches, uninstalls apps and removes their leftovers, finds duplicate and similar files, trims SSDs, defragments hard disks and shows disk health in the same app.

## More from CodingMarvel

Platter is part of [CodingMarvel](https://codingmarvel.com), a collection of small, focused apps.

- [MAX File Recovery](https://github.com/kyameinsahilwho/ReincarnateMax): Windows data recovery, also in Rust
- [Love for PDF](https://pdf.codingmarvel.com): 35+ PDF tools that run in your browser
- [Video Grid Maker](https://video-grid-maker.codingmarvel.com): video contact sheets
- [Ehmatrix](https://ehmatrix.codingmarvel.com): Eisenhower matrix to-do list
- [Pollytasks](https://pollytasks.codingmarvel.com): gamified tasks and habits
- [HabitGrid](https://habitgrid.codingmarvel.com): printable habit tracker PDFs
- [Chrome extensions](https://github.com/kyameinsahilwho/cool-chrome-extensions)
