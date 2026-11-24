//! Device discovery and communication over macOS's usbmuxd (USB and Wi-Fi).

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use idevice::{
    IdeviceService,
    amfi::AmfiClient,
    installation_proxy::InstallationProxyClient,
    lockdown::LockdownClient,
    provider::{IdeviceProvider, UsbmuxdProvider},
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice},
};
use serde::{Deserialize, Serialize};

pub const LABEL: &str = "loadside";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceInfo {
    pub udid: String,
    pub name: String,
    /// e.g. "iPhone15,2"
    pub model: Option<String>,
    pub ios_version: Option<String>,
    /// "usb" or "wifi" (macOS Wi-Fi sync)
    pub connection: String,
    pub paired: bool,
    /// iOS 16+: whether Developer Mode is enabled (None = unknown/older iOS).
    pub developer_mode: Option<bool>,
    /// Why we couldn't read details (locked, not trusted, ...).
    pub problem: Option<String>,
    /// "Show this iPhone when on Wi-Fi" is on (None = couldn't read it).
    #[serde(default)]
    pub wifi_sync: Option<bool>,
}

impl DeviceInfo {
    pub fn ios_major(&self) -> Option<u32> {
        ios_major(self.ios_version.as_deref())
    }
}

pub fn ios_major(v: Option<&str>) -> Option<u32> {
    v?.split('.').next()?.parse().ok()
}

fn conn_name(c: &Connection) -> &'static str {
    match c {
        Connection::Usb => "usb",
        Connection::Network(_) => "wifi",
        Connection::Unknown(_) => "usb",
    }
}

pub async fn usbmuxd() -> Result<UsbmuxdConnection> {
    UsbmuxdConnection::default().await.context("connecting to usbmuxd")
}

/// Every usbmuxd connection (a device on USB and Wi-Fi appears twice).
pub async fn list_all() -> Result<Vec<UsbmuxdDevice>> {
    let mut mux = usbmuxd().await?;
    mux.get_devices().await.context("listing devices from usbmuxd")
}

/// Raw usbmuxd listing, one entry per udid (USB preferred over Wi-Fi).
pub async fn list_raw() -> Result<Vec<UsbmuxdDevice>> {
    let mut mux = usbmuxd().await?;
    let devices = mux.get_devices().await.context("listing devices from usbmuxd")?;
    let mut by_udid: HashMap<String, UsbmuxdDevice> = HashMap::new();
    for d in devices {
        match by_udid.get(&d.udid) {
            Some(existing) if existing.connection_type == Connection::Usb => {}
            _ => {
                by_udid.insert(d.udid.clone(), d);
            }
        }
    }
    let mut v: Vec<_> = by_udid.into_values().collect();
    v.sort_by(|a, b| a.udid.cmp(&b.udid));
    Ok(v)
}

pub fn provider(dev: &UsbmuxdDevice) -> UsbmuxdProvider {
    dev.to_provider(UsbmuxdAddr::default(), LABEL)
}

pub async fn provider_for(udid: &str) -> Result<UsbmuxdProvider> {
    let devs = list_raw().await?;
    let dev = devs.iter().find(|d| d.udid == udid).ok_or_else(|| {
        crate::protocol::Failure::new("device_not_found", "The device isn't connected.")
            .with_hint("Connect it with a cable (or enable Wi-Fi sync in Finder), unlock it, and retry.")
    })?;
    Ok(provider(dev))
}

/// Read name / model / iOS version / Developer Mode over lockdown.
pub async fn describe(dev: &UsbmuxdDevice) -> DeviceInfo {
    let mut info = DeviceInfo {
        udid: dev.udid.clone(),
        name: "iOS device".into(),
        model: None,
        ios_version: None,
        connection: conn_name(&dev.connection_type).into(),
        paired: false,
        developer_mode: None,
        problem: None,
        wifi_sync: None,
    };
    let p = provider(dev);
    match tokio::time::timeout(std::time::Duration::from_secs(10), read_lockdown(&p)).await {
        Ok(Ok(v)) => {
            info.paired = true;
            info.name = v.0.unwrap_or(info.name);
            info.model = v.1;
            info.ios_version = v.2;
            info.developer_mode = v.3;
            info.wifi_sync = v.4;
        }
        Ok(Err(e)) => {
            let f = crate::errors::explain(&e);
            info.problem = Some(match f.hint {
                Some(h) => format!("{} {}", f.message, h),
                None => f.message,
            });
        }
        Err(_) => info.problem = Some("Timed out talking to the device. Unlock it and reconnect.".into()),
    }
    info
}

