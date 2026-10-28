//! `wdu-ui`: the desktop UI. Runs as the user, never elevated, and only talks
//! to the service through the pipe.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod devices;
mod driver_files;
mod history;
mod packages;
mod service;
mod settings;
mod shell;
mod text;
mod ui;
mod updates;

use std::borrow::Cow;

use gpui_kit::component::{Theme, ThemeRegistry, TitleBar};
use gpui_kit::{AppContext as _, AssetSource, SharedString, WindowBounds, WindowOptions, px, size};
use wdu_core::ipc::{self, Request};

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

/// GPUI Kit's default icons, and the few more WDU uses.
struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ui::icons::Extra.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ui::icons::Extra.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

/// WDU's light and dark themes: GPUI Kit's defaults with a blue accent and
/// cool greys. The appearance still follows the system.
fn install_theme(cx: &mut gpui_kit::App) {
    let registry = ThemeRegistry::global_mut(cx);
    if let Err(err) = registry.load_themes_from_str(include_str!("theme.json")) {
        eprintln!("cannot load the WDU theme: {err:#}");
        return;
    }
    let themes = registry.themes();
    let (Some(light), Some(dark)) = (themes.get("WDU Light"), themes.get("WDU Dark")) else {
        return;
    };
    let (light, dark) = (light.clone(), dark.clone());
    Theme::update(cx, |theme| {
        theme.light_theme = light;
        theme.dark_theme = dark;
    });
}

fn main() {
    start_service_if_missing();
    gpui_kit::application().with_assets(Assets).run(|cx| {
        gpui_kit::init(cx);
        gpui_kit::component::set_locale("it");
        app::bind_keys(cx);

        // The window's minimum and first size are window geometry, not text.
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1180.), px(780.)), cx)),
            window_min_size: Some(size(px(860.), px(560.))),
            ..TitleBar::window_options()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            install_theme(cx);
            Theme::sync_system_appearance(Some(window), cx);
            cx.new(|cx| app::Workspace::new(window, cx))
        })
        .expect("cannot open the WDU window");
        cx.activate(true);
    });
}
