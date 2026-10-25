//! Installation of OEM packages.
//!
//! Every package is verified before it touches the system: size and SHA-256
//! declared by the catalog, then a valid Authenticode signature from the OEM
//! itself. The package is then extracted; when it contains an INF for the
//! target device the INF is installed through PnP, otherwise the vendor
//! installer runs with its silent switches.
//!
//! Restore points, driver backup and rollback come with phase 5.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::model::Brand;
use crate::oem::xml::decode_text;
use crate::oem::{Http, extract_cab_all, sha256_file};
use crate::provider::{Candidate, Package};
use crate::trust;

const EXTRACT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Windows: success, restart needed to complete.
const ERROR_SUCCESS_REBOOT_REQUIRED: i32 = 3010;
/// Windows: success, the installer started a restart.
const ERROR_SUCCESS_REBOOT_INITIATED: i32 = 1641;
/// pnputil: package added to the store but not better for any device.
const ERROR_NO_MORE_ITEMS: i32 = 259;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum InstallMethod {
    /// INFs from the package installed through PnP.
    Inf { infs: Vec<String> },
    /// The vendor installer, run silently.
    Installer { command: String, exit_code: i32 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstallOutcome {
    pub method: InstallMethod,
    pub reboot_required: bool,
    pub signer: Option<String>,
}

/// Whether the process runs elevated, as installing drivers requires.
pub fn is_elevated() -> bool {
    unsafe { windows::Win32::UI::Shell::IsUserAnAdmin() }.as_bool()
}

/// Downloads, verifies and installs the package of an OEM candidate.
///
/// `hardware_ids` are those of the devices to update: INFs that mention none
/// of them are ignored. `work_dir` receives one folder per package.
pub fn install_package(
    http: &Http,
    candidate: &Candidate,
    hardware_ids: &[String],
    work_dir: &Path,
) -> Result<InstallOutcome> {
    let brand = candidate.source.brand().context("not an OEM candidate")?;
    let package = candidate
        .package
        .as_ref()
        .context("the catalog entry has no download")?;
    let expected_sha256 = package
        .sha256
        .as_deref()
        .context("the catalog declares no SHA-256: refusing an unverifiable package")?;
    let file_name = file_name_of(&package.url)?;
    let extension = Path::new(&file_name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if extension != "exe" && extension != "cab" {
        bail!("unsupported package type `{file_name}` (only signed .exe and .cab)");
    }

    let dir = work_dir.join(format!("{}-{}", candidate.source, safe_name(&candidate.id)));
    if dir.exists() {
        fs::remove_dir_all(&dir).with_context(|| format!("cannot clean {}", dir.display()))?;
    }
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let path = dir.join(&file_name);

    http.download_to(&package.url, &path)?;
    let size = fs::metadata(&path)?.len();
    if let Some(expected) = package.size
        && expected != size
    {
        bail!("size mismatch for {file_name}: catalog says {expected}, downloaded {size}");
    }
    let actual = sha256_file(&path)?;
    if !actual.eq_ignore_ascii_case(expected_sha256.trim()) {
        bail!("SHA-256 mismatch for {file_name}: expected {expected_sha256}, got {actual}");
    }
    let signature = trust::verify_file(&path)?;
    let signer = signature.signer.clone().unwrap_or_default();
    if !trust::is_expected_signer(brand, &signer) {
        bail!("{file_name} is signed by `{signer}`, not by {brand}");
    }
    tracing::info!(file = %file_name, signer, "package verified");

    let extracted = dir.join("extracted");
    fs::create_dir_all(&extracted)?;
    if let Err(err) = extract(brand, &extension, &path, &extracted) {
        tracing::warn!("cannot extract {file_name}, using its installer: {err:#}");
    }
    let infs = find_infs(&extracted, hardware_ids)?;

    let (method, reboot_required) = if !infs.is_empty() {
        install_infs(&infs)?
    } else if extension == "cab" {
        bail!("{file_name} contains no INF for the target devices");
    } else {
        run_installer(brand, package, &path, &extracted)?
    };
    Ok(InstallOutcome {
        method,
        reboot_required,
        signer: signature.signer,
    })
}

fn file_name_of(url: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(url)?;
    let name = parsed
        .path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|n| !n.is_empty())
        .with_context(|| format!("no file name in {url}"))?;
    Ok(safe_name(name))
}

/// Keeps names usable as a single path component.
fn safe_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    s.trim_start_matches('.').to_string()
}

/// Extraction switches differ per OEM; the extractors are the signed package itself.
fn extract(brand: Brand, extension: &str, package: &Path, dest: &Path) -> Result<()> {
    if extension == "cab" {
        extract_cab_all(package, dest)?;
        return Ok(());
    }
    let dest_arg = dest.display().to_string();
    let args: Vec<String> = match brand {
        // Dell Update Package: extract only the driver components.
        Brand::Dell => vec!["/s".into(), format!("/drivers={dest_arg}")],
        // HP SoftPaq.
        Brand::Hp => vec!["/s".into(), "/e".into(), "/f".into(), dest_arg],
        // Lenovo packages are Inno Setup.
        Brand::Lenovo => vec![
            "/VERYSILENT".into(),
            format!("/DIR={dest_arg}"),
            "/EXTRACT=YES".into(),
        ],
        Brand::Other => bail!("no extraction switches for this OEM"),
    };
    let mut cmd = Command::new(package);
    cmd.args(&args).current_dir(dest);
    let code = run(cmd, EXTRACT_TIMEOUT)?;
    tracing::debug!(code, "extractor exited");
    let extracted_any = fs::read_dir(dest)?.next().is_some();
    if !extracted_any {
        bail!("extractor exited with {code} and produced no files");
    }
    Ok(())
}

/// INFs below `dir` that name one of `hardware_ids`.
fn find_infs(dir: &Path, hardware_ids: &[String]) -> Result<Vec<PathBuf>> {
    let ids: Vec<String> = hardware_ids
        .iter()
        .map(|i| i.to_ascii_uppercase())
        .collect();
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("inf"))
            {
                let text = read_inf(&path)?.to_ascii_uppercase();
                if ids.iter().any(|id| contains_id(&text, id)) {
                    found.push(path);
                }
            }
        }
    }
    found.sort();
    Ok(found)
}

