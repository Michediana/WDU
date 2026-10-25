//! `wdu`: command line prototype for inventory, Windows Update, OEM catalogs,
//! matching and installation.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use tracing_subscriber::EnvFilter;
use wdu_core::engine::{self, ScanOptions};
use wdu_core::history::{History, HistoryRecord, Outcome};
use wdu_core::matching::{Plan, Policy, Proposal};
use wdu_core::model::{Brand, Device, MachineInfo, Snapshot};
use wdu_core::oem::{self, Catalog, CatalogQuery, EntryKind, Http};
use wdu_core::provider::Source;
use wdu_core::session::{InstallSession, SessionOptions};
use wdu_core::{install, inventory, ipc, machine, trust, wua};

#[derive(Parser)]
#[command(name = "wdu", version, about = "Windows driver updater prototype")]
struct Cli {
    /// More logging (-v info, -vv debug); RUST_LOG overrides it.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Always download catalogs again instead of revalidating the local cache.
    #[arg(long, global = true)]
    no_cache: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manufacturer, model and the identifier used for OEM catalogs.
    Machine {
        #[arg(long)]
        json: bool,
    },
    /// Installed devices and their drivers.
    Devices {
        #[command(flatten)]
        filter: DeviceFilter,
        #[arg(long)]
        json: bool,
    },
    /// Machine and devices as a JSON snapshot.
    Snapshot {
        /// Write to a file instead of stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Include devices that are not connected.
        #[arg(long)]
        all: bool,
    },
    /// Driver updates offered by Windows Update (slow: may take minutes).
    Wu {
        #[arg(long, default_value = wua::DEFAULT_DRIVER_CRITERIA)]
        criteria: String,
        #[command(flatten)]
        timeout: SearchTimeout,
        #[arg(long)]
        json: bool,
    },
    /// Policies (GPO, MDM, WSUS) that limit driver updates.
    WuPolicy {
        #[arg(long)]
        json: bool,
    },
    /// Download and install Windows Update drivers by update ID (needs
    /// administrator). Low level: no backup or rollback, use `update --only`.
    WuInstall {
        #[arg(required = true)]
        update_ids: Vec<String>,
        #[command(flatten)]
        timeout: SearchTimeout,
    },
    /// Entries of the OEM catalog for this machine, or for another model.
    Catalog {
        #[command(flatten)]
        target: CatalogTarget,
        /// Show every entry, not only drivers.
        #[arg(long)]
        all_kinds: bool,
        #[arg(long)]
        json: bool,
    },
    /// Match updates to devices and show what would be installed; with
    /// --all or --only, install it (needs administrator).
    Update(UpdateArgs),
    /// Past installations and their outcome.
    History {
        #[arg(long)]
        json: bool,
    },
    /// Check the Authenticode signature of a file.
    Verify { path: PathBuf },
    /// Send one request to the service, e.g. '{"type":"status"}'.
    Ipc { request: String },
    /// Devices, Windows Update drivers and OEM catalog in one pass.
    Report {
        #[command(flatten)]
        target: CatalogTarget,
        /// Skip the Windows Update search.
        #[arg(long)]
        no_wu: bool,
        #[command(flatten)]
        timeout: SearchTimeout,
        /// Write the full report as JSON.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

#[derive(Args)]
struct DeviceFilter {
    /// Include devices that are not connected.
    #[arg(long)]
    all: bool,
    /// Include devices Device Manager hides by default.
    #[arg(long)]
    hidden: bool,
}

#[derive(Args, Clone)]
struct CatalogTarget {
    /// Override the detected brand.
    #[arg(long, requires = "id")]
    brand: Option<Brand>,
    /// Dell System ID, HP platform ID or Lenovo machine type.
    #[arg(long, requires = "brand")]
    id: Option<String>,
    /// Target Windows 10 instead of the running OS.
    #[arg(long, conflicts_with = "win11")]
    win10: bool,
    /// Target Windows 11 instead of the running OS.
    #[arg(long)]
    win11: bool,
    /// Windows release, e.g. 24H2 (default: the running one).
    #[arg(long)]
    release: Option<String>,
    /// Lenovo: download every package descriptor for names, versions and hardware IDs.
    #[arg(long)]
    resolve: bool,
}

#[derive(Args, Clone, Copy)]
struct SearchTimeout {
    /// Abort the Windows Update search after this many seconds.
    #[arg(long = "wu-timeout", default_value_t = wua::DEFAULT_SEARCH_TIMEOUT.as_secs())]
    secs: u64,
}

impl SearchTimeout {
    fn duration(self) -> Duration {
        Duration::from_secs(self.secs)
    }
}

#[derive(Args)]
struct UpdateArgs {
    #[command(flatten)]
    target: CatalogTarget,
    /// Policy file (JSON): allow_classes, deny_classes, oem_exclusive.
    #[arg(long)]
    policy: Option<PathBuf>,
    /// Skip Windows Update.
    #[arg(long)]
    no_wu: bool,
    /// Skip the OEM catalog.
    #[arg(long)]
    no_oem: bool,
    #[command(flatten)]
    timeout: SearchTimeout,
    /// Never install, even with --all or --only.
    #[arg(long)]
    dry_run: bool,
    /// Install every proposed update.
    #[arg(long, conflicts_with = "only")]
    all: bool,
    /// Install only these proposals (Windows Update ID or OEM package ID).
    #[arg(long, num_args = 1..)]
    only: Vec<String>,
    /// Skip the System Restore point (driver backups are still made).
    #[arg(long)]
    no_restore_point: bool,
    /// Testing: treat every installation as failed, to exercise the rollback.
    #[arg(long, hide = true)]
    inject_failure: bool,
    /// List every skipped candidate, not only the counts.
    #[arg(long)]
    skipped: bool,
    /// Print the plan as JSON.
    #[arg(long)]
    json: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let level = match cli.verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level)),
        )
        .with_writer(std::io::stderr)
        .init();
    let http = || -> Result<Http> {
        let http = Http::new()?;
        Ok(if cli.no_cache {
            http
        } else {
            http.with_cache(user_dir().join("cache"))
        })
    };

    match cli.command {
        Command::Machine { json } => {
            let info = machine::read_machine_info()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&info)?);
            } else {
                print_machine(&info);
            }
        }
        Command::Devices { filter, json } => {
            let devices = list_devices(&filter)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&devices)?);
            } else {
                print_devices(&devices);
            }
        }
        Command::Snapshot { output, all } => {
            let devices = inventory::enumerate_devices(inventory::InventoryOptions {
                include_absent: all,
            })?;
            let snapshot = Snapshot::new(Some(machine::read_machine_info()?), devices);
            write_output(output.as_deref(), &snapshot.to_json()?)?;
        }
        Command::Wu {
            criteria,
            timeout,
            json,
        } => {
            let policy = wua::read_update_policy();
            for finding in &policy.findings {
                eprintln!("policy: {finding}");
            }
            let updates = wua::search_driver_updates(&criteria, timeout.duration())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&updates)?);
            } else {
                print_wu(&updates);
            }
        }
        Command::WuPolicy { json } => {
            let policy = wua::read_update_policy();
            if json {
                println!("{}", serde_json::to_string_pretty(&policy)?);
            } else if policy.findings.is_empty() {
                println!("No policy limits driver updates.");
            } else {
                for finding in &policy.findings {
                    println!("- {finding}");
                }
            }
        }
        Command::WuInstall {
            update_ids,
            timeout,
        } => {
            require_elevation()?;
            let history = history();
            let results = wua::install_driver_updates(&update_ids, timeout.duration())?;
            let mut reboot = false;
            for r in &results {
                let title = r.title.as_deref().unwrap_or(&r.update_id);
                let mut record =
                    HistoryRecord::new(Source::WindowsUpdate, &r.update_id, title, vec![]);
                fill_wu_record(&mut record, r);
                reboot |= r.reboot_required;
                print_record(&record);
                append(&history, &record);
            }
            print_reboot_notice(reboot);
        }
        Command::Catalog {
            target,
            all_kinds,
            json,
        } => {
            let info = machine::read_machine_info()?;
            let catalog = fetch_catalog(&http()?, &info, &target)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&catalog)?);
            } else {
                print_catalog(&catalog, all_kinds);
            }
        }
        Command::Update(args) => update(&http()?, &args)?,
        Command::History { json } => {
            let records = history().read()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&records)?);
            } else if records.is_empty() {
                println!("No installations recorded.");
            } else {
                records.iter().for_each(print_record);
            }
        }
        Command::Verify { path } => {
            let signature = trust::verify_file(&path)?;
            println!(
                "valid signature, signer: {}",
                signature.signer.as_deref().unwrap_or("-")
            );
        }
        Command::Ipc { request } => {
            let request: ipc::Request =
                serde_json::from_str(&request).context("invalid request")?;
            let response = ipc::call(&request)?;
            println!("{}", serde_json::to_string_pretty(&response)?);
        }
        Command::Report {
            target,
            no_wu,
            timeout,
            output,
        } => report(&http()?, &target, no_wu, timeout, output.as_deref())?,
    }
    Ok(())
}

