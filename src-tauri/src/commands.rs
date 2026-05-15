use crate::ipc::{handle_msg, AppState, ClientMsg, ServerMsg};
use std::sync::Arc;
use tauri::State;

#[tauri::command]
pub fn dispatch(state: State<Arc<AppState>>, msg: ClientMsg) -> ServerMsg {
    handle_msg(msg, &state)
}
