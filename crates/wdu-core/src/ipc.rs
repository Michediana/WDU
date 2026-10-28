//! Protocol between the service and its clients (UI, CLI) over a local named pipe.
//!
//! One JSON object per line, each carrying the protocol version `v`. The pipe
//! rejects remote clients. Its DACL gives interactive users read and write
//! access but not `FILE_CREATE_PIPE_INSTANCE`, and the service creates the
//! first instance exclusively, so no other process can pose as the service.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::io::FromRawHandle;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::core::HSTRING;

use crate::driver_store::DriverPackage;
use crate::engine::ScanResult;
use crate::history::HistoryRecord;
use crate::matching::Policy;
use crate::model::Device;

pub const PIPE_NAME: &str = r"\\.\pipe\wdu";
/// Bumped on every incompatible change to [`Request`] or [`Response`].
pub const PROTOCOL_VERSION: u32 = 1;

const MAX_REQUEST: u64 = 1 << 20;
const MAX_RESPONSE: u64 = 64 << 20;
const ERROR_PIPE_BUSY: i32 = 231;

/// SYSTEM, Administrators and the pipe owner (the service, or the user
/// running it from a console): full access. Interactive users: generic read
/// and write minus `FILE_CREATE_PIPE_INSTANCE` (0x4).
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)(A;;0x12019b;;;IU)";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello,
    Status,
    /// Start a scan in the background; poll [`Request::Status`].
    Scan,
    /// The result of the last scan.
    Plan,
    /// Install these proposals of the last scan (approval). Runs in the background.
    Install {
        ids: Vec<String>,
    },
    History,
    /// Present devices and their installed drivers.
    Devices,
    /// Third-party packages in the driver store, with superseded versions.
    DriverPackages,
    /// Remove these packages from the driver store. Only older versions no
    /// connected device uses are accepted. Runs in the background.
    RemovePackages {
        names: Vec<String>,
    },
    GetConfig,
    SetConfig {
        config: ServiceConfig,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Hello { version: String, protocol: u32 },
    Status { status: ServiceStatus },
    Plan { scan: Option<Box<ScanResult>> },
    History { records: Vec<HistoryRecord> },
    Devices { devices: Vec<Device> },
    DriverPackages { packages: Vec<DriverPackage> },
    Config { config: ServiceConfig },
    Accepted,
    Error { message: String },
}

