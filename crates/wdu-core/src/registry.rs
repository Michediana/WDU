//! Read-only access to `HKEY_LOCAL_MACHINE` values.

use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, REG_ROUTINE_FLAGS, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::core::HSTRING;

fn get(subkey: &str, value: &str, flags: REG_ROUTINE_FLAGS, buf: Option<&mut [u8]>) -> Option<u32> {
    let mut size = buf.as_ref().map_or(0, |b| b.len() as u32);
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(subkey),
            &HSTRING::from(value),
            flags,
            None,
            buf.map(|b| b.as_mut_ptr().cast()),
            Some(&mut size),
        )
    }
    .ok()
    .ok()?;
    Some(size)
}

pub fn read_dword(subkey: &str, value: &str) -> Option<u32> {
    let mut buf = [0u8; 4];
    get(subkey, value, RRF_RT_REG_DWORD, Some(&mut buf))?;
    Some(u32::from_le_bytes(buf))
}

pub fn read_string(subkey: &str, value: &str) -> Option<String> {
    // First call only reports the required size.
    let size = get(subkey, value, RRF_RT_REG_SZ, None)?;
    let mut buf = vec![0u8; size as usize];
    let size = get(subkey, value, RRF_RT_REG_SZ, Some(&mut buf))?;
    buf.truncate(size as usize);
    let wide: Vec<u16> = buf
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    Some(String::from_utf16_lossy(&wide[..end]))
}
