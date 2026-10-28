//! Updates: the summary of the last scan, the scan itself while it runs,
//! and the proposals to install.

use std::collections::{BTreeMap, HashSet};

use gpui_kit::component::StyledExt as _;
use gpui_kit::component::accordion::Accordion;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::table::{Table, TableBody, TableCell, TableHead, TableHeader, TableRow};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, App, Context, Entity, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _,
};
use wdu_core::engine::ScanResult;
use wdu_core::ipc::{Request, ServiceState};
use wdu_core::matching::Proposal;

use crate::service::{self, Service};
use crate::text;
use crate::ui::{self, Lucide, PageHeader};

pub struct UpdatesPage {
    service: Entity<Service>,
    /// Proposals marked for installation, by candidate ID.
    selected: HashSet<String>,
    /// An installation was asked for and the service has not answered yet.
    installing: bool,
    _subscriptions: Vec<Subscription>,
}

impl UpdatesPage {
    pub fn new(service: Entity<Service>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&service, |this: &mut Self, service, cx| {
            // Forget selections the new plan no longer proposes.
            let proposed: HashSet<&str> = match service.read(cx).scan() {
                Some(Some(scan)) => scan
                    .plan
                    .proposals
                    .iter()
                    .map(|p| p.candidate.id.as_str())
                    .collect(),
                _ => HashSet::new(),
            };
            this.selected.retain(|id| proposed.contains(id.as_str()));
            cx.notify();
        });
        UpdatesPage {
            service,
            selected: HashSet::new(),
            installing: false,
            _subscriptions: vec![observe],
        }
    }

    pub fn scan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let task = service::call(Request::Scan, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                if let Err(err) = result {
                    window.push_notification(
                        Notification::error(err).title("Impossibile avviare la ricerca"),
                        cx,
                    );
                }
                this.service.update(cx, |s, cx| s.refresh_status(cx));
            })
            .ok();
        })
        .detach();
    }

    fn install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<String> = self.selected.iter().cloned().collect();
        self.installing = true;
        cx.notify();
        let task = service::call(Request::Install { ids }, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.installing = false;
                match result {
                    Ok(_) => this.selected.clear(),
                    Err(err) => window.push_notification(
                        Notification::error(err).title("Impossibile avviare l'installazione"),
                        cx,
                    ),
                }
                this.service.update(cx, |s, cx| s.refresh_status(cx));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn confirm_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.selected.len();
        let page = cx.entity().downgrade();
        let title: SharedString = if n == 1 {
            "Installare 1 aggiornamento?".into()
        } else {
            format!("Installare {n} aggiornamenti?").into()
        };
        window.open_alert_dialog(cx, move |alert, _, _| {
            let page = page.clone();
            alert
                .title(title.clone())
                .description(
                    "Prima viene creato un punto di ripristino e salvato il driver attuale: \
                     se un dispositivo smette di funzionare, il driver precedente viene \
                     ripristinato.",
                )
                .show_cancel(true)
                .cancel_text("Annulla")
                .ok_text("Installa")
                .ok_variant(ButtonVariant::Primary)
                .on_ok(move |_, window, cx| {
                    page.update(cx, |page, cx| page.install(window, cx)).ok();
                    true
                })
        });
    }

    fn toggle(&mut self, id: &str, checked: bool, cx: &mut Context<Self>) {
        if checked {
            self.selected.insert(id.to_string());
        } else {
            self.selected.remove(id);
        }
        cx.notify();
    }

    // --- regions --------------------------------------------------------------

    /// The state of the PC at a glance, and the two commands of the page.
    fn render_summary(&self, cx: &Context<Self>) -> impl IntoElement {
        let service = self.service.read(cx);
        let theme = cx.theme();
        let busy = service.is_busy();
        let online = service.is_online();
        let proposals = match service.scan() {
            Some(Some(scan)) => Some(scan.plan.proposals.len()),
            _ => None,
        };

        let (mark, title, detail): (AnyElement, SharedString, Option<SharedString>) =
            match (service.offline(), service.status()) {
                (Some(err), _) => (
                    ui::status_mark(Lucide::ServerOff, theme.danger).into_any_element(),
                    "Servizio WDU non raggiungibile".into(),
                    Some(err.to_string().into()),
                ),
                (None, None) => (
                    working_mark(theme.muted_foreground).into_any_element(),
                    "Connessione al servizio…".into(),
                    None,
                ),
                (None, Some(status)) if status.state != ServiceState::Idle => (
                    working_mark(theme.primary).into_any_element(),
                    text::state(status.state).into(),
                    status.current.clone().map(Into::into),
                ),
                (None, Some(_)) => match proposals {
                    Some(0) => (
                        ui::status_mark(Lucide::ShieldCheck, theme.success).into_any_element(),
                        "I driver sono aggiornati".into(),
                        None,
                    ),
                    Some(1) => (
                        ui::status_mark(Lucide::Download, theme.primary).into_any_element(),
                        "1 aggiornamento disponibile".into(),
                        None,
                    ),
                    Some(n) => (
                        ui::status_mark(Lucide::Download, theme.primary).into_any_element(),
                        format!("{n} aggiornamenti disponibili").into(),
                        None,
                    ),
                    None => (
                        ui::status_mark(IconName::Search, theme.muted_foreground)
                            .into_any_element(),
                        "Nessuna ricerca eseguita".into(),
                        Some(
                            "Cerca gli aggiornamenti per confrontare i driver installati con \
                             Windows Update e il catalogo del produttore."
                                .into(),
                        ),
                    ),
                },
            };

        let status = service.status();
        let schedule = status.map(|s| {
            let last = match s.last_scan.as_deref() {
                Some(at) => text::when(Some(at)),
                None => "mai".into(),
            };
            let next = match s.next_scan.as_deref() {
                Some(at) => text::when(Some(at)),
                None => "disattivata".into(),
            };
            format!("Ultima ricerca {last} · Prossima {next}")
        });
        let scanning = service.state() == Some(ServiceState::Scanning);
        let steps: Vec<AnyElement> = if scanning {
            status
                .map(|s| {
                    s.scan_steps
                        .iter()
                        .enumerate()
                        .map(|(i, step)| {
                            ui::scan_step(step, service.step_elapsed(i), cx).into_any_element()
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let n = self.selected.len();
        let install_label: SharedString = if n > 0 {
            format!("Installa ({n})…").into()
        } else {
            "Installa…".into()
        };

        v_flex()
            .w_full()
            .p_5()
            .gap_4()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.group_box)
            .child(
                h_flex()
                    .w_full()
                    .gap_4()
                    .child(mark)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(div().text_lg().font_semibold().child(title))
                            .when_some(detail, |this, detail| this.child(ui::muted(detail, cx)))
                            .when_some(schedule, |this, schedule| {
                                this.child(ui::muted(schedule, cx))
                            }),
                    )
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .gap_2()
                            .child(
                                Button::new("scan")
                                    .icon(IconName::Search)
                                    .label("Cerca aggiornamenti")
                                    .loading(scanning)
                                    .disabled(!online || busy)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.scan(window, cx)),
                                    ),
                            )
                            .child(
                                Button::new("install")
                                    .primary()
                                    .icon(Lucide::Download)
                                    .label(install_label)
                                    .loading(self.installing)
                                    .disabled(!online || busy || n == 0)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.confirm_install(window, cx)
                                    })),
                            ),
                    ),
            )
            .when(!steps.is_empty(), |this| {
                this.child(
                    v_flex()
                        .pt_3()
                        .gap_1()
                        .border_t_1()
                        .border_color(theme.border)
                        .children(steps),
                )
            })
    }

    /// What went wrong in the last operation or scan, when something did.
    fn render_problems(&self, cx: &App) -> Vec<AnyElement> {
        let service = self.service.read(cx);
        let mut alerts = Vec::new();
        if let Some(status) = service.status() {
            if let Some(err) = &status.last_error {
                alerts.push(
                    Alert::error("last-error", err.clone())
                        .title("L'ultima operazione non è riuscita")
                        .into_any_element(),
                );
            }
            if !status.warnings.is_empty() {
                alerts.push(
                    Alert::warning("warnings", status.warnings.join("\n"))
                        .title("Avvisi")
                        .into_any_element(),
                );
            }
        }
        if let Some(Some(scan)) = service.scan() {
            let mut lines: Vec<String> = scan
                .source_errors
                .iter()
                .map(|e| {
                    format!(
                        "{} non disponibile: {}",
                        text::source_name(&e.source),
                        e.error
                    )
                })
                .collect();
            lines.extend(
                scan.wu_policy
                    .findings
                    .iter()
                    .map(|f| format!("Policy: {f}")),
            );
            if !lines.is_empty() {
                alerts.push(
                    Alert::warning("scan-incomplete", lines.join("\n"))
                        .title("La ricerca potrebbe essere incompleta")
                        .into_any_element(),
                );
            }
        }
        alerts
    }

    fn render_proposals(&self, scan: &ScanResult, cx: &Context<Self>) -> impl IntoElement {
        let proposals = &scan.plan.proposals;
        let all = !proposals.is_empty()
            && proposals
                .iter()
                .all(|p| self.selected.contains(&p.candidate.id));
        let busy = self.service.read(cx).is_busy();

        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .justify_between()
                    .child(ui::section_title("Aggiornamenti proposti"))
                    .when(!proposals.is_empty(), |this| {
                        this.child(
                            Button::new("select-all")
                                .ghost()
                                .small()
                                .label(if all {
                                    "Deseleziona tutti"
                                } else {
                                    "Seleziona tutti"
                                })
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let proposed = this
                                        .service
                                        .read(cx)
                                        .scan()
                                        .flatten()
                                        .map(|s| {
                                            s.plan
                                                .proposals
                                                .iter()
                                                .map(|p| p.candidate.id.clone())
                                                .collect::<Vec<_>>()
                                        })
                                        .unwrap_or_default();
                                    if all {
                                        this.selected.clear();
                                    } else {
                                        this.selected.extend(proposed);
                                    }
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(if proposals.is_empty() {
                div()
                    .rounded(cx.theme().radius_lg)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(ui::empty_state(
                        Lucide::PackageCheck,
                        "Nessun aggiornamento da installare",
                        Some(
                            "Ogni driver è già alla versione più recente offerta dai cataloghi."
                                .into(),
                        ),
                        cx,
                    ))
                    .into_any_element()
            } else {
                v_flex()
                    .rounded(cx.theme().radius_lg)
                    .border_1()
                    .border_color(cx.theme().border)
                    .overflow_hidden()
                    .children(
                        proposals
                            .iter()
                            .enumerate()
                            .map(|(i, p)| self.render_proposal(p, i == 0, busy, cx)),
                    )
                    .into_any_element()
            })
    }

    fn render_proposal(
        &self,
        proposal: &Proposal,
        first: bool,
        busy: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let c = &proposal.candidate;
        let id = c.id.clone();
        let checked = self.selected.contains(&c.id);
        let theme = cx.theme();

        v_flex()
            .id(SharedString::from(format!("proposal-{}", c.id)))
            .px_4()
            .py_3()
            .gap_1p5()
            .when(!first, |this| this.border_t_1().border_color(theme.border))
            .when(checked, |this| this.bg(theme.accent.opacity(0.5)))
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        Checkbox::new(SharedString::from(format!("select-{}", c.id)))
                            .label(c.title.clone())
                            .checked(checked)
                            .disabled(busy)
                            .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                                this.toggle(&id, *checked, cx)
                            })),
                    )
                    .child(div().flex_1())
                    .when_some(c.driver_version.clone(), |this, version| {
                        this.child(
                            div()
                                .text_sm()
                                .font_family(theme.mono_font_family.clone())
                                .text_color(theme.muted_foreground)
                                .child(version),
                        )
                    })
                    .child(Tag::secondary().small().child(text::source(c.source))),
            )
            .children(proposal.devices.iter().map(|d| {
                let name = d.name.as_deref().unwrap_or(&d.instance_id);
                let from = match &d.installed_version {
                    Some(v) => format!(
                        "attuale {v} ({})",
                        d.installed_date.as_deref().map_or("?".into(), text::day)
                    ),
                    None => "nessun driver installato".into(),
                };
                h_flex()
                    .pl_7()
                    .gap_2()
                    .text_sm()
                    .child(Lucide::Usb)
                    .child(div().min_w_0().truncate().child(name.to_string()))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(theme.muted_foreground)
                            .child(format!("{from} · {}", text::reason(d.reason))),
                    )
            }))
    }

    fn render_details(&self, scan: Option<&ScanResult>, cx: &App) -> Option<impl IntoElement> {
        let service = self.service.read(cx);
        let steps = service
            .status()
            .filter(|s| s.state != ServiceState::Scanning)
            .map(|s| s.scan_steps.as_slice())
            .unwrap_or_default();
        let skipped = scan.map(|s| s.plan.skipped.as_slice()).unwrap_or_default();
        if steps.is_empty() && skipped.is_empty() {
            return None;
        }

        let mut accordion = Accordion::new("scan-details").multiple(true);
        if !steps.is_empty() {
            let rows: Vec<AnyElement> = steps
                .iter()
                .map(|step| ui::scan_step(step, None, cx).into_any_element())
                .collect();
            accordion = accordion.item(|item| {
                item.icon(IconName::Info)
                    .title("Dettagli dell'ultima ricerca")
                    .child(v_flex().gap_1().children(rows))
            });
        }
        if !skipped.is_empty() {
            let mut counts = BTreeMap::new();
            for s in skipped {
                *counts.entry(text::skip_kind(&s.reason)).or_insert(0) += 1;
            }
            let summary = counts
                .into_iter()
                .map(|(reason, n)| format!("{n} {reason}"))
                .collect::<Vec<_>>()
                .join(" · ");
            let table = Table::new()
                .small()
                .child(
                    TableHeader::new().child(
                        TableRow::new()
                            .child(TableHead::new().child("Fonte"))
                            .child(TableHead::new().child("Aggiornamento"))
                            .child(TableHead::new().child("Dispositivo"))
                            .child(TableHead::new().child("Motivo")),
                    ),
                )
                .child(TableBody::new().children(skipped.iter().map(|s| {
                    TableRow::new()
                        .child(TableCell::new().child(text::source(s.source)))
                        .child(TableCell::new().child(s.title.clone()))
                        .child(TableCell::new().child(s.device.clone().unwrap_or("–".into())))
                        .child(TableCell::new().child(text::skip(&s.reason)))
                })));
            let title = format!("Scartati · {}", skipped.len());
            accordion = accordion.item(|item| {
                item.icon(IconName::Ban)
                    .title(title)
                    .child(v_flex().gap_3().child(ui::muted(summary, cx)).child(table))
            });
        }
        Some(accordion)
    }
}

impl Render for UpdatesPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cx = &*cx;
        let scan = self.service.read(cx).scan().flatten();
        div()
            .id("updates")
            .size_full()
            .overflow_y_scrollbar()
            .child(
                v_flex()
                    .p_6()
                    .gap_5()
                    .child(PageHeader::new("Aggiornamenti"))
                    .child(self.render_summary(cx))
                    .children(self.render_problems(cx))
                    .children(scan.map(|scan| self.render_proposals(scan, cx)))
                    .children(self.render_details(scan, cx)),
            )
    }
}

/// The status mark while the service works.
fn working_mark(color: Hsla) -> impl IntoElement {
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size_12()
        .rounded_full()
        .bg(color.opacity(0.14))
        .child(Spinner::new().color(color))
}
