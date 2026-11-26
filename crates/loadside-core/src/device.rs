//! Device discovery and communication over macOS's usbmuxd (USB and Wi-Fi).
//!
//! Uses usbmuxd's v3 plist protocol (`kLibUSBMuxVersion = 3`) for both `Listen`
//! and `ListDevices` so macOS delivers Wi-Fi (`ConnectionType = Network`) attach
//! and detach events reliably.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use idevice::{
    Idevice, IdeviceError, IdeviceService,
    amfi::AmfiClient,
    installation_proxy::InstallationProxyClient,
    lockdown::LockdownClient,
    pairing_file::PairingFile,
    provider::IdeviceProvider,
    usbmuxd::UsbmuxdAddr,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};
use tracing::{debug, warn};

pub const LABEL: &str = "loadside";

const USBMUXD_SOCKET_DEFAULT: &str = "/var/run/usbmuxd";
const USBMUXD_TIMEOUT: Duration = Duration::from_secs(5);
const LOCKDOWN_TIMEOUT: Duration = Duration::from_secs(8);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const USBMUXD_PLIST_VERSION: u32 = 1;
const USBMUXD_PLIST_MESSAGE: u32 = 8;
const MAX_USBMUXD_PACKET: usize = 4 * 1024 * 1024;
const LIBUSBMUX_VERSION: i64 = 3;

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

/// A live transport entry reported by usbmuxd (USB or Wi-Fi).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedDevice {
    pub device_id: u32,
    pub udid: String,
    /// "usb" or "wifi"
    pub connection: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    Attached(AttachedDevice),
    Detached(u32),
}

// ---------------------------------------------------------------------------
// Direct v3 usbmuxd plist framing (`kLibUSBMuxVersion = 3`)
// ---------------------------------------------------------------------------

trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}

async fn open_usbmuxd_stream() -> Result<Box<dyn AsyncStream>> {
    let env_addr = std::env::var("USBMUXD_SOCKET_ADDRESS").ok();
    let addr = env_addr.as_deref().map(str::trim).filter(|s| !s.is_empty());

    let connect_fut = async {
        let stream: Box<dyn AsyncStream> = match addr {
            None => Box::new(
                UnixStream::connect(USBMUXD_SOCKET_DEFAULT)
                    .await
                    .with_context(|| format!("connecting to usbmuxd at {USBMUXD_SOCKET_DEFAULT}"))?,
            ),
            Some(a) if a.starts_with("UNIX:") || a.starts_with('/') => {
                let path = a.strip_prefix("UNIX:").unwrap_or(a);
                Box::new(
                    UnixStream::connect(path)
                        .await
                        .with_context(|| format!("connecting to usbmuxd at {path}"))?,
                )
            }
            Some(a) => Box::new(
                TcpStream::connect(a)
                    .await
                    .with_context(|| format!("connecting to usbmuxd at {a}"))?,
            ),
        };
        Ok::<Box<dyn AsyncStream>, anyhow::Error>(stream)
    };

    tokio::time::timeout(USBMUXD_TIMEOUT, connect_fut)
        .await
        .map_err(|_| anyhow::anyhow!("timed out connecting to usbmuxd"))?
}

fn usbmuxd_addr() -> UsbmuxdAddr {
    UsbmuxdAddr::from_env_var().unwrap_or_default()
}

fn build_v3_request(message_type: &str) -> plist::Dictionary {
    let mut dict = plist::Dictionary::new();
    dict.insert("MessageType".into(), plist::Value::String(message_type.into()));
    dict.insert("ClientVersionString".into(), plist::Value::String(format!("loadside-{}", crate::VERSION)));
    dict.insert("ProgName".into(), plist::Value::String("loadsided".into()));
    dict.insert("kLibUSBMuxVersion".into(), plist::Value::Integer(LIBUSBMUX_VERSION.into()));
    dict
}

