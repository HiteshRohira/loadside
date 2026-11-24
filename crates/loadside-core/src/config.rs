use serde::{Deserialize, Serialize};

use crate::paths;

pub const DEFAULT_ANISETTE_URL: &str = "https://ani.iloader.app";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// anisette-v3 compatible server used to obtain machine provisioning data.
    /// Your password and 2FA codes are never sent here; they go to Apple only.
    pub anisette_url: String,
    /// Automatically re-sign apps before their provisioning profile expires.
    pub auto_refresh: bool,
    /// Refresh an app when less than this many hours of validity remain.
    pub refresh_threshold_hours: u32,
    /// How often to check whether something needs refreshing (device
    /// connections also trigger a check immediately).
    pub check_interval_minutes: u32,
    /// Name shown on the development certificate in your Apple account.
    pub machine_name: String,
    /// Show macOS notifications for background refreshes and failures.
    pub notifications: bool,
    /// When the account hits its certificate limit during an unattended
    /// refresh, revoke certificates previously created by Loadside (never
    /// other tools' certificates). Interactive installs always ask.
    pub auto_revoke_own_certs: bool,
    /// Keep a copy of every installed IPA so it can be refreshed later.
    pub keep_library_copy: bool,
    /// The user finished (or skipped) onboarding in the app.
    pub onboarding_done: bool,
    /// Identify to Apple as a current Mac instead of whatever the anisette
    /// server reports. Public servers report macOS 10.12, which Apple now
    /// answers with SMS-only 2FA and HTTP 403s.
    pub modern_client_identity: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            anisette_url: DEFAULT_ANISETTE_URL.to_string(),
            auto_refresh: true,
            refresh_threshold_hours: 72,
            check_interval_minutes: 30,
            machine_name: default_machine_name(),
            notifications: true,
            auto_revoke_own_certs: false,
            keep_library_copy: true,
            onboarding_done: false,
            modern_client_identity: true,
        }
    }
}

/// "Loadside-<this Mac's host name>", e.g. "Loadside-Hiteshs-MacBook-Pro".
pub fn default_machine_name() -> String {
    let mut buf = [0u8; 256];
    let host = if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else {
        String::new()
    };
    let short: String = host
        .split('.')
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(30)
        .collect();
    if short.is_empty() { "Loadside".into() } else { format!("Loadside-{short}") }
}

/// HTTPS, or plain HTTP only to this Mac (a self-hosted server).
pub fn anisette_url_ok(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") {
        return lower.len() > "https://".len();
    }
    ["http://localhost", "http://127.0.0.1", "http://[::1]"]
        .iter()
        .any(|p| lower == *p || lower.starts_with(&format!("{p}:")) || lower.starts_with(&format!("{p}/")))
}

impl Config {
    pub fn load() -> Self {
        match std::fs::read(paths::config_path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!("config.json is invalid ({e}); using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        crate::state::write_atomic(&paths::config_path(), &serde_json::to_vec_pretty(self)?)
    }

    pub fn sanitized(mut self) -> Self {
        self.refresh_threshold_hours = self.refresh_threshold_hours.clamp(1, 24 * 6);
        self.check_interval_minutes = self.check_interval_minutes.clamp(5, 24 * 60);
        if self.anisette_url.trim().is_empty() {
            self.anisette_url = DEFAULT_ANISETTE_URL.to_string();
        }
        self.anisette_url = self.anisette_url.trim().trim_end_matches('/').to_string();
        if !anisette_url_ok(&self.anisette_url) {
            tracing::warn!("Ignoring insecure anisette URL {}; using the default", self.anisette_url);
            self.anisette_url = DEFAULT_ANISETTE_URL.to_string();
        }
        // "Loadside" was the old default on every Mac; certificates are
        // recognised as ours by this name, so it must differ between Macs.
        if self.machine_name.trim().is_empty() || self.machine_name == "Loadside" {
            self.machine_name = default_machine_name();
        }
        self
    }
}
