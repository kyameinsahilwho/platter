//! Drive optimization through Windows' own `defrag.exe` (trim, defragment, analyze), its event-log
//! history and the built-in "Optimize drives" scheduled task.
//!
//! Note: defrag.exe exits with code 0 even when it fails (e.g. "insufficient privileges (0x89000024)"),
//! so success is judged from the output as well as the exit code.

use crate::util;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::Emitter;

const ADMIN: &str = "Optimizing drives needs administrator rights. Restart Platter as administrator.";
const BUSY: &str = "Another optimization is already running.";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Trim,
    Defrag,
    Analyze,
}

impl Mode {
    fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "trim" | "retrim" => Some(Mode::Trim),
            "defrag" | "defragment" => Some(Mode::Defrag),
            "analyze" | "analyse" => Some(Mode::Analyze),
            _ => None,
        }
    }
    fn flag(self) -> &'static str {
        match self {
            Mode::Trim => "/L",
            Mode::Defrag => "/D",
            Mode::Analyze => "/A",
        }
    }
    fn key(self) -> &'static str {
        match self {
            Mode::Trim => "trim",
            Mode::Defrag => "defrag",
            Mode::Analyze => "analyze",
        }
    }
    fn stage(self) -> &'static str {
        match self {
            Mode::Trim => "Retrim",
            Mode::Defrag => "Defragmentation",
            Mode::Analyze => "Analysis",
        }
    }
    fn noun(self) -> &'static str {
        match self {
            Mode::Trim => "trim",
            Mode::Defrag => "defragmentation",
            Mode::Analyze => "analysis",
        }
    }
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct OptimizeProgress {
    pub letter: String,
    pub mode: String,
    pub percent: Option<f32>,
    pub stage: String,
    pub line: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct OptimizeDone {
    pub letter: String,
    pub mode: String,
    pub ok: bool,
    pub message: String,
    pub fragmented_percent: Option<f32>,
    pub report: String,
}

struct Running {
    pid: u32,
    /// Our own handle to defrag.exe. While it is open Windows can't hand the PID to another
    /// process, so cancel never signals or kills a stranger.
    process: OwnedHandle,
    stop: Arc<StopState>,
}

/// What `optimize_cancel` actually did to the run.
#[derive(Default)]
struct StopState {
    /// Stop was clicked while defrag.exe was still running.
    requested: AtomicBool,
    /// defrag.exe didn't stop after Ctrl+C (or couldn't be sent one) and was terminated.
    killed: AtomicBool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Stop {
    /// Nobody pressed Stop (or it came after defrag.exe had already finished).
    None,
    /// Stop was pressed and defrag.exe was asked to cancel with Ctrl+C, as in a console.
    Asked,
    /// defrag.exe was terminated. The Storage Optimizer service may still be busy with the volume.
    Killed,
}

impl StopState {
    fn outcome(&self) -> Stop {
        if self.killed.load(Ordering::SeqCst) {
            Stop::Killed
        } else if self.requested.load(Ordering::SeqCst) {
            Stop::Asked
        } else {
            Stop::None
        }
    }
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

fn running() -> std::sync::MutexGuard<'static, Option<Running>> {
    RUNNING.lock().unwrap_or_else(|e| e.into_inner())
}

fn parse_letter(letter: &str) -> Result<char, String> {
    let t = letter.trim().trim_end_matches('\\').trim_end_matches(':');
    let mut it = t.chars();
    match (it.next(), it.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => Ok(c.to_ascii_uppercase()),
        _ => Err("Pick a drive to optimize.".into()),
    }
}

/// Everything that can fail before a process is spawned.
fn preflight(letter: &str, mode: &str) -> Result<(char, Mode), String> {
    let l = parse_letter(letter)?;
    let m = Mode::parse(mode).ok_or("Platter doesn't know that kind of optimization.")?;
    if !util::is_elevated() {
        return Err(ADMIN.into());
    }
    if running().is_some() {
        return Err(BUSY.into());
    }
    Ok((l, m))
}

fn defrag_exe() -> std::path::PathBuf {
    util::env_path("SystemRoot")
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
        .join("System32")
        .join("defrag.exe")
}

#[tauri::command]
pub async fn optimize_start(app: tauri::AppHandle, letter: String, mode: String) -> Result<(), String> {
    let (l, m) = preflight(&letter, &mode)?;

    let mut guard = running();
    if guard.is_some() {
        return Err(BUSY.into());
    }
    allow_ctrl_c_in_children();
    let mut child = Command::new(defrag_exe())
        .args([format!("{l}:").as_str(), m.flag(), "/U", "/V"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(util::CREATE_NO_WINDOW)
        .spawn()
        .map_err(|_| "Windows' drive optimizer (defrag.exe) couldn't be started.".to_string())?;

    let process = match child.as_handle().try_clone_to_owned() {
        Ok(h) => h,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Windows' drive optimizer (defrag.exe) couldn't be started.".into());
        }
    };
    let stop = Arc::new(StopState::default());
    let pid = child.id();
    *guard = Some(Running { pid, process, stop: stop.clone() });
    drop(guard);

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    std::thread::spawn(move || {
        let err_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut e) = stderr {
                let _ = e.read_to_end(&mut buf);
            }
            buf
        });

        let letter_s = l.to_string();
        let mut parser = OutputParser::new(m);
        if let Some(mut out) = stdout {
            let mut chunk = [0u8; 4096];
            loop {
                match out.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        for ev in parser.feed(&chunk[..n]) {
                            let _ = app.emit("optimize-progress", progress_payload(&letter_s, m, ev));
                        }
                    }
                }
            }
        }
        for ev in parser.finish() {
            let _ = app.emit("optimize-progress", progress_payload(&letter_s, m, ev));
        }
        let status = child.wait();
        let err_bytes = err_reader.join().unwrap_or_default();
        for line in split_lines(&err_bytes) {
            parser.push_report(&line);
        }

        let exit_ok = status.map(|s| s.success()).unwrap_or(false);

        let done = {
            // Read under the lock: optimize_cancel decides and records what it does while holding it.
            let mut g = running();
            let done = parser.summarize(l, exit_ok, stop.outcome());
            if g.as_ref().map(|r| r.pid) == Some(pid) {
                *g = None;
            }
            done
        };
        let _ = app.emit("optimize-done", done);
    });
    Ok(())
}

fn progress_payload(letter: &str, m: Mode, ev: ProgressLine) -> OptimizeProgress {
    OptimizeProgress { letter: letter.to_string(), mode: m.key().into(), percent: ev.percent, stage: ev.stage, line: ev.line }
}

/// How long defrag.exe gets to wind down after Ctrl+C before it is terminated.
const CANCEL_GRACE_MS: u32 = 15_000;

