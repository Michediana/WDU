//! Installed drivers: the packages added to the driver store, each newest
//! version with its older ones under it, and the removal of old versions
//! no device uses.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use gpui_kit::component::StyledExt as _;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::table::{Column, ColumnSort, DataTable, TableDelegate, TableState};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Size, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use wdu_core::driver_store::{DriverPackage, PackageUse};
use wdu_core::ipc::{Request, Response, ServiceState};

use crate::service::{self, Service, WorkFinished};
use crate::shell;
use crate::text;
use crate::ui::{self, Lucide, PageHeader};

const NAME: usize = 0;
const CLASS: usize = 1;
const VERSION: usize = 2;
const DATE: usize = 3;
const PROVIDER: usize = 4;
const SIZE: usize = 5;
const USAGE: usize = 6;
const ACTIONS: usize = 7;

/// One line of the table, by index into the packages.
#[derive(Clone, Copy)]
enum Row {
    Newest { ix: usize, older: usize, open: bool },
    Older { ix: usize },
}

impl Row {
    fn ix(self) -> usize {
        match self {
            Row::Newest { ix, .. } | Row::Older { ix } => ix,
        }
    }
}

pub struct PackageTable {
    packages: Vec<DriverPackage>,
    rows: Vec<Row>,
    /// Groups shown, after the filters.
    groups: usize,
    query: String,
    only_old: bool,
    sort: Option<(usize, ColumnSort)>,
    /// Newest packages whose older versions are shown, by published name.
    expanded: HashSet<String>,
    /// Older versions marked for removal, by published name.
    to_remove: HashSet<String>,
    loaded: bool,
}

impl PackageTable {
    fn new() -> Self {
        PackageTable {
            packages: Vec::new(),
            rows: Vec::new(),
            groups: 0,
            query: String::new(),
            only_old: false,
            sort: None,
            expanded: HashSet::new(),
            to_remove: HashSet::new(),
            loaded: false,
        }
    }

    fn set_packages(&mut self, packages: Vec<DriverPackage>) {
        self.to_remove.retain(|name| {
            packages
                .iter()
                .any(|p| &p.published_name == name && p.is_old_unused())
        });
        self.packages = packages;
        self.loaded = true;
        self.apply();
    }

    /// Old versions no device uses: what may be removed.
    fn old(&self) -> impl Iterator<Item = &DriverPackage> {
        self.packages.iter().filter(|p| p.is_old_unused())
    }

    fn chosen(&self) -> Vec<&DriverPackage> {
        self.packages
            .iter()
            .filter(|p| self.to_remove.contains(&p.published_name))
            .collect()
    }

    fn apply(&mut self) {
        let query = self.query.trim().to_lowercase();
        let packages = &self.packages;
        let by_name: HashMap<&str, usize> = packages
            .iter()
            .enumerate()
            .map(|(i, p)| (p.published_name.as_str(), i))
            .collect();
        // Older versions hang under the newest package of the same driver.
        let mut older: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, p) in packages.iter().enumerate() {
            if let Some(newest) = p.superseded_by.as_deref().and_then(|n| by_name.get(n)) {
                older.entry(*newest).or_default().push(i);
            }
        }
        let mut groups: Vec<(usize, Vec<usize>)> = packages
            .iter()
            .enumerate()
            .filter(|(_, p)| p.superseded_by.is_none())
            .map(|(i, _)| {
                let mut versions = older.remove(&i).unwrap_or_default();
                versions.sort_by(|a, b| {
                    text::version_key(packages[*b].version.as_deref())
                        .cmp(&text::version_key(packages[*a].version.as_deref()))
                });
                (i, versions)
            })
            .collect();
        groups.retain(|(newest, older)| {
            (!self.only_old || older.iter().any(|i| packages[*i].is_old_unused()))
                && (query.is_empty()
                    || matches(&packages[*newest], &query)
                    || older.iter().any(|i| matches(&packages[*i], &query)))
        });
        let (col, sort) = self.sort.unwrap_or((NAME, ColumnSort::Ascending));
        groups.sort_by(|a, b| {
            let order = compare(col, &packages[a.0], &packages[b.0]);
            if sort == ColumnSort::Descending {
                order.reverse()
            } else {
                order
            }
        });

