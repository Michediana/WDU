//! `wdu-service`: the privileged part of WDU, running as LocalSystem.
//!
//! The UI runs as the user and talks to it over the named pipe in
//! [`wdu_core::ipc`]; this process is the only one that installs drivers.

use std::ffi::OsString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use wdu_core::{engine, ipc};
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use wdu_service::{init_logging, run};

const SERVICE_NAME: &str = "wdu";
const DISPLAY_NAME: &str = "WDU Driver Updater";
const DESCRIPTION: &str = "Finds driver updates on Windows Update and in Dell, HP and Lenovo catalogs, and installs the approved ones.";

#[derive(Parser)]
#[command(name = "wdu-service", version, about = DISPLAY_NAME)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register and start the service (administrator).
    Install,
    /// Stop and remove the service (administrator).
    Uninstall,
    /// Run in this console instead of as a service, for development.
    Run {
        /// Stop when this process exits (the UI that started us); logs go to
        /// a file, since the console is hidden.
        #[arg(long, hide = true)]
        parent: Option<u32>,
    },
    /// Entry point used by the Service Control Manager.
    #[command(hide = true)]
    Service,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Install => install(),
        Command::Uninstall => uninstall(),
        Command::Run { parent: Some(pid) } => wdu_service::run_for_parent(pid),
        Command::Run { parent: None } => {
            init_logging(None)?;
            run(Arc::new(AtomicBool::new(false)))
        }
        Command::Service => {
            service_dispatcher::start(SERVICE_NAME, ffi_service_main)
                .context("not started by the Service Control Manager")?;
            Ok(())
        }
    }
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    let _ = init_logging(Some(&engine::data_dir().join("logs").join("service.log")));
    if let Err(err) = run_as_service() {
        tracing::error!("service failed: {err:#}");
    }
}

fn run_as_service() -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let handler_stop = stop.clone();
    let status_handle =
        service_control_handler::register(SERVICE_NAME, move |control| match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                handler_stop.store(true, Ordering::SeqCst);
                ipc::wake(ipc::PIPE_NAME);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })?;
    let status = |state, accept| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    status_handle.set_service_status(status(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
    ))?;
    tracing::info!("service started");

    let result = run(stop);
    if let Err(err) = &result {
        tracing::error!("{err:#}");
    }
    status_handle
        .set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty()))?;
    result
}

fn install() -> Result<()> {
    let exe = std::env::current_exe()?;
    // LocalSystem runs this file: users must not be able to replace it.
    let program_files = std::env::var_os("ProgramFiles").unwrap_or_default();
    if program_files.is_empty() || !exe.starts_with(&program_files) {
        eprintln!(
            "warning: {} is not under Program Files; a user who can write there could run code as SYSTEM",
            exe.display()
        );
    }
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("cannot open the Service Control Manager (run as administrator)")?;
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec!["service".into()],
        dependencies: vec![],
        // LocalSystem.
        account_name: None,
        account_password: None,
    };
    let service = manager
        .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)
        .context("cannot create the service")?;
    service.set_description(DESCRIPTION)?;
    service
        .start::<&str>(&[])
        .context("cannot start the service")?;
    println!("service `{SERVICE_NAME}` installed and started");
    Ok(())
}

fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("cannot open the Service Control Manager (run as administrator)")?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )
        .context("cannot open the service")?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        service.stop()?;
        for _ in 0..30 {
            if service.query_status()?.current_state == ServiceState::Stopped {
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    service.delete()?;
    println!("service `{SERVICE_NAME}` removed");
    Ok(())
}