type LockdownValues = (Option<String>, Option<String>, Option<String>, Option<bool>, Option<bool>);

const WIRELESS_DOMAIN: &str = "com.apple.mobile.wireless_lockdown";

async fn read_lockdown(p: &impl IdeviceProvider) -> Result<LockdownValues> {
    let mut lockdown = LockdownClient::connect(p).await.context("connecting to lockdown")?;
    let pairing = p.get_pairing_file().await.context("reading pair record")?;
    lockdown.start_session(&pairing).await.context("starting lockdown session")?;
    let s = |v: Result<plist::Value, _>| v.ok().and_then(|v: plist::Value| v.as_string().map(str::to_string));
    let name = s(lockdown.get_value(Some("DeviceName"), None).await);
    let model = s(lockdown.get_value(Some("ProductType"), None).await);
    let version = s(lockdown.get_value(Some("ProductVersion"), None).await);
    let dev_mode = lockdown
        .get_value(Some("DeveloperModeStatus"), Some("com.apple.security.mac.amfi"))
        .await
        .ok()
        .and_then(|v| v.as_boolean());
    let wifi = lockdown.get_value(Some("EnableWifiConnections"), Some(WIRELESS_DOMAIN)).await.ok().and_then(|v| v.as_boolean());
    Ok((name, model, version, dev_mode, wifi))
}

pub async fn list() -> Result<Vec<DeviceInfo>> {
    let raw = list_raw().await?;
    let mut out = Vec::with_capacity(raw.len());
    for d in &raw {
        out.push(describe(d).await);
    }
    Ok(out)
}

/// Turn on "Show this iPhone when on Wi-Fi" (what Finder's checkbox does).
/// The device then appears over Wi-Fi while it's on the same network as this Mac.
pub async fn enable_wifi_sync(p: &UsbmuxdProvider) -> Result<()> {
    let mut lockdown = LockdownClient::connect(p).await.context("connecting to lockdown")?;
    let pairing = p.get_pairing_file().await.context("reading pair record")?;
    lockdown.start_session(&pairing).await.context("starting lockdown session")?;
    lockdown
        .set_value("EnableWifiConnections", true.into(), Some(WIRELESS_DOMAIN))
        .await
        .context("turning on Wi-Fi sync")?;
    Ok(())
}

/// Make Settings → Privacy & Security → Developer Mode visible on the device
/// (it is hidden until a developer tool asks for it). USB only.
pub async fn reveal_developer_mode(p: &impl IdeviceProvider) -> Result<()> {
    let mut amfi = AmfiClient::connect(p).await.context("connecting to AMFI")?;
    amfi.reveal_developer_mode_option_in_ui().await.context("revealing the Developer Mode option")?;
    Ok(())
}

/// An open connection to a device through usbmuxd (USB or Wi-Fi sync).
#[derive(Debug)]
pub struct Conn(pub UsbmuxdProvider);

impl Conn {
    pub fn describe(&self) -> String {
        "usbmuxd".into()
    }

    async fn instproxy(&self) -> Result<InstallationProxyClient> {
        InstallationProxyClient::connect(&self.0).await.context("connecting to installation_proxy")
    }

    /// Which of `bundle_ids` are currently installed.
    pub async fn installed(&mut self, bundle_ids: Vec<String>) -> Result<HashSet<String>> {
        let mut ip = self.instproxy().await?;
        let apps = ip.get_apps(Some("User"), Some(bundle_ids)).await.context("listing installed apps")?;
        Ok(apps.into_keys().collect())
    }

    pub async fn uninstall(&mut self, bundle_id: &str) -> Result<()> {
        let mut ip = self.instproxy().await?;
        ip.uninstall(bundle_id.to_string(), None).await.context("uninstalling app")?;
        Ok(())
    }

    /// Upload and install a signed .app (progress 0..=100).
    pub async fn install(&mut self, app: &std::path::Path, progress: impl Fn(u64) + Send + Sync) -> Result<(), rootcause::Report> {
        isideload::sideload::install::install_app(&self.0, app, progress).await
    }
}
