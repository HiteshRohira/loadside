//! IPC protocol between `loadsided` and its clients (CLI, macOS app).
//!
//! Transport: a Unix domain socket carrying newline-delimited JSON.
//! Client -> daemon: [`Request`]. Daemon -> client: [`ServerMessage`]
//! (responses to requests, plus events for subscribed connections).
//! All timestamps are unix seconds.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::{AccountInfo, AppRecord};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Response {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<Failure>,
    },
    Event {
        event: Event,
    },
}

/// A user-facing error: a stable `code`, a readable `message`, an
/// actionable `hint`, and the raw technical `detail` for the logs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Failure {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Failure {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), hint: None, detail: None }
    }
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(h) = &self.hint {
            write!(f, "\n  → {h}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Failure {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Job { job: JobInfo },
    Devices { devices: Vec<crate::device::DeviceInfo> },
    Apps { apps: Vec<AppRecord> },
    Account { account: Option<AccountInfo>, logged_in: bool },
    Prompt { prompt: Prompt },
    PromptClosed { id: String },
    Log { line: LogLine },
    Notice { level: String, title: String, message: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Install,
    Refresh,
    Export,
    Login,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobInfo {
    pub id: String,
    pub kind: JobKind,
    pub title: String,
    pub app_id: Option<String>,
    pub device_udid: Option<String>,
    pub status: JobStatus,
    pub stage: String,
    /// 0.0 ..= 1.0
    pub progress: f32,
    pub background: bool,
    pub created_at: i64,
    pub finished_at: Option<i64>,
    pub error: Option<Failure>,
    pub output_path: Option<String>,
    pub warnings: Vec<String>,
}

/// Something the daemon needs a human to answer (2FA code, which
/// certificate to revoke, ...). Answer with the `respond` method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Prompt {
    pub id: String,
    /// `two_factor` | `max_certs`
    pub kind: String,
    pub payload: Value,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogLine {
    pub ts: i64,
    pub level: String,
    pub target: String,
    pub message: String,
}

/// Response of the `status` method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: String,
    pub pid: u32,
    pub started_at: i64,
    pub account: Option<AccountInfo>,
    pub logged_in: bool,
    pub devices: Vec<crate::device::DeviceInfo>,
    pub apps: Vec<AppRecord>,
    pub jobs: Vec<JobInfo>,
    pub prompts: Vec<Prompt>,
    pub config: crate::config::Config,
    pub next_check_at: Option<i64>,
}

/// Parameters of the `install` method.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct InstallParams {
    pub path: String,
    pub device_udid: Option<String>,
    pub options: crate::state::InstallOptions,
    /// Also write the signed IPA here.
    pub export_path: Option<String>,
    /// Set to false to only sign/export without installing.
    pub install: Option<bool>,
}
