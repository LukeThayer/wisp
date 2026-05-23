//! Structured trace emission for cross-process testing.
//!
//! When the env var `WISP_TRACE_FILE` is set, this module writes one JSON
//! object per line to that file path. Each event has the shape:
//!
//! ```json
//! {"t": 0.123, "src": "server", "kind": "client_connected", ...extra}
//! ```
//!
//! Where `t` is seconds since process start, `src` identifies the producer
//! (e.g. `"server"`, `"observer-1"`, `"client"`), and `kind` plus the
//! event-specific extra fields describe what happened. The `wisp-net-test`
//! Skill spawns a server + N observers with distinct trace files and reads
//! them back to validate sync, replication, and input behavior.
//!
//! All emissions are wrapped in `emit!()` / `event()` so they cost nothing
//! when the env var is unset (one map lookup + early return).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use bevy::prelude::*;
use serde_json::{json, Value};

static SINK: OnceLock<Option<Mutex<File>>> = OnceLock::new();

fn sink() -> &'static Option<Mutex<File>> {
    SINK.get_or_init(|| {
        let path = std::env::var("WISP_TRACE_FILE").ok()?;
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(f) => Some(Mutex::new(f)),
            Err(e) => {
                eprintln!("wisp::trace: failed to open {:?}: {e}", path);
                None
            }
        }
    })
}

/// Wall-clock seconds since the unix epoch. Using wall-clock instead of a
/// per-process monotonic clock so events from different processes share a
/// timeline — without that, "did observer-2 receive observer-1's lantern?"
/// can't be answered by comparing trace times.
fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Identifier for the current process in the trace stream. Set via env var
/// `WISP_TRACE_SRC` (default `"unknown"`). The orchestrator sets a unique
/// value per process (`"server"`, `"observer-1"`, …).
fn src() -> String {
    std::env::var("WISP_TRACE_SRC").unwrap_or_else(|_| "unknown".to_string())
}

/// Emit one trace event. `extra` is merged into the top-level JSON object —
/// pass `json!({"id": 42, "pos": [1,2,3]})` etc.
pub fn event(kind: &str, extra: Value) {
    let Some(file) = sink() else { return };
    let mut obj = json!({
        "t": now_secs(),
        "src": src(),
        "kind": kind,
    });
    if let Value::Object(extra_map) = extra {
        if let Value::Object(ref mut obj_map) = obj {
            for (k, v) in extra_map {
                obj_map.insert(k, v);
            }
        }
    }
    let line = format!("{}\n", obj);
    if let Ok(mut guard) = file.lock() {
        let _ = guard.write_all(line.as_bytes());
        let _ = guard.flush();
    }
}

/// Plugin that emits a `"start"` event at app startup — useful as a sentinel
/// so the harness knows the binary actually booted.
pub struct TracePlugin;

impl Plugin for TracePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, emit_start);
    }
}

fn emit_start() {
    event("start", json!({}));
}
