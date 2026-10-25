//! Core of the Windows driver updater: inventory, machine identity, update
//! sources, matching and installation.

pub mod history;
pub mod matching;
pub mod model;
pub mod oem;
pub mod provider;
pub mod time;

#[cfg(windows)]
pub mod install;
#[cfg(windows)]
pub mod inventory;
#[cfg(windows)]
pub mod machine;
#[cfg(windows)]
mod registry;
#[cfg(windows)]
pub mod trust;
#[cfg(windows)]
pub mod wua;
