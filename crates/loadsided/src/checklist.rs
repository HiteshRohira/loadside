//! Device connection helpers, setup actions (Developer Mode, Wi-Fi sync) and
//! the setup checklist that drives onboarding.

use std::sync::Arc;
use std::time::{Duration, Instant};

use loadside_core::{
    device::{self, Conn, DeviceInfo},
    errors, pipeline,
    protocol::Failure,
    secrets,
    setup::{self, CheckStatus, SetupAction, SetupCheck, SetupReport},
};
use serde_json::Value;
use tracing::info;

use crate::engine::Engine;

pub const NOT_CONNECTED_HINT: &str =
    "Make sure it's on the same Wi-Fi network as this Mac and unlocked. If it still doesn't appear, open Xcode → Window → Devices and Simulators once so macOS reconnects it over the network.";

fn f(code: &str, msg: &str) -> Failure {
    Failure::new(code, msg)
}

type NetChecks = (setup::Reachability, setup::Reachability);
static NET_CHECKS: std::sync::Mutex<Option<(Instant, String, NetChecks)>> = std::sync::Mutex::new(None);
static PASSWORD_SAVED: std::sync::Mutex<Option<(String, bool)>> = std::sync::Mutex::new(None);

/// Apple + anisette reachability, cached so polling UIs don't hammer the servers.
async fn net_checks(anisette_url: &str, force: bool) -> NetChecks {
    if !force {
        if let Some((at, url, r)) = NET_CHECKS.lock().unwrap().clone() {
            // Failures are re-checked sooner than successes.
            let ttl = if r.0.ok && r.1.ok { 300 } else { 20 };
            if url == anisette_url && at.elapsed() < Duration::from_secs(ttl) {
                return r;
            }
        }
    }
    let r = tokio::join!(setup::check_apple(), setup::check_anisette(anisette_url));
    *NET_CHECKS.lock().unwrap() = Some((Instant::now(), anisette_url.to_string(), r.clone()));
    r
}

/// Whether a password is in the Keychain, read once per email (each read can prompt).
fn password_saved(email: &str) -> bool {
    if let Some((e, v)) = PASSWORD_SAVED.lock().unwrap().clone() {
        if e == email {
            return v;
        }
    }
    // A Keychain lookup can wait on the system: let the runtime move other
    // tasks off this thread meanwhile.
    let v = tokio::task::block_in_place(|| secrets::has_password(email));
    *PASSWORD_SAVED.lock().unwrap() = Some((email.to_string(), v));
    v
}

/// Call after login / logout so the cached answer is re-read.
pub fn forget_password_cache() {
    *PASSWORD_SAVED.lock().unwrap() = None;
}

