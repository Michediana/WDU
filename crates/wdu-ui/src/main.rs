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

/// If no service answers on the pipe, starts `wdu-service run` from next to
/// this exe, elevated through UAC, tied to this process: it stops when the
/// UI closes. The UI itself stays unelevated; it picks the service up on its
/// next status poll.
fn start_service_if_missing() {
    if ipc::call(&Request::Hello).is_ok() {
        return;
    }
    std::thread::spawn(|| {
        if let Err(err) = launch_service() {
            eprintln!("cannot start wdu-service: {err:#}");
        }
    });
}

fn launch_service() -> windows::core::Result<()> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
    };
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    use windows::core::{HSTRING, PCWSTR, w};

    let exe = std::env::current_exe()
        .map_err(|err| windows::core::Error::new(windows::core::HRESULT(-1), err.to_string()))?
        .with_file_name("wdu-service.exe");
    let file = HSTRING::from(exe.as_os_str());
    let params = HSTRING::from(format!("run --parent {}", std::process::id()));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        ShellExecuteExW(&mut info)?;
        if !info.hProcess.is_invalid() {
            let _ = CloseHandle(info.hProcess);
        }
    }
    Ok(())
}

fn main() {
    start_service_if_missing();
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![service])
        .run(tauri::generate_context!())
        .expect("cannot start the WDU window");
}
