//! The WDU window. Every service call runs on its own thread, since a call
//! may wait for the pipe; replies come back through a channel and wake the
//! window. Strings from catalogs are untrusted, but egui only ever draws them
//! as text.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Button, Color32, Label, Layout, RichText, TextEdit, Ui};
use egui_extras::{Column, TableBuilder};
use lucide_icons::Icon;
use wdu_core::driver_files::{self, DriverFile};
use wdu_core::driver_store::{self, DriverPackage, PackageUse};
use wdu_core::engine::ScanResult;
use wdu_core::history::{HistoryRecord, Outcome};
use wdu_core::ipc::{self, Request, Response, ServiceConfig, ServiceState, ServiceStatus};
use wdu_core::model::Device;

use crate::{shell, text};

const POLL: Duration = Duration::from_secs(2);
const OK: Color32 = Color32::from_rgb(0x2e, 0x9e, 0x4f);
/// Width of one icon button in a table's leading column of actions.
const ACTION_WIDTH: f32 = 24.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Updates,
    Drivers,
    Packages,
    History,
    Settings,
}

/// What a reply answers. At most one call of each kind is in flight.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Call {
    Status,
    Plan,
    History,
    Devices,
    Packages,
    GetConfig,
    SetConfig,
    Scan,
    Install,
    Remove,
}

struct Reply {
    call: Call,
    result: Result<Response, String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Name,
    Driver,
    Class,
    Version,
    Date,
    Provider,
    Origin,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PackageSort {
    Name,
    Class,
    Version,
    Date,
    Provider,
    Size,
    Usage,
}

/// One line of the driver packages table.
#[derive(Clone, Copy)]
enum PackageRow<'a> {
    Newest {
        package: &'a DriverPackage,
        older: usize,
        open: bool,
    },
    Older {
        package: &'a DriverPackage,
        last: bool,
    },
}

/// The settings form, editing a copy of the service's config.
struct Settings {
    config: ServiceConfig,
    deny: String,
    allow: String,
}

impl Settings {
    fn new(config: ServiceConfig) -> Self {
        Settings {
            deny: config.policy.deny_classes.join(", "),
            allow: config.policy.allow_classes.join(", "),
            config,
        }
    }

    fn to_config(&self) -> ServiceConfig {
        let classes = |text: &str| {
            text.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        };
        let mut config = self.config.clone();
        config.policy.deny_classes = classes(&self.deny);
        config.policy.allow_classes = classes(&self.allow);
        config
    }
}

/// The driver details window. The files are read on a thread of their own:
/// with the signatures checked, that can take a moment.
struct DriverDetails {
    device: String,
    rx: Receiver<Result<Vec<DriverFile>, String>>,
    files: Option<Result<Vec<DriverFile>, String>>,
}

pub struct App {
    ctx: egui::Context,
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
    pending: HashSet<Call>,
    /// Requests made while drawing, sent once the frame is done.
    outbox: Vec<(Call, Request)>,
    last_poll: Instant,
    tab: Tab,

    status: Option<ServiceStatus>,
    offline: Option<String>,
    /// `None` until the first plan arrives; then the last scan, if any.
    scan: Option<Option<Box<ScanResult>>>,
    selected: HashSet<String>,
    history: Vec<HistoryRecord>,

    devices: Option<Vec<Device>>,
    devices_error: Option<String>,
    filter: String,
    show_hidden: bool,
    show_no_driver: bool,
    sort: SortKey,
    ascending: bool,

    packages: Option<Vec<DriverPackage>>,
    packages_error: Option<String>,
    package_filter: String,
    only_old: bool,
    package_sort: PackageSort,
    package_ascending: bool,
    /// Newest packages whose older versions are shown.
    expanded: HashSet<String>,
    /// Older versions marked for removal.
    to_remove: HashSet<String>,
    confirm_remove: bool,
    /// A removal was accepted and has not finished yet.
    removing: bool,

    settings: Option<Settings>,
    settings_note: String,

    confirm_install: bool,
    driver_details: Option<DriverDetails>,
    alert: Option<String>,
}

impl App {
    pub fn new(ctx: egui::Context) -> Self {
        // Lucide gets a family of its own: the font also maps letters and digits
        // (for ligatures), and its icons overlap egui's own icon font.
        ctx.add_font(egui::epaint::text::FontInsert::new(
            "lucide",
            egui::FontData::from_static(lucide_icons::LUCIDE_FONT_BYTES),
            vec![egui::epaint::text::InsertFontFamily {
                family: lucide(),
                priority: egui::epaint::text::FontPriority::Highest,
            }],
        ));
        let (tx, rx) = channel();
        let mut app = App {
            ctx,
            tx,
            rx,
            pending: HashSet::new(),
            outbox: Vec::new(),
            last_poll: Instant::now(),
            tab: Tab::Updates,
            status: None,
            offline: None,
            scan: None,
            selected: HashSet::new(),
            history: Vec::new(),
            devices: None,
            devices_error: None,
            filter: String::new(),
            show_hidden: false,
            show_no_driver: false,
            sort: SortKey::Name,
            ascending: true,
            packages: None,
            packages_error: None,
            package_filter: String::new(),
            only_old: false,
            package_sort: PackageSort::Name,
            package_ascending: true,
            expanded: HashSet::new(),
            to_remove: HashSet::new(),
            confirm_remove: false,
            removing: false,
            settings: None,
            settings_note: String::new(),
            confirm_install: false,
            driver_details: None,
            alert: None,
        };
        app.send(Call::Status, Request::Status);
        app.send(Call::Plan, Request::Plan);
        app.send(Call::History, Request::History);
        app
    }

