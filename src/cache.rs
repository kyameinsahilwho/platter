//! Small persistent cache so the UI can paint last-known results instantly at launch
//! (stale-while-revalidate) instead of waiting for PowerShell, the event log or a clean-up measure.
//!
//! Files live in `%LOCALAPPDATA%\app.platter.disks\cache\<key>.json` as `{ "savedAt": <ms since epoch>, "data": … }`.
//! Writes go to a temp file and are renamed into place, so a crash never leaves a half-written entry.

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const VERSION: u32 = 1;

fn dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    let d = PathBuf::from(base).join("app.platter.disks").join("cache");
    std::fs::create_dir_all(&d).ok()?;
    Some(d)
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 48 && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[derive(Serialize, serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub saved_at: u64,
    #[serde(default)]
    pub version: u32,
    pub data: Value,
}

/// Store `value` under `key`. Failures are ignored: the cache is an optimisation only.
pub fn write<T: Serialize>(key: &str, value: &T) {
    if !valid_key(key) {
        return;
    }
    let Some(d) = dir() else { return };
    let Ok(data) = serde_json::to_value(value) else { return };
    let entry = Entry { saved_at: now_ms(), version: VERSION, data };
    let Ok(bytes) = serde_json::to_vec(&entry) else { return };
    // Unique temp name: the standard and the elevated instance (or two threads) may write at once.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = d.join(format!("{key}.{}.{n}.tmp", std::process::id()));
    let dst = d.join(format!("{key}.json"));
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, &dst).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Forget `key`, e.g. clean-up sizes once files have been deleted, so they're never shown as current.
pub fn remove(key: &str) {
    if valid_key(key) {
        if let Some(d) = dir() {
            let _ = std::fs::remove_file(d.join(format!("{key}.json")));
        }
    }
}

pub fn read(key: &str) -> Option<Entry> {
    if !valid_key(key) {
        return None;
    }
    let bytes = std::fs::read(dir()?.join(format!("{key}.json"))).ok()?;
    let e: Entry = serde_json::from_slice(&bytes).ok()?;
    (e.version == VERSION).then_some(e)
}

/// Last-known values for several keys in one IPC round trip. Missing or unreadable keys are omitted.
#[tauri::command]
pub fn cache_get_many(keys: Vec<String>) -> HashMap<String, Entry> {
    keys.into_iter().filter_map(|k| read(&k).map(|e| (k, e))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_key_validation() {
        write("test-roundtrip", &serde_json::json!({ "a": 1 }));
        let e = read("test-roundtrip").expect("entry");
        assert_eq!(e.data["a"], 1);
        assert!(e.saved_at > 0);
        assert!(read("../evil").is_none());
        assert!(!valid_key("a/b") && !valid_key("") && valid_key("disks"));
        if let Some(d) = dir() {
            let _ = std::fs::remove_file(d.join("test-roundtrip.json"));
        }
    }
}
