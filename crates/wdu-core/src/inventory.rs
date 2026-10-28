//! Device enumeration through SetupAPI.

use anyhow::{Context, Result};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_ALLCLASSES, DIGCF_PRESENT, HDEVINFO, SETUP_DI_GET_CLASS_DEVS_FLAGS, SP_DEVINFO_DATA,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDevicePropertyW,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_Class, DEVPKEY_Device_ClassGuid, DEVPKEY_Device_CompatibleIds,
    DEVPKEY_Device_DevNodeStatus, DEVPKEY_Device_DeviceDesc, DEVPKEY_Device_DriverDate,
    DEVPKEY_Device_DriverDesc, DEVPKEY_Device_DriverInfPath, DEVPKEY_Device_DriverProvider, DEVPKEY_Device_DriverVersion,
    DEVPKEY_Device_FriendlyName, DEVPKEY_Device_HardwareIds, DEVPKEY_Device_InstanceId,
    DEVPKEY_Device_IsPresent, DEVPKEY_Device_Manufacturer, DEVPKEY_Device_ProblemCode, DEVPROPTYPE,
};
use windows::Win32::Foundation::DEVPROPKEY;
use windows::core::PCWSTR;

use crate::model::{Device, DeviceStatus, DriverInfo};

/// `DN_NO_SHOW_IN_DM` from cfg.h: the device is hidden in Device Manager.
const DN_NO_SHOW_IN_DM: u32 = 0x4000_0000;

#[derive(Debug, Clone, Copy, Default)]
pub struct InventoryOptions {
    /// Also list devices that were installed once but are not connected now.
    pub include_absent: bool,
}

/// Enumerates all devices of all classes.
pub fn enumerate_devices(options: InventoryOptions) -> Result<Vec<Device>> {
    let mut flags: SETUP_DI_GET_CLASS_DEVS_FLAGS = DIGCF_ALLCLASSES;
    if !options.include_absent {
        flags |= DIGCF_PRESENT;
    }
    let set = DevInfoSet(
        unsafe { SetupDiGetClassDevsW(None, PCWSTR::null(), None, flags) }
            .context("SetupDiGetClassDevsW failed")?,
    );

    let mut devices = Vec::new();
    for index in 0.. {
        let mut data = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        if unsafe { SetupDiEnumDeviceInfo(set.0, index, &mut data) }.is_err() {
            // ERROR_NO_MORE_ITEMS
            break;
        }
        match read_device(&set, &data) {
            Some(device) => devices.push(device),
            None => tracing::warn!(index, "skipping device without instance ID"),
        }
    }
    Ok(devices)
}

/// Re-reads one device, e.g. to check it after an installation.
pub fn find_device(instance_id: &str) -> Result<Option<Device>> {
    Ok(enumerate_devices(InventoryOptions {
        include_absent: true,
    })?
    .into_iter()
    .find(|d| d.instance_id.eq_ignore_ascii_case(instance_id)))
}

struct DevInfoSet(HDEVINFO);

impl Drop for DevInfoSet {
    fn drop(&mut self) {
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

fn read_device(set: &DevInfoSet, data: &SP_DEVINFO_DATA) -> Option<Device> {
    let prop = |key: &DEVPROPKEY| read_property(set.0, data, key);
    let string = |key: &DEVPROPKEY| prop(key).and_then(|p| p.into_string());

    let instance_id = string(&DEVPKEY_Device_InstanceId)?;
    let present = prop(&DEVPKEY_Device_IsPresent)
        .and_then(|p| p.into_bool())
        .unwrap_or(true);
    let node_status = prop(&DEVPKEY_Device_DevNodeStatus)
        .and_then(|p| p.into_u32())
        .unwrap_or(0);

    let inf_name = string(&DEVPKEY_Device_DriverInfPath);
    let driver = inf_name.is_some().then(|| DriverInfo {
        description: string(&DEVPKEY_Device_DriverDesc),
        provider: string(&DEVPKEY_Device_DriverProvider),
        version: string(&DEVPKEY_Device_DriverVersion),
        date: prop(&DEVPKEY_Device_DriverDate)
            .and_then(|p| p.into_u64())
            .and_then(crate::time::filetime_to_date),
        inf_name,
    });

    Some(Device {
        instance_id,
        name: string(&DEVPKEY_Device_FriendlyName).or_else(|| string(&DEVPKEY_Device_DeviceDesc)),
        hardware_ids: prop(&DEVPKEY_Device_HardwareIds)
            .map(|p| p.into_string_list())
            .unwrap_or_default(),
        compatible_ids: prop(&DEVPKEY_Device_CompatibleIds)
            .map(|p| p.into_string_list())
            .unwrap_or_default(),
        class: string(&DEVPKEY_Device_Class),
        class_guid: prop(&DEVPKEY_Device_ClassGuid).and_then(|p| p.into_guid()),
        manufacturer: string(&DEVPKEY_Device_Manufacturer),
        driver,
        status: DeviceStatus {
            present,
            hidden: node_status & DN_NO_SHOW_IN_DM != 0,
            problem_code: prop(&DEVPKEY_Device_ProblemCode)
                .and_then(|p| p.into_u32())
                .unwrap_or(0),
        },
    })
}

/// Raw property value; the type tag is kept for debugging but conversions
/// rely on the buffer layout the documented property type implies.
struct Property {
    #[allow(dead_code)]
    ty: DEVPROPTYPE,
    buf: Vec<u8>,
}

fn read_property(set: HDEVINFO, data: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<Property> {
    let mut ty = DEVPROPTYPE::default();
    let mut size = 0u32;
    // First call only reports the required size.
    let _ = unsafe { SetupDiGetDevicePropertyW(set, data, key, &mut ty, None, Some(&mut size), 0) };
    if size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    unsafe {
        SetupDiGetDevicePropertyW(set, data, key, &mut ty, Some(&mut buf), Some(&mut size), 0)
    }
    .ok()?;
    buf.truncate(size as usize);
    Some(Property { ty, buf })
}

impl Property {
    fn utf16(&self) -> Vec<u16> {
        self.buf
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    fn into_string(self) -> Option<String> {
        let wide = self.utf16();
        let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
        let s = String::from_utf16_lossy(&wide[..end]);
        (!s.is_empty()).then_some(s)
    }

    /// REG_MULTI_SZ style: NUL-separated, double-NUL terminated.
    fn into_string_list(self) -> Vec<String> {
        self.utf16()
            .split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .map(String::from_utf16_lossy)
            .collect()
    }

    fn into_u32(self) -> Option<u32> {
        Some(u32::from_le_bytes(self.buf.get(..4)?.try_into().ok()?))
    }

    fn into_u64(self) -> Option<u64> {
        Some(u64::from_le_bytes(self.buf.get(..8)?.try_into().ok()?))
    }

    /// DEVPROP_BOOLEAN: 0x00 false, 0xFF true.
    fn into_bool(self) -> Option<bool> {
        Some(*self.buf.first()? != 0)
    }

    fn into_guid(self) -> Option<String> {
        let b: &[u8; 16] = self.buf.get(..16)?.try_into().ok()?;
        Some(format!(
            "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            u16::from_le_bytes([b[4], b[5]]),
            u16::from_le_bytes([b[6], b[7]]),
            b[8],
            b[9],
            b[10],
            b[11],
            b[12],
            b[13],
            b[14],
            b[15]
        ))
    }
}