    /// Forwards one request to the service, off the UI thread.
    fn send(&mut self, call: Call, request: Request) {
        if !self.pending.insert(call) {
            return;
        }
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let result = match ipc::call(&request) {
                Ok(Response::Error { message }) => Err(message),
                Ok(response) => Ok(response),
                Err(err) => Err(format!("{err:#}")),
            };
            let _ = tx.send(Reply { call, result });
            ctx.request_repaint();
        });
    }

    fn handle(&mut self, Reply { call, result }: Reply) {
        self.pending.remove(&call);
        match (call, result) {
            (Call::Status, Ok(Response::Status { status })) => {
                self.offline = None;
                // A scan or installation just ended: reload what it changed.
                let was_busy = self
                    .status
                    .as_ref()
                    .is_some_and(|s| s.state != ServiceState::Idle);
                if was_busy && status.state == ServiceState::Idle {
                    self.send(Call::Plan, Request::Plan);
                    self.send(Call::History, Request::History);
                    if self.devices.is_some() {
                        self.send(Call::Devices, Request::Devices);
                    }
                    if self.packages.is_some() {
                        self.send(Call::Packages, Request::DriverPackages);
                    }
                    if std::mem::take(&mut self.removing)
                        && let Some(err) = &status.last_error
                    {
                        self.alert = Some(format!("Non tutti i driver sono stati rimossi: {err}"));
                    }
                }
                self.status = Some(status);
            }
            (Call::Status, Err(err)) => {
                self.offline = Some(format!("Servizio WDU non raggiungibile: {err}"));
            }
            (Call::Plan, Ok(Response::Plan { scan })) => {
                self.selected.retain(|id| {
                    scan.as_ref()
                        .is_some_and(|s| s.plan.proposals.iter().any(|p| &p.candidate.id == id))
                });
                self.scan = Some(scan);
            }
            (Call::History, Ok(Response::History { records })) => self.history = records,
            (Call::Devices, Ok(Response::Devices { devices })) => {
                self.devices = Some(devices);
                self.devices_error = None;
            }
            (Call::Devices, Err(err)) => {
                self.devices_error = Some(format!("Impossibile leggere i dispositivi: {err}"));
            }
            (Call::Packages, Ok(Response::DriverPackages { packages })) => {
                self.to_remove.retain(|name| {
                    packages
                        .iter()
                        .any(|p| &p.published_name == name && p.is_old_unused())
                });
                self.packages = Some(packages);
                self.packages_error = None;
            }
            (Call::Packages, Err(err)) => {
                self.packages_error = Some(format!("Impossibile leggere i driver: {err}"));
            }
            (Call::GetConfig, Ok(Response::Config { config })) => {
                self.settings = Some(Settings::new(config));
            }
            (Call::SetConfig, Ok(_)) => {
                self.settings_note = "Salvato.".into();
                self.send(Call::Status, Request::Status);
            }
            (Call::SetConfig, Err(err)) => self.settings_note = format!("Errore: {err}"),
            (Call::Scan | Call::Install | Call::Remove, result) => {
                match result {
                    Err(err) => self.alert = Some(err),
                    Ok(_) if call == Call::Install => self.selected.clear(),
                    Ok(_) if call == Call::Remove => {
                        self.to_remove.clear();
                        self.removing = true;
                    }
                    Ok(_) => {}
                }
                self.send(Call::Status, Request::Status);
            }
            // Failed reloads keep what is shown; the status banner tells why.
            _ => {}
        }
    }

    fn open(&mut self, tab: Tab) {
        self.tab = tab;
        match tab {
            Tab::Updates => {}
            Tab::Drivers => {
                self.send(Call::Devices, Request::Devices);
                self.send(Call::History, Request::History);
            }
            Tab::Packages => self.send(Call::Packages, Request::DriverPackages),
            Tab::History => self.send(Call::History, Request::History),
            Tab::Settings => {
                self.settings_note.clear();
                self.send(Call::GetConfig, Request::GetConfig);
            }
        }
    }

    fn open_driver_details(&mut self, instance_id: String, device: String) {
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ =
                tx.send(driver_files::driver_files(&instance_id).map_err(|err| format!("{err:#}")));
            ctx.request_repaint();
        });
        self.driver_details = Some(DriverDetails {
            device,
            rx,
            files: None,
        });
    }

    fn busy(&self) -> bool {
        self.status
            .as_ref()
            .is_none_or(|s| s.state != ServiceState::Idle)
    }

    // --- layout ---------------------------------------------------------------

    fn header(&mut self, ui: &mut Ui) {
        ui.add_space(6.0);
        ui.heading("Aggiornamento driver");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            for (tab, glyph, label) in [
                (Tab::Updates, Icon::Download, "Aggiornamenti"),
                (Tab::Drivers, Icon::Cpu, "Dispositivi"),
                (Tab::Packages, Icon::Package, "Driver installati"),
                (Tab::History, Icon::History, "Storico"),
                (Tab::Settings, Icon::Settings, "Impostazioni"),
            ] {
                if ui
                    .selectable_label(self.tab == tab, (icon(glyph), label))
                    .clicked()
                    && self.tab != tab
                {
                    self.open(tab);
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (theme, glyph, tip) = match ui.ctx().theme() {
                    egui::Theme::Dark => (egui::Theme::Light, Icon::Sun, "Tema chiaro"),
                    egui::Theme::Light => (egui::Theme::Dark, Icon::Moon, "Tema scuro"),
                };
                if ui
                    .add(Button::new(icon(glyph)).frame(false))
                    .on_hover_text(tip)
                    .clicked()
                {
                    ui.ctx().set_theme(theme);
                }
            });
        });
        ui.add_space(4.0);
    }

    fn banners(&self, ui: &mut Ui) {
        if let Some(offline) = &self.offline {
            banner(ui, ui.visuals().error_fg_color, Icon::ServerOff, offline);
        }
        if self.status.as_ref().is_some_and(|s| s.reboot_required) {
            banner(
                ui,
                ui.visuals().warn_fg_color,
                Icon::RotateCcw,
                "Un'installazione richiede il riavvio. Riavvia il PC quando ti è comodo: \
                 WDU non riavvia mai da solo.",
            );
        }
    }

    // --- updates --------------------------------------------------------------

    fn updates(&mut self, ui: &mut Ui) {
        let busy = self.busy();
        let online = self.offline.is_none();

        ui.horizontal(|ui| match &self.status {
            Some(status) => {
                let color = if busy {
                    ui.visuals().hyperlink_color
                } else {
                    ui.visuals().text_color()
                };
                badge(ui, text::state(status.state), color);
                if let Some(current) = &status.current {
                    ui.label(format!("({current})"));
                }
            }
            None => badge(ui, "…", ui.visuals().text_color()),
        });
        ui.add_space(4.0);
        let status = self.status.as_ref();
        egui::Grid::new("scan-times").num_columns(2).show(ui, |ui| {
            ui.weak("Ultima ricerca");
            ui.label(match status.and_then(|s| s.last_scan.as_deref()) {
                Some(at) => text::when(Some(at)),
                None => "mai".into(),
            });
            ui.end_row();
            ui.weak("Prossima ricerca");
            ui.label(match status {
                None => "–".into(),
                Some(s) if s.next_scan.is_none() => "disattivata".into(),
                Some(s) => text::when(s.next_scan.as_deref()),
            });
            ui.end_row();
        });
        ui.add_space(4.0);

        let n = self.selected.len();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    online && !busy,
                    Button::new((icon(Icon::Search), "Cerca aggiornamenti")),
                )
                .clicked()
            {
                self.outbox.push((Call::Scan, Request::Scan));
            }
            let label = if n > 0 {
                format!("Installa selezionati ({n})")
            } else {
                "Installa selezionati".into()
            };
            if ui
                .add_enabled(
                    online && !busy && n > 0,
                    Button::new((icon(Icon::Download), label)),
                )
                .clicked()
            {
                self.confirm_install = true;
            }
        });
        ui.add_space(6.0);

        if let Some(status) = &self.status {
            if let Some(err) = &status.last_error {
                banner(ui, ui.visuals().error_fg_color, Icon::CircleAlert, err);
            }
            for warning in &status.warnings {
                ui.label(format!("• {warning}"));
            }
        }
        if let Some(Some(scan)) = &self.scan {
            for finding in &scan.wu_policy.findings {
                ui.label(format!("• Policy: {finding}"));
            }
            for e in &scan.source_errors {
                ui.label(format!(
                    "• {} non disponibile: {}",
                    text::source_name(&e.source),
                    e.error
                ));
            }
        }

        ui.add_space(8.0);
        ui.heading("Aggiornamenti proposti");
        ui.add_space(4.0);
        let Some(Some(scan)) = &self.scan else {
            ui.weak("Nessuna ricerca eseguita.");
            return;
        };
        if scan.plan.proposals.is_empty() {
            ui.weak("Tutti i driver sono aggiornati.");
        }
        for p in &scan.plan.proposals {
            let c = &p.candidate;
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    let mut checked = self.selected.contains(&c.id);
                    if ui
                        .checkbox(&mut checked, RichText::new(&c.title).strong())
                        .changed()
                    {
                        if checked {
                            self.selected.insert(c.id.clone());
                        } else {
                            self.selected.remove(&c.id);
                        }
                    }
                    badge(ui, text::source(c.source), ui.visuals().text_color());
                    if let Some(version) = &c.driver_version {
                        ui.weak(version);
                    }
                });
                ui.indent(&c.id, |ui| {
                    for d in &p.devices {
                        let from = match &d.installed_version {
                            Some(v) => {
                                format!("{v} ({})", d.installed_date.as_deref().unwrap_or("?"))
                            }
                            None => "nessun driver".into(),
                        };
                        let name = d.name.as_deref().unwrap_or(&d.instance_id);
                        ui.label(format!(
                            "{name} — attuale {from}: {}",
                            text::reason(d.reason)
                        ));
                    }
                });
            });
        }

        let skipped = &scan.plan.skipped;
        if skipped.is_empty() {
            return;
        }
        ui.add_space(8.0);
        egui::CollapsingHeader::new(format!("Scartati: {}", skipped.len()))
            .id_salt("skipped")
            .show(ui, |ui| {
                let mut counts = BTreeMap::new();
                for s in skipped {
                    *counts.entry(text::skip_kind(&s.reason)).or_insert(0) += 1;
                }
                for (reason, n) in counts {
                    ui.label(format!("{n} — {reason}"));
                }
                ui.add_space(6.0);
                TableBuilder::new(ui)
                    .id_salt("skipped-table")
                    .vscroll(false)
                    .striped(true)
                    .resizable(true)
                    .column(Column::initial(130.0).clip(true))
                    .column(Column::initial(320.0).clip(true))
                    .column(Column::initial(220.0).clip(true))
                    .column(Column::remainder().clip(true).resizable(false))
                    .header(22.0, |mut row| {
                        for title in ["Fonte", "Aggiornamento", "Dispositivo", "Motivo"] {
                            row.col(|ui| {
                                ui.strong(title);
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(22.0, skipped.len(), |mut row| {
                            let s = &skipped[row.index()];
                            row.col(|ui| cell(ui, text::source(s.source)));
                            row.col(|ui| cell(ui, &s.title));
                            row.col(|ui| cell(ui, s.device.as_deref().unwrap_or("–")));
                            row.col(|ui| cell(ui, &text::skip(&s.reason)));
                        });
                    });
            });
    }

    // --- installed drivers ----------------------------------------------------

    fn drivers(&mut self, ui: &mut Ui) {
        // Instance ID (lowercase) → last successful WDU installation for that device.
        let mut by_wdu = HashMap::new();
        for r in &self.history {
            if r.outcome == Outcome::Succeeded {
                for id in &r.devices {
                    by_wdu.insert(id.to_lowercase(), r);
                }
            }
        }

        let query = self.filter.trim().to_lowercase();
        let mut rows: Vec<(&Device, String)> = Vec::new();
        if let Some(devices) = &self.devices {
            for d in devices {
                if (!self.show_hidden && d.status.hidden)
                    || (!self.show_no_driver && d.driver.is_none())
                {
                    continue;
                }
                let driver = d.driver.as_ref();
                let matches = query.is_empty()
                    || [
                        d.name.as_deref(),
                        d.class.as_deref(),
                        d.manufacturer.as_deref(),
                        driver.and_then(|v| v.description.as_deref()),
                        driver.and_then(|v| v.provider.as_deref()),
                        driver.and_then(|v| v.version.as_deref()),
                        driver.and_then(|v| v.inf_name.as_deref()),
                        Some(d.instance_id.as_str()),
                    ]
                    .into_iter()
                    .flatten()
                    .any(|v| v.to_lowercase().contains(&query));
                if matches {
                    rows.push((d, origin(d, &by_wdu)));
                }
            }
        }
        let (key, ascending) = (self.sort, self.ascending);
        rows.sort_by(|a, b| {
            let order = compare(key, a, b);
            if ascending { order } else { order.reverse() }
        });

        ui.horizontal(|ui| {
            ui.add(
                TextEdit::singleline(&mut self.filter)
                    .hint_text("Cerca dispositivo, classe, fornitore…")
                    .desired_width(260.0),
            );
            ui.checkbox(&mut self.show_hidden, "Mostra dispositivi nascosti");
            ui.checkbox(&mut self.show_no_driver, "Mostra dispositivi senza driver");
            if let Some(devices) = &self.devices {
                ui.weak(format!("{} di {} dispositivi", rows.len(), devices.len()));
            }
            if ui.button((icon(Icon::RefreshCw), "Aggiorna")).clicked() {
                self.outbox.push((Call::Devices, Request::Devices));
                self.outbox.push((Call::History, Request::History));
            }
        });
        ui.add_space(6.0);

        if let Some(err) = &self.devices_error {
            ui.colored_label(ui.visuals().error_fg_color, err);
        }
        if self.devices.is_none() {
            if self.devices_error.is_none() {
                ui.weak("Lettura dei dispositivi…");
            }
            return;
        }
        if rows.is_empty() {
            ui.weak("Nessun dispositivo corrisponde ai filtri.");
            return;
        }

        // oemNN.inf → INF name the vendor shipped, when the driver store is loaded.
        let original: HashMap<String, &str> = self
            .packages
            .iter()
            .flatten()
            .filter_map(|p| {
                Some((p.published_name.to_lowercase(), p.original_name.as_deref()?))
            })
            .collect();

        let sort = &mut self.sort;
        let asc = &mut self.ascending;
        let alert = &mut self.alert;
        let mut details = None;
        // Auto columns take the width of their content on the first frame,
        // then keep it; the remainder column goes last: as the first one, it
        // soaks up every drag of the columns after it and resizing looks inverted.
        TableBuilder::new(ui)
            .id_salt("devices-table")
            .striped(true)
            .resizable(true)
            .auto_shrink(false)
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::exact(2.0 * ACTION_WIDTH).resizable(false))
            .column(Column::auto().at_least(120.0).at_most(460.0).clip(true))
            .column(Column::auto().at_least(80.0).at_most(420.0).clip(true))
            .column(Column::auto().at_least(60.0).at_most(200.0).clip(true))
            .column(Column::auto().at_least(60.0).at_most(200.0).clip(true))
            .column(Column::auto().at_least(60.0).at_most(120.0).clip(true))
            .column(Column::auto().at_least(60.0).at_most(260.0).clip(true))
            .column(
                Column::remainder()
                    .at_least(160.0)
                    .clip(true)
                    .resizable(false),
            )
            .header(24.0, |mut row| {
                row.col(|_| {});
                for (k, title) in [
                    (SortKey::Name, "Dispositivo"),
                    (SortKey::Driver, "Driver"),
                    (SortKey::Class, "Classe"),
                    (SortKey::Version, "Versione"),
                    (SortKey::Date, "Data"),
                    (SortKey::Provider, "Fornitore"),
                    (SortKey::Origin, "Origine"),
                ] {
                    row.col(|ui| {
                        let title = RichText::new(title).strong();
                        let header = match (*sort == k, *asc) {
                            (true, true) => Button::new((title, icon(Icon::ChevronUp))),
                            (true, false) => Button::new((title, icon(Icon::ChevronDown))),
                            _ => Button::new(title),
                        }
                        .frame(false);
                        if ui.add(header).clicked() {
                            *asc = *sort != k || !*asc;
                            *sort = k;
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(38.0, rows.len(), |mut row| {
                    let (d, origin) = &rows[row.index()];
                    let driver = d.driver.as_ref();
                    row.col(|ui| {
                        if row_button(ui, Icon::SquareArrowOutUpRight, "Proprietà del dispositivo")
                            && let Err(err) = shell::device_properties(&d.instance_id)
                        {
                            *alert = Some(err);
                        }
                        let clicked = ui
                            .add_enabled_ui(driver.is_some(), |ui| {
                                row_button(ui, Icon::Files, "Dettagli driver")
                            })
                            .inner;
                        if clicked {
                            let name = d.name.as_deref().unwrap_or(&d.instance_id);
                            details = Some((d.instance_id.clone(), name.to_string()));
                        }
                    });
                    row.col(|ui| {
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                if d.status.problem_code != 0 {
                                    badge(
                                        ui,
                                        format!("problema {}", d.status.problem_code),
                                        ui.visuals().error_fg_color,
                                    );
                                }
                                cell(ui, d.name.as_deref().unwrap_or(&d.instance_id));
                            });
                            if let Some(m) = &d.manufacturer {
                                sub(ui, m);
                            }
                        });
                    });
                    row.col(|ui| {
                        ui.vertical(|ui| {
                            let Some(v) = driver else {
                                cell(ui, "Nessun driver");
                                return;
                            };
                            cell(ui, v.description.as_deref().unwrap_or("–"));
                            if let Some(inf) = v.inf_name.as_deref() {
                                match original.get(&inf.to_lowercase()) {
                                    Some(name) => sub(ui, &format!("{name} ({inf})")),
                                    None => sub(ui, inf),
                                }
                            }
                        });
                    });
                    row.col(|ui| cell(ui, d.class.as_deref().unwrap_or("–")));
                    row.col(|ui| {
                        cell(ui, driver.and_then(|v| v.version.as_deref()).unwrap_or("–"))
                    });
                    row.col(|ui| {
                        let date = driver.and_then(|v| v.date.as_deref());
                        cell(ui, &date.map_or("–".into(), text::day));
                    });
                    row.col(|ui| {
                        cell(
                            ui,
                            driver.and_then(|v| v.provider.as_deref()).unwrap_or("–"),
                        )
                    });
                    row.col(|ui| cell(ui, origin));
                });
            });
        if let Some((instance_id, device)) = details {
            self.open_driver_details(instance_id, device);
        }
    }

    // --- driver packages ------------------------------------------------------

    fn packages(&mut self, ui: &mut Ui) {
        let can_remove = self.offline.is_none() && !self.busy();
        let query = self.package_filter.trim().to_lowercase();
        let matches = |p: &DriverPackage| {
            query.is_empty()
                || [
                    Some(p.published_name.as_str()),
                    p.original_name.as_deref(),
                    p.provider.as_deref(),
                    p.class.as_deref(),
                    p.version.as_deref(),
                ]
                .into_iter()
                .flatten()
                .chain(p.devices.iter().filter_map(|d| d.name.as_deref()))
                .any(|v| v.to_lowercase().contains(&query))
        };

        // Older versions hang under the newest package of the same driver.
        let mut groups: Vec<(&DriverPackage, Vec<&DriverPackage>)> = Vec::new();
        let mut old: Vec<&str> = Vec::new();
        let mut old_size = 0;
        if let Some(packages) = &self.packages {
            let mut older: HashMap<&str, Vec<&DriverPackage>> = HashMap::new();
            for p in packages {
                if p.is_old_unused() {
                    old.push(&p.published_name);
                    old_size += p.size;
                }
                if let Some(by) = &p.superseded_by {
                    older.entry(by.as_str()).or_default().push(p);
                }
            }
            for p in packages.iter().filter(|p| p.superseded_by.is_none()) {
                let mut versions = older.remove(p.published_name.as_str()).unwrap_or_default();
                versions.sort_by(|a, b| {
                    version_key(b.version.clone()).cmp(&version_key(a.version.clone()))
                });
                groups.push((p, versions));
            }
        }
        groups.retain(|(newest, older)| {
            (!self.only_old || older.iter().any(|p| p.is_old_unused()))
                && (matches(newest) || older.iter().any(|p| matches(p)))
        });
        let (key, ascending) = (self.package_sort, self.package_ascending);
        groups.sort_by(|a, b| {
            let order = compare_packages(key, a.0, b.0);
            if ascending { order } else { order.reverse() }
        });

        // A search or the old-versions filter shows every group open.
        let force_open = self.only_old || !query.is_empty();
        let mut rows: Vec<PackageRow> = Vec::new();
        for (newest, older) in &groups {
            let open =
                !older.is_empty() && (force_open || self.expanded.contains(&newest.published_name));
            rows.push(PackageRow::Newest {
                package: newest,
                older: older.len(),
                open,
            });
            if open {
                for (i, p) in older.iter().enumerate() {
                    rows.push(PackageRow::Older {
                        package: p,
                        last: i + 1 == older.len(),
                    });
                }
            }
        }

        ui.horizontal(|ui| {
            ui.add(
                TextEdit::singleline(&mut self.package_filter)
                    .hint_text("Cerca driver, fornitore, dispositivo…")
                    .desired_width(260.0),
            );
            ui.checkbox(&mut self.only_old, "Solo versioni vecchie non usate");
            if let Some(packages) = &self.packages {
                ui.weak(format!(
                    "{} driver, {} pacchetti",
                    groups.len(),
                    packages.len()
                ));
            }
            if ui.button((icon(Icon::RefreshCw), "Aggiorna")).clicked() {
                self.outbox.push((Call::Packages, Request::DriverPackages));
            }
        });
        ui.add_space(6.0);

        if let Some(err) = &self.packages_error {
            ui.colored_label(ui.visuals().error_fg_color, err);
        }
        let Some(packages) = &self.packages else {
            if self.packages_error.is_none() {
                ui.weak("Lettura del driver store…");
            }
            return;
        };

        if let Some(status) = self
            .status
            .as_ref()
            .filter(|s| s.state == ServiceState::Removing)
        {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(text::state(status.state));
                if let Some(current) = &status.current {
                    ui.weak(current);
                }
            });
            ui.add_space(6.0);
        } else if !old.is_empty() {
            let color = ui.visuals().warn_fg_color;
            egui::Frame::new()
                .fill(color.gamma_multiply(0.12))
                .stroke(egui::Stroke::new(1.0, color))
                .corner_radius(4)
                .inner_margin(10)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let n = old.len();
                    ui.horizontal(|ui| {
                        ui.label(icon(Icon::TriangleAlert).color(color));
                        ui.colored_label(
                            color,
                            RichText::new(format!(
                                "{n} version{} vecchi{} da rimuovere · {}",
                                if n == 1 { "e" } else { "i" },
                                if n == 1 { "a" } else { "e" },
                                text::size(old_size)
                            ))
                            .strong(),
                        );
                    });
                    ui.label(
                        "Per ognuna c'è un pacchetto più recente dello stesso driver e nessun \
                         dispositivo collegato la usa.",
                    );
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let all = old.iter().all(|name| self.to_remove.contains(*name));
                        let label = if all {
                            "Deseleziona tutte"
                        } else {
                            "Seleziona tutte"
                        };
                        if ui.button(label).clicked() {
                            if all {
                                self.to_remove.clear();
                            } else {
                                self.to_remove
                                    .extend(old.iter().map(|name| name.to_string()));
                                self.expanded.extend(
                                    groups
                                        .iter()
                                        .filter(|(_, older)| {
                                            older.iter().any(|p| p.is_old_unused())
                                        })
                                        .map(|(newest, _)| newest.published_name.clone()),
                                );
                            }
                        }
                        let selected = self.to_remove.len();
                        let label = if selected > 0 {
                            format!("Rimuovi selezionate ({selected})")
                        } else {
                            "Rimuovi selezionate".into()
                        };
                        if ui
                            .add_enabled(
                                can_remove && selected > 0,
                                Button::new((icon(Icon::Trash2), label)),
                            )
                            .clicked()
                        {
                            self.confirm_remove = true;
                        }
                    });
                });
            ui.add_space(6.0);
        }
        ui.weak(
            "Sono elencati i pacchetti aggiunti al driver store, non quelli integrati in Windows. \
             «Nessun dispositivo» non vuol dire inutilizzato: estensioni, filtri di rete, \
             stampanti e servizi non risultano come driver di un dispositivo.",
        );
        ui.add_space(6.0);
        if rows.is_empty() {
            ui.weak(if packages.is_empty() {
                "Nessun pacchetto nel driver store."
            } else {
                "Nessun pacchetto corrisponde ai filtri."
            });
            return;
        }

        let sort = &mut self.package_sort;
        let asc = &mut self.package_ascending;
        let expanded = &mut self.expanded;
        let to_remove = &mut self.to_remove;
        let alert = &mut self.alert;
        TableBuilder::new(ui)
            .id_salt("packages-table")
            .striped(true)
            .resizable(true)
            .auto_shrink(false)
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::exact(2.0 * ACTION_WIDTH).resizable(false))
            .column(Column::auto().at_least(160.0).at_most(480.0).clip(true))
            .column(Column::auto().at_least(60.0).at_most(200.0).clip(true))
            .column(Column::auto().at_least(60.0).at_most(200.0).clip(true))
            .column(Column::auto().at_least(60.0).at_most(120.0).clip(true))
            .column(Column::auto().at_least(60.0).at_most(260.0).clip(true))
            .column(Column::auto().at_least(60.0).at_most(120.0).clip(true))
            .column(
                Column::remainder()
                    .at_least(200.0)
                    .clip(true)
                    .resizable(false),
            )
            .header(24.0, |mut row| {
                row.col(|_| {});
                for (k, title) in [
                    (PackageSort::Name, "Driver"),
                    (PackageSort::Class, "Classe"),
                    (PackageSort::Version, "Versione"),
                    (PackageSort::Date, "Data"),
                    (PackageSort::Provider, "Fornitore"),
                    (PackageSort::Size, "Dimensione"),
                    (PackageSort::Usage, "Uso"),
                ] {
                    row.col(|ui| {
                        let title = RichText::new(title).strong();
                        let header = match (*sort == k, *asc) {
                            (true, true) => Button::new((title, icon(Icon::ChevronUp))),
                            (true, false) => Button::new((title, icon(Icon::ChevronDown))),
                            _ => Button::new(title),
                        }
                        .frame(false);
                        if ui.add(header).clicked() {
                            *asc = *sort != k || !*asc;
                            *sort = k;
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(38.0, rows.len(), |mut row| match rows[row.index()] {
                    PackageRow::Newest {
                        package: p,
                        older,
                        open,
                    } => {
                        row.col(|ui| {
                            if let Err(err) = package_actions(ui, p) {
                                *alert = Some(err);
                            }
                        });
                        row.col(|ui| {
                            if older > 0 {
                                let (_, toggle) = ui.allocate_exact_size(
                                    egui::vec2(16.0, 16.0),
                                    egui::Sense::click(),
                                );
                                egui::collapsing_header::paint_default_icon(
                                    ui,
                                    if open { 1.0 } else { 0.0 },
                                    &toggle,
                                );
                                let toggle = toggle.on_hover_cursor(egui::CursorIcon::PointingHand);
                                if toggle.clicked() && !force_open {
                                    if open {
                                        expanded.remove(&p.published_name);
                                    } else {
                                        expanded.insert(p.published_name.clone());
                                    }
                                }
                            } else {
                                ui.add_space(16.0 + ui.spacing().item_spacing.x);
                            }
                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    cell(
                                        ui,
                                        p.original_name.as_deref().unwrap_or(&p.published_name),
                                    );
                                    if older > 0 {
                                        let color = ui.visuals().warn_fg_color;
                                        badge(
                                            ui,
                                            format!(
                                                "+{older} version{}",
                                                if older == 1 { "e" } else { "i" }
                                            ),
                                            color,
                                        );
                                    }
                                });
                                sub(ui, &p.published_name);
                            });
                        });
                        row.col(|ui| cell(ui, p.class.as_deref().unwrap_or("–")));
                        row.col(|ui| cell(ui, p.version.as_deref().unwrap_or("–")));
                        row.col(|ui| cell(ui, &p.date.as_deref().map_or("–".into(), text::day)));
                        row.col(|ui| cell(ui, p.provider.as_deref().unwrap_or("–")));
                        row.col(|ui| cell(ui, &text::size(p.size)));
                        row.col(|ui| {
                            ui.vertical(|ui| {
                                cell(ui, &text::package_use(p));
                                let devices: Vec<&str> = p
                                    .devices
                                    .iter()
                                    .map(|d| d.name.as_deref().unwrap_or(&d.instance_id))
                                    .collect();
                                if !devices.is_empty() {
                                    sub(ui, &devices.join(", "));
                                }
                            });
                        });
                    }
                    PackageRow::Older { package: p, last } => {
                        let removable = p.is_old_unused();
                        let mut checked = to_remove.contains(&p.published_name);
                        row.set_selected(checked);
                        row.col(|ui| {
                            if let Err(err) = package_actions(ui, p) {
                                *alert = Some(err);
                            }
                        });
                        row.col(|ui| {
                            tree_branch(ui, last);
                            if removable {
                                if ui.checkbox(&mut checked, "").changed() {
                                    if checked {
                                        to_remove.insert(p.published_name.clone());
                                    } else {
                                        to_remove.remove(&p.published_name);
                                    }
                                }
                            } else {
                                ui.add_space(
                                    ui.spacing().interact_size.y + ui.spacing().item_spacing.x,
                                );
                            }
                            ui.vertical(|ui| {
                                weak_cell(
                                    ui,
                                    p.original_name.as_deref().unwrap_or(&p.published_name),
                                );
                                sub(ui, &p.published_name);
                            });
                        });
                        row.col(|ui| weak_cell(ui, p.class.as_deref().unwrap_or("–")));
                        row.col(|ui| weak_cell(ui, p.version.as_deref().unwrap_or("–")));
                        row.col(|ui| {
                            weak_cell(ui, &p.date.as_deref().map_or("–".into(), text::day))
                        });
                        row.col(|ui| weak_cell(ui, p.provider.as_deref().unwrap_or("–")));
                        row.col(|ui| weak_cell(ui, &text::size(p.size)));
                        row.col(|ui| {
                            ui.vertical(|ui| {
                                let color = if removable {
                                    ui.visuals().warn_fg_color
                                } else {
                                    ui.visuals().text_color()
                                };
                                badge(ui, "versione vecchia", color);
                                sub(ui, &text::package_use(p));
                            });
                        });
                    }
                });
            });
    }

    // --- history --------------------------------------------------------------

    fn history(&mut self, ui: &mut Ui) {
        if self.history.is_empty() {
            ui.weak("Nessuna installazione registrata.");
            return;
        }
        let records: Vec<&HistoryRecord> = self.history.iter().rev().collect();
        TableBuilder::new(ui)
            .id_salt("history-table")
            .striped(true)
            .resizable(true)
            .auto_shrink(false)
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::initial(150.0).clip(true))
            .column(Column::initial(120.0).clip(true))
            .column(Column::initial(320.0).clip(true))
            .column(Column::initial(220.0).clip(true))
            .column(Column::remainder().clip(true).resizable(false))
            .header(24.0, |mut row| {
                for title in ["Data", "Fonte", "Aggiornamento", "Esito", "Dettagli"] {
                    row.col(|ui| {
                        ui.strong(title);
                    });
                }
            })
            .body(|body| {
                body.rows(26.0, records.len(), |mut row| {
                    let r = records[row.index()];
                    row.col(|ui| cell(ui, &text::when(Some(&r.at))));
                    row.col(|ui| cell(ui, text::source(r.source)));
                    row.col(|ui| cell(ui, &r.title));
                    row.col(|ui| {
                        let (glyph, color) = match r.outcome {
                            Outcome::Succeeded => (Icon::CircleCheck, OK),
                            Outcome::Failed => (Icon::CircleX, ui.visuals().error_fg_color),
                            Outcome::RolledBack => (Icon::Undo2, ui.visuals().error_fg_color),
                        };
                        ui.label(icon(glyph).color(color));
                        badge(ui, text::outcome(r.outcome), color);
                        if r.reboot_required {
                            ui.weak("riavvio richiesto");
                        }
                    });
                    row.col(|ui| cell(ui, r.detail.as_deref().unwrap_or("")));
                });
            });
    }

    // --- settings -------------------------------------------------------------

    fn settings(&mut self, ui: &mut Ui) {
        let Some(form) = &mut self.settings else {
            ui.weak("Lettura delle impostazioni…");
            return;
        };
        let config = &mut form.config;
        ui.spacing_mut().item_spacing.y = 8.0;
        ui.horizontal(|ui| {
            ui.label("Controlla ogni");
            ui.add(egui::DragValue::new(&mut config.check_interval_hours).range(0..=744));
            ui.label("ore");
            ui.weak("0 disattiva i controlli automatici.");
        });
        ui.checkbox(
            &mut config.auto_install,
            "Installa automaticamente dopo i controlli pianificati, senza approvazione",
        );
        ui.checkbox(&mut config.windows_update, "Cerca su Windows Update");
        ui.checkbox(
            &mut config.oem_catalog,
            "Cerca nel catalogo del produttore (Dell, HP, Lenovo)",
        );
        ui.checkbox(
            &mut config.policy.oem_exclusive,
            "Se il produttore copre un dispositivo, ignora Windows Update per quel dispositivo",
        );
        egui::Grid::new("classes")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("Classi escluse");
                ui.add(
                    TextEdit::singleline(&mut form.deny)
                        .hint_text("es. Display, Bluetooth")
                        .desired_width(320.0),
                );
                ui.end_row();
                ui.label("Solo queste classi");
                ui.add(
                    TextEdit::singleline(&mut form.allow)
                        .hint_text("vuoto: tutte")
                        .desired_width(320.0),
                );
                ui.end_row();
            });
        ui.horizontal(|ui| {
            let saving = self.pending.contains(&Call::SetConfig);
            if ui
                .add_enabled(!saving, Button::new((icon(Icon::Save), "Salva")))
                .clicked()
            {
                let config = form.to_config();
                self.settings_note.clear();
                self.outbox
                    .push((Call::SetConfig, Request::SetConfig { config }));
            }
            ui.weak(&self.settings_note);
        });
    }

    // --- dialogs --------------------------------------------------------------

    fn dialogs(&mut self, ctx: &egui::Context) {
        if self.confirm_install {
            let n = self.selected.len();
            let mut install = false;
            let modal = egui::Modal::new(egui::Id::unique("confirm-install")).show(ctx, |ui| {
                ui.set_max_width(440.0);
                ui.heading(format!("Installare {n} aggiornament{}?", if n == 1 { "o" } else { "i" }));
                ui.add_space(6.0);
                ui.label(
                    "Prima viene creato un punto di ripristino e salvato il driver attuale: \
                     se un dispositivo smette di funzionare, il driver precedente viene ripristinato.",
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    install = ui.button((icon(Icon::Download), "Installa")).clicked();
                    if ui.button("Annulla").clicked() {
                        ui.close();
                    }
                });
            });
            if install {
                let ids = self.selected.iter().cloned().collect();
                self.send(Call::Install, Request::Install { ids });
            }
            if install || modal.should_close() {
                self.confirm_install = false;
            }
        }

        if self.confirm_remove {
            let chosen: Vec<&DriverPackage> = self
                .packages
                .iter()
                .flatten()
                .filter(|p| self.to_remove.contains(&p.published_name))
                .collect();
            let n = chosen.len();
            let size: u64 = chosen.iter().map(|p| p.size).sum();
            let mut remove = false;
            let modal = egui::Modal::new(egui::Id::unique("confirm-remove")).show(ctx, |ui| {
                ui.set_max_width(460.0);
                ui.heading(format!(
                    "Rimuovere {n} version{} vecchi{}?",
                    if n == 1 { "e" } else { "i" },
                    if n == 1 { "a" } else { "e" }
                ));
                ui.add_space(6.0);
                ui.label(format!(
                    "I pacchetti vengono tolti dal driver store di Windows e liberano {}.                      Nessun dispositivo collegato li usa e per ognuno resta installata la versione                      più recente.",
                    text::size(size)
                ));
                ui.add_space(8.0);
                egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                    for p in &chosen {
                        ui.horizontal(|ui| {
                            ui.label(p.original_name.as_deref().unwrap_or(&p.published_name));
                            ui.weak(p.version.as_deref().unwrap_or("–"));
                            ui.weak(format!("({})", p.published_name));
                        });
                    }
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    remove = ui.button((icon(Icon::Trash2), "Rimuovi")).clicked();
                    if ui.button("Annulla").clicked() {
                        ui.close();
                    }
                });
            });
            if remove {
                let names = chosen.iter().map(|p| p.published_name.clone()).collect();
                self.send(Call::Remove, Request::RemovePackages { names });
            }
            if remove || modal.should_close() {
                self.confirm_remove = false;
            }
        }

        if let Some(details) = &mut self.driver_details {
            if let Ok(files) = details.rx.try_recv() {
                details.files = Some(files);
            }
            let modal = egui::Modal::new(egui::Id::unique("driver-details")).show(ctx, |ui| {
                ui.set_width(560.0);
                ui.heading("Dettagli driver");
                ui.weak(&details.device);
                ui.add_space(6.0);
                match &details.files {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Lettura dei file del driver…");
                        });
                    }
                    Some(Err(err)) => {
                        ui.colored_label(
                            ui.visuals().error_fg_color,
                            format!("Impossibile leggere i file del driver: {err}"),
                        );
                    }
                    Some(Ok(files)) if files.is_empty() => {
                        ui.label(
                            "Nessun file del driver richiesto o caricato per questo dispositivo.",
                        );
                    }
                    Some(Ok(files)) => {
                        egui::ScrollArea::vertical()
                            .max_height(420.0)
                            .show(ui, |ui| {
                                for (i, f) in files.iter().enumerate() {
                                    driver_file(ui, i, f);
                                }
                            });
                    }
                }
                ui.add_space(10.0);
                if ui.button("Chiudi").clicked() {
                    ui.close();
                }
            });
            if modal.should_close() {
                self.driver_details = None;
            }
        }

        if let Some(message) = &self.alert {
            let modal = egui::Modal::new(egui::Id::unique("alert")).show(ctx, |ui| {
                ui.set_max_width(440.0);
                ui.label(message);
                ui.add_space(10.0);
                if ui.button("OK").clicked() {
                    ui.close();
                }
            });
            if modal.should_close() {
                self.alert = None;
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        while let Ok(reply) = self.rx.try_recv() {
            self.handle(reply);
        }
        if self.last_poll.elapsed() >= POLL {
            self.last_poll = Instant::now();
            self.send(Call::Status, Request::Status);
        }
        ui.ctx().request_repaint_after(POLL);

        egui::Panel::top("header").show(ui, |ui| self.header(ui));
        egui::CentralPanel::default_margins().show(ui, |ui| {
            self.banners(ui);
            match self.tab {
                Tab::Updates => {
                    egui::ScrollArea::vertical()
                        .auto_shrink(false)
                        .show(ui, |ui| self.updates(ui));
                }
                Tab::Drivers => self.drivers(ui),
                Tab::Packages => self.packages(ui),
                Tab::History => self.history(ui),
                Tab::Settings => self.settings(ui),
            }
        });
        self.dialogs(&ui.ctx().clone());
        for (call, request) in std::mem::take(&mut self.outbox) {
            self.send(call, request);
        }
    }
}