/// Per-user data: the catalog cache.
fn user_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("WDU")
}

fn history() -> History {
    History::new(engine::data_dir().join("history.jsonl"))
}

fn append(history: &History, record: &HistoryRecord) {
    if let Err(err) = history.append(record) {
        eprintln!(
            "warning: cannot record in {}: {err:#}",
            history.path().display()
        );
    }
}

fn require_elevation() -> Result<()> {
    if !install::is_elevated() {
        bail!("installing drivers needs administrator rights: run from an elevated prompt");
    }
    Ok(())
}

fn list_devices(filter: &DeviceFilter) -> Result<Vec<Device>> {
    let mut devices = inventory::enumerate_devices(inventory::InventoryOptions {
        include_absent: filter.all,
    })?;
    if !filter.hidden {
        devices.retain(|d| !d.status.hidden);
    }
    devices.sort_by(|a, b| (&a.class, &a.name).cmp(&(&b.class, &b.name)));
    Ok(devices)
}

fn catalog_query(info: &MachineInfo, target: &CatalogTarget) -> Result<CatalogQuery> {
    let (brand, system_id) = match (target.brand, &target.id) {
        (Some(brand), Some(id)) => (brand, id.clone()),
        _ => {
            let query = CatalogQuery::for_machine(info)
                .context("pass --brand and --id to query another model")?;
            (query.brand, query.system_id)
        }
    };
    let windows11 = if target.win10 {
        false
    } else {
        target.win11 || info.os.is_windows11()
    };
    Ok(CatalogQuery {
        brand,
        system_id,
        windows11,
        display_version: target
            .release
            .clone()
            .or_else(|| info.os.display_version.clone()),
        resolve_packages: target.resolve,
    })
}