fn encode_plist_packet(tag: u32, dict: &plist::Dictionary) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    plist::Value::Dictionary(dict.clone())
        .to_writer_xml(&mut payload)
        .context("serializing usbmuxd plist request")?;
    let total_len = u32::try_from(16 + payload.len()).context("usbmuxd request too large")?;
    let mut buf = Vec::with_capacity(total_len as usize);
    buf.extend_from_slice(&total_len.to_le_bytes());
    buf.extend_from_slice(&USBMUXD_PLIST_VERSION.to_le_bytes());
    buf.extend_from_slice(&USBMUXD_PLIST_MESSAGE.to_le_bytes());
    buf.extend_from_slice(&tag.to_le_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
}

async fn write_plist_packet<W: AsyncWrite + Unpin>(stream: &mut W, tag: u32, dict: &plist::Dictionary) -> Result<()> {
    let packet = encode_plist_packet(tag, dict)?;
    stream.write_all(&packet).await.context("writing usbmuxd request")?;
    stream.flush().await.context("flushing usbmuxd request")?;
    Ok(())
}

async fn read_plist_packet<R: AsyncRead + Unpin>(stream: &mut R) -> Result<Option<plist::Dictionary>> {
    let mut header = [0u8; 16];
    let mut read = 0usize;
    while read < header.len() {
        let n = stream.read(&mut header[read..]).await.context("reading usbmuxd packet header")?;
        if n == 0 {
            if read == 0 {
                return Ok(None);
            }
            bail!("usbmuxd closed connection mid-header");
        }
        read += n;
    }
    let total_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if total_len < 16 || total_len > MAX_USBMUXD_PACKET {
        bail!("invalid usbmuxd packet size: {total_len}");
    }
    let payload_len = total_len - 16;
    let mut payload = vec![0u8; payload_len];
    stream.read_exact(&mut payload).await.context("reading usbmuxd packet payload")?;
    let value: plist::Value = plist::from_bytes(&payload).context("parsing usbmuxd plist payload")?;
    let dict = value
        .into_dictionary()
        .ok_or_else(|| anyhow::anyhow!("usbmuxd plist payload was not a dictionary"))?;
    Ok(Some(dict))
}

fn dict_u32(dict: &plist::Dictionary, key: &str) -> Option<u32> {
    dict.get(key).and_then(|v| v.as_unsigned_integer()).and_then(|n| u32::try_from(n).ok())
}

fn connection_from_str(s: &str) -> &'static str {
    if s.eq_ignore_ascii_case("network") || s.to_ascii_lowercase().starts_with("network") {
        "wifi"
    } else {
        "usb"
    }
}

fn parse_attached_dict(dict: &plist::Dictionary) -> Option<AttachedDevice> {
    let props = dict.get("Properties").and_then(|v| v.as_dictionary());
    let device_id = dict_u32(dict, "DeviceID").or_else(|| props.and_then(|p| dict_u32(p, "DeviceID")))?;
    let udid = props
        .and_then(|p| p.get("SerialNumber"))
        .and_then(|v| v.as_string())
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let conn = props
        .and_then(|p| p.get("ConnectionType"))
        .and_then(|v| v.as_string())
        .map(connection_from_str)
        .unwrap_or("usb");
    Some(AttachedDevice {
        device_id,
        udid,
        connection: conn.into(),
    })
}

fn parse_watch_event(dict: &plist::Dictionary) -> Option<WatchEvent> {
    let msg = dict.get("MessageType").and_then(|v| v.as_string())?;
    match msg {
        "Attached" => parse_attached_dict(dict).map(WatchEvent::Attached),
        "Detached" => dict_u32(dict, "DeviceID").map(WatchEvent::Detached),
        _ => None,
    }
}

/// Deduplicate transport entries to one best entry per UDID.
/// Always prefers USB over Wi-Fi; within the same transport, prefers the highest `device_id`.
pub fn prefer_best_connections(devices: Vec<AttachedDevice>) -> Vec<AttachedDevice> {
    let mut by_udid: HashMap<String, AttachedDevice> = HashMap::new();
    for d in devices {
        let replace = match by_udid.get(&d.udid) {
            None => true,
            Some(existing) => match (existing.connection.as_str(), d.connection.as_str()) {
                ("usb", "wifi") => false,
                ("wifi", "usb") => true,
                _ => d.device_id >= existing.device_id,
            },
        };
        if replace {
            by_udid.insert(d.udid.clone(), d);
        }
    }
    let mut v: Vec<_> = by_udid.into_values().collect();
    v.sort_by(|a, b| a.udid.cmp(&b.udid));
    v
}