// --- drivers helpers ----------------------------------------------------------

// Inbox drivers keep their own INF name; packages added to the driver store
// (by Windows Update, a vendor setup or WDU) are published as oemNN.inf.
fn origin(d: &Device, by_wdu: &HashMap<String, &HistoryRecord>) -> String {
    if let Some(r) = by_wdu.get(&d.instance_id.to_lowercase()) {
        return format!(
            "Installato da WDU ({}, {})",
            text::source(r.source),
            text::when(Some(&r.at))
        );
    }
    let Some(inf) = d.driver.as_ref().and_then(|v| v.inf_name.as_deref()) else {
        return "–".into();
    };
    if driver_store::is_oem_inf(inf) {
        "Pacchetto aggiuntivo"
    } else {
        "Integrato in Windows"
    }
    .into()
}

/// The buttons of a driver package row: Windows' properties of its INF, and
/// the INF selected in Explorer.
fn package_actions(ui: &mut Ui, p: &DriverPackage) -> Result<(), String> {
    let inf = || {
        shell::package_inf(
            &p.published_name,
            p.original_name.as_deref(),
            p.store_path.as_deref(),
        )
    };
    if row_button(ui, Icon::SquareArrowOutUpRight, "Proprietà del file INF") {
        shell::file_properties(&inf())?;
    }
    if row_button(ui, Icon::FolderOpen, "Apri la cartella del driver") {
        shell::show_in_folder(&inf())?;
    }
    Ok(())
}

