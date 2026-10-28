//! Settings: the service's config, edited as a copy and saved at once.

use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::form::{Field, Form};
use gpui_kit::component::input::{Input, InputEvent, InputState, NumberInput};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{Disableable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _,
};
use wdu_core::ipc::{Request, Response, ServiceConfig};

use crate::service::{self, Service};
use crate::ui::{self, Lucide, PageHeader};

pub struct SettingsPage {
    service: Entity<Service>,
    /// The config as the service last reported it.
    saved: Option<ServiceConfig>,
    /// The config being edited; the text fields hold the rest.
    draft: Option<ServiceConfig>,
    interval: Entity<InputState>,
    deny: Entity<InputState>,
    allow: Entity<InputState>,
    error: Option<String>,
    saving: bool,
    _subscriptions: Vec<Subscription>,
}

impl SettingsPage {
    pub fn new(service: Entity<Service>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let interval = cx.new(|cx| InputState::new(window, cx).step(1.).min(0.).max(744.));
        let deny = cx.new(|cx| InputState::new(window, cx).placeholder("es. Display, Bluetooth"));
        let allow = cx.new(|cx| InputState::new(window, cx).placeholder("Vuoto: tutte le classi"));
        // Save and discard follow the fields as they are typed.
        let changed =
            |_: &mut Self, _: Entity<InputState>, event: &InputEvent, cx: &mut Context<Self>| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            };
        let subscriptions = vec![
            cx.subscribe(&interval, changed),
            cx.subscribe(&deny, changed),
            cx.subscribe(&allow, changed),
        ];
        SettingsPage {
            service,
            saved: None,
            draft: None,
            interval,
            deny,
            allow,
            error: None,
            saving: false,
            _subscriptions: subscriptions,
        }
    }

    /// Reads the config again, dropping unsaved edits.
    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let task = service::call(Request::GetConfig, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(Response::Config { config }) => {
                        this.error = None;
                        this.load(config, window, cx);
                    }
                    Ok(_) => {}
                    Err(err) => this.error = Some(err),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load(&mut self, config: ServiceConfig, window: &mut Window, cx: &mut Context<Self>) {
        let hours = config.check_interval_hours.to_string();
        let deny = config.policy.deny_classes.join(", ");
        let allow = config.policy.allow_classes.join(", ");
        self.interval
            .update(cx, |s, cx| s.set_value(hours, window, cx));
        self.deny.update(cx, |s, cx| s.set_value(deny, window, cx));
        self.allow
            .update(cx, |s, cx| s.set_value(allow, window, cx));
        self.saved = Some(config.clone());
        self.draft = Some(config);
    }

    /// The draft with the text fields read in; `None` while the hours are
    /// not a number.
    fn edited(&self, cx: &gpui_kit::App) -> Option<ServiceConfig> {
        let mut config = self.draft.clone()?;
        config.check_interval_hours = self.interval.read(cx).value().trim().parse().ok()?;
        config.policy.deny_classes = classes(&self.deny.read(cx).value());
        config.policy.allow_classes = classes(&self.allow.read(cx).value());
        Some(config)
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.edited(cx) else {
            return;
        };
        self.saving = true;
        cx.notify();
        let task = service::call(
            Request::SetConfig {
                config: config.clone(),
            },
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.saving = false;
                match result {
                    Ok(_) => {
                        this.saved = Some(config);
                        window.push_notification(Notification::success("Impostazioni salvate"), cx);
                        this.service.update(cx, |s, cx| s.refresh_status(cx));
                    }
                    Err(err) => window.push_notification(
                        Notification::error(err).title("Impossibile salvare le impostazioni"),
                        cx,
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn edit(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut ServiceConfig)) {
        if let Some(draft) = &mut self.draft {
            change(draft);
            cx.notify();
        }
    }
}

impl Render for SettingsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = PageHeader::new("Impostazioni");
        let Some(draft) = self.draft.clone() else {
            let body = match &self.error {
                Some(err) => Alert::error("settings-error", err.clone())
                    .title("Impossibile leggere le impostazioni")
                    .into_any_element(),
                None => ui::loading("Lettura delle impostazioni…", cx).into_any_element(),
            };
            return v_flex()
                .size_full()
                .p_6()
                .gap_5()
                .child(header)
                .child(body)
                .into_any_element();
        };
        let edited = self.edited(cx);
        let dirty = edited.as_ref() != self.saved.as_ref();
        let valid = edited.is_some();
        let checkbox = |id: &'static str, label: &'static str, checked: bool| {
            Checkbox::new(id).label(label).checked(checked)
        };

        div()
            .id("settings")
            .size_full()
            .overflow_y_scrollbar()
            .child(
                v_flex()
                    .p_6()
                    .gap_6()
                    .max_w(gpui_kit::rems(48.))
                    .child(header)
                    .child(
                        v_flex()
                            .gap_3()
                            .child(ui::section_title("Ricerca pianificata"))
                            .child(
                                Form::new()
                                    .child(
                                        Field::new()
                                            .label("Intervallo tra le ricerche, in ore")
                                            .description("0 disattiva le ricerche automatiche.")
                                            .child(div().w_48().child(
                                                NumberInput::new(&self.interval).placeholder("24"),
                                            )),
                                    )
                                    .child(
                                        Field::new().label_indent(false).child(
                                            checkbox(
                                                "auto-install",
                                                "Installa da solo dopo le ricerche pianificate, \
                                                 senza chiedere",
                                                draft.auto_install,
                                            )
                                            .on_change(cx.listener(
                                                |this, checked: &bool, _, cx| {
                                                    let checked = *checked;
                                                    this.edit(cx, |c| c.auto_install = checked)
                                                },
                                            )),
                                        ),
                                    ),
                            ),
                    )
                    .child(
                        v_flex().gap_3().child(ui::section_title("Fonti")).child(
                            Form::new()
                                .child(
                                    Field::new().label_indent(false).child(
                                        checkbox(
                                            "windows-update",
                                            "Windows Update",
                                            draft.windows_update,
                                        )
                                        .on_change(
                                            cx.listener(|this, checked: &bool, _, cx| {
                                                let checked = *checked;
                                                this.edit(cx, |c| c.windows_update = checked)
                                            }),
                                        ),
                                    ),
                                )
                                .child(
                                    Field::new().label_indent(false).child(
                                        checkbox(
                                            "oem-catalog",
                                            "Catalogo del produttore (Dell, HP, Lenovo)",
                                            draft.oem_catalog,
                                        )
                                        .on_change(
                                            cx.listener(|this, checked: &bool, _, cx| {
                                                let checked = *checked;
                                                this.edit(cx, |c| c.oem_catalog = checked)
                                            }),
                                        ),
                                    ),
                                )
                                .child(
                                    Field::new()
                                        .label_indent(false)
                                        .description(
                                            "Il produttore ha validato i propri driver per \
                                                 questo modello: Windows Update non viene proposto \
                                                 per i dispositivi che il suo catalogo copre.",
                                        )
                                        .child(
                                            checkbox(
                                                "oem-exclusive",
                                                "Preferisci il catalogo del produttore",
                                                draft.policy.oem_exclusive,
                                            )
                                            .on_change(cx.listener(
                                                |this, checked: &bool, _, cx| {
                                                    let checked = *checked;
                                                    this.edit(cx, |c| {
                                                        c.policy.oem_exclusive = checked
                                                    })
                                                },
                                            )),
                                        ),
                                ),
                        ),
                    )
                    .child(
                        v_flex()
                            .gap_3()
                            .child(ui::section_title("Classi di dispositivi"))
                            .child(
                                Form::new()
                                    .child(
                                        Field::new()
                                            .label("Classi escluse")
                                            .description(
                                                "Mai aggiornate, da nessuna fonte. Separale con \
                                                 una virgola.",
                                            )
                                            .child(Input::new(&self.deny)),
                                    )
                                    .child(
                                        Field::new()
                                            .label("Solo queste classi")
                                            .child(Input::new(&self.allow)),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_3()
                            .pt_2()
                            .child(
                                Button::new("save")
                                    .primary()
                                    .icon(Lucide::Save)
                                    .label("Salva")
                                    .loading(self.saving)
                                    .disabled(!dirty || !valid || self.saving)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.save(window, cx)),
                                    ),
                            )
                            .child(
                                Button::new("discard")
                                    .ghost()
                                    .label("Annulla modifiche")
                                    .disabled(!dirty || self.saving)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        if let Some(saved) = this.saved.clone() {
                                            this.load(saved, window, cx);
                                            cx.notify();
                                        }
                                    })),
                            )
                            .when(!valid, |this| {
                                this.child(ui::muted("Indica le ore con un numero intero.", cx))
                            }),
                    ),
            )
            .into_any_element()
    }
}

fn classes(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}