/// INFs are UTF-16 with BOM, UTF-8, or ANSI.
fn read_inf(path: &Path) -> Result<String> {
    let bytes = fs::read(path)?;
    Ok(decode_text(&bytes).unwrap_or_else(|_| bytes.iter().map(|&b| b as char).collect()))
}

/// Whole-ID match: `PCI\VEN_8086&DEV_43E4` must not match inside
/// `PCI\VEN_8086&DEV_43E4&SUBSYS_...`.
fn contains_id(text: &str, id: &str) -> bool {
    let part_of_id = |c: char| c.is_ascii_alphanumeric() || matches!(c, '&' | '_' | '\\');
    text.match_indices(id).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + id.len()..].chars().next();
        !before.is_some_and(part_of_id) && !after.is_some_and(part_of_id)
    })
}

fn system32(exe: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    Path::new(&root).join("System32").join(exe)
}

fn install_infs(infs: &[PathBuf]) -> Result<(InstallMethod, bool)> {
    let mut installed = Vec::new();
    let mut reboot = false;
    for inf in infs {
        let out = Command::new(system32("pnputil.exe"))
            .arg("/add-driver")
            .arg(inf)
            .arg("/install")
            .output()
            .context("cannot run pnputil")?;
        let code = out.status.code().unwrap_or(-1);
        tracing::info!(inf = %inf.display(), code, "pnputil");
        match code {
            0 => {}
            ERROR_SUCCESS_REBOOT_REQUIRED => reboot = true,
            // Staged in the driver store but not better for any device.
            ERROR_NO_MORE_ITEMS => continue,
            _ => bail!(
                "pnputil failed on {} with {code}: {}",
                inf.display(),
                String::from_utf8_lossy(&out.stdout).trim()
            ),
        }
        installed.push(inf.display().to_string());
    }
    if installed.is_empty() {
        bail!("PnP did not install the driver on any device: the installed one ranks higher");
    }
    Ok((InstallMethod::Inf { infs: installed }, reboot))
}

