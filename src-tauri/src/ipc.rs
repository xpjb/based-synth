use crate::history::History;
use crate::params::{Params, Patch};
use crate::performance::Performer;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct AppState {
    pub params: Arc<Params>,
    pub performer: Arc<Performer>,
    pub patches_dir: PathBuf,
    pub history: History,
    /// Authoritative patch lineage: name of the most-recently loaded/saved
    /// patch, plus whether any Param has fired since that load (dirty).
    /// Drives both the dropdown's startup selection and when the history
    /// logger should emit a `patch_dirty` marker.
    pub patch: Arc<Mutex<PatchState>>,
}

#[derive(Default, Debug)]
pub struct PatchState {
    pub name: Option<String>,
    pub dirty: bool,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClientMsg {
    NoteOn { note: u8, velocity: f32 },
    NoteOff { note: u8 },
    AllOff,
    Param { name: String, value: f32 },
    LoadPatch { name: String },
    SavePatch { name: String },
    DeletePatch { name: String },
    ListPatches,
    GetState,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMsg {
    Patches { names: Vec<String>, current: Option<String> },
    State { patch: Patch },
    Ok,
    Error { message: String },
}

pub fn handle_msg(msg: ClientMsg, state: &AppState) -> ServerMsg {
    match msg {
        ClientMsg::NoteOn { note, velocity } => {
            state.performer.note_on(note, velocity);
            state.history.note_on(note, velocity);
            ServerMsg::Ok
        }
        ClientMsg::NoteOff { note } => {
            state.performer.note_off(note);
            state.history.note_off(note);
            ServerMsg::Ok
        }
        ClientMsg::AllOff => {
            state.performer.all_off();
            ServerMsg::Ok
        }
        ClientMsg::Param { name, value } => {
            if !state.params.set(&name, value) {
                return ServerMsg::Error {
                    message: format!("Unknown param: {}", name),
                };
            }
            // First param after a clean load: emit a dirty marker BEFORE the
            // param event, so log readers see the transition first.
            {
                let mut ps = state.patch.lock().unwrap();
                if !ps.dirty {
                    state.history.patch_dirty(ps.name.as_deref());
                    ps.dirty = true;
                }
            }
            state.history.param(&name, value);
            let mut refresh = false;
            if name == "arp_enabled" || name == "chord_type" {
                state.performer.all_off();
            }
            if name == "chord_type" && value > 0.5 && state.params.mono.load() > 0.5 {
                state.params.mono.store(0.0);
                refresh = true;
            }
            if name == "mono" && value > 0.5 && state.params.chord_type.load() > 0.5 {
                state.params.mono.store(0.0);
                refresh = true;
            }
            if refresh {
                ServerMsg::State {
                    patch: state.params.to_patch(),
                }
            } else {
                ServerMsg::Ok
            }
        }
        ClientMsg::LoadPatch { name } => {
            let safe = sanitize(&name);
            let path = state.patches_dir.join(format!("{}.json", safe));
            match std::fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str::<Patch>(&content) {
                    Ok(mut patch) => {
                        if patch.chord_type > 0.5 {
                            patch.mono = 0.0;
                        }
                        state.params.apply_patch(&patch);
                        state.performer.all_off();
                        state.history.patch_load(&safe, patch.clone());
                        *state.patch.lock().unwrap() = PatchState { name: Some(safe.clone()), dirty: false };
                        ServerMsg::State { patch }
                    }
                    Err(e) => ServerMsg::Error {
                        message: e.to_string(),
                    },
                },
                Err(e) => ServerMsg::Error {
                    message: e.to_string(),
                },
            }
        }
        ClientMsg::SavePatch { name } => {
            let safe = sanitize(&name);
            if safe.is_empty() {
                return ServerMsg::Error {
                    message: "Invalid patch name".into(),
                };
            }
            let _ = std::fs::create_dir_all(&state.patches_dir);
            let path = state.patches_dir.join(format!("{}.json", safe));
            let patch = state.params.to_patch();
            match serde_json::to_string_pretty(&patch) {
                Ok(json) => match std::fs::write(&path, json) {
                    Ok(_) => {
                        state.history.patch_save(&safe, patch);
                        *state.patch.lock().unwrap() = PatchState { name: Some(safe.clone()), dirty: false };
                        ServerMsg::Ok
                    }
                    Err(e) => ServerMsg::Error {
                        message: e.to_string(),
                    },
                },
                Err(e) => ServerMsg::Error {
                    message: e.to_string(),
                },
            }
        }
        ClientMsg::DeletePatch { name } => {
            let safe = sanitize(&name);
            let path = state.patches_dir.join(format!("{}.json", safe));
            match std::fs::remove_file(&path) {
                Ok(_) => ServerMsg::Ok,
                Err(e) => ServerMsg::Error {
                    message: e.to_string(),
                },
            }
        }
        ClientMsg::ListPatches => ServerMsg::Patches {
            names: list_patches(&state.patches_dir),
            current: state.patch.lock().unwrap().name.clone(),
        },
        ClientMsg::GetState => ServerMsg::State {
            patch: state.params.to_patch(),
        },
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == ' ')
        .map(|c| if c == ' ' { '_' } else { c })
        .take(64)
        .collect()
}

fn list_patches(dir: &Path) -> Vec<String> {
    let _ = std::fs::create_dir_all(dir);
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
    }
    // Case-insensitive sort so MIXED case patches interleave naturally
    // instead of producing two alphabetical ranks (all uppercase, then all lowercase).
    names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    names
}