/// Active usbmuxd v3 `Listen` stream that receives both USB and Wi-Fi events.
pub struct UsbmuxdListener {
    stream: Box<dyn AsyncStream>,
}

impl UsbmuxdListener {
    /// Connect to usbmuxd and subscribe with `kLibUSBMuxVersion = 3`.
    /// Returns only after usbmuxd acknowledges the `Listen` request.
    pub async fn connect() -> Result<Self> {
        let stream = open_usbmuxd_stream().await?;
        Self::handshake(stream).await
    }

    async fn handshake(mut stream: Box<dyn AsyncStream>) -> Result<Self> {
        let req = build_v3_request("Listen");
        tokio::time::timeout(USBMUXD_TIMEOUT, async {
            write_plist_packet(&mut stream, 1, &req).await?;
            let reply = read_plist_packet(&mut stream)
                .await?
                .ok_or_else(|| anyhow::anyhow!("usbmuxd closed connection before acknowledging Listen"))?;
            let msg = reply.get("MessageType").and_then(|v| v.as_string()).unwrap_or("");
            let code = reply.get("Number").and_then(|v| v.as_signed_integer()).unwrap_or(-1);
            if msg != "Result" || code != 0 {
                bail!("usbmuxd rejected Listen request (MessageType={msg:?}, Number={code})");
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for usbmuxd Listen acknowledgment"))??;
        Ok(Self { stream })
    }

    /// Wait for the next `Attached` or `Detached` event (`Ok(None)` on clean EOF).
    pub async fn next_event(&mut self) -> Result<Option<WatchEvent>> {
        loop {
            let Some(dict) = read_plist_packet(&mut self.stream).await? else {
                return Ok(None);
            };
            if let Some(ev) = parse_watch_event(&dict) {
                return Ok(Some(ev));
            }
            debug!("Ignoring usbmuxd event: {:?}", dict.get("MessageType"));
        }
    }
}

/// Every usbmuxd connection (a device on USB and Wi-Fi appears twice).
pub async fn list_all() -> Result<Vec<AttachedDevice>> {
    let mut stream = open_usbmuxd_stream().await?;
    let req = build_v3_request("ListDevices");
    let reply = tokio::time::timeout(USBMUXD_TIMEOUT, async {
        write_plist_packet(&mut stream, 1, &req).await?;
        read_plist_packet(&mut stream)
            .await?
            .ok_or_else(|| anyhow::anyhow!("usbmuxd closed connection before replying to ListDevices"))
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out listing devices from usbmuxd"))??;

    let list = reply
        .get("DeviceList")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("usbmuxd ListDevices response missing DeviceList"))?;
    let mut out = Vec::with_capacity(list.len());
    for item in list {
        if let Some(d) = item.as_dictionary().and_then(parse_attached_dict) {
            out.push(d);
        }
    }
    Ok(out)
}

/// Raw usbmuxd listing, one entry per udid (USB preferred over Wi-Fi, newest `device_id` preferred).
pub async fn list_raw() -> Result<Vec<AttachedDevice>> {
    let devices = list_all().await?;
    Ok(prefer_best_connections(devices))
}

// ---------------------------------------------------------------------------
// Resilient provider & lockdown metadata
// ---------------------------------------------------------------------------

/// An `IdeviceProvider` bound to a UDID that automatically re-resolves `device_id`
/// if a Wi-Fi device reconnects under a new usbmuxd ID mid-operation.
#[derive(Debug, Clone)]
pub struct DynamicUsbmuxdProvider {
    pub addr: UsbmuxdAddr,
    pub udid: String,
    pub device_id: Arc<AtomicU32>,
    pub label: String,
}

impl DynamicUsbmuxdProvider {
    pub fn new(device_id: u32, udid: impl Into<String>) -> Self {
        Self {
            addr: usbmuxd_addr(),
            udid: udid.into(),
            device_id: Arc::new(AtomicU32::new(device_id)),
            label: LABEL.into(),
        }
    }
}

impl IdeviceProvider for DynamicUsbmuxdProvider {
    fn connect(&self, port: u16) -> Pin<Box<dyn Future<Output = Result<Idevice, IdeviceError>> + Send>> {
        let addr = self.addr.clone();
        let udid = self.udid.clone();
        let device_id_cell = self.device_id.clone();
        let label = self.label.clone();

        Box::pin(async move {
            let current_id = device_id_cell.load(Ordering::Relaxed);
            let first_try = tokio::time::timeout(CONNECT_TIMEOUT, async {
                let mux = addr.connect(0).await?;
                mux.connect_to_device(current_id, port, &label).await
            })
            .await;

            match first_try {
                Ok(Ok(dev)) => return Ok(dev),
                Ok(Err(first_err)) => {
                    // Wi-Fi devices can briefly reconnect with a new usbmuxd DeviceID
                    // between AFC upload and installation_proxy. Re-resolve once by UDID.
                    if let Ok(devs) = list_raw().await {
                        if let Some(fresh) = devs.into_iter().find(|d| d.udid == udid && d.device_id != current_id) {
                            debug!(
                                "Device {udid} moved from usbmuxd id {current_id} to {} ({}); retrying port {port}",
                                fresh.device_id, fresh.connection
                            );
                            device_id_cell.store(fresh.device_id, Ordering::Relaxed);
                            let retry = tokio::time::timeout(CONNECT_TIMEOUT, async {
                                let mux = addr.connect(0).await?;
                                mux.connect_to_device(fresh.device_id, port, &label).await
                            })
                            .await;
                            match retry {
                                Ok(r) => return r,
                                Err(_) => {
                                    return Err(IdeviceError::Socket(std::io::Error::new(
                                        std::io::ErrorKind::TimedOut,
                                        "timed out connecting to device through usbmuxd",
                                    )));
                                }
                            }
                        }
                    }
                    Err(first_err)
                }
                Err(_) => Err(IdeviceError::Socket(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out connecting to device through usbmuxd",
                ))),
            }
        })
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn get_pairing_file(&self) -> Pin<Box<dyn Future<Output = Result<PairingFile, IdeviceError>> + Send>> {
        let addr = self.addr.clone();
        let udid = self.udid.clone();

        Box::pin(async move {
            match tokio::time::timeout(USBMUXD_TIMEOUT, async {
                let mut mux = addr.connect(0).await?;
                mux.get_pair_record(&udid).await
            })
            .await
            {
                Ok(r) => r,
                Err(_) => Err(IdeviceError::Socket(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out reading pair record from usbmuxd",
                ))),
            }
        })
    }
}

pub fn provider(dev: &AttachedDevice) -> DynamicUsbmuxdProvider {
    DynamicUsbmuxdProvider::new(dev.device_id, dev.udid.clone())
}

pub async fn provider_for(udid: &str) -> Result<DynamicUsbmuxdProvider> {
    let devs = list_raw().await?;
    let dev = devs.iter().find(|d| d.udid == udid).ok_or_else(|| {
        crate::protocol::Failure::new("device_not_found", "The device isn't connected.")
            .with_hint("Connect it with a cable (or enable Wi-Fi sync in Finder), unlock it, and retry.")
    })?;
    Ok(provider(dev))
}

fn is_transient_describe_error(code: &str) -> bool {
    matches!(code, "network" | "timeout" | "device_not_found" | "unknown")
}

/// Read name / model / iOS version / Developer Mode over lockdown.
/// Retries once after a short pause on transient Wi-Fi/startup hiccups.
pub async fn describe(dev: &AttachedDevice) -> DeviceInfo {
    let mut info = DeviceInfo {
        udid: dev.udid.clone(),
        name: "iOS device".into(),
        model: None,
        ios_version: None,
        connection: dev.connection.clone(),
        paired: false,
        developer_mode: None,
        problem: None,
        wifi_sync: None,
    };
    let p = provider(dev);
    for attempt in 0..2 {
        match tokio::time::timeout(LOCKDOWN_TIMEOUT, read_lockdown(&p)).await {
            Ok(Ok(v)) => {
                info.paired = true;
                info.problem = None;
                info.name = v.0.unwrap_or(info.name);
                info.model = v.1;
                info.ios_version = v.2;
                info.developer_mode = v.3;
                info.wifi_sync = v.4;
                return info;
            }
            Ok(Err(e)) => {
                let f = crate::errors::explain(&e);
                let msg = match f.hint {
                    Some(h) => format!("{} {}", f.message, h),
                    None => f.message,
                };
                if attempt == 0 && is_transient_describe_error(&f.code) {
                    debug!("Transient lockdown probe error for {} ({}), retrying shortly", dev.udid, f.code);
                    tokio::time::sleep(Duration::from_millis(1200)).await;
                    continue;
                }
                info.problem = Some(msg);
                return info;
            }
            Err(_) => {
                if attempt == 0 {
                    debug!("Lockdown probe timed out for {}, retrying once", dev.udid);
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    continue;
                }
                info.problem = Some("Timed out talking to the device. Unlock it and reconnect.".into());
                return info;
            }
        }
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
    let wifi = lockdown
        .get_value(Some("EnableWifiConnections"), Some(WIRELESS_DOMAIN))
        .await
        .ok()
        .and_then(|v| v.as_boolean());
    Ok((name, model, version, dev_mode, wifi))
}

pub async fn list() -> Result<Vec<DeviceInfo>> {
    let raw = list_raw().await?;
    let futs = raw.iter().map(describe);
    Ok(futures::future::join_all(futs).await)
}

/// Turn on "Show this iPhone when on Wi-Fi" (what Finder's checkbox does).
/// The device then appears over Wi-Fi while it's on the same network as this Mac.
pub async fn enable_wifi_sync(p: &impl IdeviceProvider) -> Result<()> {
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

/// Briefly browse `_apple-mobdev2._tcp` via macOS's `dns-sd` to encourage an
/// unlocked iPhone on the same Wi-Fi to re-announce itself to `usbmuxd`.
pub async fn nudge_wifi_discovery() {
    let mut cmd = tokio::process::Command::new("/usr/bin/dns-sd");
    cmd.args(["-B", "_apple-mobdev2._tcp", "local."])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let Ok(mut child) = cmd.spawn() else {
        return;
    };
    tokio::time::sleep(Duration::from_secs(3)).await;
    if let Err(e) = child.kill().await {
        warn!("Failed to stop dns-sd discovery nudge: {e}");
    }
    let _ = child.wait().await;
}

/// An open connection to a device through usbmuxd (USB or Wi-Fi sync).
#[derive(Debug)]
pub struct Conn(pub DynamicUsbmuxdProvider);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_v3_listen_request_with_version_3() {
        let req = build_v3_request("Listen");
        assert_eq!(req.get("MessageType").and_then(|v| v.as_string()), Some("Listen"));
        assert_eq!(req.get("kLibUSBMuxVersion").and_then(|v| v.as_signed_integer()), Some(3));
        assert_eq!(req.get("ProgName").and_then(|v| v.as_string()), Some("loadsided"));
        assert!(req.get("ClientVersionString").and_then(|v| v.as_string()).is_some());

        let bytes = encode_plist_packet(7, &req).unwrap();
        let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let msg = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let tag = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        assert_eq!(len, bytes.len());
        assert_eq!(version, 1);
        assert_eq!(msg, 8);
        assert_eq!(tag, 7);
    }

    #[test]
    fn parses_network_attach_even_without_network_address() {
        let mut props = plist::Dictionary::new();
        props.insert("ConnectionType".into(), plist::Value::String("Network".into()));
        props.insert("SerialNumber".into(), plist::Value::String("00008120-000E59EC3A44201E".into()));
        props.insert("DeviceID".into(), plist::Value::Integer(10397.into()));

        let mut msg = plist::Dictionary::new();
        msg.insert("MessageType".into(), plist::Value::String("Attached".into()));
        msg.insert("DeviceID".into(), plist::Value::Integer(10397.into()));
        msg.insert("Properties".into(), plist::Value::Dictionary(props));

        assert_eq!(
            parse_watch_event(&msg),
            Some(WatchEvent::Attached(AttachedDevice {
                device_id: 10397,
                udid: "00008120-000E59EC3A44201E".into(),
                connection: "wifi".into(),
            }))
        );

        let mut det = plist::Dictionary::new();
        det.insert("MessageType".into(), plist::Value::String("Detached".into()));
        det.insert("DeviceID".into(), plist::Value::Integer(10397.into()));
        assert_eq!(parse_watch_event(&det), Some(WatchEvent::Detached(10397)));
    }

    #[test]
    fn deduplication_prefers_usb_then_highest_device_id() {
        let input = vec![
            AttachedDevice { device_id: 10, udid: "A".into(), connection: "wifi".into() },
            AttachedDevice { device_id: 12, udid: "A".into(), connection: "wifi".into() },
            AttachedDevice { device_id: 5, udid: "B".into(), connection: "wifi".into() },
            AttachedDevice { device_id: 3, udid: "B".into(), connection: "usb".into() },
            AttachedDevice { device_id: 8, udid: "B".into(), connection: "wifi".into() },
        ];
        let out = prefer_best_connections(input);
        assert_eq!(
            out,
            vec![
                AttachedDevice { device_id: 12, udid: "A".into(), connection: "wifi".into() },
                AttachedDevice { device_id: 3, udid: "B".into(), connection: "usb".into() },
            ]
        );
    }

    #[tokio::test]
    async fn listener_roundtrips_v3_handshake_and_wifi_events() {
        let (client, mut server) = UnixStream::pair().unwrap();

        let server_task = tokio::spawn(async move {
            let req = read_plist_packet(&mut server).await.unwrap().unwrap();
            assert_eq!(req.get("MessageType").and_then(|v| v.as_string()), Some("Listen"));
            assert_eq!(req.get("kLibUSBMuxVersion").and_then(|v| v.as_signed_integer()), Some(3));

            let mut ack = plist::Dictionary::new();
            ack.insert("MessageType".into(), plist::Value::String("Result".into()));
            ack.insert("Number".into(), plist::Value::Integer(0.into()));
            write_plist_packet(&mut server, 1, &ack).await.unwrap();

            let mut props = plist::Dictionary::new();
            props.insert("ConnectionType".into(), plist::Value::String("Network".into()));
            props.insert("SerialNumber".into(), plist::Value::String("TEST-UDID-1".into()));
            let mut att = plist::Dictionary::new();
            att.insert("MessageType".into(), plist::Value::String("Attached".into()));
            att.insert("DeviceID".into(), plist::Value::Integer(42.into()));
            att.insert("Properties".into(), plist::Value::Dictionary(props));
            write_plist_packet(&mut server, 0, &att).await.unwrap();

            let mut det = plist::Dictionary::new();
            det.insert("MessageType".into(), plist::Value::String("Detached".into()));
            det.insert("DeviceID".into(), plist::Value::Integer(42.into()));
            write_plist_packet(&mut server, 0, &det).await.unwrap();
        });

        let mut listener = UsbmuxdListener::handshake(Box::new(client)).await.unwrap();
        assert_eq!(
            listener.next_event().await.unwrap(),
            Some(WatchEvent::Attached(AttachedDevice {
                device_id: 42,
                udid: "TEST-UDID-1".into(),
                connection: "wifi".into(),
            }))
        );
        assert_eq!(listener.next_event().await.unwrap(), Some(WatchEvent::Detached(42)));
        assert_eq!(listener.next_event().await.unwrap(), None);
        server_task.await.unwrap();
    }
}
