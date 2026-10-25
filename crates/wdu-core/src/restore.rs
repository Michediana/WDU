//! System Restore points around an installation session.
//!
//! Windows may have System Restore turned off, or skip a point created within
//! 24 hours of the previous one (`SystemRestorePointCreationFrequency`), so a
//! restore point is never the only safety net: drivers are also backed up.

use anyhow::{Result, bail};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Restore::{
    BEGIN_SYSTEM_CHANGE, CANCELLED_OPERATION, DEVICE_DRIVER_INSTALL, END_SYSTEM_CHANGE,
    RESTOREPOINTINFO_EVENT_TYPE, RESTOREPOINTINFO_TYPE, RESTOREPOINTINFOW, SRSetRestorePointW,
    STATEMGRSTATUS,
};

/// An open restore point: changes made until [`RestorePoint::end`] belong to it.
pub struct RestorePoint {
    sequence: i64,
    open: bool,
}

fn set(
    event: RESTOREPOINTINFO_EVENT_TYPE,
    kind: RESTOREPOINTINFO_TYPE,
    sequence: i64,
    description: &str,
) -> Result<i64> {
    // At most 64 characters are kept; the array must stay NUL-terminated.
    let mut text = [0u16; 256];
    for (dst, src) in text.iter_mut().zip(description.encode_utf16().take(63)) {
        *dst = src;
    }
    // Packed structs: fields are only written and read by value.
    let info = RESTOREPOINTINFOW {
        dwEventType: event,
        dwRestorePtType: kind,
        llSequenceNumber: sequence,
        szDescription: text,
    };
    let mut status = STATEMGRSTATUS::default();
    let ok = unsafe { SRSetRestorePointW(&info, &mut status) };
    let (code, sequence) = (status.nStatus, status.llSequenceNumber);
    if !ok.as_bool() || code != ERROR_SUCCESS {
        let err = windows::core::Error::from_hresult(code.to_hresult());
        bail!(
            "System Restore refused the restore point: {}",
            err.message()
        );
    }
    Ok(sequence)
}

impl RestorePoint {
    pub fn begin(description: &str) -> Result<Self> {
        let sequence = set(BEGIN_SYSTEM_CHANGE, DEVICE_DRIVER_INSTALL, 0, description)?;
        tracing::info!(sequence, "restore point started");
        Ok(RestorePoint {
            sequence,
            open: true,
        })
    }

    /// Closes the restore point and keeps it.
    pub fn end(mut self) -> Result<()> {
        self.open = false;
        set(END_SYSTEM_CHANGE, DEVICE_DRIVER_INSTALL, self.sequence, "")?;
        Ok(())
    }

    /// Closes the restore point and discards it: nothing was changed.
    pub fn cancel(mut self) -> Result<()> {
        self.open = false;
        set(END_SYSTEM_CHANGE, CANCELLED_OPERATION, self.sequence, "")?;
        Ok(())
    }
}

impl Drop for RestorePoint {
    fn drop(&mut self) {
        if self.open {
            let _ = set(END_SYSTEM_CHANGE, DEVICE_DRIVER_INSTALL, self.sequence, "");
        }
    }
}
