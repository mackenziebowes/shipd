//! What is kept on disk, per site: the state (state.json, rewritten after every event) and the history (history.jsonl,
//! appended to, one line per event, effect and dry-run record). The adapter stamps the time; `step` never sees a clock.
use crate::types::ShipState;
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct Paths {
    pub home: PathBuf,
}

impl Paths {
    /// SHIPD_HOME, or ~/.local/share/shipd.
    pub fn from_env() -> Paths {
        let home = match std::env::var("SHIPD_HOME") {
            Ok(h) if !h.is_empty() => PathBuf::from(h),
            _ => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share/shipd"),
        };
        Paths { home }
    }
    pub fn site_dir(&self, site: &str) -> PathBuf {
        self.home.join(site)
    }
    pub fn work(&self, site: &str) -> PathBuf {
        self.site_dir(site).join("work")
    }
    pub fn target(&self, site: &str) -> PathBuf {
        self.site_dir(site).join("target")
    }
    fn state_file(&self, site: &str) -> PathBuf {
        self.site_dir(site).join("state.json")
    }
    fn history_file(&self, site: &str) -> PathBuf {
        self.site_dir(site).join("history.jsonl")
    }
}

pub fn load_state(paths: &Paths, site: &str) -> Result<Option<ShipState>, String> {
    let file = paths.state_file(site);
    if !file.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    serde_json::from_str(&text).map(Some).map_err(|e| format!("{} is not a shipd state: {e}", file.display()))
}

pub fn save_state(paths: &Paths, site: &str, state: &ShipState) -> Result<(), String> {
    std::fs::create_dir_all(paths.site_dir(site)).map_err(|e| format!("cannot make {}: {e}", paths.site_dir(site).display()))?;
    let file = paths.state_file(site);
    let text = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    std::fs::write(&file, text + "\n").map_err(|e| format!("cannot write {}: {e}", file.display()))
}

/// Appends one record. `kind` says what it is; the rest of the fields are that record's words and commits.
pub fn append(paths: &Paths, site: &str, kind: &str, fields: Value) -> Result<(), String> {
    std::fs::create_dir_all(paths.site_dir(site)).map_err(|e| e.to_string())?;
    let mut record = json!({ "at": now(), "kind": kind });
    if let (Value::Object(base), Value::Object(extra)) = (&mut record, fields) {
        base.extend(extra);
    }
    let file = paths.history_file(site);
    let mut out = std::fs::OpenOptions::new().create(true).append(true).open(&file).map_err(|e| format!("cannot open {}: {e}", file.display()))?;
    writeln!(out, "{record}").map_err(|e| format!("cannot write {}: {e}", file.display()))
}

pub fn read_history(paths: &Paths, site: &str) -> Vec<Value> {
    let text = std::fs::read_to_string(paths.history_file(site)).unwrap_or_default();
    text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// Unix seconds, from the system clock. Only the adapter calls this.
pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
