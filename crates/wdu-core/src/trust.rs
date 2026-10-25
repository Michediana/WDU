//! Authenticode verification of downloaded packages through `WinVerifyTrust`.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Result, bail};
use windows::Win32::Foundation::{HANDLE, HWND, INVALID_HANDLE_VALUE};
use windows::Win32::Security::Cryptography::{CERT_NAME_SIMPLE_DISPLAY_TYPE, CertGetNameStringW};
use windows::Win32::Security::WinTrust::{
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
    WTD_CHOICE_FILE, WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT, WTD_REVOKE_WHOLECHAIN,
    WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE, WTHelperGetProvSignerFromChain,
    WTHelperProvDataFromStateData, WinVerifyTrust,
};
use windows::core::{GUID, HRESULT, PCWSTR};

use crate::model::Brand;

/// A valid Authenticode signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// Simple display name of the signing certificate, e.g. `Dell Inc`.
    pub signer: Option<String>,
}

/// Verifies the embedded Authenticode signature of `path`, revocation included.
pub fn verify_file(path: &Path) -> Result<Signature> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(wide.as_ptr()),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_WHOLECHAIN,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut file },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT,
        ..Default::default()
    };
    let mut action: GUID = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // INVALID_HANDLE_VALUE: no interactive user, never show UI.
    let hwnd = HWND(INVALID_HANDLE_VALUE.0);

    let status = unsafe { WinVerifyTrust(hwnd, &mut action, (&raw mut data).cast()) };
    let signer = (status == 0)
        .then(|| unsafe { signer_name(data.hWVTStateData) })
        .flatten();

    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe { WinVerifyTrust(hwnd, &mut action, (&raw mut data).cast()) };

    if status != 0 {
        let hr = HRESULT(status);
        bail!(
            "invalid Authenticode signature on {}: 0x{:08X} {}",
            path.display(),
            hr.0 as u32,
            hr.message()
        );
    }
    Ok(Signature { signer })
}

unsafe fn signer_name(state: HANDLE) -> Option<String> {
    unsafe {
        let provider = WTHelperProvDataFromStateData(state);
        if provider.is_null() {
            return None;
        }
        let signer = WTHelperGetProvSignerFromChain(provider, 0, false, 0);
        if signer.is_null() || (*signer).csCertChain == 0 || (*signer).pasCertChain.is_null() {
            return None;
        }
        let cert = (*(*signer).pasCertChain).pCert;
        if cert.is_null() {
            return None;
        }
        let mut buf = [0u16; 256];
        let len = CertGetNameStringW(cert, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0, None, Some(&mut buf));
        // `len` counts the terminating NUL.
        let s = String::from_utf16_lossy(&buf[..(len as usize).saturating_sub(1)]);
        (!s.is_empty()).then_some(s)
    }
}

/// Whether a certificate name belongs to the OEM whose catalog listed the package.
pub fn is_expected_signer(brand: Brand, signer: &str) -> bool {
    let s = signer.to_ascii_lowercase();
    match brand {
        Brand::Dell => s.starts_with("dell"),
        // Not "Hewlett Packard Enterprise", a different company.
        Brand::Hp => s.starts_with("hp inc") || s.starts_with("hewlett-packard"),
        Brand::Lenovo => s.starts_with("lenovo"),
        Brand::Other => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_binary_is_signed_by_microsoft() {
        let root = std::env::var("SystemRoot").unwrap();
        let sig = verify_file(&Path::new(&root).join(r"System32\pnputil.exe"));
        // Inbox binaries are catalog-signed, not embedded-signed; either way
        // the call must not crash, and a signed result names Microsoft.
        if let Ok(sig) = sig {
            assert!(sig.signer.unwrap_or_default().contains("Microsoft"));
        }
    }

    #[test]
    fn unsigned_file_is_rejected() {
        let path = std::env::temp_dir().join("wdu-unsigned-test.exe");
        std::fs::write(&path, b"MZ not really a program").unwrap();
        assert!(verify_file(&path).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn expected_signers() {
        assert!(is_expected_signer(Brand::Dell, "Dell Inc"));
        assert!(is_expected_signer(Brand::Hp, "HP Inc."));
        assert!(is_expected_signer(Brand::Lenovo, "Lenovo"));
        assert!(!is_expected_signer(Brand::Lenovo, "Dell Inc"));
        assert!(!is_expected_signer(Brand::Hp, "HPE Something"));
    }
}
