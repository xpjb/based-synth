use crate::params::Patch;
use serde::Serialize;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Coalesce window for param events. Multiple writes to the same param within
/// this window collapse to the latest value before being written.
const COALESCE_MS: u64 = 50;

/// Format version embedded in session_start. Bump on breaking schema changes.
const FORMAT_VERSION: u32 = 1;

/// Public handle held by AppState. Cheap to clone, send-able.
#[derive(Clone)]
pub struct History {
    tx: Sender<LogMsg>,
}

/// Messages sent from the IPC layer to the logger thread.
enum LogMsg {
    NoteOn { note: u8, vel: f32 },
    NoteOff { note: u8 },
    Param { name: String, value: f32 },
    PatchLoad { name: String, patch: Patch },
    PatchSave { name: String, patch: Patch },
}

/// Wire-format event. Stays minimal and self-describing so a piano-roll
/// consumer or live-stream client can parse line-by-line.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LogEvent<'a> {
    SessionStart { v: u32, wall_unix: u64, patch: &'a Patch },
    PatchLoad { name: &'a str, patch: &'a Patch },
    PatchSave { name: &'a str, patch: &'a Patch },
    PatchDirty { base: Option<&'a str> },
    Param { name: &'a str, value: f32 },
    NoteOn { id: u32, ch: u8, note: u8, vel: f32 },
    NoteOff { id: u32, ch: u8, note: u8 },
}

/// Wrapper that prefixes every line with a session-relative `t` (ms).
#[derive(Serialize)]
struct LogLine<'a> {
    t: u64,
    #[serde(flatten)]
    event: LogEvent<'a>,
}

impl History {
    /// Spawn the logger thread, write the session header, return a handle.
    /// If the file can't be opened, falls back to a no-op handle that drops events.
    pub fn spawn(history_dir: PathBuf, initial_patch: Patch) -> Self {
        let _ = std::fs::create_dir_all(&history_dir);
        let wall_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = history_dir.join(format!("session_{}.jsonl", wall_unix));
        eprintln!("[chonk] history: {}", path.display());

        let file = match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[chonk] history: failed to open log: {e}");
                let (tx, _rx) = mpsc::channel();
                return Self { tx };
            }
        };

        let (tx, rx) = mpsc::channel::<LogMsg>();
        thread::spawn(move || logger_loop(rx, file, wall_unix, initial_patch));
        Self { tx }
    }

    pub fn note_on(&self, note: u8, vel: f32) {
        let _ = self.tx.send(LogMsg::NoteOn { note, vel });
    }
    pub fn note_off(&self, note: u8) {
        let _ = self.tx.send(LogMsg::NoteOff { note });
    }
    pub fn param(&self, name: &str, value: f32) {
        let _ = self.tx.send(LogMsg::Param { name: name.to_string(), value });
    }
    pub fn patch_load(&self, name: &str, patch: Patch) {
        let _ = self.tx.send(LogMsg::PatchLoad { name: name.to_string(), patch });
    }
    pub fn patch_save(&self, name: &str, patch: Patch) {
        let _ = self.tx.send(LogMsg::PatchSave { name: name.to_string(), patch });
    }
}

