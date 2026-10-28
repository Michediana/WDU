//! History: every installation WDU made, newest first, with its outcome.

use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use wdu_core::history::{HistoryRecord, Outcome};

use crate::service::Service;
use crate::text;
use crate::ui::{self, Lucide, PageHeader};

const DATE: usize = 0;
const SOURCE: usize = 1;
const TITLE: usize = 2;
const OUTCOME: usize = 3;
const DETAIL: usize = 4;

pub struct HistoryTable {
    /// Newest first.
    records: Vec<HistoryRecord>,
}

impl TableDelegate for HistoryTable {
    fn columns_count(&self, _: &App) -> usize {
        5
    }

    fn rows_count(&self, _: &App) -> usize {
        self.records.len()
    }

    // Column widths are a starting layout the user resizes; they are table
    // geometry, not text, and do not follow the zoom.
    fn column(&self, col_ix: usize, _: &App) -> Column {
        match col_ix {
            DATE => Column::new("date", "Data").width(px(170.)),
            SOURCE => Column::new("source", "Fonte").width(px(140.)),
            TITLE => Column::new("title", "Aggiornamento").width(px(340.)),
            OUTCOME => Column::new("outcome", "Esito").width(px(260.)),
            _ => Column::new("detail", "Dettagli").width(px(360.)),
        }
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        ui::empty_state(
            Lucide::RotateCcwClock,
            "Nessuna installazione registrata",
            Some("Gli aggiornamenti installati da WDU compariranno qui, con il loro esito.".into()),
            cx,
        )
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(r) = self.records.get(row_ix) else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        match col_ix {
            DATE => div().child(text::when(Some(&r.at))).into_any_element(),
            SOURCE => div().child(text::source(r.source)).into_any_element(),
            TITLE => div().truncate().child(r.title.clone()).into_any_element(),
            OUTCOME => {
                let (icon, color) = match r.outcome {
                    Outcome::Succeeded => (IconName::CircleCheck, theme.success),
                    Outcome::Failed => (IconName::CircleX, theme.danger),
                    Outcome::RolledBack => (IconName::Undo2, theme.warning),
                };
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .child(Icon::new(icon).small().text_color(color).flex_shrink_0())
                    .child(div().truncate().child(text::outcome(r.outcome)))
                    .when(r.reboot_required, |this| {
                        this.child(Tag::warning().xsmall().child("riavvio richiesto"))
                    })
                    .into_any_element()
            }
            DETAIL => div()
                .truncate()
                .text_color(theme.muted_foreground)
                .child(r.detail.clone().unwrap_or_default())
                .into_any_element(),
            _ => div().into_any_element(),
        }
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, _: &App) -> Option<SharedString> {
        let r = self.records.get(row_ix)?;
        Some(
            match col_ix {
                DATE => text::when(Some(&r.at)),
                SOURCE => text::source(r.source).to_string(),
                TITLE => r.title.clone(),
                OUTCOME => text::outcome(r.outcome).to_string(),
                _ => r.detail.clone()?,
            }
            .into(),
        )
    }
}

pub struct HistoryPage {
    table: Entity<TableState<HistoryTable>>,
    _subscriptions: Vec<Subscription>,
}

impl HistoryPage {
    pub fn new(service: Entity<Service>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let records = newest_first(service.read(cx).history());
        let table = cx.new(|cx| {
            TableState::new(HistoryTable { records }, window, cx)
                .sortable(false)
                .col_movable(false)
        });
        let observe = cx.observe(&service, |this: &mut Self, service, cx| {
            let records = newest_first(service.read(cx).history());
            this.table.update(cx, |table, cx| {
                if table.delegate().records != records {
                    table.delegate_mut().records = records;
                    cx.notify();
                }
            });
        });
        HistoryPage {
            table,
            _subscriptions: vec![observe],
        }
    }
}

impl Render for HistoryPage {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .p_6()
            .gap_4()
            .child(PageHeader::new("Storico").description(
                "Prima di ogni installazione WDU crea un punto di ripristino e salva il driver \
                 attuale: se il dispositivo non funziona, il driver precedente torna da solo.",
            ))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(DataTable::new(&self.table).stripe(true).bordered(true)),
            )
    }
}

fn newest_first(records: &[HistoryRecord]) -> Vec<HistoryRecord> {
    records.iter().rev().cloned().collect()
}
