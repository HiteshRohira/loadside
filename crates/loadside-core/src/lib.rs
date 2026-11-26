//! Loadside core: everything needed to sign and install IPAs on iOS devices
//! using a (free or paid) Apple ID, without Xcode.
//!
//! The heavy lifting for Apple authentication, the developer services API and
//! code signing is done by the `isideload` crate; device communication uses
//! `idevice`. This crate adds IPA preparation (bundle ID rewrites, extension
//! stripping, tweak injection), state/config persistence, friendly error
//! mapping and the IPC protocol shared by the daemon, CLI and macOS app.

pub mod account;
pub mod client;
pub mod config;
pub mod deb;
pub mod device;
pub mod errors;
pub mod github;
pub mod ipa;
pub mod macho;
pub mod paths;
pub mod pipeline;
pub mod profile;
pub mod protocol;
pub mod secrets;
pub mod setup;
pub mod state;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Must be called once at process start, before any network access.
pub fn init() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = isideload::init();
}

/// Current unix time in seconds.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// Re-exported so the daemon doesn't need to pin matching versions itself.
pub use idevice;
pub use rootcause;
