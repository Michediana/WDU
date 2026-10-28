//! The privileged backend of WDU: serves the named pipe in [`wdu_core::ipc`]
//! and is the only code that installs drivers.
//!
//! The `wdu-service` binary runs it as a Windows service or in a console;
//! `wdu-ui` links it too, so a single portable exe can start its own backend.

mod worker;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;
use wdu_core::{engine, ipc};

use crate::worker::Service;

/// Serves the pipe until `stop` is set.
pub fn run(stop: Arc<AtomicBool>) -> Result<()> {
    let service = Service::start(engine::data_dir(), stop.clone())?;
    tracing::info!(pipe = ipc::PIPE_NAME, "listening");
    ipc::serve(move |request| service.handle(request), stop)
}

/// Serves the pipe until process `pid` (the UI that started us) exits. Logs
/// go to a file, since there is no console to show them.
pub fn run_for_parent(pid: u32) -> Result<()> {
    init_logging(Some(
        &engine::data_dir().join("logs").join("service-ui.log"),
    ))?;
    let stop = Arc::new(AtomicBool::new(false));
    let result = stop_with_parent(pid, stop.clone()).and_then(|()| run(stop));
    if let Err(err) = &result {
        tracing::error!("{err:#}");
    }
    result
}

/// Sets `stop` and wakes the pipe once process `pid` exits.
fn stop_with_parent(pid: u32, stop: Arc<AtomicBool>) -> Result<()> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Threading::{
        INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }
        .with_context(|| format!("cannot open parent process {pid}"))?;
    // HANDLE is not Send.
    let raw = process.0 as usize;
    std::thread::spawn(move || {
        let process = HANDLE(raw as *mut _);
        unsafe {
            WaitForSingleObject(process, INFINITE);
            let _ = CloseHandle(process);
        }
        tracing::info!(pid, "parent exited, stopping");
        stop.store(true, Ordering::SeqCst);
        ipc::wake(ipc::PIPE_NAME);
    });
    Ok(())
}

/// Logs to `file`, or to stderr when there is none.
pub fn init_logging(file: Option<&Path>) -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match file {
        Some(path) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            builder
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .init();
        }
        None => builder.with_writer(std::io::stderr).init(),
    }
    Ok(())
}