fn compare(
    key: SortKey,
    (a, a_origin): &(&Device, String),
    (b, b_origin): &(&Device, String),
) -> Ordering {
    let lower = |s: Option<&str>| s.unwrap_or("").to_lowercase();
    let driver = |d: &Device, f: fn(&wdu_core::model::DriverInfo) -> Option<&str>| {
        d.driver.as_ref().and_then(f).map(String::from)
    };
    match key {
        SortKey::Name => lower(a.name.as_deref().or(Some(&a.instance_id)))
            .cmp(&lower(b.name.as_deref().or(Some(&b.instance_id)))),
        SortKey::Driver => lower(driver(a, |v| v.description.as_deref()).as_deref())
            .cmp(&lower(driver(b, |v| v.description.as_deref()).as_deref())),
        SortKey::Class => lower(a.class.as_deref()).cmp(&lower(b.class.as_deref())),
        SortKey::Version => version_key(driver(a, |v| v.version.as_deref()))
            .cmp(&version_key(driver(b, |v| v.version.as_deref()))),
        SortKey::Date => driver(a, |v| v.date.as_deref()).cmp(&driver(b, |v| v.date.as_deref())),
        SortKey::Provider => lower(driver(a, |v| v.provider.as_deref()).as_deref())
            .cmp(&lower(driver(b, |v| v.provider.as_deref()).as_deref())),
        SortKey::Origin => a_origin.to_lowercase().cmp(&b_origin.to_lowercase()),
    }
}