/// Stop the running optimization. Returns at once; the outcome arrives as `optimize-done`.
#[tauri::command]
pub async fn optimize_cancel() -> Result<(), String> {
    std::thread::Builder::new()
        .name("optimize-cancel".into())
        .spawn(cancel_running)
        .map(|_| ())
        .map_err(|_| "Platter couldn't stop the optimization.".to_string())
}

fn cancel_running() {
    let (pid, process, stop) = {
        let g = running();
        let Some(r) = g.as_ref() else { return };
        // A run that already ended on its own keeps its real result.
        if exited(&r.process, 0) {
            return;
        }
        // Another Stop click is already handling it.
        if r.stop.requested.swap(true, Ordering::SeqCst) {
            return;
        }
        match r.process.try_clone() {
            Ok(h) => (r.pid, h, r.stop.clone()),
            Err(_) => {
                // Can't keep a handle of our own: stop it here, while the lock keeps Running alive.
                if terminate(&r.process) {
                    r.stop.killed.store(true, Ordering::SeqCst);
                }
                return;
            }
        }
    };
    // `process` keeps the PID reserved, so `pid` can't refer to another program from here on.
    stop_process(pid, &process, CANCEL_GRACE_MS, &stop);
}

/// Ctrl+C is the documented way to interrupt defrag.exe: it then asks the Storage Optimizer service
/// to cancel and exits (reporting 0x89000006). Only if that doesn't work, or Ctrl+C can't be sent,
/// is defrag.exe terminated — through our own handle, never by PID.
fn stop_process(pid: u32, process: &OwnedHandle, grace_ms: u32, stop: &StopState) {
    if send_ctrl_c(pid) && exited(process, grace_ms) {
        return;
    }
    // Under the lock, so the worker's summary sees either "asked" or "killed", never a half state.
    let _g = running();
    if !exited(process, 0) && terminate(process) {
        stop.killed.store(true, Ordering::SeqCst);
    }
}

fn exited(process: &OwnedHandle, wait_ms: u32) -> bool {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    unsafe { WaitForSingleObject(process.as_raw_handle() as _, wait_ms) == WAIT_OBJECT_0 }
}

fn terminate(process: &OwnedHandle) -> bool {
    use windows_sys::Win32::System::Threading::TerminateProcess;
    unsafe { TerminateProcess(process.as_raw_handle() as _, 1) != 0 }
}

/// Console functions (windows-sys has them only behind the `Win32_System_Console` feature).
mod console {
    pub const CTRL_C_EVENT: u32 = 0;
    pub const CTRL_BREAK_EVENT: u32 = 1;
    pub type Handler = unsafe extern "system" fn(u32) -> i32;
    #[link(name = "kernel32")]
    extern "system" {
        pub fn AttachConsole(pid: u32) -> i32;
        pub fn FreeConsole() -> i32;
        pub fn GenerateConsoleCtrlEvent(event: u32, group: u32) -> i32;
        pub fn SetConsoleCtrlHandler(handler: Option<Handler>, add: i32) -> i32;
    }
}

/// Keeps Platter alive when it sends Ctrl+C to defrag's console (every attached process gets it).
/// A real handler, not the "ignore Ctrl+C" flag: that flag would be inherited by the next defrag.exe.
unsafe extern "system" fn swallow_ctrl_c(event: u32) -> i32 {
    let mine = event == console::CTRL_C_EVENT || event == console::CTRL_BREAK_EVENT;
    if mine {
        CTRL_C_SEEN.store(true, Ordering::SeqCst);
    }
    mine as i32
}

static CTRL_C_SEEN: AtomicBool = AtomicBool::new(false);

/// A process started with Ctrl+C disabled (e.g. launched in a new process group) passes that on to
/// every child, and defrag.exe would then ignore our Ctrl+C. Clear it before starting defrag.exe.
fn allow_ctrl_c_in_children() {
    unsafe { console::SetConsoleCtrlHandler(None, 0) };
}

/// Send Ctrl+C to the hidden console of `pid`. Works when Platter has no console of its own (the
/// release build is a GUI app); returns false if it has one (debug build) or the console can't be reached.
fn send_ctrl_c(pid: u32) -> bool {
    static CONSOLE: Mutex<()> = Mutex::new(());
    let _g = CONSOLE.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        if console::AttachConsole(pid) == 0 {
            return false;
        }
        // Register after attaching: attaching resets the handler list (a handler added before
        // AttachConsole wasn't called in testing). Remove-then-add keeps a single registration.
        console::SetConsoleCtrlHandler(Some(swallow_ctrl_c), 0);
        if console::SetConsoleCtrlHandler(Some(swallow_ctrl_c), 1) == 0 {
            console::FreeConsole();
            return false;
        }
        CTRL_C_SEEN.store(false, Ordering::SeqCst);
        let sent = console::GenerateConsoleCtrlEvent(console::CTRL_C_EVENT, 0) != 0;
        if sent {
            // The event reaches this process on its own thread a moment later. Stay attached until our
            // handler has swallowed it; detaching first would leave Windows' default handler (exit) in charge.
            let t = std::time::Instant::now();
            while !CTRL_C_SEEN.load(Ordering::SeqCst) && t.elapsed() < std::time::Duration::from_secs(3) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        console::FreeConsole();
        sent
    }
}

// ---------------------------------------------------------------------------------------------
// defrag.exe output parsing
// ---------------------------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
struct ProgressLine {
    percent: Option<f32>,
    stage: String,
    line: String,
}

struct OutputParser {
    mode: Mode,
    pending: Vec<u8>,
    last_line: String,
    stage: String,
    /// Report lines; consecutive progress updates of the same stage collapse into one line.
    report: Vec<String>,
    last_report_was_progress_of: Option<String>,
}

