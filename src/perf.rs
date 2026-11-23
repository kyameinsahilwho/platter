//! Startup/interaction timeline for measuring performance. Off unless the environment variable
//! `PLATTER_PERF` names a log file; then every mark appends "<ms since main() started>\t<name>".
//! The first line, `rust:process-created`, is negative: how long Windows took to load the process
//! before `main` ran, so the timeline can also be read from process creation.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();
static LOG: OnceLock<Option<std::sync::Mutex<std::fs::File>>> = OnceLock::new();

/// Call first thing in main().
pub fn init() {
    START.get_or_init(Instant::now);
    LOG.get_or_init(|| {
        let path = std::env::var_os("PLATTER_PERF")?;
        std::fs::OpenOptions::new().create(true).append(true).open(path).ok().map(std::sync::Mutex::new)
    });
    if enabled() {
        if let Some(ms) = crate::util::ms_since_process_created() {
            write_line(-ms, "rust:process-created");
        }
    }
}

fn write_line(ms: f64, name: &str) {
    let Some(Some(log)) = LOG.get() else { return };
    if let Ok(mut f) = log.lock() {
        let _ = writeln!(f, "{ms:.1}\t{name}");
    }
}

pub fn mark(name: &str) {
    if !enabled() {
        return;
    }
    let ms = START.get().map(|s| s.elapsed().as_secs_f64() * 1000.0).unwrap_or(0.0);
    write_line(ms, name);
}

/// `mark(name)` the first time `once` is passed here, nothing afterwards.
pub fn mark_once(once: &AtomicBool, name: &str) {
    if enabled() && !once.swap(true, Ordering::Relaxed) {
        mark(name);
    }
}

pub fn enabled() -> bool {
    matches!(LOG.get(), Some(Some(_)))
}

/// UI-side marks ("js:first-render", …) so both halves share one clock.
#[tauri::command]
pub fn perf_mark(name: String) {
    mark(&name);
}

#[tauri::command]
pub fn perf_enabled() -> bool {
    enabled()
}