fn logger_loop(rx: mpsc::Receiver<LogMsg>, file: File, wall_unix: u64, initial_patch: Patch) {
    let start = Instant::now();
    let mut writer = BufWriter::new(file);

    // Header.
    write_event(
        &mut writer,
        0,
        LogEvent::SessionStart {
            v: FORMAT_VERSION,
            wall_unix,
            patch: &initial_patch,
        },
    );

    // State tracked on the logger thread.
    let mut last_loaded: Option<String> = None;
    let mut dirty: bool = false;
    let mut next_id: u32 = 1;
    let mut active_notes: HashMap<u8, u32> = HashMap::new();
    let mut pending_params: HashMap<String, (f32, Instant)> = HashMap::new();

    loop {
        let timeout = if pending_params.is_empty() {
            Duration::from_secs(60)
        } else {
            Duration::from_millis(20)
        };
        match rx.recv_timeout(timeout) {
            Ok(LogMsg::Param { name, value }) => {
                // First mutation after a clean load: emit a dirty marker.
                if !dirty {
                    if let Some(base) = last_loaded.as_deref() {
                        write_event(
                            &mut writer,
                            t_ms(start),
                            LogEvent::PatchDirty { base: Some(base) },
                        );
                    }
                    dirty = true;
                }
                pending_params.insert(name, (value, Instant::now()));
            }
            Ok(LogMsg::NoteOn { note, vel }) => {
                flush_all_pending(&mut writer, &mut pending_params, start);
                let id = next_id;
                next_id = next_id.wrapping_add(1);
                active_notes.insert(note, id);
                write_event(
                    &mut writer,
                    t_ms(start),
                    LogEvent::NoteOn { id, ch: 0, note, vel },
                );
            }
            Ok(LogMsg::NoteOff { note }) => {
                flush_all_pending(&mut writer, &mut pending_params, start);
                let id = active_notes.remove(&note).unwrap_or_else(|| {
                    let i = next_id;
                    next_id = next_id.wrapping_add(1);
                    i
                });
                write_event(
                    &mut writer,
                    t_ms(start),
                    LogEvent::NoteOff { id, ch: 0, note },
                );
            }
            Ok(LogMsg::PatchLoad { name, patch }) => {
                flush_all_pending(&mut writer, &mut pending_params, start);
                write_event(
                    &mut writer,
                    t_ms(start),
                    LogEvent::PatchLoad { name: &name, patch: &patch },
                );
                last_loaded = Some(name);
                dirty = false;
                active_notes.clear(); // matches all_off() in ipc.rs
            }
            Ok(LogMsg::PatchSave { name, patch }) => {
                flush_all_pending(&mut writer, &mut pending_params, start);
                write_event(
                    &mut writer,
                    t_ms(start),
                    LogEvent::PatchSave { name: &name, patch: &patch },
                );
                last_loaded = Some(name);
                dirty = false;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                flush_all_pending(&mut writer, &mut pending_params, start);
                let _ = writer.flush();
                break;
            }
        }
        flush_stale_pending(&mut writer, &mut pending_params, start);
    }
}

#[inline]
fn t_ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

fn write_event(writer: &mut BufWriter<File>, t: u64, event: LogEvent<'_>) {
    let line = LogLine { t, event };
    if let Ok(json) = serde_json::to_string(&line) {
        if writeln!(writer, "{}", json).is_ok() {
            // Flush every event so live tailers/streamers see records immediately.
            let _ = writer.flush();
        }
    }
}

/// Emit every pending param immediately (used before logical-ordering events
/// like notes / patch changes so the file's event order matches reality).
fn flush_all_pending(
    writer: &mut BufWriter<File>,
    pending: &mut HashMap<String, (f32, Instant)>,
    start: Instant,
) {
    if pending.is_empty() {
        return;
    }
    // Sort for deterministic order — replay tools appreciate it.
    let mut keys: Vec<String> = pending.keys().cloned().collect();
    keys.sort();
    let now_t = t_ms(start);
    for k in keys {
        if let Some((value, _)) = pending.remove(&k) {
            write_event(writer, now_t, LogEvent::Param { name: &k, value });
        }
    }
}

/// Emit params whose last update is older than COALESCE_MS.
fn flush_stale_pending(
    writer: &mut BufWriter<File>,
    pending: &mut HashMap<String, (f32, Instant)>,
    start: Instant,
) {
    if pending.is_empty() {
        return;
    }
    let now = Instant::now();
    let cutoff = Duration::from_millis(COALESCE_MS);
    let now_t = t_ms(start);
    let mut to_flush: Vec<String> = Vec::new();
    for (k, (_, last)) in pending.iter() {
        if now.duration_since(*last) >= cutoff {
            to_flush.push(k.clone());
        }
    }
    to_flush.sort();
    for k in to_flush {
        if let Some((value, _)) = pending.remove(&k) {
            write_event(writer, now_t, LogEvent::Param { name: &k, value });
        }
    }
}
