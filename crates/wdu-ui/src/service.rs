//! The service as the window sees it: its status, polled, and the last scan
//! and history, reloaded when a scan or an installation ends. Every call
//! runs on the background executor, since a call may wait for the pipe.

use std::time::{Duration, Instant};

use gpui_kit::{App, AppContext as _, Context, EventEmitter, Task};
use wdu_core::engine::{ScanResult, StepState};
use wdu_core::history::HistoryRecord;
use wdu_core::ipc::{self, Request, Response, ServiceState, ServiceStatus};

const POLL: Duration = Duration::from_secs(2);
/// While a scan runs, so each step shows as it starts.
const SCAN_POLL: Duration = Duration::from_millis(500);

/// One request to the service, off the UI thread. A service error comes
/// back as `Err` like a broken pipe does.
pub fn call(request: Request, cx: &App) -> Task<Result<Response, String>> {
    cx.background_spawn(async move {
        match ipc::call(&request) {
            Ok(Response::Error { message }) => Err(message),
            Ok(response) => Ok(response),
            Err(err) => Err(format!("{err:#}")),
        }
    })
}

/// A scan, an installation or a removal just ended: what they change must
/// be read again.
pub struct WorkFinished {
    /// The service's error for that work, if any.
    pub error: Option<String>,
}

pub struct Service {
    status: Option<ServiceStatus>,
    offline: Option<String>,
    /// `None` until the first plan arrives; then the last scan, if any.
    scan: Option<Option<Box<ScanResult>>>,
    history: Vec<HistoryRecord>,
    /// The running scan step, and when this window first saw it.
    step_since: Option<(usize, Instant)>,
    _poll: Task<()>,
}

impl EventEmitter<WorkFinished> for Service {}

impl Service {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let poll = cx.spawn(async move |this, cx| {
            loop {
                let status = cx.update(|cx| call(Request::Status, cx)).await;
                let Ok(interval) = this.update(cx, |this, cx| {
                    this.apply_status(status, cx);
                    this.poll_interval()
                }) else {
                    return;
                };
                cx.background_executor().timer(interval).await;
            }
        });
        let mut service = Service {
            status: None,
            offline: None,
            scan: None,
            history: Vec::new(),
            step_since: None,
            _poll: poll,
        };
        service.reload_plan(cx);
        service.reload_history(cx);
        service
    }

    pub fn status(&self) -> Option<&ServiceStatus> {
        self.status.as_ref()
    }

    /// Why the service cannot be reached, while it cannot.
    pub fn offline(&self) -> Option<&str> {
        self.offline.as_deref()
    }

    pub fn is_online(&self) -> bool {
        self.offline.is_none() && self.status.is_some()
    }

    /// Scanning, installing or removing; also before the first status.
    pub fn is_busy(&self) -> bool {
        self.status
            .as_ref()
            .is_none_or(|s| s.state != ServiceState::Idle)
    }

    pub fn state(&self) -> Option<ServiceState> {
        self.status.as_ref().map(|s| s.state)
    }

    /// `None` until the first plan arrives.
    pub fn scan(&self) -> Option<Option<&ScanResult>> {
        self.scan.as_ref().map(|s| s.as_deref())
    }

    pub fn history(&self) -> &[HistoryRecord] {
        &self.history
    }

    /// How long the running scan step has been running.
    pub fn step_elapsed(&self, step_ix: usize) -> Option<Duration> {
        self.step_since
            .filter(|(running, _)| *running == step_ix)
            .map(|(_, since)| since.elapsed())
    }

    fn poll_interval(&self) -> Duration {
        if self.state() == Some(ServiceState::Scanning) {
            SCAN_POLL
        } else {
            POLL
        }
    }

    /// Asks for the status now rather than at the next poll, after a
    /// command the service just accepted.
    pub fn refresh_status(&mut self, cx: &mut Context<Self>) {
        let task = call(Request::Status, cx);
        cx.spawn(async move |this, cx| {
            let status = task.await;
            this.update(cx, |this, cx| this.apply_status(status, cx))
                .ok();
        })
        .detach();
    }

    pub fn reload_plan(&mut self, cx: &mut Context<Self>) {
        let task = call(Request::Plan, cx);
        cx.spawn(async move |this, cx| {
            if let Ok(Response::Plan { scan }) = task.await {
                this.update(cx, |this, cx| {
                    this.scan = Some(scan);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    pub fn reload_history(&mut self, cx: &mut Context<Self>) {
        let task = call(Request::History, cx);
        cx.spawn(async move |this, cx| {
            if let Ok(Response::History { records }) = task.await {
                this.update(cx, |this, cx| {
                    this.history = records;
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn apply_status(&mut self, status: Result<Response, String>, cx: &mut Context<Self>) {
        let status = match status {
            Ok(Response::Status { status }) => status,
            Ok(_) => return,
            Err(err) => {
                self.offline = Some(err);
                cx.notify();
                return;
            }
        };
        self.offline = None;
        let was_busy = self
            .status
            .as_ref()
            .is_some_and(|s| s.state != ServiceState::Idle);
        let finished = was_busy && status.state == ServiceState::Idle;

        let running = status
            .scan_steps
            .iter()
            .rposition(|s| s.state == StepState::Running);
        self.step_since = match (running, self.step_since) {
            (Some(i), Some((j, since))) if i == j => Some((i, since)),
            (Some(i), _) => Some((i, Instant::now())),
            (None, _) => None,
        };
        let error = status.last_error.clone();
        self.status = Some(status);
        if finished {
            self.reload_plan(cx);
            self.reload_history(cx);
            cx.emit(WorkFinished { error });
        }
        cx.notify();
    }
}