fn fetch_catalog(http: &Http, info: &MachineInfo, target: &CatalogTarget) -> Result<Catalog> {
    let query = catalog_query(info, target)?;
    tracing::info!(brand = %query.brand, id = %query.system_id, "fetching OEM catalog");
    oem::fetch_catalog(http, &query)
}

/// The catalog to use when the command line overrides the detected one.
fn catalog_override(target: &CatalogTarget) -> Result<Option<CatalogQuery>> {
    let overridden = target.brand.is_some()
        || target.win10
        || target.win11
        || target.release.is_some()
        || target.resolve;
    if !overridden {
        return Ok(None);
    }
    Ok(Some(catalog_query(&machine::read_machine_info()?, target)?))
}

fn update(http: &Http, args: &UpdateArgs) -> Result<()> {
    let policy = match &args.policy {
        Some(path) => Policy::from_json(
            &std::fs::read_to_string(path)
                .with_context(|| format!("cannot read {}", path.display()))?,
        )
        .with_context(|| format!("invalid policy {}", path.display()))?,
        None => Policy::default(),
    };
    let options = ScanOptions {
        oem: !args.no_oem,
        wu: !args.no_wu,
        wu_timeout: args.timeout.duration(),
        catalog: if args.no_oem {
            None
        } else {
            catalog_override(&args.target)?
        },
    };
    eprintln!("searching ...");
    let scan = engine::scan(http, &options, &policy)?;
    let plan = &scan.plan;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&scan)?);
    } else {
        for finding in &scan.wu_policy.findings {
            println!("policy: {finding}");
        }
        for e in &scan.source_errors {
            println!("{} unavailable: {}", e.source, e.error);
        }
        print_plan(plan, args.skipped);
    }

    if args.dry_run || (!args.all && args.only.is_empty()) {
        if !args.json && !args.dry_run && !plan.proposals.is_empty() {
            println!();
            println!("Dry run: nothing installed. Use --all or --only <ID> to install.");
        }
        return Ok(());
    }

    let selected: Vec<&Proposal> = if args.all {
        plan.proposals.iter().collect()
    } else {
        let unknown: Vec<&String> = args
            .only
            .iter()
            .filter(|id| scan.proposal(id).is_none())
            .collect();
        if !unknown.is_empty() {
            bail!("not among the proposed updates: {unknown:?}");
        }
        args.only
            .iter()
            .filter_map(|id| scan.proposal(id))
            .collect()
    };
    if selected.is_empty() {
        return Ok(());
    }
    require_elevation()?;
    if scan.wu_policy.device_install_restricted {
        println!("warning: device installation restrictions may refuse these drivers");
    }

    println!();
    let mut session = InstallSession::begin(
        http,
        SessionOptions {
            wu_timeout: args.timeout.duration(),
            restore_point: !args.no_restore_point,
            inject_failure: args.inject_failure,
            ..Default::default()
        },
    )?;
    let mut reboot = false;
    for p in selected {
        println!(
            "installing [{}] {} {}",
            p.candidate.source, p.candidate.id, p.candidate.title
        );
        let record = session.install(p);
        reboot |= record.reboot_required;
        print_record(&record);
    }
    for warning in session.finish() {
        println!("warning: {warning}");
    }
    print_reboot_notice(reboot);
    Ok(())
}

