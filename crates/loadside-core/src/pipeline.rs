//! prepare → register device → sign → (export) → install.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use isideload::{
    dev::developer_session::DevicesApi,
    sideload::{
        SideloaderBuilder, TeamSelection,
        builder::{ExtensionsBehavior, ExtensionsBehaviorSelection, MaxCertsBehavior},
    },
    util::callbacks::{ExtensionsBehaviorCallbackBox, MaxCertsCallbackBox},
};
use tracing::info;

use crate::{
    account::{self, CertSummary, Session},
    config::Config,
    device,
    errors::RcExt,
    ipa, paths, profile,
    state::{ExtensionsMode, InstallOptions},
};

/// (stage, fraction 0..=1)
pub type Progress = Arc<dyn Fn(&str, f32) + Send + Sync>;

/// Decides which certificates to revoke when the account is at its limit.
/// Returning None aborts.
pub type CertChooser = Arc<dyn Fn(Vec<CertSummary>) -> BoxFuture<'static, Option<Vec<String>>> + Send + Sync>;

/// Opens a connection to the target device (USB or Wi-Fi sync).
pub type Connector = Arc<dyn Fn() -> BoxFuture<'static, Result<device::Conn>> + Send + Sync>;

/// The device an app is signed for / installed on.
#[derive(Clone)]
pub struct Target {
    pub udid: String,
    pub name: String,
    pub connect: Connector,
}

pub struct Request<'a> {
    pub source: &'a Path,
    pub options: &'a InstallOptions,
    pub device: Option<&'a Target>,
    pub export_to: Option<&'a Path>,
    pub install: bool,
    /// For refreshes: the bundle ID the installed app has. Signing must
    /// produce exactly this, or we'd install a second copy instead.
    pub expect_bundle_id: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub name: String,
    pub version: Option<String>,
    pub original_bundle_id: String,
    pub bundle_id: String,
    pub expires_at: Option<i64>,
    pub device_udid: Option<String>,
    pub device_name: Option<String>,
    pub exported: Option<PathBuf>,
    pub warnings: Vec<String>,
    /// The account and team that actually signed it.
    pub email: String,
    pub team_id: String,
}

/// Removes the scratch directory however we leave.
struct WorkDir(PathBuf);
impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub async fn run(
    session: &mut Session,
    config: &Config,
    req: Request<'_>,
    progress: Progress,
    certs: Option<CertChooser>,
) -> Result<Outcome> {
    let src_path = req.source.to_path_buf();
    let original = tokio::task::spawn_blocking(move || ipa::inspect(&src_path)).await??;
    let work = WorkDir(paths::work_dir().join(uuid::Uuid::new_v4().to_string()));
    std::fs::create_dir_all(&work.0)?;

    progress("Preparing app", 0.02);
    let (src, w, opts) = (req.source.to_path_buf(), work.0.clone(), req.options.clone());
    let prepared = tokio::task::spawn_blocking(move || ipa::prepare(&src, &w, &opts)).await??;
    let mut warnings = prepared.warnings.clone();
    for w in &warnings {
        tracing::warn!("{w}");
    }

    // Device registration (needed so the profile includes this device).
    if let Some(t) = req.device {
        progress("Registering device", 0.08);
        let team = session.team.clone();
        session.dev.ensure_device_registered(&team, &t.name, &t.udid, None).await.rc()?;
    }
    let device_udid = req.device.map(|t| t.udid.as_str());

    progress("Preparing certificate & App IDs", 0.12);
    let max_certs: MaxCertsBehavior<MaxCertsCallbackBox> = match certs {
        None => MaxCertsBehavior::Error,
        Some(chooser) => {
            let machine = config.machine_name.clone();
            MaxCertsBehavior::Prompt(Box::new(move |list| {
                let chooser = chooser.clone();
                let summaries = account::summarize_certs(&list, &machine);
                Box::pin(async move { Ok(chooser(summaries).await) })
            }))
        }
    };
    let ext = match req.options.extensions {
        ExtensionsMode::ReuseMain => ExtensionsBehavior::ReuseMain,
        _ => ExtensionsBehavior::RegisterAll,
    };
    let mut sideloader = SideloaderBuilder::<MaxCertsCallbackBox, ExtensionsBehaviorCallbackBox>::new(
        session.dev.clone(),
        session.email.clone(),
    )
    .team_selection(TeamSelection::First)
    .storage(Box::new(account::storage()))
    .machine_name(config.machine_name.clone())
    .max_certs_behavior(max_certs)
    .extensions_behavior(ExtensionsBehaviorSelection::Fixed(ext))
    .delete_app_after_install(false)
    .build();

    let p2 = progress.clone();
    let sign_progress = move |f: f32| {
        let p = p2.clone();
        async move { p("Signing", 0.12 + f * 0.48) }
    };
    let (signed, _special) = sideloader
        .sign_app(
            prepared.app_dir.clone(),
            Some(session.team.clone()),
            req.options.increased_memory_limit,
            Some(sign_progress),
            None,
            device_udid,
        )
        .await
        .rc()
        .context("signing failed")?;
    // The sideloader may have refreshed anisette / tokens; keep its session.
    session.dev = sideloader.get_dev_session().clone();

    let signed_path = signed.clone();
    let final_info = tokio::task::spawn_blocking(move || ipa::inspect(&signed_path)).await??;
    if let Some(expected) = req.expect_bundle_id {
        if final_info.bundle_id != expected {
            return Err(crate::protocol::Failure::new(
                "team_mismatch",
                format!("Signing now produces {} instead of {expected}, so this would install a second copy.", final_info.bundle_id),
            )
            .with_hint("This app was installed with a different Apple ID or team. Switch back to it in Account, or remove the app and install it again.")
            .into());
        }
    }
    let expires_at = profile::parse_file(&signed.join("embedded.mobileprovision")).ok().and_then(|p| p.expires_at);

    let exported = match req.export_to {
        Some(out) => {
            progress("Packaging IPA", 0.62);
            let (s, o) = (signed.clone(), out.to_path_buf());
            tokio::task::spawn_blocking(move || ipa::package(&s, &o)).await??;
            Some(out.to_path_buf())
        }
        None => None,
    };

    if req.install {
        let t = req.device.context("no device selected to install on")?;
        progress("Connecting to device", 0.64);
        let mut conn = (t.connect)().await?;
        info!("Installing over {}", conn.describe());
        progress("Installing", 0.65);
        let p3 = progress.clone();
        conn.install(&signed, move |pct| {
            let stage = if pct < 70 { "Uploading to device" } else { "Installing" };
            p3(stage, 0.65 + (pct as f32 / 100.0) * 0.35)
        })
        .await
        .context("installation failed")?;
        info!("Installed {} ({})", final_info.name, final_info.bundle_id);
    }

    if original.encrypted == Some(true) {
        warnings.push("The original binary was encrypted.".into());
    }
    progress("Done", 1.0);
    Ok(Outcome {
        name: final_info.name,
        version: final_info.version,
        original_bundle_id: original.bundle_id,
        bundle_id: final_info.bundle_id,
        expires_at,
        device_udid: req.device.map(|t| t.udid.clone()),
        device_name: req.device.map(|t| t.name.clone()),
        exported,
        warnings,
        email: session.email.clone(),
        team_id: session.team.team_id.clone(),
    })
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

pub fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}