impl Response {
    pub fn error(err: impl std::fmt::Display) -> Self {
        Response::Error {
            message: err.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    #[default]
    Idle,
    Scanning,
    Installing,
    Removing,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub state: ServiceState,
    /// Proposal being installed, or package being removed.
    pub current: Option<String>,
    /// RFC 3339, UTC.
    pub last_scan: Option<String>,
    pub next_scan: Option<String>,
    pub last_error: Option<String>,
    /// An installation needs a restart; the service never restarts by itself.
    pub reboot_required: bool,
    /// Non-fatal problems of the last operation (e.g. System Restore off).
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServiceConfig {
    /// Hours between automatic scans; 0 turns them off.
    pub check_interval_hours: u32,
    /// After an automatic scan, install every proposal without approval.
    pub auto_install: bool,
    pub windows_update: bool,
    pub oem_catalog: bool,
    pub policy: Policy,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        ServiceConfig {
            check_interval_hours: 24,
            auto_install: false,
            windows_update: true,
            oem_catalog: true,
            policy: Policy::default(),
        }
    }
}

impl ServiceConfig {
    pub fn validate(&self) -> Result<()> {
        if self.check_interval_hours > 24 * 31 {
            bail!("the check interval must be at most 744 hours");
        }
        let classes = self
            .policy
            .allow_classes
            .iter()
            .chain(&self.policy.deny_classes);
        for class in classes {
            let ok = !class.trim().is_empty()
                && class.len() <= 64
                && class
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "{}-_ ".contains(c));
            if !ok {
                bail!("invalid device class `{class}`");
            }
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct Envelope<'a, T> {
    v: u32,
    #[serde(flatten)]
    body: &'a T,
}

fn encode<T: Serialize>(body: &T) -> Result<Vec<u8>> {
    let mut line = serde_json::to_vec(&Envelope {
        v: PROTOCOL_VERSION,
        body,
    })?;
    line.push(b'\n');
    Ok(line)
}

/// Checks the version before looking at the body.
fn decode<T: DeserializeOwned>(line: &str) -> Result<T> {
    let mut value: serde_json::Value = serde_json::from_str(line).context("malformed message")?;
    let version = value
        .as_object_mut()
        .and_then(|o| o.remove("v"))
        .and_then(|v| v.as_u64())
        .context("message without protocol version")?;
    if version != u64::from(PROTOCOL_VERSION) {
        bail!("protocol version {version} not supported (expected {PROTOCOL_VERSION})");
    }
    serde_json::from_value(value).context("unknown or invalid message")
}

/// Reads one line of at most `max` bytes; `None` at end of stream.
fn read_line(reader: &mut impl BufRead, max: u64) -> Result<Option<String>> {
    let mut line = String::new();
    let n = reader.by_ref().take(max).read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if !line.ends_with('\n') && n as u64 >= max {
        bail!("message longer than {max} bytes");
    }
    Ok(Some(line))
}

/// Sends one request to the service and waits for the answer.
pub fn call(request: &Request) -> Result<Response> {
    call_on(PIPE_NAME, request)
}

pub fn call_on(pipe: &str, request: &Request) -> Result<Response> {
    let file = open(pipe)?;
    (&file).write_all(&encode(request)?)?;
    let line = read_line(&mut BufReader::new(&file), MAX_RESPONSE)?
        .context("the service closed the connection")?;
    decode(&line)
}

fn open(pipe: &str) -> Result<File> {
    for _ in 0..50 {
        match OpenOptions::new().read(true).write(true).open(pipe) {
            Ok(file) => return Ok(file),
            Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!("the WDU service is not running")
            }
            Err(err) => return Err(err).with_context(|| format!("cannot connect to {pipe}")),
        }
    }
    bail!("the WDU service is busy")
}

/// Unblocks [`serve_on`] so it can see its stop flag.
pub fn wake(pipe: &str) {
    let _ = OpenOptions::new().read(true).write(true).open(pipe);
}

struct PipeSecurity(PSECURITY_DESCRIPTOR);

impl PipeSecurity {
    fn new() -> Result<Self> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(PIPE_SDDL),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .context("invalid pipe security descriptor")?;
        Ok(PipeSecurity(sd))
    }

    fn create(&self, pipe: &str, first: bool) -> Result<HANDLE> {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: false.into(),
        };
        let mut open_mode = PIPE_ACCESS_DUPLEX;
        if first {
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let handle = unsafe {
            CreateNamedPipeW(
                &HSTRING::from(pipe),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                Some(&attributes),
            )
        };
        if handle.is_invalid() {
            let err = windows::core::Error::from_thread();
            if first {
                return Err(err).context(format!(
                    "cannot create {pipe}: is another instance of the service running?"
                ));
            }
            return Err(err).context(format!("cannot create {pipe}"));
        }
        Ok(handle)
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
    }
}

/// Serves clients on [`PIPE_NAME`] until `stop` is set (then call [`wake`]).
pub fn serve(
    handler: impl Fn(Request) -> Response + Send + Sync + 'static,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    serve_on(PIPE_NAME, handler, stop)
}

pub fn serve_on(
    pipe: &str,
    handler: impl Fn(Request) -> Response + Send + Sync + 'static,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let handler = Arc::new(handler);
    let security = PipeSecurity::new()?;
    let mut first = true;
    loop {
        let handle = security.create(pipe, first)?;
        first = false;
        let connected = unsafe { ConnectNamedPipe(handle, None) };
        if stop.load(Ordering::SeqCst) {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Ok(());
        }
        if let Err(err) = connected
            && err.code() != ERROR_PIPE_CONNECTED.to_hresult()
        {
            tracing::warn!("pipe connection failed: {err}");
            unsafe {
                let _ = CloseHandle(handle);
            }
            continue;
        }
        let file = unsafe { File::from_raw_handle(handle.0) };
        let handler = handler.clone();
        std::thread::spawn(move || {
            if let Err(err) = serve_client(&file, &*handler) {
                tracing::debug!("client dropped: {err:#}");
            }
        });
    }
}

fn serve_client(file: &File, handler: &(dyn Fn(Request) -> Response + Send + Sync)) -> Result<()> {
    let mut reader = BufReader::new(file);
    while let Some(line) = read_line(&mut reader, MAX_REQUEST)? {
        let response = match decode::<Request>(&line) {
            Ok(request) => handler(request),
            Err(err) => Response::error(format!("{err:#}")),
        };
        let mut writer = file;
        writer.write_all(&encode(&response)?)?;
        writer.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_response_round_trip() {
        let pipe = format!(r"\\.\pipe\wdu-test-{}", std::process::id());
        let stop = Arc::new(AtomicBool::new(false));
        let server = {
            let (pipe, stop) = (pipe.clone(), stop.clone());
            std::thread::spawn(move || {
                serve_on(
                    &pipe,
                    |request| match request {
                        Request::Hello => Response::Hello {
                            version: "test".into(),
                            protocol: PROTOCOL_VERSION,
                        },
                        Request::Install { ids } => Response::error(ids.join(",")),
                        _ => Response::Accepted,
                    },
                    stop,
                )
            })
        };
        // The server needs a moment to create the pipe.
        let mut hello = None;
        for _ in 0..50 {
            match call_on(&pipe, &Request::Hello) {
                Ok(r) => {
                    hello = Some(r);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        assert_eq!(
            hello.unwrap(),
            Response::Hello {
                version: "test".into(),
                protocol: 1
            }
        );
        assert_eq!(
            call_on(
                &pipe,
                &Request::Install {
                    ids: vec!["a".into(), "b".into()]
                }
            )
            .unwrap(),
            Response::error("a,b")
        );

        // A second server cannot take over the same pipe.
        let squatter = serve_on(
            &pipe,
            |_| Response::Accepted,
            Arc::new(AtomicBool::new(true)),
        );
        assert!(squatter.is_err());

        stop.store(true, Ordering::SeqCst);
        wake(&pipe);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn version_is_checked() {
        let line = String::from_utf8(encode(&Request::Status).unwrap()).unwrap();
        assert_eq!(line, "{\"v\":1,\"type\":\"status\"}\n");
        assert_eq!(decode::<Request>(&line).unwrap(), Request::Status);
        let err = decode::<Request>(r#"{"v":2,"type":"status"}"#).unwrap_err();
        assert!(err.to_string().contains("version 2"));
        assert!(decode::<Request>(r#"{"type":"status"}"#).is_err());
        assert!(decode::<Request>(r#"{"v":1,"type":"format_disk"}"#).is_err());
    }

    #[test]
    fn config_validation() {
        assert!(ServiceConfig::default().validate().is_ok());
        let mut c = ServiceConfig {
            check_interval_hours: 10_000,
            ..Default::default()
        };
        assert!(c.validate().is_err());
        c.check_interval_hours = 24;
        c.policy.deny_classes = vec![
            "Display".into(),
            "{4d36e968-e325-11ce-bfc1-08002be10318}".into(),
        ];
        assert!(c.validate().is_ok());
        c.policy.deny_classes = vec!["x\"; drop".into()];
        assert!(c.validate().is_err());
    }
}