fn fill_wu_record(record: &mut HistoryRecord, result: &wua::WuInstallResult) {
    record.outcome = if result.succeeded() {
        Outcome::Succeeded
    } else {
        Outcome::Failed
    };
    record.reboot_required = result.reboot_required;
    record.detail = result.error.clone();
}

fn print_reboot_notice(reboot: bool) {
    if reboot {
        println!();
        println!("A restart is required to complete the installation. Restart when convenient.");
    }
}

#[derive(serde::Serialize)]
struct Report {
    snapshot: Snapshot,
    windows_update: Option<Vec<wua::WuDriverUpdate>>,
    windows_update_error: Option<String>,
    catalog: Option<Catalog>,
    catalog_error: Option<String>,
}

fn report(
    http: &Http,
    target: &CatalogTarget,
    no_wu: bool,
    timeout: SearchTimeout,
    output: Option<&Path>,
) -> Result<()> {
    let info = machine::read_machine_info()?;
    print_machine(&info);

    let devices = list_devices(&DeviceFilter {
        all: false,
        hidden: false,
    })?;
    println!();
    println!("== Devices ({})", devices.len());
    print_devices(&devices);

    let (windows_update, windows_update_error) = if no_wu {
        (None, None)
    } else {
        println!();
        println!("== Windows Update drivers");
        match wua::search_driver_updates(wua::DEFAULT_DRIVER_CRITERIA, timeout.duration()) {
            Ok(updates) => {
                print_wu(&updates);
                (Some(updates), None)
            }
            Err(err) => {
                println!("error: {err:#}");
                (None, Some(format!("{err:#}")))
            }
        }
    };

    println!();
    println!("== OEM catalog");
    let (catalog, catalog_error) = match fetch_catalog(http, &info, target) {
        Ok(catalog) => {
            print_catalog(&catalog, false);
            (Some(catalog), None)
        }
        Err(err) => {
            println!("{err:#}");
            (None, Some(format!("{err:#}")))
        }
    };

    if let Some(path) = output {
        let devices = inventory::enumerate_devices(Default::default())?;
        let report = Report {
            snapshot: Snapshot::new(Some(info), devices),
            windows_update,
            windows_update_error,
            catalog,
            catalog_error,
        };
        write_output(Some(path), &serde_json::to_string_pretty(&report)?)?;
    }
    Ok(())
}

fn write_output(path: Option<&Path>, content: &str) -> Result<()> {
    match path {
        Some(path) => {
            std::fs::write(path, content)
                .with_context(|| format!("cannot write {}", path.display()))?;
            eprintln!("written {}", path.display());
        }
        None => println!("{content}"),
    }
    Ok(())
}

fn or_dash(s: &Option<String>) -> &str {
    s.as_deref().unwrap_or("-")
}

