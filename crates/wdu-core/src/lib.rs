//! Core of the Windows driver updater: inventory, machine identity and update sources.

pub mod model;
pub mod oem;
pub mod time;

#[cfg(windows)]
pub mod inventory;
#[cfg(windows)]
pub mod machine;
#[cfg(windows)]
pub mod wua;
