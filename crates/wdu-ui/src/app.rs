//! The WDU window: title bar, navigation, and the page in view. Strings
//! from catalogs are untrusted, but GPUI only ever draws them as text.

use gpui_kit::component::StyledExt as _;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::sidebar::{
    Sidebar, SidebarFooter, SidebarHeader, SidebarMenu, SidebarMenuItem,
};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Theme, ThemeMode, TitleBar, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyBinding, ParentElement as _, Render, SharedString, Styled as _, Subscription,
    Window, actions, div, prelude::FluentBuilder as _,
};

use crate::devices::DevicesPage;
use crate::history::HistoryPage;
use crate::packages::PackagesPage;
use crate::service::Service;
use crate::settings::SettingsPage;
use crate::ui::Lucide;
use crate::updates::UpdatesPage;

actions!(
    wdu,
    [
        /// Reads the page in view again.
        Refresh
    ]
);

const CONTEXT: &str = "Workspace";

pub fn bind_keys(cx: &mut gpui_kit::App) {
    cx.bind_keys([KeyBinding::new("f5", Refresh, Some(CONTEXT))]);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Updates,
    Devices,
    Packages,
    History,
    Settings,
}

impl Page {
    const ALL: [Page; 5] = [
        Page::Updates,
        Page::Devices,
        Page::Packages,
        Page::History,
        Page::Settings,
    ];

    fn label(self) -> &'static str {
        match self {
            Page::Updates => "Aggiornamenti",
            Page::Devices => "Dispositivi",
            Page::Packages => "Driver installati",
            Page::History => "Storico",
            Page::Settings => "Impostazioni",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Page::Updates => Icon::new(Lucide::Download),
            Page::Devices => Icon::new(IconName::Cpu),
            Page::Packages => Icon::new(Lucide::Package),
            Page::History => Icon::new(Lucide::RotateCcwClock),
            Page::Settings => Icon::new(IconName::Settings),
        }
    }
}

