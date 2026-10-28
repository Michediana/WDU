//! Devices: every device with its driver, where the driver comes from, and
//! Windows' own properties and the driver files one click away.

use std::cmp::Ordering;
use std::collections::HashMap;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{
    Column, ColumnSort, DataTable, TableDelegate, TableEvent, TableState,
};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Size, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use wdu_core::driver_store;
use wdu_core::history::{HistoryRecord, Outcome};
use wdu_core::ipc::{Request, Response};
use wdu_core::model::{Device, DriverInfo};

use crate::driver_files::{open_device_properties, open_driver_files};
use crate::service::{self, Service, WorkFinished};
use crate::text;
use crate::ui::{self, PageHeader};

const NAME: usize = 0;
const DRIVER: usize = 1;
const CLASS: usize = 2;
const VERSION: usize = 3;
const DATE: usize = 4;
const PROVIDER: usize = 5;
const ORIGIN: usize = 6;
const ACTIONS: usize = 7;

struct Row {
    device: Device,
    origin: String,
}

impl Row {
    fn name(&self) -> &str {
        self.device
            .name
            .as_deref()
            .unwrap_or(&self.device.instance_id)
    }

    fn driver(&self) -> Option<&DriverInfo> {
        self.device.driver.as_ref()
    }
}

pub struct DeviceTable {
    rows: Vec<Row>,
    /// Rows that pass the filters, in display order.
    visible: Vec<usize>,
    query: String,
    show_hidden: bool,
    show_no_driver: bool,
    sort: Option<(usize, ColumnSort)>,
    loaded: bool,
}

impl DeviceTable {
    fn new() -> Self {
        DeviceTable {
            rows: Vec::new(),
            visible: Vec::new(),
            query: String::new(),
            show_hidden: false,
            show_no_driver: false,
            sort: None,
            loaded: false,
        }
    }

    fn set_devices(&mut self, devices: Vec<Device>, history: &[HistoryRecord]) {
        // Instance ID (lowercase) → last successful WDU installation for that device.
        let mut by_wdu = HashMap::new();
        for r in history {
            if r.outcome == Outcome::Succeeded {
                for id in &r.devices {
                    by_wdu.insert(id.to_lowercase(), r);
                }
            }
        }
        self.rows = devices
            .into_iter()
            .map(|device| Row {
                origin: origin(&device, &by_wdu),
                device,
            })
            .collect();
        self.loaded = true;
        self.apply();
    }

    fn apply(&mut self) {
        let query = self.query.trim().to_lowercase();
        self.visible = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                let d = &r.device;
                (self.show_hidden || !d.status.hidden)
                    && (self.show_no_driver || d.driver.is_some())
                    && (query.is_empty() || matches(r, &query))
            })
            .map(|(i, _)| i)
            .collect();
        let rows = &self.rows;
        match self.sort {
            Some((col, sort)) if sort != ColumnSort::Default => {
                self.visible.sort_by(|a, b| {
                    let order = compare(col, &rows[*a], &rows[*b]);
                    if sort == ColumnSort::Descending {
                        order.reverse()
                    } else {
                        order
                    }
                });
            }
            _ => self
                .visible
                .sort_by(|a, b| compare(NAME, &rows[*a], &rows[*b])),
        }
    }

    fn row(&self, row_ix: usize) -> Option<&Row> {
        self.visible.get(row_ix).map(|i| &self.rows[*i])
    }

    fn shown(&self) -> usize {
        self.visible.len()
    }
}

impl TableDelegate for DeviceTable {
    fn columns_count(&self, _: &App) -> usize {
        8
    }

    fn rows_count(&self, _: &App) -> usize {
        self.visible.len()
    }

    // Column widths are a starting layout the user resizes; they are table
    // geometry, not text, and do not follow the zoom.
    fn column(&self, col_ix: usize, _: &App) -> Column {
        match col_ix {
            NAME => Column::new("name", "Dispositivo")
                .width(px(260.))
                .fixed_left()
                .sortable()
                .ascending(),
            DRIVER => Column::new("driver", "Driver").width(px(220.)).sortable(),
            CLASS => Column::new("class", "Classe").width(px(100.)).sortable(),
            VERSION => Column::new("version", "Versione")
                .width(px(130.))
                .sortable(),
            DATE => Column::new("date", "Data")
                .width(px(96.))
                .sortable()
                .text_right(),
            PROVIDER => Column::new("provider", "Fornitore")
                .width(px(150.))
                .sortable(),
            ORIGIN => Column::new("origin", "Origine").width(px(190.)).sortable(),
            _ => Column::new("actions", "")
                .width(px(76.))
                .resizable(false)
                .selectable(false),
        }
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        self.sort = Some((col_ix, sort));
        self.apply();
        cx.notify();
    }

