use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths;

/// How app extensions (widgets, share extensions, ...) are handled.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionsMode {
    /// Register a separate App ID for every extension (most compatible, but
    /// each one counts against the free account's 10 App IDs / week).
    #[default]
    RegisterAll,
    /// Sign extensions with the main app's profile (uses one App ID; some
    /// extensions will not work).
    ReuseMain,
    /// Remove all extensions before signing (uses one App ID).
    Strip,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct InstallOptions {
    /// Override the bundle identifier (the team ID is appended automatically
    /// by Apple's free provisioning, so this is the part before it).
    pub bundle_id: Option<String>,
    /// Override the name shown on the home screen.
    pub display_name: Option<String>,
    /// Drop `UISupportedDevices` so the app installs on any device model.
    pub remove_supported_devices: bool,
    /// Remove the embedded Apple Watch app (saves App IDs; on by default).
    pub strip_watch_app: bool,
    pub extensions: ExtensionsMode,
    /// Expose the app's Documents folder in the Files app / Finder.
    pub file_sharing: bool,
    /// Request the increased-memory-limit entitlement (paid accounts mostly).
    pub increased_memory_limit: bool,
    /// .dylib / .framework / .deb / .bundle files to inject.
    pub inject: Vec<PathBuf>,
    /// Refresh this app automatically before it expires.
    pub auto_refresh: bool,
    /// Install even if the main binary is still FairPlay encrypted.
    pub allow_encrypted: bool,
    /// Let AltStore/SideStore/StikStore/LiveContainer receive the signing
    /// certificate and private key (they need it to refresh themselves).
    pub allow_signing_key: bool,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            bundle_id: None,
            display_name: None,
            remove_supported_devices: true,
            strip_watch_app: true,
            extensions: ExtensionsMode::RegisterAll,
            file_sharing: false,
            increased_memory_limit: false,
            inject: Vec::new(),
            auto_refresh: true,
            allow_encrypted: false,
            allow_signing_key: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccountInfo {
    pub email: String,
    pub name: Option<String>,
    pub team_id: Option<String>,
    pub team_name: Option<String>,
    /// Best guess: true for "Personal Team" (free) accounts.
    pub is_free: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppRecord {
    pub id: String,
    pub name: String,
    pub version: Option<String>,
    /// Bundle ID inside the original IPA.
    pub original_bundle_id: String,
    /// Bundle ID actually installed (includes the team suffix).
    pub bundle_id: String,
    /// Copy of the source IPA/.app inside the Loadside library.
    pub source_path: PathBuf,
    pub options: InstallOptions,
    pub device_udid: String,
    pub device_name: String,
    pub team_id: String,
    pub email: String,
    pub installed_at: i64,
    pub last_refreshed_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub last_attempt_at: Option<i64>,
    pub last_error: Option<crate::protocol::Failure>,
    pub consecutive_failures: u32,
    /// None = unknown, Some(false) = removed from the device by the user.
    pub on_device: Option<bool>,
}

impl Default for AppRecord {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            version: None,
            original_bundle_id: String::new(),
            bundle_id: String::new(),
            source_path: PathBuf::new(),
            options: InstallOptions::default(),
            device_udid: String::new(),
            device_name: String::new(),
            team_id: String::new(),
            email: String::new(),
            installed_at: 0,
            last_refreshed_at: None,
            expires_at: None,
            last_attempt_at: None,
            last_error: None,
            consecutive_failures: 0,
            on_device: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct State {
    pub account: Option<AccountInfo>,
    pub apps: Vec<AppRecord>,
}

impl State {
    /// Load state. A missing file is a fresh start; an unreadable one is an
    /// error (so we never overwrite data we couldn't read); a corrupt one is
    /// backed up under a unique name before starting fresh.
    pub fn load() -> anyhow::Result<Self> {
        let path = paths::state_path();
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(s) => Ok(s),
                Err(e) => {
                    let backup = path.with_extension(format!("json.broken-{}", crate::now()));
                    std::fs::copy(&path, &backup)
                        .map_err(|ce| anyhow::anyhow!("state.json is invalid ({e}) and couldn't be backed up ({ce})"))?;
                    tracing::error!("state.json is invalid ({e}); backed up to {}", backup.display());
                    Ok(Self::default())
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(anyhow::anyhow!("couldn't read {}: {e}", path.display())),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        write_atomic(&paths::state_path(), &serde_json::to_vec_pretty(self)?)
    }

    pub fn app(&self, id: &str) -> Option<&AppRecord> {
        self.apps.iter().find(|a| a.id == id)
    }

    pub fn app_mut(&mut self, id: &str) -> Option<&mut AppRecord> {
        self.apps.iter_mut().find(|a| a.id == id)
    }
}

/// Write via a temp file + rename so a crash never leaves a half-written file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    // Unique per write, so concurrent writers never share a temp file.
    let tmp = path.with_extension(format!("tmp{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let res = (|| -> anyhow::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}