pub struct Workspace {
    service: Entity<Service>,
    page: Page,
    updates: Entity<UpdatesPage>,
    devices: Entity<DevicesPage>,
    packages: Entity<PackagesPage>,
    history: Entity<HistoryPage>,
    settings: Entity<SettingsPage>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let service = cx.new(Service::new);
        let updates = cx.new(|cx| UpdatesPage::new(service.clone(), cx));
        let devices = cx.new(|cx| DevicesPage::new(service.clone(), window, cx));
        let packages = cx.new(|cx| PackagesPage::new(service.clone(), window, cx));
        let history = cx.new(|cx| HistoryPage::new(service.clone(), window, cx));
        let settings = cx.new(|cx| SettingsPage::new(service.clone(), window, cx));
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let observe = cx.observe(&service, |_, _, cx| cx.notify());
        Workspace {
            service,
            page: Page::Updates,
            updates,
            devices,
            packages,
            history,
            settings,
            focus,
            _subscriptions: vec![observe],
        }
    }

    fn open(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        if self.page != page {
            self.page = page;
            self.refresh(window, cx);
        }
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.page {
            Page::Updates => self.service.update(cx, |s, cx| {
                s.refresh_status(cx);
                s.reload_plan(cx);
            }),
            Page::Devices => self.devices.update(cx, |p, cx| p.reload(cx)),
            Page::Packages => self.packages.update(cx, |p, cx| p.reload(cx)),
            Page::History => self.service.update(cx, |s, cx| s.reload_history(cx)),
            Page::Settings => self.settings.update(cx, |p, cx| p.reload(window, cx)),
        }
        cx.notify();
    }

    fn render_title_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let dark = cx.theme().is_dark();
        TitleBar::new().child(
            h_flex()
                .w_full()
                .pr_2()
                .justify_between()
                .child(
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .child(div().font_semibold().child("WDU"))
                        .child(
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .child(self.page.label()),
                        ),
                )
                // The bar is the window's drag area: without occluding it, Windows
                // takes a click on the button for a click on the caption.
                .child(
                    div().occlude().child(
                        Button::new("theme")
                            .ghost()
                            .small()
                            .icon(if dark { IconName::Sun } else { IconName::Moon })
                            .tooltip(if dark { "Tema chiaro" } else { "Tema scuro" })
                            .on_click(move |_, window, cx| {
                                let mode = if dark {
                                    ThemeMode::Light
                                } else {
                                    ThemeMode::Dark
                                };
                                Theme::change(mode, Some(window), cx);
                            }),
                    ),
                ),
        )
    }

    fn render_sidebar(&self, cx: &Context<Self>) -> impl IntoElement {
        let service = self.service.read(cx);
        let theme = cx.theme();
        let proposals = match service.scan() {
            Some(Some(scan)) => scan.plan.proposals.len(),
            _ => 0,
        };
        let (dot, status): (_, SharedString) = match (service.offline(), service.state()) {
            (Some(_), _) => (theme.danger, "Servizio non raggiungibile".into()),
            (None, None) => (theme.muted_foreground, "Connessione…".into()),
            (None, Some(state)) => (
                theme.success,
                match state {
                    wdu_core::ipc::ServiceState::Idle => "Servizio attivo".into(),
                    state => crate::text::state(state).into(),
                },
            ),
        };

        let items = Page::ALL.map(|page| {
            SidebarMenuItem::new(page.label())
                .icon(page.icon())
                .active(self.page == page)
                .when(page == Page::Updates && proposals > 0, |item| {
                    item.suffix(move |_, _| {
                        Tag::primary()
                            .xsmall()
                            .rounded_full()
                            .child(proposals.to_string())
                    })
                })
                .on_click(cx.listener(move |this, _, window, cx| this.open(page, window, cx)))
        });

        Sidebar::new("navigation")
            .collapsible(false)
            .header(
                SidebarHeader::new().child(
                    h_flex()
                        .gap_3()
                        .child(
                            div()
                                .flex()
                                .flex_shrink_0()
                                .items_center()
                                .justify_center()
                                .size_8()
                                .rounded(theme.radius)
                                .bg(theme.primary)
                                .text_color(theme.primary_foreground)
                                .child(Icon::new(IconName::Cpu).small()),
                        )
                        .child(
                            v_flex()
                                .min_w_0()
                                .child(div().text_sm().font_semibold().child("WDU"))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.sidebar_foreground.opacity(0.7))
                                        .child("Aggiornamento driver"),
                                ),
                        ),
                ),
            )
            .child(SidebarMenu::new().children(items))
            .footer(
                SidebarFooter::new().child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .child(div().size_2().flex_shrink_0().rounded_full().bg(dot))
                        .child(div().truncate().child(status)),
                ),
            )
    }

    fn render_page(&self) -> AnyElement {
        match self.page {
            Page::Updates => self.updates.clone().into_any_element(),
            Page::Devices => self.devices.clone().into_any_element(),
            Page::Packages => self.packages.clone().into_any_element(),
            Page::History => self.history.clone().into_any_element(),
            Page::Settings => self.settings.clone().into_any_element(),
        }
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let reboot = self
            .service
            .read(cx)
            .status()
            .is_some_and(|s| s.reboot_required);
        let cx = &*cx;

        v_flex()
            .id("workspace")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Refresh, window, cx| this.refresh(window, cx)))
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_title_bar(cx))
            .child(
                h_flex()
                    .items_stretch()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .when(reboot, |this| {
                                this.child(
                                    Alert::warning(
                                        "reboot",
                                        "Un'installazione richiede il riavvio. Riavvia il PC quando \
                                         ti è comodo: WDU non riavvia mai da solo.",
                                    )
                                    .icon(IconName::RotateCw)
                                    .banner(),
                                )
                            })
                            .child(div().flex_1().min_h_0().child(self.render_page())),
                    ),
            )
    }
}
