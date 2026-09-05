//! Session history management for Ubuntu Miracast Server.
//!
//! Faithful port of `src/miracast_server/history.py`. Stores records in a JSON
//! array at `~/.local/share/ubuntu-miracast-server/history.json` with 0600
//! permissions, caps at 500 records (discarding oldest), and returns sessions
//! newest-first. On write failure, records are retained in memory.

use crate::config::write_json_0600;
use crate::models::{ReceiverStats, ServerSessionRecord, SourceInfo};
use chrono::Local;
use serde_json::Value;
use std::path::{Path, PathBuf};

const MAX_RECORDS: usize = 500;

/// Manages persistence of server session records.
pub struct ServerSessionHistory {
    pub history_path: PathBuf,
    pub sessions: Vec<ServerSessionRecord>,
}

impl ServerSessionHistory {
    /// Create the manager, loading existing history (or starting empty).
    pub fn new(history_path: Option<&Path>) -> Self {
        let path = match history_path {
            Some(p) => p.to_path_buf(),
            None => default_history_path(),
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut h = Self {
            history_path: path,
            sessions: Vec::new(),
        };
        h.sessions = h.load_history();
        h
    }

    fn load_history(&self) -> Vec<ServerSessionRecord> {
        if !self.history_path.exists() {
            return Vec::new();
        }
        let text = match std::fs::read_to_string(&self.history_path) {
            Ok(t) => t,
            Err(e) => {
                log::warn!(
                    "Failed to load history from {}: {} — starting with empty history",
                    self.history_path.display(),
                    e
                );
                return Vec::new();
            }
        };
        let data: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                log::warn!(
                    "Failed to load history from {}: {} — starting with empty history",
                    self.history_path.display(),
                    e
                );
                return Vec::new();
            }
        };
        let arr = match data.as_array() {
            Some(a) => a,
            None => {
                log::warn!(
                    "History file {} does not contain a JSON array — starting with empty history",
                    self.history_path.display()
                );
                return Vec::new();
            }
        };

        let mut sessions = Vec::new();
        for entry in arr {
            match ServerSessionRecord::from_dict(entry) {
                Ok(rec) => sessions.push(rec),
                Err(e) => log::error!("Failed to deserialize session record: {e}"),
            }
        }
        log::info!("Loaded {} session records from history", sessions.len());
        sessions
    }

    fn save_history(&self) {
        if let Err(e) = self.write_history() {
            log::error!(
                "persist-error: Failed to save history to {}: {}",
                self.history_path.display(),
                e
            );
        }
    }

    fn write_history(&self) -> std::io::Result<()> {
        let data: Vec<Value> = self.sessions.iter().map(|r| r.to_dict()).collect();
        write_json_0600(&self.history_path, &Value::Array(data))
    }

    /// Add a new session record (timestamped now), enforce the 500-record cap
    /// (discarding oldest), persist, and return the created record.
    pub fn add_session(
        &mut self,
        source_info: SourceInfo,
        stats: ReceiverStats,
    ) -> ServerSessionRecord {
        let record = ServerSessionRecord {
            source_info: source_info.clone(),
            stats,
            timestamp: Local::now(),
        };
        self.sessions.push(record.clone());

        if self.sessions.len() > MAX_RECORDS {
            self.sessions.sort_by_key(|r| r.timestamp);
            let start = self.sessions.len() - MAX_RECORDS;
            self.sessions.drain(0..start);
        }

        self.save_history();
        log::info!(
            "Added session record: {} ({})",
            source_info.name,
            source_info.address
        );
        record
    }

    /// Get all records sorted by timestamp descending (most recent first).
    pub fn get_sessions(&self) -> Vec<ServerSessionRecord> {
        let mut out = self.sessions.clone();
        out.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        out
    }

    /// Clear all records. On write failure, restores prior state (no data loss).
    pub fn clear(&mut self) {
        let previous = std::mem::take(&mut self.sessions);
        if let Err(e) = self.write_history() {
            log::error!(
                "persist-error: Failed to clear history file {}: {}",
                self.history_path.display(),
                e
            );
            self.sessions = previous;
            return;
        }
        log::info!("Session history cleared");
    }
}

fn default_history_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".local")
        .join("share")
        .join("ubuntu-miracast-server")
        .join("history.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str) -> (SourceInfo, ReceiverStats) {
        (
            SourceInfo {
                name: name.to_string(),
                address: "00:11:22:33:44:55".to_string(),
                model: "Test".to_string(),
                ..Default::default()
            },
            ReceiverStats {
                duration: 10,
                data_received: 100,
                frames_decoded: 50,
                frames_dropped: 1,
                ..Default::default()
            },
        )
    }

    #[test]
    fn add_and_read_back_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let mut h = ServerSessionHistory::new(Some(&path));
        let (si1, st1) = sample("first");
        h.add_session(si1, st1);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let (si2, st2) = sample("second");
        h.add_session(si2, st2);

        let sessions = h.get_sessions();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].source_info.name, "second");
    }

    #[test]
    fn persists_across_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        {
            let mut h = ServerSessionHistory::new(Some(&path));
            let (si, st) = sample("persisted");
            h.add_session(si, st);
        }
        let h2 = ServerSessionHistory::new(Some(&path));
        assert_eq!(h2.sessions.len(), 1);
        assert_eq!(h2.sessions[0].source_info.name, "persisted");
    }

    #[test]
    fn cap_discards_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let mut h = ServerSessionHistory::new(Some(&path));
        for i in 0..(MAX_RECORDS + 10) {
            let (si, st) = sample(&format!("s{i}"));
            h.add_session(si, st);
        }
        assert_eq!(h.sessions.len(), MAX_RECORDS);
    }

    #[test]
    fn clear_empties() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let mut h = ServerSessionHistory::new(Some(&path));
        let (si, st) = sample("x");
        h.add_session(si, st);
        h.clear();
        assert!(h.sessions.is_empty());
        let h2 = ServerSessionHistory::new(Some(&path));
        assert!(h2.sessions.is_empty());
    }

    #[test]
    fn malformed_file_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        std::fs::write(&path, "{ not json ]").unwrap();
        let h = ServerSessionHistory::new(Some(&path));
        assert!(h.sessions.is_empty());
    }
}
