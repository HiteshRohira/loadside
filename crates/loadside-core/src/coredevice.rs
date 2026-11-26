//! Wi-Fi transport through Apple's CoreDevice stack (`xcrun devicectl`).
//!
//! On recent iOS/macOS, an iPhone paired over the network (RemotePairing,
//! advertised as `…-supportsRP-…` on `_apple-mobdev2._tcp`) is handled by
//! `remotepairingd`/CoreDevice and is never attached to the legacy `usbmuxd`
//! service. CoreDevice still reaches it over an encrypted Wi-Fi tunnel, so we
//! fall back to `devicectl` for discovery and install when usbmuxd can't see a
//! device. Requires Xcode (or its command line tools with devicectl).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tracing::debug;

use crate::device::DeviceInfo;

const XCRUN: &str = "/usr/bin/xcrun";
const LIST_TIMEOUT: Duration = Duration::from_secs(20);
const APPS_TIMEOUT: Duration = Duration::from_secs(60);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Removes the JSON output file however we leave.
struct TempJson(PathBuf);
impl Drop for TempJson {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Run `xcrun devicectl <args> --json-output <tmp> -q` and return the parsed JSON.
/// devicectl writes a JSON document (with `info.outcome` and `result` or `error`)
/// even when the command fails, so prefer its error text over stderr.
async fn devicectl(args: &[&str], timeout: Duration) -> Result<Value> {
    let dir = crate::paths::work_dir();
    std::fs::create_dir_all(&dir).ok();
    let out = TempJson(dir.join(format!("devicectl-{}.json", uuid::Uuid::new_v4().simple())));

    let mut cmd = tokio::process::Command::new(XCRUN);
    cmd.arg("devicectl")
        .args(args)
        .arg("--json-output")
        .arg(&out.0)
        .arg("-q")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().context("running xcrun devicectl (is Xcode installed?)")?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("devicectl {} timed out", args.first().unwrap_or(&"")))?
        .context("waiting for devicectl")?;

    let json: Option<Value> = std::fs::read(&out.0).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    match json {
        Some(v) if v.pointer("/info/outcome").and_then(Value::as_str) == Some("success") => Ok(v),
        Some(v) => bail!("{}", error_text(&v).unwrap_or(stderr)),
        None if output.status.success() => bail!("devicectl returned no output"),
        None => bail!("devicectl failed: {stderr}"),
    }
}

fn error_text(v: &Value) -> Option<String> {
    let e = v.get("error")?;
    let ui = e.get("userInfo");
    let s = |k: &str| {
        ui.and_then(|u| u.get(k))
            .and_then(|x| x.get("string").or(Some(x)))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let desc = s("NSLocalizedDescription");
    let reason = s("NSLocalizedFailureReason").or_else(|| s("NSLocalizedRecoverySuggestion"));
    match (desc, reason) {
        (Some(d), Some(r)) => Some(format!("{d} {r}")),
        (Some(d), None) => Some(d),
        (None, r) => r.or_else(|| Some(e.to_string())),
    }
}

/// Map one `devicectl list devices` entry to a [`DeviceInfo`] if it's a paired,
/// currently reachable physical device on the local network.
fn parse_device(d: &Value) -> Option<DeviceInfo> {
    let hw = d.get("hardwareProperties")?;
    let conn = d.get("connectionProperties")?;
    let props = d.get("deviceProperties");
    let str_at = |v: Option<&Value>, k: &str| v.and_then(|v| v.get(k)).and_then(Value::as_str).map(str::to_string);

    if str_at(Some(hw), "reality").as_deref() != Some("physical") {
        return None;
    }
    if str_at(Some(conn), "pairingState").as_deref() != Some("paired") {
        return None;
    }
    // Only devices CoreDevice can reach over the network right now. USB is
    // already served by usbmuxd; an unreachable device has no transport.
    if str_at(Some(conn), "transportType").as_deref() != Some("localNetwork") {
        return None;
    }
    if str_at(Some(conn), "tunnelState").as_deref() == Some("unavailable") {
        return None;
    }
    let udid = str_at(Some(hw), "udid")?;
    let developer_mode = str_at(props, "developerModeStatus").map(|s| s == "enabled");
    Some(DeviceInfo {
        udid,
        name: str_at(props, "name").unwrap_or_else(|| "iOS device".into()),
        model: str_at(Some(hw), "productType"),
        ios_version: str_at(props, "osVersionNumber"),
        connection: "wifi".into(),
        paired: true,
        developer_mode,
        problem: None,
        wifi_sync: Some(true),
    })
}

fn parse_list(v: &Value) -> Vec<DeviceInfo> {
    v.pointer("/result/devices")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(parse_device).collect())
        .unwrap_or_default()
}

