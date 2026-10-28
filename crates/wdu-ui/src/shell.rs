//! Windows' own property sheets, for a device or a driver package.

use std::path::{Path, PathBuf};

/// Device Manager's properties for one device. Runs in `rundll32`, so the
/// sheet lives on after this call and a crash in it cannot take the UI down.
/// Instance IDs never contain spaces, so the ID needs no quoting.
pub fn device_properties(instance_id: &str) -> Result<(), String> {
    std::process::Command::new(windows_dir().join(r"System32\rundll32.exe"))
        .args([
            "devmgr.dll,DeviceProperties_RunDLL",
            "/DeviceID",
            instance_id,
        ])
        .spawn()
        .map(drop)
        .map_err(|err| format!("Impossibile aprire le proprietà del dispositivo: {err}"))
}

/// Explorer's properties for a file. The sheet opens on a thread of its own.
pub fn file_properties(path: &Path) -> Result<(), String> {
    use windows::Win32::UI::Shell::{SHOP_FILEPATH, SHObjectProperties};
    use windows::core::{HSTRING, PCWSTR};

    let name = HSTRING::from(path.as_os_str());
    if unsafe { SHObjectProperties(None, SHOP_FILEPATH, &name, PCWSTR::null()) }.as_bool() {
        Ok(())
    } else {
        Err(format!(
            "Impossibile aprire le proprietà di {}",
            path.display()
        ))
    }
}

/// An Explorer window on the file's folder, with the file selected.
pub fn show_in_folder(path: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    // Explorer reads `/select,` itself, so the path goes in quotes as is.
    std::process::Command::new(windows_dir().join("explorer.exe"))
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn()
        .map(drop)
        .map_err(|err| format!("Impossibile aprire la cartella del driver: {err}"))
}

/// The INF of a driver package: the copy in the driver store when its folder
/// is known, else the one Windows published under `INF`.
pub fn package_inf(
    published_name: &str,
    original_name: Option<&str>,
    store_path: Option<&str>,
) -> PathBuf {
    match (store_path, original_name) {
        (Some(dir), Some(name)) => Path::new(dir).join(name),
        _ => windows_dir().join("INF").join(published_name),
    }
}

fn windows_dir() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}
