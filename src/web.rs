use crate::params::{Params, Patch};
use crate::performance::{Broadcast, Performer};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct AppState {
    pub params: Arc<Params>,
    pub performer: Arc<Performer>,
    pub patches_dir: PathBuf,
    pub broadcast: broadcast::Sender<Broadcast>,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ClientMsg {
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
enum ServerMsg {
    Patches { names: Vec<String> },
    State { patch: Patch },
    Ok,
    Error { message: String },
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/ws", get(ws_upgrade))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |s| handle_ws(s, state))
}

async fn handle_ws(mut ws: WebSocket, state: AppState) {
    let mut sub = state.broadcast.subscribe();
    loop {
        tokio::select! {
            incoming = ws.recv() => {
                let Some(Ok(msg)) = incoming else { break; };
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Close(_) => break,
                    _ => continue,
                };
                let response = match serde_json::from_str::<ClientMsg>(&text) {
                    Ok(m) => handle_msg(m, &state),
                    Err(e) => ServerMsg::Error {
                        message: format!("Bad msg: {}", e),
                    },
                };
                let Ok(json) = serde_json::to_string(&response) else { continue; };
                if ws.send(Message::Text(json)).await.is_err() { break; }
            }
            pushed = sub.recv() => {
                match pushed {
                    Ok(b) => {
                        let Ok(json) = serde_json::to_string(&b) else { continue; };
                        if ws.send(Message::Text(json)).await.is_err() { break; }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => { /* skip */ }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

fn handle_msg(msg: ClientMsg, state: &AppState) -> ServerMsg {
    match msg {
        ClientMsg::NoteOn { note, velocity } => {
            state.performer.note_on(note, velocity);
            ServerMsg::Ok
        }
        ClientMsg::NoteOff { note } => {
            state.performer.note_off(note);
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
            let mut refresh = false;
            // Mode-altering params need a clean slate to avoid stuck notes.
            if name == "arp_enabled" || name == "chord_type" {
                state.performer.all_off();
            }
            // Polyphony is required for chord notes to coexist — auto-disable mono.
            if name == "chord_type" && value > 0.5 && state.params.mono.load() > 0.5 {
                state.params.mono.store(0.0);
                refresh = true;
            }
            // And refuse mono-on while a chord is engaged.
            if name == "mono"
                && value > 0.5
                && state.params.chord_type.load() > 0.5
            {
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
                        // Enforce the chord/mono constraint at load time too.
                        if patch.chord_type > 0.5 {
                            patch.mono = 0.0;
                        }
                        state.params.apply_patch(&patch);
                        state.performer.all_off();
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
                    Ok(_) => ServerMsg::Ok,
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

fn list_patches(dir: &PathBuf) -> Vec<String> {
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
    names.sort();
    names
}