        // A search or the old-versions filter shows every group open.
        let force_open = self.only_old || !query.is_empty();
        self.groups = groups.len();
        self.rows.clear();
        for (newest, older) in groups {
            let open = !older.is_empty()
                && (force_open || self.expanded.contains(&packages[newest].published_name));
            self.rows.push(Row::Newest {
                ix: newest,
                older: older.len(),
                open,
            });
            if open {
                self.rows
                    .extend(older.into_iter().map(|ix| Row::Older { ix }));
            }
        }
    }

    fn toggle_open(&mut self, name: &str) {
        if !self.expanded.remove(name) {
            self.expanded.insert(name.to_string());
        }
        self.apply();
    }

    fn set_marked(&mut self, name: &str, marked: bool) {
        if marked {
            self.to_remove.insert(name.to_string());
        } else {
            self.to_remove.remove(name);
        }
    }

    /// Marks every old version, or none when all are marked already.
    fn toggle_all(&mut self) {
        let old: Vec<String> = self.old().map(|p| p.published_name.clone()).collect();
        if old.iter().all(|name| self.to_remove.contains(name)) {
            self.to_remove.clear();
            return;
        }
        // Open the groups that hold them, so every mark is in sight.
        let groups: Vec<String> = self.old().filter_map(|p| p.superseded_by.clone()).collect();
        self.expanded.extend(groups);
        self.to_remove.extend(old);
        self.apply();
    }
}