/// Truncates to `width` characters so table columns stay aligned.
fn fit(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(width.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn print_machine(info: &MachineInfo) {
    println!("Manufacturer : {}", info.manufacturer);
    println!("Model        : {}", info.model);
    println!("SKU          : {}", or_dash(&info.system_sku));
    println!("Family       : {}", or_dash(&info.system_family));
    println!(
        "Baseboard    : {} {}",
        or_dash(&info.baseboard_manufacturer),
        or_dash(&info.baseboard_product)
    );
    println!("BIOS         : {}", or_dash(&info.bios_version));
    println!(
        "OS           : {} ({}, {}, {})",
        info.os.caption,
        info.os.version,
        or_dash(&info.os.display_version),
        info.os.architecture
    );
    println!(
        "OEM catalog  : {} {}",
        info.brand(),
        info.oem_system_id().as_deref().unwrap_or("-")
    );
}

fn print_devices(devices: &[Device]) {
    println!(
        "{:<16} {:<44} {:<18} {:<10} {:<24} HARDWARE ID",
        "CLASS", "NAME", "VERSION", "DATE", "PROVIDER"
    );
    for d in devices {
        let driver = d.driver.as_ref();
        let mut name = d.name.clone().unwrap_or_default();
        if d.status.problem_code != 0 {
            name = format!("[!{}] {name}", d.status.problem_code);
        }
        println!(
            "{:<16} {:<44} {:<18} {:<10} {:<24} {}",
            fit(or_dash(&d.class), 16),
            fit(&name, 44),
            driver.map_or("-", |dr| or_dash(&dr.version)),
            driver.map_or("-", |dr| or_dash(&dr.date)),
            fit(driver.map_or("-", |dr| or_dash(&dr.provider)), 24),
            d.hardware_ids.first().map_or("-", String::as_str),
        );
    }
}

fn print_wu(updates: &[wua::WuDriverUpdate]) {
    if updates.is_empty() {
        println!("No driver updates offered.");
        return;
    }
    for u in updates {
        println!("{}", u.title);
        println!(
            "    id {}  hwid {}  class {}  version {}  date {}  provider {}",
            u.update_id,
            or_dash(&u.hardware_id),
            or_dash(&u.driver_class),
            or_dash(&u.driver_version),
            or_dash(&u.driver_date),
            or_dash(&u.provider)
        );
    }
}

fn print_catalog(catalog: &Catalog, all_kinds: bool) {
    println!(
        "{} {} {}",
        catalog.brand,
        catalog.system_id,
        catalog.model_name.as_deref().unwrap_or("")
    );
    println!("source: {}", catalog.source_url);
    let shown: Vec<_> = catalog
        .entries
        .iter()
        .filter(|e| all_kinds || e.kind == EntryKind::Driver)
        .collect();
    println!(
        "{} entries ({} shown{})",
        catalog.entries.len(),
        shown.len(),
        if all_kinds { "" } else { ", drivers only" }
    );
    println!(
        "{:<10} {:<56} {:<20} {:<10} {:>5}",
        "ID", "NAME", "VERSION", "DATE", "HWIDS"
    );
    for e in shown {
        println!(
            "{:<10} {:<56} {:<20} {:<10} {:>5}",
            fit(&e.id, 10),
            fit(&e.name, 56),
            fit(or_dash(&e.version), 20),
            or_dash(&e.release_date),
            e.hardware_ids.len()
        );
    }
}

fn print_plan(plan: &Plan, list_skipped: bool) {
    println!();
    println!("== Proposed updates ({})", plan.proposals.len());
    for p in &plan.proposals {
        let c = &p.candidate;
        println!(
            "[{}] {}  {}  {}",
            c.source,
            c.id,
            c.title,
            or_dash(&c.driver_version)
        );
        for d in &p.devices {
            println!(
                "    {} ({})  {} {} -> {:?}  via {}",
                fit(or_dash(&d.name), 44),
                or_dash(&d.class),
                or_dash(&d.installed_version),
                or_dash(&d.installed_date),
                d.reason,
                d.matched_id
            );
        }
    }
    if plan.proposals.is_empty() {
        println!("Nothing to update.");
    }
    println!();
    println!("== Skipped ({})", plan.skipped.len());
    for (reason, count) in plan.skipped_by_reason() {
        println!("    {count:>4}  {reason}");
    }
    if list_skipped {
        for s in &plan.skipped {
            println!(
                "[{}] {}  {}  {}  {}",
                s.source,
                fit(&s.id, 38),
                fit(&s.title, 48),
                s.device.as_deref().unwrap_or("-"),
                s.reason
            );
        }
    }
}

fn print_record(r: &HistoryRecord) {
    let outcome = match r.outcome {
        Outcome::Succeeded if r.reboot_required => "ok, restart needed",
        Outcome::Succeeded => "ok",
        Outcome::Failed => "FAILED",
        Outcome::RolledBack => "FAILED, previous driver restored",
    };
    println!("{}  [{}] {}  {}  {outcome}", r.at, r.source, r.id, r.title);
    if let Some(detail) = &r.detail {
        println!("    {detail}");
    }
}