    fn loading(&self, _: &App) -> bool {
        !self.loaded
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        ui::empty_state(
            IconName::Search,
            "Nessun dispositivo corrisponde ai filtri",
            Some("Cambia la ricerca o mostra anche i dispositivi nascosti e senza driver.".into()),
            cx,
        )
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(row) = self.row(row_ix) else {
            return menu;
        };
        let id = row.device.instance_id.clone();
        let files_id = id.clone();
        let name: SharedString = row.name().to_string().into();
        menu.item(
            PopupMenuItem::new("Proprietà del dispositivo")
                .icon(IconName::ExternalLink)
                .on_click(move |_, window, cx| open_device_properties(&id, window, cx)),
        )
        .item(
            PopupMenuItem::new("File del driver…")
                .icon(IconName::FileText)
                .disabled(row.driver().is_none())
                .on_click(move |_, window, cx| {
                    open_driver_files(files_id.clone(), name.clone(), window, cx)
                }),
        )
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, _: &App) -> Option<SharedString> {
        let row = self.row(row_ix)?;
        let driver = row.driver();
        Some(
            match col_ix {
                NAME => row.name().to_string(),
                DRIVER => driver.and_then(|v| v.description.clone())?,
                CLASS => row.device.class.clone()?,
                VERSION => driver.and_then(|v| v.version.clone())?,
                DATE => driver.and_then(|v| v.date.clone())?,
                PROVIDER => driver.and_then(|v| v.provider.clone())?,
                ORIGIN => row.origin.clone(),
                _ => return None,
            }
            .into(),
        )
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        // Rows hold two lines: a one-line cell sits on the row's center line.
        h_flex()
            .size_full()
            .min_w_0()
            .when(matches!(col_ix, DATE), |this| this.justify_end())
            .child(self.render_cell(row_ix, col_ix, cx))
    }
}

impl DeviceTable {
    fn render_cell(
        &self,
        row_ix: usize,
        col_ix: usize,
        cx: &mut Context<TableState<Self>>,
    ) -> gpui_kit::AnyElement {
        let Some(row) = self.row(row_ix) else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let driver = row.driver();
        let dash = || "–".to_string();
        match col_ix {
            NAME => two_lines(
                h_flex()
                    .gap_1p5()
                    .min_w_0()
                    .when(row.device.status.problem_code != 0, |this| {
                        this.child(
                            Tag::danger()
                                .xsmall()
                                .child(format!("problema {}", row.device.status.problem_code)),
                        )
                    })
                    .child(div().truncate().child(row.name().to_string())),
                row.device.manufacturer.clone(),
                cx,
            )
            .into_any_element(),
            DRIVER => match driver {
                None => div()
                    .text_color(theme.muted_foreground)
                    .child("Nessun driver")
                    .into_any_element(),
                Some(v) => two_lines(
                    div()
                        .truncate()
                        .child(v.description.clone().unwrap_or_else(dash)),
                    v.inf_name.clone(),
                    cx,
                )
                .into_any_element(),
            },
            CLASS => cell(row.device.class.clone().unwrap_or_else(dash)),
            VERSION => div()
                .truncate()
                .font_family(theme.mono_font_family.clone())
                .child(driver.and_then(|v| v.version.clone()).unwrap_or_else(dash))
                .into_any_element(),
            DATE => cell(
                driver
                    .and_then(|v| v.date.as_deref())
                    .map_or_else(dash, text::day),
            ),
            PROVIDER => cell(driver.and_then(|v| v.provider.clone()).unwrap_or_else(dash)),
            ORIGIN => cell(row.origin.clone()),
            ACTIONS => {
                let id = row.device.instance_id.clone();
                let name: SharedString = row.name().to_string().into();
                let files_id = id.clone();
                h_flex()
                    .gap_1()
                    .child(
                        Button::new(SharedString::from(format!("props-{id}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::ExternalLink)
                            .tooltip("Proprietà del dispositivo")
                            .on_click(move |_, window, cx| open_device_properties(&id, window, cx)),
                    )
                    .child(
                        Button::new(SharedString::from(format!("files-{files_id}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::FileText)
                            .tooltip("File del driver…")
                            .disabled(driver.is_none())
                            .on_click(move |_, window, cx| {
                                open_driver_files(files_id.clone(), name.clone(), window, cx)
                            }),
                    )
                    .into_any_element()
            }
            _ => div().into_any_element(),
        }
    }
}

pub struct DevicesPage {
    service: Entity<Service>,
    table: Entity<TableState<DeviceTable>>,
    search: Entity<InputState>,
    error: Option<String>,
    loading: bool,
    _subscriptions: Vec<Subscription>,
}

impl DevicesPage {
    pub fn new(service: Entity<Service>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let table = cx.new(|cx| TableState::new(DeviceTable::new(), window, cx));
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Cerca dispositivo, classe, fornitore…")
        });
        let subscriptions = vec![
            cx.subscribe(
                &search,
                |this: &mut Self, search, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        let query = search.read(cx).value().to_string();
                        this.update_table(cx, |t| t.query = query);
                    }
                },
            ),
            cx.subscribe_in(
                &table,
                window,
                |_, table, event: &TableEvent, window, cx| {
                    if let TableEvent::DoubleClickedRow(row_ix) = event
                        && let Some(row) = table.read(cx).delegate().row(*row_ix)
                        && row.driver().is_some()
                    {
                        let id = row.device.instance_id.clone();
                        let name = row.name().to_string().into();
                        open_driver_files(id, name, window, cx);
                    }
                },
            ),
            cx.subscribe(&service, |this: &mut Self, _, _: &WorkFinished, cx| {
                this.reload(cx)
            }),
            cx.observe(&table, |_, _, cx| cx.notify()),
        ];
        DevicesPage {
            service,
            table,
            search,
            error: None,
            loading: false,
            _subscriptions: subscriptions,
        }
    }

    /// Reads the devices again, keeping the table as it is meanwhile.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        self.service.update(cx, |s, cx| s.reload_history(cx));
        let task = service::call(Request::Devices, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(Response::Devices { devices }) => {
                        this.error = None;
                        let history = this.service.read(cx).history().to_vec();
                        this.update_table(cx, |t| t.set_devices(devices, &history));
                    }
                    Ok(_) => {}
                    Err(err) => this.error = Some(err),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn update_table(&mut self, cx: &mut Context<Self>, edit: impl FnOnce(&mut DeviceTable)) {
        self.table.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            edit(delegate);
            delegate.apply();
            // Rows only: `refresh` would also reset the column widths and sort.
            cx.notify();
        });
    }
}

impl Render for DevicesPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let delegate = self.table.read(cx).delegate();
        let (shown, total) = (delegate.shown(), delegate.rows.len());
        let (show_hidden, show_no_driver) = (delegate.show_hidden, delegate.show_no_driver);
        let count = delegate
            .loaded
            .then(|| format!("{shown} di {total} dispositivi"));