impl TableDelegate for PackageTable {
    fn columns_count(&self, _: &App) -> usize {
        8
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    // Column widths are a starting layout the user resizes; they are table
    // geometry, not text, and do not follow the zoom.
    fn column(&self, col_ix: usize, _: &App) -> Column {
        match col_ix {
            NAME => Column::new("name", "Driver")
                .width(px(300.))
                .fixed_left()
                .sortable()
                .ascending(),
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
            SIZE => Column::new("size", "Dimensione")
                .width(px(100.))
                .sortable()
                .text_right(),
            USAGE => Column::new("usage", "Uso").width(px(220.)).sortable(),
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
        self.sort = (sort != ColumnSort::Default).then_some((col_ix, sort));
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
        if self.packages.is_empty() {
            ui::empty_state(
                Lucide::Package,
                "Nessun pacchetto nel driver store",
                None,
                cx,
            )
            .into_any_element()
        } else {
            ui::empty_state(
                IconName::Search,
                "Nessun pacchetto corrisponde ai filtri",
                None,
                cx,
            )
            .into_any_element()
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(row) = self.rows.get(row_ix) else {
            return menu;
        };
        let p = &self.packages[row.ix()];
        let (props, folder) = (inf_path(p), inf_path(p));
        menu.item(
            PopupMenuItem::new("Proprietà del file INF")
                .icon(IconName::ExternalLink)
                .on_click(move |_, window, cx| {
                    if let Err(err) = shell::file_properties(&props) {
                        window.push_notification(Notification::error(err), cx);
                    }
                }),
        )
        .item(
            PopupMenuItem::new("Apri la cartella del driver")
                .icon(IconName::FolderOpen)
                .on_click(move |_, window, cx| {
                    if let Err(err) = shell::show_in_folder(&folder) {
                        window.push_notification(Notification::error(err), cx);
                    }
                }),
        )
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, _: &App) -> Option<SharedString> {
        let p = &self.packages[self.rows.get(row_ix)?.ix()];
        Some(
            match col_ix {
                NAME => p
                    .original_name
                    .clone()
                    .unwrap_or_else(|| p.published_name.clone()),
                CLASS => p.class.clone()?,
                VERSION => p.version.clone()?,
                DATE => p.date.clone()?,
                PROVIDER => p.provider.clone()?,
                SIZE => text::size(p.size),
                USAGE => text::package_use(p),
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
            .when(matches!(col_ix, DATE | SIZE), |this| this.justify_end())
            .child(self.render_cell(row_ix, col_ix, cx))
    }
}

impl PackageTable {
    fn render_cell(
        &self,
        row_ix: usize,
        col_ix: usize,
        cx: &mut Context<TableState<Self>>,
    ) -> gpui_kit::AnyElement {
        let Some(row) = self.rows.get(row_ix).copied() else {
            return div().into_any_element();
        };
        let p = &self.packages[row.ix()];
        let theme = cx.theme();
        let is_older = matches!(row, Row::Older { .. });
        let dash = || "–".to_string();
        let plain = |text: String| {
            div()
                .truncate()
                .when(is_older, |this| this.text_color(theme.muted_foreground))
                .child(text)
                .into_any_element()
        };
        match col_ix {
            NAME => self.render_name(row, cx).into_any_element(),
            CLASS => plain(p.class.clone().unwrap_or_else(dash)),
            VERSION => div()
                .truncate()
                .font_family(theme.mono_font_family.clone())
                .when(is_older, |this| this.text_color(theme.muted_foreground))
                .child(p.version.clone().unwrap_or_else(dash))
                .into_any_element(),
            DATE => plain(p.date.as_deref().map_or_else(dash, text::day)),
            PROVIDER => plain(p.provider.clone().unwrap_or_else(dash)),
            SIZE => plain(text::size(p.size)),
            USAGE => {
                let devices: Vec<&str> = p
                    .devices
                    .iter()
                    .map(|d| d.name.as_deref().unwrap_or(&d.instance_id))
                    .collect();
                v_flex()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .when(p.is_old_unused(), |this| {
                                this.child(Tag::warning().xsmall().child("vecchia, non usata"))
                            })
                            .child(div().truncate().child(text::package_use(p))),
                    )
                    .when(!devices.is_empty(), |this| {
                        this.child(
                            div()
                                .truncate()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(devices.join(", ")),
                        )
                    })
                    .into_any_element()
            }
            ACTIONS => {
                let name = p.published_name.clone();
                h_flex()
                    .gap_1()
                    .child(
                        Button::new(SharedString::from(format!("inf-props-{name}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::ExternalLink)
                            .tooltip("Proprietà del file INF")
                            .on_click({
                                let inf = inf_path(p);
                                move |_, window, cx| {
                                    if let Err(err) = shell::file_properties(&inf) {
                                        window.push_notification(Notification::error(err), cx);
                                    }
                                }
                            }),
                    )
                    .child(
                        Button::new(SharedString::from(format!("inf-folder-{name}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::FolderOpen)
                            .tooltip("Apri la cartella del driver")
                            .on_click({
                                let inf = inf_path(p);
                                move |_, window, cx| {
                                    if let Err(err) = shell::show_in_folder(&inf) {
                                        window.push_notification(Notification::error(err), cx);
                                    }
                                }
                            }),
                    )
                    .into_any_element()
            }
            _ => div().into_any_element(),
        }
    }
}

impl PackageTable {
    /// The tree column: disclosure and name for a newest package, the
    /// removal mark and name for an older one.
    fn render_name(&self, row: Row, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let p = &self.packages[row.ix()];
        let theme = cx.theme();
        let name = p.published_name.clone();
        let title = p.original_name.clone().unwrap_or_else(|| name.clone());
        let table = cx.entity();
        let label = v_flex()
            .min_w_0()
            .child(div().truncate().child(title))
            .child(
                div()
                    .truncate()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(name.clone()),
            );

        match row {
            Row::Newest { older, open, .. } => h_flex()
                .min_w_0()
                .gap_1p5()
                .child(if older > 0 {
                    Button::new(SharedString::from(format!("open-{name}")))
                        .ghost()
                        .xsmall()
                        .icon(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .accessibility_label(if open {
                            "Nascondi le versioni vecchie"
                        } else {
                            "Mostra le versioni vecchie"
                        })
                        .on_click(move |_, _, cx| {
                            table.update(cx, |table, cx| {
                                table.delegate_mut().toggle_open(&name);
                                cx.notify();
                            })
                        })
                        .into_any_element()
                } else {
                    div().size_6().flex_shrink_0().into_any_element()
                })
                .child(label)
                .when(older > 0, |this| {
                    this.child(div().flex_1())
                        .child(Tag::secondary().xsmall().child(if older == 1 {
                            "+1 versione".to_string()
                        } else {
                            format!("+{older} versioni")
                        }))
                })
                .into_any_element(),
            Row::Older { .. } => h_flex()
                .min_w_0()
                .pl_7()
                .gap_2()
                .text_color(theme.muted_foreground)
                .child(if p.is_old_unused() {
                    let marked = self.to_remove.contains(&name);
                    Checkbox::new(SharedString::from(format!("remove-{name}")))
                        .checked(marked)
                        .accessibility_label("Rimuovi questa versione")
                        .on_change(move |checked: &bool, _, cx| {
                            table.update(cx, |table, cx| {
                                table.delegate_mut().set_marked(&name, *checked);
                                cx.notify();
                            })
                        })
                        .into_any_element()
                } else {
                    div().size_4().flex_shrink_0().into_any_element()
                })
                .child(label)
                .into_any_element(),
        }
    }
}

pub struct PackagesPage {
    service: Entity<Service>,
    table: Entity<TableState<PackageTable>>,
    search: Entity<InputState>,
    error: Option<String>,
    loading: bool,
    /// A removal was accepted and has not finished yet.
    removing: bool,
    _subscriptions: Vec<Subscription>,
}

impl PackagesPage {
    pub fn new(service: Entity<Service>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let table = cx.new(|cx| TableState::new(PackageTable::new(), window, cx));
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Cerca driver, fornitore, dispositivo…")
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
                &service,
                window,
                |this: &mut Self, _, done: &WorkFinished, window, cx| {
                    if std::mem::take(&mut this.removing)
                        && let Some(err) = &done.error
                    {
                        window.push_notification(
                            Notification::error(err.clone())
                                .title("Non tutti i driver sono stati rimossi"),
                            cx,
                        );
                    }
                    this.reload(cx);
                },
            ),
            cx.observe(&service, |_, _, cx| cx.notify()),
            cx.observe(&table, |_, _, cx| cx.notify()),
        ];
        PackagesPage {
            service,
            table,
            search,
            error: None,
            loading: false,
            removing: false,
            _subscriptions: subscriptions,
        }
    }

    /// Reads the driver store again, keeping the table as it is meanwhile.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        let task = service::call(Request::DriverPackages, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(Response::DriverPackages { packages }) => {
                        this.error = None;
                        this.update_table(cx, |t| t.set_packages(packages));
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

    fn update_table(&mut self, cx: &mut Context<Self>, edit: impl FnOnce(&mut PackageTable)) {
        self.table.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            edit(delegate);
            delegate.apply();
            // Rows only: `refresh` would also reset the column widths and sort.
            cx.notify();
        });
    }

    fn confirm_remove(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let delegate = self.table.read(cx).delegate();
        let chosen = delegate.chosen();
        let n = chosen.len();
        let size: u64 = chosen.iter().map(|p| p.size).sum();
        let names: Vec<String> = chosen.iter().map(|p| p.published_name.clone()).collect();
        let list: Vec<(String, String)> = chosen
            .iter()
            .map(|p| {
                (
                    p.original_name
                        .clone()
                        .unwrap_or_else(|| p.published_name.clone()),
                    format!(
                        "{} · {}",
                        p.version.as_deref().unwrap_or("–"),
                        p.published_name
                    ),
                )
            })
            .collect();
        let title: SharedString = if n == 1 {
            "Rimuovere 1 versione vecchia?".into()
        } else {
            format!("Rimuovere {n} versioni vecchie?").into()
        };
        let description: SharedString = format!(
            "I pacchetti vengono tolti dal driver store di Windows e liberano {}. Nessun \
             dispositivo collegato li usa e per ognuno resta installata la versione più recente.",
            text::size(size)
        )
        .into();
        let page = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let page = page.clone();
            let names = names.clone();
            alert
                .title(title.clone())
                .description(
                    v_flex().gap_3().child(description.clone()).child(
                        v_flex()
                            .max_h_48()
                            .overflow_hidden()
                            .gap_1()
                            .children(list.iter().map(|(name, detail)| {
                                h_flex()
                                    .gap_2()
                                    .text_sm()
                                    .child(div().truncate().child(name.clone()))
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(detail.clone()),
                                    )
                            })),
                    ),
                )
                .show_cancel(true)
                .cancel_text("Annulla")
                .ok_text("Rimuovi")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let names = names.clone();
                    page.update(cx, |page, cx| page.remove(names, window, cx))
                        .ok();
                    true
                })
        });
    }

    fn remove(&mut self, names: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let task = service::call(Request::RemovePackages { names }, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(_) => {
                        this.removing = true;
                        this.update_table(cx, |t| t.to_remove.clear());
                    }
                    Err(err) => window.push_notification(
                        Notification::error(err).title("Impossibile rimuovere i driver"),
                        cx,
                    ),
                }
                this.service.update(cx, |s, cx| s.refresh_status(cx));
            })
            .ok();
        })
        .detach();
    }

    /// The old versions that may go, and the commands that remove them.
    fn render_cleanup(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        let service = self.service.read(cx);
        if let Some(status) = service
            .status()
            .filter(|s| s.state == ServiceState::Removing)
        {
            return Some(
                h_flex()
                    .gap_3()
                    .px_4()
                    .py_3()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.group_box)
                    .child(Spinner::new().small())
                    .child(
                        div()
                            .text_sm()
                            .font_medium()
                            .child(text::state(status.state)),
                    )
                    .children(status.current.clone().map(|c| ui::muted(c, cx)))
                    .into_any_element(),
            );
        }
        let delegate = self.table.read(cx).delegate();
        let old: Vec<&DriverPackage> = delegate.old().collect();
        if old.is_empty() {
            return None;
        }
        let n = old.len();
        let size: u64 = old.iter().map(|p| p.size).sum();
        let marked = delegate.to_remove.len();
        let all = old
            .iter()
            .all(|p| delegate.to_remove.contains(&p.published_name));
        let can_remove = service.is_online() && !service.is_busy();
        let headline = if n == 1 {
            format!("1 versione vecchia non usata · {}", text::size(size))
        } else {
            format!("{n} versioni vecchie non usate · {}", text::size(size))
        };

        Some(
            h_flex()
                .gap_4()
                .px_4()
                .py_3()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .bg(theme.group_box)
                .child(
                    Icon::new(Lucide::Trash)
                        .text_color(theme.warning)
                        .flex_shrink_0(),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(div().text_sm().font_semibold().child(headline))
                        .child(ui::muted(
                            "Per ognuna c'è un pacchetto più recente dello stesso driver e \
                             nessun dispositivo collegato la usa.",
                            cx,
                        )),
                )
                .child(
                    Button::new("mark-all")
                        .ghost()
                        .label(if all {
                            "Deseleziona tutte"
                        } else {
                            "Seleziona tutte"
                        })
                        .on_click(
                            cx.listener(|this, _, _, cx| this.update_table(cx, |t| t.toggle_all())),
                        ),
                )
                .child(
                    Button::new("remove")
                        .icon(Lucide::Trash)
                        .label(if marked > 0 {
                            format!("Rimuovi ({marked})…")
                        } else {
                            "Rimuovi…".into()
                        })
                        .disabled(!can_remove || marked == 0)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.confirm_remove(window, cx)),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl Render for PackagesPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let delegate = self.table.read(cx).delegate();
        let count = delegate.loaded.then(|| {
            format!(
                "{} driver, {} pacchetti",
                delegate.groups,
                delegate.packages.len()
            )
        });
        let only_old = delegate.only_old;

        v_flex()
            .size_full()
            .p_6()
            .gap_4()
            .child(
                PageHeader::new("Driver installati")
                    .description(
                        "I pacchetti aggiunti al driver store, non quelli integrati in Windows. \
                         «Nessun dispositivo» non vuol dire inutilizzato: estensioni, filtri di \
                         rete, stampanti e servizi non risultano come driver di un dispositivo.",
                    )
                    .action(
                        Button::new("reload-packages")
                            .icon(IconName::RefreshCw)
                            .label("Aggiorna")
                            .loading(self.loading)
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    ),
            )
            .children(self.render_cleanup(cx))
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
                        Checkbox::new("only-old")
                            .label("Solo con versioni vecchie non usate")
                            .checked(only_old)
                            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                let checked = *checked;
                                this.update_table(cx, |t| t.only_old = checked);
                            })),
                    )
                    .child(div().flex_1())
                    .children(count.map(|count| ui::muted(count, cx))),
            )
            .children(self.error.clone().map(|err| {
                Alert::error("packages-error", err).title("Impossibile leggere il driver store")
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

fn inf_path(p: &DriverPackage) -> std::path::PathBuf {
    shell::package_inf(
        &p.published_name,
        p.original_name.as_deref(),
        p.store_path.as_deref(),
    )
}

fn matches(p: &DriverPackage, query: &str) -> bool {
    [
        Some(p.published_name.as_str()),
        p.original_name.as_deref(),
        p.provider.as_deref(),
        p.class.as_deref(),
        p.version.as_deref(),
    ]
    .into_iter()
    .flatten()
    .chain(p.devices.iter().filter_map(|d| d.name.as_deref()))
    .any(|v| v.to_lowercase().contains(query))
}

fn compare(col: usize, a: &DriverPackage, b: &DriverPackage) -> Ordering {
    let lower = |s: Option<&str>| s.unwrap_or("").to_lowercase();
    let usage = |p: &DriverPackage| match p.usage() {
        PackageUse::InUse => 0,
        PackageUse::AbsentDevices => 1,
        PackageUse::NoDevice => 2,
    };
    match col {
        CLASS => lower(a.class.as_deref()).cmp(&lower(b.class.as_deref())),
        VERSION => {
            text::version_key(a.version.as_deref()).cmp(&text::version_key(b.version.as_deref()))
        }
        DATE => a.date.cmp(&b.date),
        PROVIDER => lower(a.provider.as_deref()).cmp(&lower(b.provider.as_deref())),
        SIZE => a.size.cmp(&b.size),
        // Old versions first, then by how much the package is used.
        USAGE => (!a.is_old_unused(), usage(a)).cmp(&(!b.is_old_unused(), usage(b))),
        _ => lower(a.original_name.as_deref())
            .cmp(&lower(b.original_name.as_deref()))
            .then_with(|| {
                text::version_key(b.version.as_deref())
                    .cmp(&text::version_key(a.version.as_deref()))
            }),
    }
}
