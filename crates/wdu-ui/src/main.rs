//! `wdu-ui`: the desktop UI. Runs as the user, never elevated, and only talks
//! to the service through the pipe.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use wdu_core::ipc::{self, Request, Response};

/// Forwards one request to the service. Off the main thread: a call may wait
/// for the pipe.
#[tauri::command(async)]
fn service(request: Request) -> Result<Response, String> {
    ipc::call(&request).map_err(|err| format!("{err:#}"))
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![service])
        .run(tauri::generate_context!())
        .expect("cannot start the WDU window");
}