/// Paired physical devices CoreDevice can currently reach over Wi-Fi.
/// Returns an empty list if devicectl isn't available.
pub async fn list() -> Vec<DeviceInfo> {
    if !Path::new(XCRUN).exists() {
        return Vec::new();
    }
    match devicectl(&["list", "devices"], LIST_TIMEOUT).await {
        Ok(v) => parse_list(&v),
        Err(e) => {
            debug!("CoreDevice listing unavailable: {e:#}");
            Vec::new()
        }
    }
}

pub async fn find(udid: &str) -> Option<DeviceInfo> {
    list().await.into_iter().find(|d| d.udid == udid)
}

/// Which of `bundle_ids` are installed on the device.
pub async fn installed(udid: &str, bundle_ids: &[String]) -> Result<HashSet<String>> {
    let v = devicectl(&["device", "info", "apps", "--device", udid], APPS_TIMEOUT)
        .await
        .context("listing installed apps over Wi-Fi")?;
    let wanted: HashSet<&str> = bundle_ids.iter().map(String::as_str).collect();
    Ok(v.pointer("/result/apps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| a.get("bundleIdentifier").and_then(Value::as_str))
        .filter(|b| wanted.contains(b))
        .map(str::to_string)
        .collect())
}

pub async fn uninstall(udid: &str, bundle_id: &str) -> Result<()> {
    devicectl(&["device", "uninstall", "app", "--device", udid, bundle_id], APPS_TIMEOUT)
        .await
        .context("uninstalling app over Wi-Fi")?;
    Ok(())
}

/// Install (or upgrade) a signed `.app` bundle.
pub async fn install(udid: &str, app: &Path) -> Result<()> {
    let path = app.to_str().context("app path is not valid UTF-8")?;
    devicectl(&["device", "install", "app", "--device", udid, path], INSTALL_TIMEOUT)
        .await
        .context("installing over Wi-Fi (CoreDevice)")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reachable_wifi_device_and_skips_others() {
        let v: Value = serde_json::json!({
            "info": {"outcome": "success"},
            "result": {"devices": [
                {
                    "connectionProperties": {"pairingState": "paired", "transportType": "localNetwork", "tunnelState": "disconnected"},
                    "deviceProperties": {"name": "Hitesh’s iPhone", "osVersionNumber": "27.2.1", "developerModeStatus": "enabled"},
                    "hardwareProperties": {"reality": "physical", "productType": "iPhone15,5", "udid": "00008120-000E59EC3A44201E"}
                },
                {
                    "connectionProperties": {"pairingState": "paired", "transportType": "sameMachine", "tunnelState": "connected"},
                    "deviceProperties": {"name": "iPhone 17 Pro"},
                    "hardwareProperties": {"reality": "simulated", "udid": "SIM"}
                },
                {
                    "connectionProperties": {"pairingState": "paired", "tunnelState": "unavailable"},
                    "deviceProperties": {"name": "Away iPad"},
                    "hardwareProperties": {"reality": "physical", "udid": "AWAY"}
                },
                {
                    "connectionProperties": {"pairingState": "unpaired", "transportType": "localNetwork"},
                    "hardwareProperties": {"reality": "physical", "udid": "UNPAIRED"}
                }
            ]}
        });
        let list = parse_list(&v);
        assert_eq!(list.len(), 1);
        let d = &list[0];
        assert_eq!(d.udid, "00008120-000E59EC3A44201E");
        assert_eq!(d.name, "Hitesh’s iPhone");
        assert_eq!(d.connection, "wifi");
        assert!(d.paired);
        assert_eq!(d.developer_mode, Some(true));
        assert_eq!(d.ios_version.as_deref(), Some("27.2.1"));
    }

    #[test]
    fn error_text_prefers_localized_description() {
        let v = serde_json::json!({
            "info": {"outcome": "failed"},
            "error": {"code": 1, "userInfo": {
                "NSLocalizedDescription": {"string": "The device is locked."},
                "NSLocalizedRecoverySuggestion": {"string": "Unlock it and try again."}
            }}
        });
        assert_eq!(error_text(&v).as_deref(), Some("The device is locked. Unlock it and try again."));
    }
}