        v_flex()
            .size_full()
            .p_6()
            .gap_4()
            .child(
                PageHeader::new("Dispositivi")
                    .description(
                        "I dispositivi di questo PC e il driver che usano. Doppio clic su una \
                         riga per i file del driver.",
                    )
                    .action(
                        Button::new("reload-devices")
                            .icon(IconName::RefreshCw)
                            .label("Aggiorna")
                            .loading(self.loading)
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    ),
            )
            .child(
                h_flex()
                    .gap_4()
                    .child(
                        Input::new(&self.search)
                            .prefix(Icon::new(IconName::Search).small())
                            .cleanable(true)
                            .w_80(),
                    )
                    .child(
                        Checkbox::new("show-hidden")
                            .label("Dispositivi nascosti")
                            .checked(show_hidden)
                            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                let checked = *checked;
                                this.update_table(cx, |t| t.show_hidden = checked);
                            })),
                    )
                    .child(
                        Checkbox::new("show-no-driver")
                            .label("Dispositivi senza driver")
                            .checked(show_no_driver)
                            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                let checked = *checked;
                                this.update_table(cx, |t| t.show_no_driver = checked);
                            })),
                    )
                    .child(div().flex_1())
                    .children(count.map(|count| ui::muted(count, cx))),
            )
            .children(self.error.clone().map(|err| {
                gpui_kit::component::alert::Alert::error("devices-error", err)
                    .title("Impossibile leggere i dispositivi")
            }))
            .child(
                div().flex_1().min_h_0().child(
                    // Two lines of text per row: the table's row height is
                    // its own pixel geometry, set by `Size`.
                    DataTable::new(&self.table)
                        .with_size(Size::Size(px(52.)))
                        .header_row_height(px(36.))
                        .stripe(true)
                        .bordered(true),
                ),
            )
    }
}

fn cell(text: String) -> gpui_kit::AnyElement {
    div().truncate().child(text).into_any_element()
}

/// A cell with its value and, under it, a smaller muted line.
fn two_lines(first: impl IntoElement, second: Option<String>, cx: &App) -> impl IntoElement {
    v_flex()
        .min_w_0()
        .child(first)
        .when_some(second, |this, second| {
            this.child(
                div()
                    .truncate()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(second),
            )
        })
}

fn matches(row: &Row, query: &str) -> bool {
    let d = &row.device;
    let driver = d.driver.as_ref();
    [
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
    .any(|v| v.to_lowercase().contains(query))
}

fn compare(col: usize, a: &Row, b: &Row) -> Ordering {
    let lower = |s: Option<&str>| s.unwrap_or("").to_lowercase();
    let driver =
        |r: &Row, f: fn(&DriverInfo) -> Option<&str>| r.driver().and_then(f).map(str::to_lowercase);
    match col {
        DRIVER => {
            driver(a, |v| v.description.as_deref()).cmp(&driver(b, |v| v.description.as_deref()))
        }
        CLASS => lower(a.device.class.as_deref()).cmp(&lower(b.device.class.as_deref())),
        VERSION => text::version_key(a.driver().and_then(|v| v.version.as_deref())).cmp(
            &text::version_key(b.driver().and_then(|v| v.version.as_deref())),
        ),
        DATE => driver(a, |v| v.date.as_deref()).cmp(&driver(b, |v| v.date.as_deref())),
        PROVIDER => driver(a, |v| v.provider.as_deref()).cmp(&driver(b, |v| v.provider.as_deref())),
        ORIGIN => a.origin.to_lowercase().cmp(&b.origin.to_lowercase()),
        _ => a.name().to_lowercase().cmp(&b.name().to_lowercase()),
    }
}

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
