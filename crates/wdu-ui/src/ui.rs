//! Presentation shared by the pages: the page frame, section titles, empty
//! states and the line of a scan step.

use std::time::Duration;

use gpui_kit::component::StyledExt as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, Hsla, IntoElement, ParentElement as _, RenderOnce, SharedString, Styled as _,
    Window, div, prelude::FluentBuilder as _,
};
use wdu_core::engine::{ScanStep, StepState};

use crate::text;

/// Icons outside GPUI Kit's default set, embedded on demand.
pub mod icons {
    gpui_kit::assets::icon_assets!(pub Extra, [
        Download,
        RotateCcwClock,
        Package,
        PackageCheck,
        Save,
        ServerOff,
        ShieldAlert,
        ShieldCheck,
        Trash,
        Usb,
    ]);
}

pub use gpui_kit::assets::IconName as Lucide;

/// The title row of a page: what it shows, a line on it, and its commands.
#[derive(IntoElement)]
pub struct PageHeader {
    title: SharedString,
    description: Option<SharedString>,
    actions: Vec<AnyElement>,
}

impl PageHeader {
    pub fn new(title: impl Into<SharedString>) -> Self {
        PageHeader {
            title: title.into(),
            description: None,
            actions: Vec::new(),
        }
    }

    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.actions.push(action.into_any_element());
        self
    }
}

impl RenderOnce for PageHeader {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        h_flex()
            .w_full()
            .gap_4()
            .justify_between()
            .child(
                v_flex()
                    .min_w_0()
                    .gap_1()
                    .child(div().text_xl().font_semibold().child(self.title))
                    .when_some(self.description, |this, description| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(description),
                        )
                    }),
            )
            .child(h_flex().flex_shrink_0().gap_2().children(self.actions))
    }
}

/// A section title inside a page.
pub fn section_title(title: impl Into<SharedString>) -> impl IntoElement {
    div().text_base().font_semibold().child(title.into())
}

/// Secondary text: metadata and help.
pub fn muted(text: impl Into<SharedString>, cx: &App) -> gpui_kit::Div {
    div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

/// A round, tinted badge around an icon: the status mark of a summary.
pub fn status_mark(icon: impl Into<Icon>, color: Hsla) -> impl IntoElement {
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size_12()
        .rounded_full()
        .bg(color.opacity(0.14))
        .text_color(color)
        .child(Icon::new(icon).large())
}

/// What a page shows while it has nothing: one icon, one line, and help.
pub fn empty_state(
    icon: impl Into<Icon>,
    title: impl Into<SharedString>,
    description: Option<SharedString>,
    cx: &App,
) -> impl IntoElement {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .py_8()
        .child(
            div()
                .mb_2()
                .text_color(cx.theme().muted_foreground)
                .child(Icon::new(icon).size_10()),
        )
        .child(div().text_base().font_medium().child(title.into()))
        .when_some(description, |this, description| {
            this.child(muted(description, cx).max_w_96().text_center())
        })
}

/// A full-page wait for data that is being read.
pub fn loading(label: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    h_flex()
        .size_full()
        .justify_center()
        .gap_2()
        .text_color(cx.theme().muted_foreground)
        .child(Spinner::new().small())
        .child(div().text_sm().child(label.into()))
}

/// One line of the scan steps: state, what, where, what it found, and for
/// the running step how long it has run.
pub fn scan_step(step: &ScanStep, elapsed: Option<Duration>, cx: &App) -> impl IntoElement {
    let (what, place) = text::scan_step(step);
    let theme = cx.theme();
    let mark: AnyElement = match step.state {
        StepState::Running => Spinner::new()
            .small()
            .color(theme.primary)
            .into_any_element(),
        StepState::Done => Icon::new(IconName::CircleCheck)
            .small()
            .text_color(theme.success)
            .into_any_element(),
        StepState::Failed => Icon::new(IconName::CircleX)
            .small()
            .text_color(theme.danger)
            .into_any_element(),
    };
    let mut found = Vec::new();
    if let Some(n) = text::scan_found(step) {
        found.push(n);
    }
    if step.state == StepState::Done
        && let Some(detail) = &step.detail
    {
        found.push(detail.clone());
    }
    let failure = (step.state == StepState::Failed)
        .then(|| step.detail.clone())
        .flatten();

    h_flex()
        .w_full()
        .items_start()
        .gap_3()
        .py_1()
        .child(
            h_flex()
                .size_5()
                .flex_shrink_0()
                .justify_center()
                .child(mark),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .child(div().text_sm().font_medium().child(what))
                        .child(muted(place, cx)),
                )
                .when(!found.is_empty(), |this| {
                    this.child(muted(found.join(" · "), cx))
                })
                .when_some(failure, |this, failure| {
                    this.child(div().text_sm().text_color(theme.danger).child(failure))
                }),
        )
        .when_some(elapsed, |this, elapsed| {
            this.child(muted(format!("{} s", elapsed.as_secs()), cx).flex_shrink_0())
        })
}