impl OutputParser {
    fn new(mode: Mode) -> Self {
        OutputParser {
            mode,
            pending: Vec::new(),
            last_line: String::new(),
            stage: mode.stage().into(),
            report: Vec::new(),
            last_report_was_progress_of: None,
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> Vec<ProgressLine> {
        let mut events = Vec::new();
        for &b in bytes {
            if b == b'\r' || b == b'\n' {
                let raw = std::mem::take(&mut self.pending);
                if let Some(ev) = self.line(&raw) {
                    events.push(ev);
                }
            } else {
                self.pending.push(b);
            }
        }
        events
    }

    fn finish(&mut self) -> Vec<ProgressLine> {
        let raw = std::mem::take(&mut self.pending);
        self.line(&raw).into_iter().collect()
    }

    fn line(&mut self, raw: &[u8]) -> Option<ProgressLine> {
        let text = decode_oem(raw);
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let progress = parse_progress(text);
        let percent = match &progress {
            Some((stage, pct)) => {
                if let Some(s) = stage {
                    self.stage = s.clone();
                }
                Some(*pct)
            }
            None => None,
        };
        self.push_report_inner(text, progress.is_some());
        if text == self.last_line {
            return None; // throttle identical consecutive lines (\r rewrites)
        }
        self.last_line = text.to_string();
        Some(ProgressLine { percent, stage: self.stage.clone(), line: text.to_string() })
    }

    fn push_report(&mut self, text: &str) {
        let t = text.trim();
        if !t.is_empty() {
            self.push_report_inner(t, false);
        }
    }

    fn push_report_inner(&mut self, text: &str, is_progress: bool) {
        if is_progress {
            if self.last_report_was_progress_of.as_deref() == Some(self.stage.as_str()) {
                if let Some(last) = self.report.last_mut() {
                    *last = text.to_string();
                    return;
                }
            }
            self.last_report_was_progress_of = Some(self.stage.clone());
        } else {
            self.last_report_was_progress_of = None;
        }
        self.report.push(text.to_string());
    }

    fn summarize(&self, letter: char, exit_ok: bool, stop: Stop) -> OptimizeDone {
        let report = self.report.join("\n").trim().to_string();
        let fragmented = self.report.iter().rev().find_map(|l| parse_fragmented(l));
        let error = self.report.iter().find_map(|l| parse_error(l));
        let mode = self.mode;

        let (ok, message) = if matches!(&error, Some((0x8900_0006, _))) {
            // defrag.exe confirmed that the Storage Optimizer cancelled the operation.
            (false, "Stopped before finishing.".to_string())
        } else if stop == Stop::Killed || (stop == Stop::Asked && !exit_ok) {
            // defrag.exe ended without confirming the cancel, so the service may still be working.
            (
                false,
                format!("Stopped the {} of drive {letter}. Windows may finish the current pass in the background.", mode.noun()),
            )
        } else if let Some((code, text)) = &error {
            (false, friendly_error(mode, letter, *code, text))
        } else if !exit_ok {
            (false, format!("Windows couldn't finish the {} of drive {letter}.", mode.noun()))
        } else {
            let msg = match mode {
                Mode::Trim => "Trim finished. The SSD now knows which blocks are free.".to_string(),
                Mode::Defrag => "Defragmentation finished. Files are now stored in fewer pieces.".to_string(),
                Mode::Analyze => match fragmented {
                    Some(p) => format!("Analysis finished: {}% of the drive is fragmented.", fmt_pct(p)),
                    None => "Analysis finished.".to_string(),
                },
            };
            (true, msg)
        };
        OptimizeDone {
            letter: letter.to_string(),
            mode: mode.key().into(),
            ok,
            message,
            fragmented_percent: fragmented,
            report,
        }
    }
}

fn fmt_pct(p: f32) -> String {
    if p.fract() == 0.0 {
        format!("{}", p as i64)
    } else {
        format!("{p:.1}")
    }
}

/// defrag writes in the console's OEM code page. Decode it properly; fall back to lossy UTF-8.
fn decode_oem(raw: &[u8]) -> String {
    use windows_sys::Win32::Globalization::{MultiByteToWideChar, CP_OEMCP};
    if raw.is_empty() {
        return String::new();
    }
    if raw.is_ascii() {
        return String::from_utf8_lossy(raw).into_owned();
    }
    let len = raw.len().min(i32::MAX as usize) as i32;
    let n = unsafe { MultiByteToWideChar(CP_OEMCP, 0, raw.as_ptr(), len, std::ptr::null_mut(), 0) };
    if n <= 0 {
        return String::from_utf8_lossy(raw).into_owned();
    }
    let mut buf = vec![0u16; n as usize];
    let n = unsafe { MultiByteToWideChar(CP_OEMCP, 0, raw.as_ptr(), len, buf.as_mut_ptr(), n) };
    if n <= 0 {
        return String::from_utf8_lossy(raw).into_owned();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

fn split_lines(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|&b| b == b'\r' || b == b'\n')
        .map(|l| decode_oem(l).trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Number immediately before a '%' (allowing one space), e.g. "42% complete" or "42 %".
fn percent_in(s: &str) -> Option<f32> {
    let idx = s.find('%')?;
    let before = s[..idx].trim_end();
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_digit() || *c == '.' || *c == ',')
        .last()
        .map(|(i, _)| i)?;
    before[start..].replace(',', ".").parse::<f32>().ok().filter(|p| (0.0..=100.0).contains(p))
}

/// "Retrim:  42% complete..." → (Some("Retrim"), 42). "42% complete" → (None, 42).
/// Report lines such as "Total fragmented space = 3%" are not progress.
fn parse_progress(line: &str) -> Option<(Option<String>, f32)> {
    let pct = percent_in(line)?;
    let pct_pos = line.find('%')?;
    if line[..pct_pos].contains('=') {
        return None;
    }
    if let Some(colon) = line[..pct_pos].find(':') {
        let stage = line[..colon].trim();
        if !stage.is_empty() && stage.chars().all(|c| c.is_alphabetic() || c == ' ' || c == '-') {
            return Some((Some(stage.to_string()), pct));
        }
        return None;
    }
    let after = line[pct_pos + 1..].trim_start().to_ascii_lowercase();
    if line[..pct_pos].trim().chars().all(|c| c.is_ascii_digit() || c == '.' || c == ',' || c == ' ')
        || after.starts_with("complete")
    {
        return Some((None, pct));
    }
    None
}

/// "Total fragmented space = 3%" / "Fragmented space = 12 %"
fn parse_fragmented(line: &str) -> Option<f32> {
    let lower = line.to_ascii_lowercase();
    if !lower.contains("fragmented space") {
        return None;
    }
    let eq = line.find('=')?;
    percent_in(&line[eq + 1..])
}

/// Lines like "The storage optimizer cannot start because ... (0x89000024)".
fn parse_error(line: &str) -> Option<(u32, String)> {
    let t = line.trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    let open = t.rfind("(0x")?;
    let close = t[open..].find(')')? + open;
    let hex = &t[open + 3..close];
    if hex.len() != 8 {
        return None;
    }
    let code = u32::from_str_radix(hex, 16).ok()?;
    if code & 0x8000_0000 == 0 {
        return None; // not a failure HRESULT
    }
    let text = t[..open].trim().trim_end_matches('.').to_string();
    Some((code, text))
}

fn friendly_error(mode: Mode, letter: char, code: u32, text: &str) -> String {
    match code {
        0x8900_0020 => format!("This drive doesn't support {}.", mode.noun()),
        0x8900_0024 => ADMIN.into(),
        0x8900_0001 => format!("Windows couldn't find drive {letter}:."),
        0x8900_002F => "Windows can't optimize this drive because its file system isn't supported.".into(),
        _ if text.is_empty() => format!("Windows couldn't finish the {} of drive {letter} (error 0x{code:08X}).", mode.noun()),
        _ => format!("Windows couldn't finish the {} of drive {letter}. {text}.", mode.noun()),
    }
}

// ---------------------------------------------------------------------------------------------
// History (Application event log) and schedule
// ---------------------------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct OptimizeEvent {
    pub letter: Option<String>,
    /// "Trim" | "Defragment" | "Analyze" | "Boot optimization" | "Slab consolidation" | "Other"
    pub action: String,
    pub time: String,
    pub ok: bool,
    pub message: String,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawEvent {
    id: Option<u32>,
    level: Option<u32>,
    time: Option<String>,
    message: Option<String>,
}

const HISTORY_SCRIPT: &str = r#"
try {
  $e=@(Get-WinEvent -FilterHashtable @{LogName='Application';ProviderName='Microsoft-Windows-Defrag'} -MaxEvents 60 -ErrorAction Stop)
} catch {
  if ("$($_.FullyQualifiedErrorId)" -match 'NoMatchingEventsFound|NoMatchingProvider' -or "$($_.Exception.Message)" -match 'No events were found') { '[]'; exit 0 }
  throw
}
$out=@($e | ForEach-Object { [ordered]@{ id=$_.Id; level=[int]$_.Level; time=$_.TimeCreated.ToString('o'); message="$($_.Message)" } })
ConvertTo-Json -InputObject $out -Depth 3 -Compress
"#;

/// How many of the newest Defrag events are read before filtering (as `Get-WinEvent -MaxEvents 60`).
const HISTORY_READ: usize = 60;

fn history() -> Result<Vec<OptimizeEvent>, String> {
    let events = match evt::defrag_events(HISTORY_READ) {
        Ok(raw) => events_from_raw(raw),
        Err(_) => history_powershell()?,
    };
    crate::perf::mark("rust:history-done");
    crate::cache::write("history", &events);
    Ok(events)
}

/// The original `Get-WinEvent` script; used only when the Event Log API fails.
fn history_powershell() -> Result<Vec<OptimizeEvent>, String> {
    let out = util::powershell(HISTORY_SCRIPT)
        .map_err(|e| format!("Windows didn't share the optimization history: {}.", e.trim().trim_end_matches('.')))?;
    parse_history(&out)
}

fn parse_history(json: &str) -> Result<Vec<OptimizeEvent>, String> {
    let json = json.trim();
    if json.is_empty() {
        return Ok(vec![]);
    }
    let raw: Vec<RawEvent> =
        serde_json::from_str(json).map_err(|_| "Windows returned optimization history Platter couldn't read.".to_string())?;
    Ok(events_from_raw(raw))
}

fn events_from_raw(raw: Vec<RawEvent>) -> Vec<OptimizeEvent> {
    let mut events: Vec<OptimizeEvent> = raw
        .into_iter()
        .filter_map(|r| {
            let message = r.message.unwrap_or_default().trim().to_string();
            let time = r.time.filter(|t| !t.is_empty())?;
            // Only look at "what/where", never at the "because: …" reason.
            let head = message.split(" because").next().unwrap_or(&message);
            let lower = head.to_ascii_lowercase();
            let action = if lower.contains("trim") {
                "Trim"
            } else if lower.contains("boot optimization") {
                "Boot optimization"
            } else if lower.contains("slab consolidation") {
                "Slab consolidation"
            } else if lower.contains("analysis") {
                "Analyze"
            } else if lower.contains("defragmentation") {
                "Defragment"
            } else {
                "Other"
            };
            let info = matches!(r.level, Some(0) | Some(4));
            let ok = match r.id {
                Some(258) => true,
                Some(264) => false,
                _ => info && message.to_ascii_lowercase().contains("successfully"),
            };
            Some(OptimizeEvent { letter: letter_in(head), action: action.into(), time, ok, message })
        })
        .collect();
    // ISO 8601 with the same offset sorts lexically; sort explicitly in case the log isn't ordered.
    events.sort_by(|a, b| b.time.cmp(&a.time));
    events.truncate(40);
    events
}

/// "(C:)" → "C"
fn letter_in(s: &str) -> Option<String> {
    let b = s.as_bytes();
    (0..b.len().saturating_sub(3)).find_map(|i| {
        (b[i] == b'(' && b[i + 1].is_ascii_alphabetic() && b[i + 2] == b':' && b[i + 3] == b')')
            .then(|| (b[i + 1] as char).to_ascii_uppercase().to_string())
    })
}

#[tauri::command]
pub async fn optimize_history() -> Result<Vec<OptimizeEvent>, String> {
    tauri::async_runtime::spawn_blocking(history)
        .await
        .map_err(|_| "Reading the optimization history stopped unexpectedly.".to_string())?
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct OptimizeSchedule {
    pub enabled: bool,
    pub last_run: Option<String>,
    pub next_run: Option<String>,
}

// Fallback only. The Task Scheduler COM API answers in ~50 ms; Get-ScheduledTask needs ~2 s to load its module.
const SCHEDULE_SCRIPT: &str = r#"
$s=New-Object -ComObject Schedule.Service
$s.Connect()
try { $t=$s.GetFolder('\Microsoft\Windows\Defrag').GetTask('ScheduledDefrag') } catch { '{"enabled":false}'; exit 0 }
function iso($d){ if($d -is [DateTime] -and $d.Year -ge 2000){ [DateTime]::SpecifyKind($d,'Local').ToString('o') } else { $null } }
ConvertTo-Json -Compress -InputObject ([ordered]@{ enabled=([bool]$t.Enabled -and $t.State -ne 1); lastRun=(iso $t.LastRunTime); nextRun=(iso $t.NextRunTime) })
"#;

fn schedule() -> Result<OptimizeSchedule, String> {
    let s = match crate::disks::com::on_mta_thread(task::defrag_schedule) {
        Some(Ok(s)) => s,
        // The script drives the same Schedule.Service object, so when the Task Scheduler service itself
        // can't be reached it would only fail the same way after a powershell.exe spawn.
        Some(Err(hr)) => match task::unreachable_reason(hr) {
            Some(why) => return Err(format!("Windows didn't share the optimization schedule: {why}.")),
            None => schedule_powershell()?,
        },
        // The COM thread couldn't start or panicked: the script runs in its own process.
        None => schedule_powershell()?,
    };
    crate::perf::mark("rust:schedule-done");
    crate::cache::write("schedule", &s);
    Ok(s)
}

/// The original COM-through-PowerShell script; used only when the in-process COM call fails.
fn schedule_powershell() -> Result<OptimizeSchedule, String> {
    let out = util::powershell(SCHEDULE_SCRIPT)
        .map_err(|e| format!("Windows didn't share the optimization schedule: {}.", e.trim().trim_end_matches('.')))?;
    serde_json::from_str(out.trim()).map_err(|_| "Windows returned a schedule Platter couldn't read.".to_string())
}

#[tauri::command]
pub async fn optimize_schedule() -> Result<OptimizeSchedule, String> {
    tauri::async_runtime::spawn_blocking(schedule)
        .await
        .map_err(|_| "Reading the optimization schedule stopped unexpectedly.".to_string())?
}

// ---------------------------------------------------------------------------------------------
// Native readers: Event Log API (wevtapi) for history, Task Scheduler COM for the schedule.
// ---------------------------------------------------------------------------------------------

/// Local-time ISO 8601 strings exactly like .NET's `DateTime.ToString("o")` for a local time:
/// `2026-11-23T13:03:41.2919096+05:30`.
mod iso {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct FileTime {
        lo: u32,
        hi: u32,
    }
    /// SYSTEMTIME: year, month, day-of-week, day, hour, minute, second, milliseconds.
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct SystemTime([u16; 8]);

    #[link(name = "kernel32")]
    extern "system" {
        fn FileTimeToSystemTime(ft: *const FileTime, st: *mut SystemTime) -> i32;
        fn SystemTimeToFileTime(st: *const SystemTime, ft: *mut FileTime) -> i32;
        fn GetDynamicTimeZoneInformation(tz: *mut c_void) -> u32;
        fn SystemTimeToTzSpecificLocalTimeEx(tz: *const c_void, utc: *const SystemTime, local: *mut SystemTime) -> i32;
        fn TzSpecificLocalTimeToSystemTimeEx(tz: *const c_void, local: *const SystemTime, utc: *mut SystemTime) -> i32;
    }

    /// DYNAMIC_TIME_ZONE_INFORMATION is 432 bytes; this is comfortably larger and 4-byte aligned.
    struct Tz([u32; 120]);
    impl Tz {
        fn current() -> Option<Tz> {
            let mut tz = Tz([0; 120]);
            // TIME_ZONE_ID_INVALID = 0xFFFFFFFF
            (unsafe { GetDynamicTimeZoneInformation(tz.0.as_mut_ptr() as *mut c_void) } != u32::MAX).then_some(tz)
        }
        fn ptr(&self) -> *const c_void {
            self.0.as_ptr() as *const c_void
        }
    }

    fn to_ft(v: u64) -> FileTime {
        FileTime { lo: v as u32, hi: (v >> 32) as u32 }
    }
    fn from_ft(f: FileTime) -> u64 {
        (f.hi as u64) << 32 | f.lo as u64
    }
    fn st_of(v: u64) -> Option<SystemTime> {
        let mut st = SystemTime::default();
        (unsafe { FileTimeToSystemTime(&to_ft(v), &mut st) } != 0).then_some(st)
    }
    fn ft_of(st: &SystemTime) -> Option<u64> {
        let mut ft = FileTime::default();
        (unsafe { SystemTimeToFileTime(st, &mut ft) } != 0).then(|| from_ft(ft))
    }

    /// `local` and `utc` are the same instant as 100 ns ticks since 1601 on the local and UTC clocks.
    fn format(local: u64, utc: u64) -> Option<String> {
        let st = st_of(local)?.0;
        let frac = local % 10_000_000;
        let offset_min = ((local as i128 - utc as i128) as f64 / 600_000_000.0).round() as i64;
        let (sign, off) = if offset_min < 0 { ('-', -offset_min) } else { ('+', offset_min) };
        Some(format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{frac:07}{sign}{:02}:{:02}",
            st[0],
            st[1],
            st[3],
            st[4],
            st[5],
            st[6],
            off / 60,
            off % 60
        ))
    }

    /// A UTC FILETIME (event log `TimeCreated`) as local time with its offset.
    pub fn from_utc_filetime(utc: u64) -> Option<String> {
        let tz = Tz::current()?;
        let utc_st = st_of(utc)?;
        let mut local_st = SystemTime::default();
        if unsafe { SystemTimeToTzSpecificLocalTimeEx(tz.ptr(), &utc_st, &mut local_st) } == 0 {
            return None;
        }
        // SYSTEMTIME keeps milliseconds only; offsets are whole minutes, so the sub-ms part carries over.
        let local = ft_of(&local_st)? + utc % 10_000;
        format(local, utc)
    }

    /// An OLE Automation DATE holding a *local* wall-clock time (Task Scheduler's run times), like
    /// `[DateTime]::SpecifyKind($d,'Local').ToString('o')`. None before the year 2000 ("never").
    pub fn from_local_ole_date(date: f64) -> Option<String> {
        // Days from 1601-01-01 (FILETIME epoch) to 1899-12-30 (OLE epoch).
        const OLE_EPOCH_DAYS: i64 = 109_205;
        if !date.is_finite() || date < 36_526.0 {
            // 36526 = 2000-01-01
            return None;
        }
        // .NET rounds OLE dates to the millisecond.
        let ms = (date * 86_400_000.0).round() as i64 + OLE_EPOCH_DAYS * 86_400_000;
        let local = (ms as u64).checked_mul(10_000)?;
        let tz = Tz::current()?;
        let local_st = st_of(local)?;
        let mut utc_st = SystemTime::default();
        if unsafe { TzSpecificLocalTimeToSystemTimeEx(tz.ptr(), &local_st, &mut utc_st) } == 0 {
            return None;
        }
        format(local, ft_of(&utc_st)?)
    }
}

/// Application-log events from the `Microsoft-Windows-Defrag` provider through the Event Log API.
mod evt {
    use super::RawEvent;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_ITEMS};
    use windows_sys::Win32::System::EventLog::{
        EvtClose, EvtCreateRenderContext, EvtFormatMessage, EvtFormatMessageEvent, EvtNext, EvtOpenPublisherMetadata,
        EvtQuery, EvtQueryChannelPath, EvtQueryReverseDirection, EvtRender, EvtRenderContextSystem,
        EvtRenderEventValues, EVT_HANDLE,
    };

    const PROVIDER: &str = "Microsoft-Windows-Defrag";
    // Every event of the provider, like Get-WinEvent -FilterHashtable @{ProviderName=…}: it writes
    // 258 (completed) and 264 (failed); anything else still goes through the same parsing rules.
    const QUERY: &str = "*[System[Provider[@Name='Microsoft-Windows-Defrag']]]";
    // EVT_SYSTEM_PROPERTY_ID indexes into the system render context.
    const SYS_EVENT_ID: usize = 2;
    const SYS_LEVEL: usize = 4;
    const SYS_TIME_CREATED: usize = 8;
    const ERROR_EVT_UNRESOLVED_VALUE_INSERT: u32 = 15029;
    const ERROR_EVT_UNRESOLVED_PARAMETER_INSERT: u32 = 15030;

    struct H(EVT_HANDLE);
    impl Drop for H {
        fn drop(&mut self) {
            if self.0 != 0 {
                unsafe { EvtClose(self.0) };
            }
        }
    }
    fn handle(h: EVT_HANDLE) -> Result<H, u32> {
        if h == 0 {
            Err(unsafe { GetLastError() })
        } else {
            Ok(H(h))
        }
    }

    /// The newest `max` events, newest first. Err carries the Win32 error.
    pub fn defrag_events(max: usize) -> Result<Vec<RawEvent>, u32> {
        std::thread::scope(|s| {
            // Loading the provider's message table (~15 ms) overlaps the log scan.
            let meta = s.spawn(|| {
                let name = crate::util::wide(PROVIDER);
                handle(unsafe { EvtOpenPublisherMetadata(0, name.as_ptr(), std::ptr::null(), 0, 0) })
            });
            let (path, query) = (crate::util::wide("Application"), crate::util::wide(QUERY));
            let flags = EvtQueryChannelPath | EvtQueryReverseDirection;
            let rs = handle(unsafe { EvtQuery(0, path.as_ptr(), query.as_ptr(), flags) })?;
            let ctx = handle(unsafe { EvtCreateRenderContext(0, std::ptr::null(), EvtRenderContextSystem) })?;
            let mut events: Vec<H> = Vec::with_capacity(max);
            while events.len() < max {
                let mut batch = vec![0isize; max - events.len()];
                let mut got = 0u32;
                if unsafe { EvtNext(rs.0, batch.len() as u32, batch.as_mut_ptr(), u32::MAX, 0, &mut got) } == 0 {
                    let e = unsafe { GetLastError() };
                    if e == ERROR_NO_MORE_ITEMS {
                        break;
                    }
                    return Err(e);
                }
                if got == 0 {
                    break;
                }
                events.extend(batch[..(got as usize).min(batch.len())].iter().map(|&h| H(h)));
            }
            if events.is_empty() {
                // Nothing to format, so a provider that isn't installed doesn't matter.
                return Ok(vec![]);
            }
            // A provider whose message table can't be loaded (missing/unregistered message DLL or MUI)
            // still has readable events: format them with empty messages, as Get-WinEvent does, rather
            // than failing the whole read and paying for the PowerShell fallback. Events 258/264 still
            // decide `ok`.
            let meta = meta.join().ok().and_then(Result::ok).unwrap_or(H(0));
            let mut values: Vec<u64> = vec![0; 256];
            let mut text: Vec<u16> = vec![0; 512];
            events
                .iter()
                .map(|ev| {
                    let (id, level, created) = render_system(ctx.0, ev.0, &mut values)?;
                    Ok(RawEvent {
                        id,
                        level,
                        time: created.and_then(super::iso::from_utc_filetime),
                        message: Some(format_message(meta.0, ev.0, &mut text)),
                    })
                })
                .collect()
        })
    }

    /// (EventID, Level, TimeCreated as FILETIME ticks)
    fn render_system(
        ctx: EVT_HANDLE,
        ev: EVT_HANDLE,
        buf: &mut Vec<u64>,
    ) -> Result<(Option<u32>, Option<u32>, Option<u64>), u32> {
        let (mut used, mut count) = (0u32, 0u32);
        loop {
            let size = (buf.len() * 8) as u32;
            let ok = unsafe {
                EvtRender(ctx, ev, EvtRenderEventValues, size, buf.as_mut_ptr().cast(), &mut used, &mut count)
            };
            if ok != 0 {
                break;
            }
            let e = unsafe { GetLastError() };
            if e != ERROR_INSUFFICIENT_BUFFER || used as usize <= size as usize {
                return Err(e);
            }
            buf.resize((used as usize).div_ceil(8), 0);
        }
        // Each EVT_VARIANT is 16 bytes: 8 value bytes, then Count (u32) and Type (u32).
        let value = |i: usize| -> Option<(u64, u32)> {
            if i >= count as usize {
                return None;
            }
            let (v, meta) = (buf[i * 2], buf[i * 2 + 1]);
            let ty = (meta >> 32) as u32 & 0x7f;
            (ty != 0).then_some((v, ty)) // EvtVarTypeNull
        };
        // EvtVarTypeUInt16 = 6, EvtVarTypeByte = 4, EvtVarTypeFileTime = 17
        let id = value(SYS_EVENT_ID).map(|(v, ty)| if ty == 6 { v as u16 as u32 } else { v as u32 });
        let level = value(SYS_LEVEL).map(|(v, ty)| if ty == 4 { v as u8 as u32 } else { v as u32 });
        let created = value(SYS_TIME_CREATED).filter(|&(_, ty)| ty == 17).map(|(v, _)| v);
        Ok((id, level, created))
    }

    /// The event's message in the user's language; empty if the provider can't format it
    /// (Get-WinEvent's `Message` is empty then too).
    fn format_message(meta: EVT_HANDLE, ev: EVT_HANDLE, buf: &mut Vec<u16>) -> String {
        if meta == 0 {
            return String::new();
        }
        for _ in 0..2 {
            let mut used = 0u32;
            let ok = unsafe {
                EvtFormatMessage(
                    meta,
                    ev,
                    0,
                    0,
                    std::ptr::null(),
                    EvtFormatMessageEvent,
                    buf.len() as u32,
                    buf.as_mut_ptr(),
                    &mut used,
                )
            };
            let e = if ok != 0 { 0 } else { unsafe { GetLastError() } };
            match e {
                0 | ERROR_EVT_UNRESOLVED_VALUE_INSERT | ERROR_EVT_UNRESOLVED_PARAMETER_INSERT => {
                    let n = (used as usize).min(buf.len());
                    return crate::util::from_wide(&buf[..n]);
                }
                ERROR_INSUFFICIENT_BUFFER if used as usize > buf.len() => buf.resize(used as usize, 0),
                _ => return String::new(),
            }
        }
        String::new()
    }
}

/// `\Microsoft\Windows\Defrag\ScheduledDefrag` through the Task Scheduler 2.0 COM API.
mod task {
    use super::OptimizeSchedule;
    use crate::disks::com::{self, Bstr, EmptyVariant, Unk, EMPTY};
    use std::ffi::c_void;
    use windows_sys::core::{GUID, HRESULT};

    const CLSID_TASK_SCHEDULER: GUID = GUID::from_u128(0x0f87369f_a4e5_4cfc_bd3e_73e6154572dd);
    const IID_ITASK_SERVICE: GUID = GUID::from_u128(0x2faba4c7_4da9_4013_9697_20cc3fd40f85);
    const TASK_STATE_DISABLED: i32 = 1;

    // Vtable slots after IUnknown (0-2) and IDispatch (3-6).
    // ITaskService: GetFolder 7, Connect 10. ITaskFolder: GetTask 13.
    // IRegisteredTask: get_State 9, get_Enabled 10, get_LastRunTime 15, get_NextRunTime 18.
    type GetByName = unsafe extern "system" fn(*mut c_void, *const u16, *mut *mut c_void) -> HRESULT;
    type Connect =
        unsafe extern "system" fn(*mut c_void, EmptyVariant, EmptyVariant, EmptyVariant, EmptyVariant) -> HRESULT;
    type GetI32 = unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT;
    type GetBool = unsafe extern "system" fn(*mut c_void, *mut i16) -> HRESULT;
    type GetDate = unsafe extern "system" fn(*mut c_void, *mut f64) -> HRESULT;

    /// SCHED_E_SERVICE_NOT_RUNNING
    const SCHED_E_SERVICE_NOT_RUNNING: HRESULT = 0x8004_1315_u32 as HRESULT;

    /// Some(reason) when the Task Scheduler service can't be reached at all, so the PowerShell
    /// fallback (same COM object) can't succeed either. None for in-process failures worth a retry.
    pub fn unreachable_reason(hr: HRESULT) -> Option<String> {
        if hr == SCHED_E_SERVICE_NOT_RUNNING {
            return Some("the Task Scheduler service isn't running".into());
        }
        com::service_unreachable(hr)
    }

    fn child(parent: &Unk, slot: usize, name: &str) -> Option<Unk> {
        let name = Bstr::new(name);
        let mut p = std::ptr::null_mut();
        let hr = unsafe { com::method::<GetByName>(parent.0, slot)(parent.0, name.0, &mut p) };
        (hr >= 0 && !p.is_null()).then(|| Unk(p))
    }

    /// Call on an MTA thread. Err only if the Task Scheduler itself can't be reached; a missing task
    /// is `enabled: false`, like the script.
    pub fn defrag_schedule() -> Result<OptimizeSchedule, HRESULT> {
        let svc = com::create(&CLSID_TASK_SCHEDULER, &IID_ITASK_SERVICE)?;
        let hr = unsafe { com::method::<Connect>(svc.0, 10)(svc.0, EMPTY, EMPTY, EMPTY, EMPTY) };
        if hr < 0 {
            return Err(hr);
        }
        let Some(task) = child(&svc, 7, r"\Microsoft\Windows\Defrag").and_then(|f| child(&f, 13, "ScheduledDefrag"))
        else {
            return Ok(OptimizeSchedule::default());
        };
        let t = task.0;
        let mut enabled = 0i16;
        let enabled_ok = unsafe { com::method::<GetBool>(t, 10)(t, &mut enabled) } >= 0;
        let mut state = 0i32;
        let state_ok = unsafe { com::method::<GetI32>(t, 9)(t, &mut state) } >= 0;
        let date = |slot: usize| {
            let mut d = 0f64;
            let hr = unsafe { com::method::<GetDate>(t, slot)(t, &mut d) };
            (hr >= 0).then_some(d).and_then(super::iso::from_local_ole_date)
        };
        Ok(OptimizeSchedule {
            enabled: enabled_ok && enabled != 0 && !(state_ok && state == TASK_STATE_DISABLED),
            last_run: date(15),
            next_run: date(18),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optimize_history_real() {
        let t = std::time::Instant::now();
        let h = history().expect("history");
        println!("optimize_history: {} events in {} ms", h.len(), t.elapsed().as_millis());
        for e in h.iter().take(6) {
            println!("{}", serde_json::to_string(e).unwrap());
        }
        assert!(h.len() <= 40);
        assert!(h.windows(2).all(|w| w[0].time >= w[1].time));
    }

    /// The Event Log API must give exactly what Get-WinEvent gives.
    #[test]
    fn optimize_history_native_matches_powershell() {
        let t = std::time::Instant::now();
        let native = events_from_raw(evt::defrag_events(HISTORY_READ).expect("native history"));
        let native_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = std::time::Instant::now();
        let ps = history_powershell().expect("powershell history");
        let ps_ms = t.elapsed().as_secs_f64() * 1000.0;
        println!("optimize_history native {native_ms:.1} ms ({} events), powershell {ps_ms:.1} ms", native.len());
        let j = |v: &Vec<OptimizeEvent>| v.iter().map(|e| serde_json::to_string(e).unwrap()).collect::<Vec<_>>();
        assert_eq!(j(&native), j(&ps));
    }

    #[test]
    fn optimize_schedule_native_matches_powershell() {
        let t = std::time::Instant::now();
        let native = crate::disks::com::on_mta_thread(task::defrag_schedule).unwrap().expect("native schedule");
        let native_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = std::time::Instant::now();
        let ps = schedule_powershell().expect("powershell schedule");
        let ps_ms = t.elapsed().as_secs_f64() * 1000.0;
        println!("optimize_schedule native {native_ms:.1} ms, powershell {ps_ms:.1} ms");
        assert_eq!(serde_json::to_string(&native).unwrap(), serde_json::to_string(&ps).unwrap());
    }

    /// A stopped/disabled Task Scheduler is reported directly; apartment or thread problems fall back.
    #[test]
    fn optimize_schedule_unreachable_classification() {
        let h = |u: u32| u as i32;
        let why = task::unreachable_reason(h(0x8004_1315)).expect("SCHED_E_SERVICE_NOT_RUNNING");
        assert!(why.contains("Task Scheduler"));
        assert!(task::unreachable_reason(h(0x8007_0422)).is_some()); // ERROR_SERVICE_DISABLED
        assert!(task::unreachable_reason(h(0x8007_0426)).is_some()); // ERROR_SERVICE_NOT_ACTIVE
        for u in [0xFFFF_FFFF, 0x8004_01F0, 0x8001_0106, 0x8004_0154, 0x8007_0005, 0x8004_100E] {
            assert!(task::unreachable_reason(h(u)).is_none(), "{u:#x}");
        }
    }

    #[test]
    fn optimize_iso_times() {
        // 2026-11-23T07:33:41.2919096Z as FILETIME ticks.
        let ticks: u64 = 134_083_448_212_919_096;
        let s = iso::from_utc_filetime(ticks).unwrap();
        assert_eq!(s.len(), "2026-11-23T13:03:41.2919096+05:30".len());
        assert!(s.contains(".2919096"), "{s}");
        assert!(iso::from_local_ole_date(0.0).is_none());
        // 46349.5 = 2026-11-23 12:00 local
        assert!(iso::from_local_ole_date(46_349.5).unwrap().starts_with("2026-11-23T12:00:00.0000000"));
    }

    #[test]
    fn optimize_schedule_real() {
        let t = std::time::Instant::now();
        let s = schedule().expect("schedule");
        println!("optimize_schedule: {} in {} ms", serde_json::to_string(&s).unwrap(), t.elapsed().as_millis());
    }

    #[test]
    fn optimize_preflight_not_admin() {
        if util::is_elevated() {
            return;
        }
        assert_eq!(preflight("C", "trim").unwrap_err(), ADMIN);
        assert_eq!(preflight("C:", "analyze").unwrap_err(), ADMIN);
        assert!(preflight("CC", "trim").is_err());
        assert!(preflight("C", "nuke").unwrap_err().contains("doesn't know"));
    }

    #[test]
    fn optimize_parse_output() {
        let mut p = OutputParser::new(Mode::Analyze);
        let out = b"Microsoft Drive Optimizer\r\nCopyright (c) Microsoft Corp.\r\n\r\nInvoking analysis on Local Disk (C:)...\r\n\r\n\r\n\tAnalysis:  10% complete...\r\tAnalysis:  10% complete...\r\tAnalysis:  57% complete...\r\tAnalysis: 100% complete.\r\n\r\nThe operation completed successfully.\r\n\r\nPost Defragmentation Report:\r\n\r\n\tVolume Information:\r\n\t\tVolume size                 = 279.69 GB\r\n\t\tFree space                  = 81.06 GB\r\n\t\tTotal fragmented space      = 3%\r\n\t\tLargest free space size     = 79.47 GB\r\n";
        // feed in awkward chunks to exercise buffering
        let mut evs = Vec::new();
        for c in out.chunks(7) {
            evs.extend(p.feed(c));
        }
        evs.extend(p.finish());
        let pcts: Vec<f32> = evs.iter().filter_map(|e| e.percent).collect();
        assert_eq!(pcts, vec![10.0, 57.0, 100.0]); // duplicate 10% throttled
        assert!(evs.iter().all(|e| e.stage == "Analysis"));
        let done = p.summarize('C', true, Stop::None);
        assert!(done.ok);
        assert_eq!(done.fragmented_percent, Some(3.0));
        assert_eq!(done.message, "Analysis finished: 3% of the drive is fragmented.");
        assert_eq!(done.report.matches("Analysis:").count(), 1, "report: {}", done.report);

        let mut p = OutputParser::new(Mode::Trim);
        p.feed(b"\r\nThe operation requested is not supported by the hardware backing the volume. (0x89000020)\r\n");
        let done = p.summarize('E', true, Stop::None);
        assert!(!done.ok);
        assert_eq!(done.message, "This drive doesn't support trim.");

        let mut p = OutputParser::new(Mode::Defrag);
        p.feed(b"\r\nThe storage optimizer cannot start because you have insufficient privileges to perform this operation. (0x89000024)\r\n");
        assert_eq!(p.summarize('C', true, Stop::None).message, ADMIN);

        let mut p = OutputParser::new(Mode::Defrag);
        p.feed(b"\r\nSomething odd happened. (0x8900ABCD)\r\n");
        assert_eq!(p.summarize('C', true, Stop::None).message, "Windows couldn't finish the defragmentation of drive C. Something odd happened.");

        // Cancel confirmed by defrag.exe.
        let mut p = OutputParser::new(Mode::Defrag);
        p.feed(b"\r\nThe operation was cancelled. (0x89000006)\r\n");
        let done = p.summarize('C', true, Stop::Asked);
        assert!(!done.ok);
        assert_eq!(done.message, "Stopped before finishing.");
        // defrag.exe was terminated: say the service may carry on.
        let p = OutputParser::new(Mode::Defrag);
        let done = p.summarize('C', false, Stop::Killed);
        assert!(!done.ok);
        assert!(done.message.contains("may finish the current pass"), "{}", done.message);
        let p = OutputParser::new(Mode::Defrag);
        assert!(p.summarize('C', false, Stop::Asked).message.contains("may finish the current pass"));
        // Stop pressed, but the run finished cleanly anyway: report it as finished.
        let mut p = OutputParser::new(Mode::Trim);
        p.feed(b"\r\nRetrim: 100% complete.\r\n\r\nThe operation completed successfully.\r\n");
        let done = p.summarize('C', true, Stop::Asked);
        assert!(done.ok, "{}", done.message);
        assert!(done.message.starts_with("Trim finished"));

        assert_eq!(parse_progress("Retrim:  42% complete..."), Some((Some("Retrim".into()), 42.0)));
        assert_eq!(parse_progress("Total fragmented space = 3%"), None);
        assert_eq!(parse_progress("42 % complete"), Some((None, 42.0)));
        assert_eq!(parse_fragmented("Fragmented space = 12 %"), Some(12.0));
        assert_eq!(letter_in("The storage optimizer successfully completed retrim on New Volume (D:)"), Some("D".into()));
    }

    /// A harmless console program that runs for a while and stops on Ctrl+C (never defrag).
    fn spawn_ping() -> std::process::Child {
        Command::new("ping.exe")
            .args(["-n", "60", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(util::CREATE_NO_WINDOW)
            .spawn()
            .expect("ping")
    }

    #[test]
    fn optimize_stop_process_terminates_via_handle() {
        // The test runner has a console, so Ctrl+C can't be sent and the handle-based fallback runs.
        let mut child = spawn_ping();
        let h = child.as_handle().try_clone_to_owned().unwrap();
        let stop = StopState::default();
        stop.requested.store(true, Ordering::SeqCst);
        let t = std::time::Instant::now();
        stop_process(child.id(), &h, 2_000, &stop);
        let st = child.wait().unwrap();
        println!("stopped in {} ms: {st:?}, outcome {:?}", t.elapsed().as_millis(), stop.outcome());
        assert!(exited(&h, 0));
        assert!(t.elapsed().as_secs() < 30);
        // Either Ctrl+C worked (no console of our own) or it was terminated.
        assert!(matches!(stop.outcome(), Stop::Asked | Stop::Killed));
        // Already exited: nothing more happens, and it isn't marked as killed.
        let again = StopState::default();
        stop_process(child.id(), &h, 0, &again);
        assert_eq!(again.outcome(), Stop::None);
    }

    /// Checks the Ctrl+C path the GUI build uses. It detaches this test process from its console,
    /// so run it alone: `cargo test optimize_ctrl_c -- --ignored --test-threads=1`.
    #[test]
    #[ignore]
    fn optimize_ctrl_c_stops_console_child() {
        allow_ctrl_c_in_children();
        let mut child = spawn_ping();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let h = child.as_handle().try_clone_to_owned().unwrap();
        unsafe { console::FreeConsole() };
        let sent = send_ctrl_c(child.id());
        let gone = exited(&h, 5_000);
        unsafe { console::AttachConsole(u32::MAX) }; // ATTACH_PARENT_PROCESS
        if !gone {
            let _ = child.kill();
        }
        let st = child.wait().unwrap();
        assert!(sent, "Ctrl+C wasn't sent");
        assert!(gone, "ping didn't stop on Ctrl+C: {st:?}");
    }
}
