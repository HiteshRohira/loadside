//! The setup checklist ("doctor"): everything Loadside needs outside itself,
//! with a status, a plain-language explanation and the actions that fix it.
//! The daemon computes it; the app's onboarding and the CLI's `doctor` show it.

use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Done / working.
    Ok,
    /// Needs the user to do something before Loadside can work.
    Action,
    /// Works, but something is degraded.
    Warning,
    /// Broken.
    Error,
    /// Not set up, and that's fine (optional feature).
    Optional,
    /// Can't tell yet (e.g. no device to check).
    Pending,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SetupAction {
    /// Stable id the UI maps to a call: "open_url", "login", "enable_wifi_sync",
    /// "reveal_developer_mode", "refresh_devices", "retry", ...
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_udid: Option<String>,
    /// Draw as the main button.
    #[serde(default)]
    pub primary: bool,
}

impl SetupAction {
    pub fn new(id: &str, label: &str) -> Self {
        Self { id: id.into(), label: label.into(), url: None, device_udid: None, primary: false }
    }
    pub fn url(label: &str, url: &str) -> Self {
        Self { url: Some(url.into()), ..Self::new("open_url", label) }
    }
    pub fn device(mut self, udid: &str) -> Self {
        self.device_udid = Some(udid.into());
        self
    }
    pub fn primary(mut self) -> Self {
        self.primary = true;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SetupCheck {
    pub id: String,
    /// "essentials" (needed to install anything) or "optional".
    pub group: String,
    pub title: String,
    pub status: CheckStatus,
    pub summary: String,
    /// Longer, step-by-step guidance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default)]
    pub actions: Vec<SetupAction>,
}

impl SetupCheck {
    pub fn new(id: &str, group: &str, title: &str, status: CheckStatus, summary: impl Into<String>) -> Self {
        Self { id: id.into(), group: group.into(), title: title.into(), status, summary: summary.into(), detail: None, actions: vec![] }
    }
    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }
    pub fn action(mut self, a: SetupAction) -> Self {
        self.actions.push(a);
        self
    }
    pub fn is_blocking(&self) -> bool {
        self.group == "essentials" && matches!(self.status, CheckStatus::Action | CheckStatus::Error)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SetupReport {
    pub checks: Vec<SetupCheck>,
    /// Every essential check passes.
    pub ready: bool,
    pub onboarding_done: bool,
    pub generated_at: i64,
}

/// Outcome of one reachability check.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Reachability {
    pub ok: bool,
    pub error: Option<String>,
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(6))
        .user_agent(concat!("Loadside/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_default()
}

fn describe(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".into()
    } else if e.is_connect() {
        let mut s = e.to_string();
        let mut src = std::error::Error::source(e);
        while let Some(inner) = src {
            s = inner.to_string();
            src = inner.source();
        }
        format!("couldn't connect ({s})")
    } else {
        e.to_string()
    }
}

/// Can we reach the anisette server (`GET /v3/client_info`)?
pub async fn check_anisette(url: &str) -> Reachability {
    match http().get(format!("{}/v3/client_info", url.trim_end_matches('/'))).send().await {
        Ok(r) if r.status().is_success() => match r.json::<serde_json::Value>().await {
            Ok(v) if v.get("client_info").is_some() || v.get("user_agent").is_some() => Reachability { ok: true, error: None },
            Ok(_) => Reachability { ok: false, error: Some("the server answered, but not like an anisette-v3 server".into()) },
            Err(e) => Reachability { ok: false, error: Some(format!("unexpected response: {e}")) },
        },
        Ok(r) => Reachability { ok: false, error: Some(format!("HTTP {}", r.status())) },
        Err(e) => Reachability { ok: false, error: Some(describe(&e)) },
    }
}

/// Can we reach Apple's sign-in servers? Any HTTP answer counts.
pub async fn check_apple() -> Reachability {
    match http().get("https://gsa.apple.com/grandslam/GsService2/lookup").send().await {
        Ok(_) => Reachability { ok: true, error: None },
        Err(e) => Reachability { ok: false, error: Some(describe(&e)) },
    }
}
