#![doc = include_str!("../README.md")]
#![deny(missing_docs)]

pub mod blocklist_client;
pub mod codec;
pub mod config;
pub mod ecdsa;
pub mod error;
pub mod fees;
pub mod message;
pub mod network;
pub mod packaging;

#[cfg(feature = "testing")]
pub mod testing;
pub mod utxo;

/// Package version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
