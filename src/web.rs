use crate::params::{Params, Patch};
use crate::synth::NoteEvent;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use crossbeam_queue::ArrayQueue;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub params: Arc<Params>,
    pub queue: Arc<ArrayQueue<NoteEvent>>,
    pub patches_dir: PathBuf,
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
    while let Some(Ok(msg)) = ws.recv().await {
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
        let json = match serde_json::to_string(&response) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if ws.send(Message::Text(json)).await.is_err() {
            break;
        }
    }
}

fn handle_msg(msg: ClientMsg, state: &AppState) -> ServerMsg {
    match msg {
        ClientMsg::NoteOn { note, velocity } => {
            let _ = state.queue.push(NoteEvent::On { note, velocity });
            ServerMsg::Ok
        }
        ClientMsg::NoteOff { note } => {
            let _ = state.queue.push(NoteEvent::Off { note });
            ServerMsg::Ok
        }
        ClientMsg::AllOff => {
            let _ = state.queue.push(NoteEvent::AllOff);
            ServerMsg::Ok
        }
        ClientMsg::Param { name, value } => {
            if state.params.set(&name, value) {
                ServerMsg::Ok
            } else {
                ServerMsg::Error {
                    message: format!("Unknown param: {}", name),
                }
            }
        }
        ClientMsg::LoadPatch { name } => {
            let safe = sanitize(&name);
            let path = state.patches_dir.join(format!("{}.json", safe));
            match std::fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str::<Patch>(&content) {
                    Ok(patch) => {
                        state.params.apply_patch(&patch);
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