fn compare_packages(key: PackageSort, a: &DriverPackage, b: &DriverPackage) -> Ordering {
    let lower = |s: Option<&str>| s.unwrap_or("").to_lowercase();
    let usage = |p: &DriverPackage| match p.usage() {
        PackageUse::InUse => 0,
        PackageUse::AbsentDevices => 1,
        PackageUse::NoDevice => 2,
    };
    match key {
        PackageSort::Name => lower(a.original_name.as_deref())
            .cmp(&lower(b.original_name.as_deref()))
            .then_with(|| version_key(b.version.clone()).cmp(&version_key(a.version.clone()))),
        PackageSort::Class => lower(a.class.as_deref()).cmp(&lower(b.class.as_deref())),
        PackageSort::Version => version_key(a.version.clone()).cmp(&version_key(b.version.clone())),
        PackageSort::Date => a.date.cmp(&b.date),
        PackageSort::Provider => lower(a.provider.as_deref()).cmp(&lower(b.provider.as_deref())),
        PackageSort::Size => a.size.cmp(&b.size),
        // Old versions first, then by how much the package is used.
        PackageSort::Usage => (!a.is_old_unused(), usage(a)).cmp(&(!b.is_old_unused(), usage(b))),
    }
}

/// Numeric parts of a dotted version, so `10.0` sorts after `9.1`.
fn version_key(version: Option<String>) -> Vec<u64> {
    version
        .unwrap_or_default()
        .split('.')
        .map(|n| n.trim().parse().unwrap_or(0))
        .collect()
}