fn run_installer(
    brand: Brand,
    package: &Package,
    path: &Path,
    extracted: &Path,
) -> Result<(InstallMethod, bool)> {
    let dir = path.parent().context("package has no parent directory")?;
    let (cmd, command) = match brand {
        Brand::Dell => {
            let log = format!("/l={}", dir.join("install.log").display());
            let mut cmd = Command::new(path);
            cmd.args(["/s", &log]).current_dir(dir);
            (cmd, format!("{} /s {log}", path.display()))
        }
        // SilentInstall is relative to the extracted SoftPaq.
        Brand::Hp => {
            let silent = silent_command(package)?;
            if fs::read_dir(extracted)?.next().is_none() {
                bail!("the SoftPaq was not extracted, cannot run `{silent}`");
            }
            (shell(&silent, extracted), silent)
        }
        Brand::Lenovo => {
            let silent = replace_ignore_case(
                &silent_command(package)?,
                "%PACKAGEPATH%",
                &dir.display().to_string(),
            );
            (shell(&silent, dir), silent)
        }
        Brand::Other => bail!("no installer convention for this OEM"),
    };
    tracing::info!(command, "running vendor installer");
    let code = run(cmd, INSTALL_TIMEOUT)?;
    let reboot = match (brand, code) {
        (_, 0) => false,
        (_, ERROR_SUCCESS_REBOOT_REQUIRED | ERROR_SUCCESS_REBOOT_INITIATED) => true,
        // Dell Update Package: 2 = success, restart required.
        (Brand::Dell, 2) => true,
        // Dell: 3 = soft dependency error, typically the same or a newer version installed.
        (Brand::Dell, 3) => {
            bail!("Dell installer refused: same or newer version already installed")
        }
        _ => bail!("installer `{command}` failed with exit code {code}"),
    };
    Ok((
        InstallMethod::Installer {
            command,
            exit_code: code,
        },
        reboot,
    ))
}

fn silent_command(package: &Package) -> Result<String> {
    package
        .silent_install
        .clone()
        .filter(|s| !s.trim().is_empty())
        .context("the catalog declares no silent install command")
}

/// The catalog command line, run verbatim by `cmd.exe` in `dir`.
fn shell(command_line: &str, dir: &Path) -> Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new(system32("cmd.exe"));
    // /S: strip only the outer quotes, keep the command's own quoting.
    cmd.args(["/D", "/S", "/C"])
        .raw_arg(format!("\"{command_line}\""))
        .current_dir(dir);
    cmd
}

fn replace_ignore_case(s: &str, from: &str, to: &str) -> String {
    let upper = s.to_ascii_uppercase();
    let from = from.to_ascii_uppercase();
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (at, _) in upper.match_indices(&from) {
        out.push_str(&s[last..at]);
        out.push_str(to);
        last = at + from.len();
    }
    out.push_str(&s[last..]);
    out
}

/// Runs a process without console I/O, killing it after `timeout`.
fn run(mut cmd: Command, timeout: Duration) -> Result<i32> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("cannot start {:?}", cmd.get_program()))?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return status.code().context("process ended without an exit code");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            bail!(
                "{:?} still running after {}s, killed",
                cmd.get_program(),
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_id_matching() {
        let inf = "%DEV% = INSTALL, PCI\\VEN_8086&DEV_43E4&SUBSYS_0A201028\n\
                   %DEV2% = INSTALL,PCI\\VEN_8086&DEV_A0E8";
        assert!(contains_id(inf, "PCI\\VEN_8086&DEV_43E4&SUBSYS_0A201028"));
        assert!(!contains_id(inf, "PCI\\VEN_8086&DEV_43E4"));
        assert!(contains_id(inf, "PCI\\VEN_8086&DEV_A0E8"));
        assert!(!contains_id(inf, "VEN_8086&DEV_A0E8"));
    }

    #[test]
    fn finds_matching_infs() {
        let dir = std::env::temp_dir().join("wdu-find-infs");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain(
                "[Models]\r\n%D%=S,PCI\\VEN_8086&DEV_7360\r\n"
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes),
            )
            .collect();
        fs::write(dir.join("sub").join("wifi.inf"), utf16).unwrap();
        fs::write(dir.join("other.inf"), "PCI\\VEN_10EC&DEV_8168").unwrap();
        fs::write(dir.join("readme.txt"), "PCI\\VEN_8086&DEV_7360").unwrap();
        let found = find_infs(&dir, &["pci\\ven_8086&dev_7360".into()]).unwrap();
        assert_eq!(found, [dir.join("sub").join("wifi.inf")]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_and_placeholders() {
        assert_eq!(
            file_name_of("https://downloads.dell.com/FOLDER1/3/Dell-Wireless.EXE").unwrap(),
            "Dell-Wireless.EXE"
        );
        assert_eq!(safe_name("..\\..\\evil.exe"), "_.._evil.exe");
        assert_eq!(
            replace_ignore_case("%PackagePath%\\x.exe /S", "%PACKAGEPATH%", "C:\\p"),
            "C:\\p\\x.exe /S"
        );
    }
}