impl Engine {
    /// Name to use when registering the device with Apple.
    fn device_name(&self, udid: &str) -> String {
        self.devices()
            .into_iter()
            .find(|d| d.udid == udid)
            .map(|d| d.name)
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "iOS device".into())
    }

    /// A pipeline target that connects lazily (after signing).
    pub(crate) fn target(self: &Arc<Self>, udid: &str) -> pipeline::Target {
        let engine = self.clone();
        let u = udid.to_string();
        pipeline::Target {
            udid: udid.to_string(),
            name: self.device_name(udid),
            connect: Arc::new(move || {
                let engine = engine.clone();
                let u = u.clone();
                Box::pin(async move { engine.connect(&u).await.map_err(anyhow::Error::new) })
            }),
        }
    }

    /// Open a connection: usbmuxd first (USB / classic Wi-Fi sync), then Apple's
    /// CoreDevice Wi-Fi tunnel for network-paired devices usbmuxd doesn't list.
    pub(crate) async fn connect(&self, udid: &str) -> Result<Conn, Failure> {
        if let Ok(p) = device::provider_for(udid).await {
            return Ok(Conn::Usbmuxd(p));
        }
        if loadside_core::coredevice::find(udid).await.is_some() {
            return Ok(Conn::CoreDevice(udid.to_string()));
        }
        Err(f("device_not_found", &format!("{} isn't reachable.", self.device_name(udid))).with_hint(NOT_CONNECTED_HINT))
    }

    fn require_usb(&self, udid: &str) -> Result<DeviceInfo, Failure> {
        match self.devices().into_iter().find(|d| d.udid == udid) {
            Some(d) if d.connection == "usb" => Ok(d),
            _ => Err(f("needs_usb", "Connect the device with a USB cable for this step.")
                .with_hint("Unlock the device and tap Trust if asked.")),
        }
    }

    pub async fn reveal_developer_mode(self: &Arc<Self>, udid: &str) -> Result<Value, Failure> {
        self.require_usb(udid)?;
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let p = device::provider_for(udid).await.map_err(|e| errors::explain(&e))?;
            device::reveal_developer_mode(&p).await.map_err(|e| errors::explain(&e))
        })
        .await
        .map_err(|_| f("timeout", "The device didn't answer in time.").with_hint("Unlock it, keep the cable connected, and try again."))??;
        Ok(Value::Null)
    }

    /// Turn on "Show this iPhone when on Wi-Fi" (same as Finder's checkbox).
    pub async fn enable_wifi_sync(self: &Arc<Self>, udid: &str) -> Result<Value, Failure> {
        let d = self.require_usb(udid)?;
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let p = device::provider_for(udid).await.map_err(|e| errors::explain(&e))?;
            device::enable_wifi_sync(&p).await.map_err(|e| errors::explain(&e))
        })
        .await
        .map_err(|_| f("timeout", "The device didn't answer in time.").with_hint("Unlock it, keep the cable connected, and try again."))??;
        info!("Wi-Fi sync turned on for {}", d.name);
        self.set_devices(|devs| match devs.get_mut(udid) {
            Some(d) => {
                d.wifi_sync = Some(true);
                true
            }
            None => false,
        });
        Ok(Value::Null)
    }

    // ------------------------------------------------------------------
    // Setup checklist
    // ------------------------------------------------------------------

    pub async fn setup_status(self: &Arc<Self>, force: bool) -> SetupReport {
        let cfg = self.config();
        let (apple, anisette) = net_checks(&cfg.anisette_url, force).await;
        if force {
            let _ = self.reconcile_devices(true).await;
        }
        let devices = self.devices();
        let st = self.state.lock().unwrap().clone();
        let mut checks = vec![];
        const E: &str = "essentials";
        const O: &str = "optional";

        checks.push(
            SetupCheck::new("service", E, "Background service", CheckStatus::Ok, format!("Running (v{}, pid {}).", loadside_core::VERSION, std::process::id()))
                .detail("Keeps apps refreshed even when this window is closed. Uses about 10 MB of memory."),
        );

        // Internet / Apple / anisette
        let net = if apple.ok && anisette.ok {
            SetupCheck::new("internet", E, "Apple & anisette servers", CheckStatus::Ok, "Both reachable.")
        } else if !apple.ok {
            SetupCheck::new("internet", E, "Apple & anisette servers", CheckStatus::Error, format!("Can't reach Apple's sign-in servers: {}.", apple.error.clone().unwrap_or_default()))
                .detail("Check your internet connection, VPN or firewall (gsa.apple.com must be reachable).")
                .action(SetupAction::new("retry", "Check again").primary())
        } else {
            SetupCheck::new("internet", E, "Apple & anisette servers", CheckStatus::Error, format!("Can't reach the anisette server: {}.", anisette.error.clone().unwrap_or_default()))
                .detail(format!(
                    "Apple requires a machine identity token at sign-in. Loadside gets it from {} (it never sees your password). Pick another anisette-v3 server in Settings if this one is down.",
                    cfg.anisette_url
                ))
                .action(SetupAction::new("retry", "Check again").primary())
                .action(SetupAction::new("open_settings", "Change server"))
        };
        checks.push(net);

        // Account
        let account = match &st.account {
            Some(a) => {
                let kind = match a.is_free {
                    Some(true) => " (free account: 3 apps per device, 7-day signing)",
                    Some(false) => " (paid developer account)",
                    None => "",
                };
                let saved = self.has_session() || password_saved(&a.email);
                if saved {
                    SetupCheck::new("account", E, "Apple ID", CheckStatus::Ok, format!("Signed in as {}{kind}.", a.email))
                } else {
                    SetupCheck::new("account", E, "Apple ID", CheckStatus::Warning, format!("{} is remembered, but the password isn't saved.", a.email))
                        .detail("Background refreshes need to sign in again. Sign in with \"Remember password\" on.")
                        .action(SetupAction::new("login", "Sign in").primary())
                }
            }
            None => SetupCheck::new("account", E, "Apple ID", CheckStatus::Action, "Sign in with the Apple ID to sign apps with.")
                .detail("Any Apple ID works. A free one can keep 3 apps per device and re-signs every 7 days (Loadside does that for you). Your password goes only to Apple and is stored in your Keychain. Using a secondary Apple ID is a good idea.")
                .action(SetupAction::new("login", "Sign in").primary()),
        };
        checks.push(account);

        // Device
        let usable: Vec<&DeviceInfo> = devices.iter().filter(|d| d.paired).collect();
        // Most recently seen device this Mac has paired with, if none is visible.
        let known = if devices.is_empty() { st.known_devices.iter().max_by_key(|k| k.last_seen) } else { None };
        let device_check = if let Some(d) = usable.first() {
            SetupCheck::new("device", E, "iPhone or iPad", CheckStatus::Ok, format!("{} connected ({}{}).", d.name, if d.connection == "wifi" { "Wi-Fi" } else { "USB" }, d.ios_version.as_deref().map(|v| format!(", iOS {v}")).unwrap_or_default()))
        } else if let Some(d) = devices.first() {
            SetupCheck::new("device", E, "iPhone or iPad", CheckStatus::Action, format!("{} needs attention: {}", d.name, d.problem.clone().unwrap_or_else(|| "not trusted yet".into())))
                .detail("Unlock the device. If it asks \"Trust This Computer?\", tap Trust and enter your passcode. If it never asks, unplug and plug it back in, or select it in Finder once.")
                .action(SetupAction::new("refresh_devices", "Check again").primary())
                .action(SetupAction::new("open_finder", "Open Finder"))
        } else if let Some(k) = known {
            // Paired before; it's just not visible (an asleep iPhone drops off
            // Wi-Fi). Nothing to set up, so this isn't blocking.
            SetupCheck::new("device", E, "iPhone or iPad", CheckStatus::Warning, format!("{} is set up, but not reachable right now.", k.name))
                .detail(format!(
                    "iPhones only show up over Wi-Fi while they're awake and on the same network as this Mac. Unlock it, or plug it in, and it reconnects on its own. Last seen {}.",
                    ago(k.last_seen)
                ))
                .action(SetupAction::new("refresh_devices", "Check again").primary())
        } else {
            SetupCheck::new("device", E, "iPhone or iPad", CheckStatus::Action, "Connect your iPhone or iPad with a USB cable.")
                .detail("Unlock it and tap Trust when it asks. If Wi-Fi sync is on and the device is on the same Wi-Fi as this Mac, it shows up without the cable too.")
                .action(SetupAction::new("refresh_devices", "Check again").primary())
        };
        checks.push(device_check);

        // Developer Mode (iOS 16+)
        let dm_devices: Vec<&DeviceInfo> = usable.iter().copied().filter(|d| d.ios_major().is_some_and(|v| v >= 16)).collect();
        let dm = if let (true, Some(k)) = (usable.is_empty(), known.filter(|k| k.developer_mode == Some(true))) {
            SetupCheck::new("developer_mode", E, "Developer Mode", CheckStatus::Ok, format!("On for {} (when last connected).", k.name))
        } else if usable.is_empty() {
            SetupCheck::new("developer_mode", E, "Developer Mode", CheckStatus::Pending, "Checked once a device is connected.")
        } else if let Some(d) = dm_devices.iter().find(|d| d.developer_mode == Some(false)) {
            let mut c = SetupCheck::new("developer_mode", E, "Developer Mode", CheckStatus::Action, format!("Turn on Developer Mode on {}.", d.name))
                .detail("On the device: Settings → Privacy & Security → Developer Mode → On, then restart and confirm \"Turn On\". If the option isn't there, use \"Show the switch\" below first (needs the cable).");
            if d.connection == "usb" {
                c = c.action(SetupAction::new("reveal_developer_mode", "Show the switch").device(&d.udid).primary());
            }
            c.action(SetupAction::new("refresh_devices", "Check again"))
        } else if dm_devices.is_empty() || dm_devices.iter().all(|d| d.developer_mode == Some(true)) {
            SetupCheck::new("developer_mode", E, "Developer Mode", CheckStatus::Ok, if dm_devices.is_empty() { "Not needed on this iOS version." } else { "On." })
        } else {
            SetupCheck::new("developer_mode", E, "Developer Mode", CheckStatus::Pending, "Couldn't read it yet. Unlock the device.")
        };
        checks.push(dm);

        // Profile trust (can't be detected; guidance only)
        let any_installed = st.apps.iter().any(|a| a.on_device != Some(false));
        checks.push(
            SetupCheck::new(
                "trust_profile",
                E,
                "Trust your Apple ID on the device",
                if any_installed { CheckStatus::Ok } else { CheckStatus::Optional },
                if any_installed { "Done after the first install (once per Apple ID)." } else { "Needed once, after your first install." },
            )
            .detail("If an app says \"Untrusted Developer\": Settings → General → VPN & Device Management → your Apple ID → Trust. Needs an internet connection on the device."),
        );

        // Wi-Fi sync (optional)
        let wifi = if let (true, Some(k)) = (usable.is_empty(), known.filter(|k| k.wifi_sync == Some(true))) {
            SetupCheck::new("wifi_sync", O, "Wi-Fi sync", CheckStatus::Ok, format!("On for {}. It shows up whenever it's awake on this Wi-Fi.", k.name))
        } else if usable.is_empty() {
            SetupCheck::new("wifi_sync", O, "Wi-Fi sync", CheckStatus::Pending, "Checked once a device is connected.")
        } else if let Some(d) = usable.iter().find(|d| d.connection == "wifi") {
            SetupCheck::new("wifi_sync", O, "Wi-Fi sync", CheckStatus::Ok, format!("{} is connected over Wi-Fi.", d.name))
        } else if let Some(d) = usable.iter().find(|d| d.wifi_sync == Some(false)) {
            let mut c = SetupCheck::new("wifi_sync", O, "Wi-Fi sync", CheckStatus::Optional, format!("Off for {}. Turn it on to install and refresh without the cable.", d.name))
                .detail("Works while the device is on the same Wi-Fi network as this Mac. Same as Finder's \"Show this iPhone when on Wi-Fi\".");
            if d.connection == "usb" {
                c = c.action(SetupAction::new("enable_wifi_sync", "Turn on Wi-Fi sync").device(&d.udid).primary());
            }
            c
        } else if usable.iter().any(|d| d.wifi_sync == Some(true)) {
            SetupCheck::new("wifi_sync", O, "Wi-Fi sync", CheckStatus::Ok, "On. The device shows up over Wi-Fi when it's on the same network as this Mac.")
        } else {
            SetupCheck::new("wifi_sync", O, "Wi-Fi sync", CheckStatus::Pending, "Couldn't read it yet. Unlock the device.")
        };
        checks.push(wifi);

        let ready = !checks.iter().any(|c| c.is_blocking());
        SetupReport { checks, ready, onboarding_done: cfg.onboarding_done, generated_at: loadside_core::now() }
    }
}

/// "5 minutes ago", "3 hours ago", "2 days ago".
fn ago(ts: i64) -> String {
    let d = (loadside_core::now() - ts).max(0);
    let (n, unit) = match d {
        0..=59 => return "just now".into(),
        60..=3599 => (d / 60, "minute"),
        3600..=86_399 => (d / 3600, "hour"),
        _ => (d / 86_400, "day"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}