// --- widgets ------------------------------------------------------------------

fn lucide() -> egui::FontFamily {
    egui::FontFamily::Name("lucide".into())
}

/// A Lucide icon, to put before a label.
fn icon(glyph: Icon) -> RichText {
    RichText::new(char::from(glyph)).family(lucide())
}

/// One line of text cut to the cell, in full on hover.
fn cell(ui: &mut Ui, text: &str) {
    ui.add(Label::new(text).wrap_mode(fit(ui)));
}

/// A cell of a secondary row, such as an older version.
fn weak_cell(ui: &mut Ui, text: &str) {
    ui.add(Label::new(RichText::new(text).weak()).wrap_mode(fit(ui)));
}

/// Truncated, except while an auto-sized table measures its columns: then
/// text takes its full width, so the column fits it.
fn fit(ui: &Ui) -> egui::TextWrapMode {
    if ui.is_sizing_pass() {
        egui::TextWrapMode::Extend
    } else {
        egui::TextWrapMode::Truncate
    }
}

/// A small icon button in a table's leading column of actions.
fn row_button(ui: &mut Ui, glyph: Icon, tip: &str) -> bool {
    ui.add(Button::new(icon(glyph)).frame(false))
        .on_hover_text(tip)
        .clicked()
}

/// One file in the driver details, with what Windows shows for it.
fn driver_file(ui: &mut Ui, index: usize, file: &DriverFile) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.add(Label::new(RichText::new(file.path.display().to_string()).strong()).wrap());
        egui::Grid::new(("driver-file", index))
            .num_columns(2)
            .show(ui, |ui| {
                for (label, value) in [
                    ("Fornitore", file.company.as_deref()),
                    ("Versione file", file.version.as_deref()),
                    ("Copyright", file.copyright.as_deref()),
                    (
                        "Firmatario digitale",
                        Some(file.signer.as_deref().unwrap_or("Non firmato digitalmente")),
                    ),
                ] {
                    ui.weak(label);
                    ui.add(Label::new(value.unwrap_or("–")).wrap());
                    ui.end_row();
                }
            });
    });
}

