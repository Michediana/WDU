//! The files of a device's driver, in a sheet beside the device list. The
//! files are read on the background executor: with the signatures checked,
//! that can take a moment.

use gpui_kit::component::StyledExt as _;
use gpui_kit::component::description_list::DescriptionList;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Task, Window, div, px,
};
use wdu_core::driver_files::{self, DriverFile};

use crate::shell;
use crate::ui::{self, Lucide};

/// Device Manager's properties for the device, or why they cannot open.
pub fn open_device_properties(instance_id: &str, window: &mut Window, cx: &mut App) {
    if let Err(err) = shell::device_properties(instance_id) {
        window.push_notification(Notification::error(err), cx);
    }
}

/// The sheet with the driver files of one device.
pub fn open_driver_files(
    instance_id: String,
    device: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let view = cx.new(|cx| DriverFilesView::new(instance_id, cx));
    window.open_sheet(cx, move |sheet, _, _| {
        sheet
            .title(
                v_flex()
                    .gap_0p5()
                    .child("File del driver")
                    .child(div().text_sm().font_normal().child(device.clone())),
            )
            // The sheet's width is a window region, not text: it does not zoom.
            .size(px(560.))
            .child(view.clone())
    });
}

pub struct DriverFilesView {
    files: Option<Result<Vec<DriverFile>, String>>,
    _load: Task<()>,
}

impl DriverFilesView {
    fn new(instance_id: String, cx: &mut Context<Self>) -> Self {
        let read = cx.background_spawn(async move {
            driver_files::driver_files(&instance_id).map_err(|err| format!("{err:#}"))
        });
        let load = cx.spawn(async move |this, cx| {
            let files = read.await;
            this.update(cx, |this, cx| {
                this.files = Some(files);
                cx.notify();
            })
            .ok();
        });
        DriverFilesView {
            files: None,
            _load: load,
        }
    }
}

impl Render for DriverFilesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.files {
            None => ui::loading("Lettura dei file e delle firme…", cx).into_any_element(),
            Some(Err(err)) => ui::empty_state(
                Lucide::CircleX,
                "Impossibile leggere i file del driver",
                Some(err.clone().into()),
                cx,
            )
            .into_any_element(),
            Some(Ok(files)) if files.is_empty() => ui::empty_state(
                Lucide::File,
                "Nessun file",
                Some("Il driver di questo dispositivo non richiede né carica file.".into()),
                cx,
            )
            .into_any_element(),
            Some(Ok(files)) => v_flex()
                .gap_4()
                .child(ui::muted(
                    match files.len() {
                        1 => "1 file".to_string(),
                        n => format!("{n} file"),
                    },
                    cx,
                ))
                .children(files.iter().map(|f| file(f, cx)))
                .into_any_element(),
        };
        div()
            .id("driver-files")
            .size_full()
            .overflow_y_scrollbar()
            .child(content)
    }
}

fn file(file: &DriverFile, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let (signed_icon, signed_color, signer) = match &file.signer {
        Some(signer) => (Lucide::ShieldCheck, theme.success, signer.clone()),
        None => (
            Lucide::ShieldAlert,
            theme.warning,
            "Non firmato digitalmente".to_string(),
        ),
    };
    v_flex()
        .gap_2()
        .child(
            h_flex()
                .gap_2()
                .items_start()
                .child(Icon::new(Lucide::FileText).small().mt_0p5())
                .child(
                    div()
                        .min_w_0()
                        .text_sm()
                        .font_medium()
                        .font_family(theme.mono_font_family.clone())
                        .child(file.path.display().to_string()),
                ),
        )
        .child(
            DescriptionList::new()
                .columns(1)
                .small()
                .item("Fornitore", file.company.clone().unwrap_or("–".into()), 1)
                .item(
                    "Versione file",
                    file.version.clone().unwrap_or("–".into()),
                    1,
                )
                .item("Copyright", file.copyright.clone().unwrap_or("–".into()), 1)
                .item(
                    "Firmatario",
                    h_flex()
                        .gap_1p5()
                        .child(Icon::new(signed_icon).small().text_color(signed_color))
                        .child(signer)
                        .into_any_element(),
                    1,
                ),
        )
}
