//! `wdu`: command line prototype for inventory, Windows Update and OEM catalogs.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use tracing_subscriber::EnvFilter;
use wdu_core::model::{Brand, Device, MachineInfo, Snapshot};
use wdu_core::oem::{self, Catalog, CatalogQuery, EntryKind, Http};
use wdu_core::{inventory, machine, wua};

#[derive(Parser)]
#[command(name = "wdu", version, about = "Windows driver updater prototype")]
struct Cli {
    /// More logging (-v info, -vv debug); RUST_LOG overrides it.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

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
        #[arg(long)]
        json: bool,
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
    /// Devices, Windows Update drivers and OEM catalog in one pass.
    Report {
        #[command(flatten)]
        target: CatalogTarget,
        /// Skip the Windows Update search.
        #[arg(long)]
        no_wu: bool,
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
        Command::Wu { criteria, json } => {
            let updates = wua::search_driver_updates(&criteria)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&updates)?);
            } else {
                print_wu(&updates);
            }
        }
        Command::Catalog {
            target,
            all_kinds,
            json,
        } => {
            let info = machine::read_machine_info()?;
            let catalog = fetch_catalog(&info, &target)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&catalog)?);
            } else {
                print_catalog(&catalog, all_kinds);
            }
        }
        Command::Report {
            target,
            no_wu,
            output,
        } => report(&target, no_wu, output.as_deref())?,
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
            let brand = info.brand();
            if brand == Brand::Other {
                bail!(
                    "manufacturer `{}` has no supported OEM catalog; pass --brand and --id to query another model",
                    info.manufacturer
                );
            }
            let id = info
                .oem_system_id()
                .context("cannot determine the OEM system identifier of this machine")?;
            (brand, id)
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

fn fetch_catalog(info: &MachineInfo, target: &CatalogTarget) -> Result<Catalog> {
    let query = catalog_query(info, target)?;
    tracing::info!(brand = %query.brand, id = %query.system_id, "fetching OEM catalog");
    oem::fetch_catalog(&Http::new()?, &query)
}

#[derive(serde::Serialize)]
struct Report {
    snapshot: Snapshot,
    windows_update: Option<Vec<wua::WuDriverUpdate>>,
    windows_update_error: Option<String>,
    catalog: Option<Catalog>,
    catalog_error: Option<String>,
}

fn report(target: &CatalogTarget, no_wu: bool, output: Option<&std::path::Path>) -> Result<()> {
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
        match wua::search_driver_updates(wua::DEFAULT_DRIVER_CRITERIA) {
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
    let (catalog, catalog_error) = match fetch_catalog(&info, target) {
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

fn write_output(path: Option<&std::path::Path>, content: &str) -> Result<()> {
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
            "    hwid {}  class {}  date {}  provider {}",
            or_dash(&u.hardware_id),
            or_dash(&u.driver_class),
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