/// The tree line joining an older version to the row above it.
fn tree_branch(ui: &mut Ui, last: bool) {
    let height = ui.available_height();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(24.0, height), egui::Sense::hover());
    let stroke = egui::Stroke::new(1.0, ui.visuals().weak_text_color().gamma_multiply(0.6));
    let x = rect.left() + 8.0;
    let y = rect.center().y;
    // The table's row spacing would leave a gap; reach into it.
    let top = rect.top() - 4.0;
    let bottom = if last { y } else { rect.bottom() + 4.0 };
    ui.painter()
        .line_segment([egui::pos2(x, top), egui::pos2(x, bottom)], stroke);
    ui.painter().line_segment(
        [egui::pos2(x, y), egui::pos2(rect.right() - 2.0, y)],
        stroke,
    );
}

/// A second, smaller line under a cell's text.
fn sub(ui: &mut Ui, text: &str) {
    ui.add(Label::new(RichText::new(text).small().weak()).wrap_mode(fit(ui)));
}

fn badge(ui: &mut Ui, text: impl Into<String>, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.18))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(6, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().color(color));
        });
}

fn banner(ui: &mut Ui, color: Color32, glyph: Icon, text: &str) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.12))
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(4)
        .inner_margin(8)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.label(icon(glyph).color(color));
                ui.add(Label::new(RichText::new(text).color(color)).wrap());
            });
        });
    ui.add_space(6.0);
}
