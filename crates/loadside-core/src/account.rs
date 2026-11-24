//! Apple ID sign-in and developer-account operations.

use anyhow::{Context, Result};
use isideload::{
    anisette::{AnisetteClientInfo, remote_v3::{ANISETTE_STATE_KEY, RemoteV3AnisetteProvider}},
    auth::builder::AppleAccountBuilder,
    dev::developer_session::{
        AppIdsApi, CertificatesApi, DeveloperSession, DeveloperTeam, TeamsApi,
    },
    util::{callbacks::TwoFactorCallback, keyring_storage::KeyringStorage, storage::SideloadingStorage},
};
use serde::{Deserialize, Serialize};

use crate::{config::Config, errors::RcExt, secrets};

pub use isideload::auth::apple_account::{TwoFactorCallbackParams, TwoFactorCallbackResponse};

pub fn storage() -> KeyringStorage {
    KeyringStorage::new(secrets::STORAGE_SERVICE.to_string())
}

/// A current macOS identity (iloader issue #1203): Apple answers the 10.12
/// identity public anisette servers report with SMS-only 2FA and 403s, and
/// "<macOS;" identities with 503. The user agent must match the client info.
pub const MODERN_CLIENT_INFO: &str = "<MacBookAir10,1> <Mac OS X;27.3;26C5035r> <com.apple.AuthKit/1 (com.apple.akd/1.0)>";
pub const MODERN_USER_AGENT: &str = "akd/1.0 CFNetwork/3896.300.31 Darwin/27.3.0";

fn client_override(config: &Config) -> Option<AnisetteClientInfo> {
    config
        .modern_client_identity
        .then(|| AnisetteClientInfo { client_info: MODERN_CLIENT_INFO.into(), user_agent: MODERN_USER_AGENT.into() })
}

/// Anisette state is tied to the server and the client identity: Apple
/// refuses a login that reuses another identity's machine (-22410), and a
/// new server must never receive the old server's provisioning data.
pub fn anisette_storage(config: &Config) -> KeyringStorage {
    use sha2::{Digest, Sha256};
    let identity = client_override(config).map(|c| c.client_info).unwrap_or_else(|| "server".into());
    let digest = Sha256::digest(format!("{}|{}", config.anisette_url, identity).as_bytes());
    KeyringStorage::new(format!("{}.anisette-{}", secrets::STORAGE_SERVICE, &hex::encode(digest)[..12]))
}

/// Forget the anisette (machine identity) state for the current server and
/// identity so a fresh one is provisioned on next sign-in.
pub fn reset_anisette(config: &Config) -> Result<()> {
    anisette_storage(config).delete(ANISETTE_STATE_KEY).rc()
}

/// isideload's log masking slices the first byte of the address, so reject
/// anything that isn't a plain ASCII email before it gets there.
pub fn validate_email(email: &str) -> Result<()> {
    let ok = email.is_ascii()
        && email.len() <= 254
        && !email.chars().any(|c| c.is_whitespace())
        && email.split_once('@').is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.'));
    if ok {
        Ok(())
    } else {
        Err(crate::protocol::Failure::new("bad_email", "That doesn't look like an Apple ID email address.")
            .with_hint("Enter the full email of your Apple ID, e.g. name@example.com.")
            .into())
    }
}

#[derive(Clone)]
pub struct Session {
    pub email: String,
    pub name: Option<String>,
    pub dev: DeveloperSession,
    pub teams: Vec<DeveloperTeam>,
    pub team: DeveloperTeam,
    /// Teams Apple marks as free ("Xcode Free Provisioning Program").
    pub free_team_ids: Vec<String>,
    pub token_expires_at: Option<i64>,
    pub logged_in_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamSummary {
    pub team_id: String,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub is_free: bool,
}

/// Fallback when the membership list couldn't be read. Free teams used to be
/// named "<name> (Personal Team)"; Apple now names them after the person and
/// reports type "Individual", exactly like paid individual teams.
fn team_name_says_free(t: &DeveloperTeam) -> bool {
    t.name.as_deref().is_some_and(|n| n.contains("Personal Team"))
        || t.r#type.as_deref().is_some_and(|k| k.eq_ignore_ascii_case("free") || k.contains("Personal"))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTeam {
    team_id: String,
    #[serde(default)]
    memberships: Vec<RawMembership>,
}

#[derive(Deserialize)]
struct RawMembership {
    #[serde(default)]
    name: Option<String>,
}

/// Free teams have a single membership named "Xcode Free Provisioning
/// Program"; paid ones list "Apple Developer Program" (same rule AltStore
/// uses). isideload's team type doesn't expose memberships, so ask directly.
async fn free_team_ids(dev: &mut DeveloperSession, teams: &[DeveloperTeam]) -> Vec<String> {
    use isideload::dev::device_type::{DeveloperDeviceType, dev_url};
    match dev.send_dev_request::<Vec<RawTeam>>(&dev_url("listTeams", DeveloperDeviceType::Any), None, "teams").await {
        Ok(raw) => raw
            .into_iter()
            .filter(|t| {
                t.memberships.len() == 1
                    && t.memberships[0].name.as_deref().is_some_and(|n| n.to_lowercase().contains("free"))
            })
            .map(|t| t.team_id)
            .collect(),
        Err(e) => {
            tracing::warn!("Couldn't read team memberships, guessing free/paid from the team name: {e}");
            teams.iter().filter(|t| team_name_says_free(t)).map(|t| t.team_id.clone()).collect()
        }
    }
}

impl Session {
    /// True if the developer token is (about to be) expired.
    pub fn is_stale(&self) -> bool {
        let now = crate::now();
        match self.token_expires_at {
            Some(exp) => now > exp - 300,
            // Unknown expiry: be conservative and re-auth every few hours.
            None => now - self.logged_in_at > 3 * 3600,
        }
    }

    pub fn account_info(&self) -> crate::state::AccountInfo {
        crate::state::AccountInfo {
            email: self.email.clone(),
            name: self.name.clone(),
            team_id: Some(self.team.team_id.clone()),
            team_name: self.team.name.clone(),
            is_free: Some(self.is_free(&self.team)),
        }
    }

    pub fn is_free(&self, t: &DeveloperTeam) -> bool {
        self.free_team_ids.contains(&t.team_id)
    }

    pub fn team_summaries(&self) -> Vec<TeamSummary> {
        self.teams
            .iter()
            .map(|t| TeamSummary { team_id: t.team_id.clone(), name: t.name.clone(), kind: t.r#type.clone(), is_free: self.is_free(t) })
            .collect()
    }

    pub fn select_team(&mut self, team_id: &str) -> Result<()> {
        let t = self
            .teams
            .iter()
            .find(|t| t.team_id == team_id)
            .cloned()
            .with_context(|| format!("no team with ID {team_id} on this account"))?;
        self.team = t;
        Ok(())
    }
}

fn normalize_expiry(raw: u64) -> Option<i64> {
    if raw == 0 {
        None
    } else if raw > 10_000_000_000 {
        Some((raw / 1000) as i64) // milliseconds
    } else {
        Some(raw as i64)
    }
}

pub async fn login<C: TwoFactorCallback>(
    config: &Config,
    email: &str,
    password: &str,
    preferred_team: Option<&str>,
    two_factor: C,
) -> Result<Session> {
    validate_email(email)?;
    let provider = RemoteV3AnisetteProvider::new(&config.anisette_url, Box::new(anisette_storage(config)), "0".to_string())
        .rc()?
        .set_client_info_override(client_override(config));
    let mut account = AppleAccountBuilder::new(email)
        .anisette_provider(provider)
        .login(password, two_factor)
        .await
        .rc()?;
    let name = account.get_name().ok().map(|(f, l)| format!("{f} {l}").trim().to_string()).filter(|s| !s.is_empty());
    let token = account.get_app_token("xcode.auth").await.rc().context("getting developer token")?;
    let adsid = account
        .spd
        .as_ref()
        .and_then(|d| d.get("adsid"))
        .and_then(|v| v.as_string())
        .context("sign-in response is missing adsid")?
        .to_string();
    let token_expires_at = normalize_expiry(token.expiry);
    let mut dev = DeveloperSession::new(token, adsid, account.grandslam_client.clone(), account.anisette_generator.clone());
    let teams = dev.list_teams().await.rc()?;
    let free_team_ids = free_team_ids(&mut dev, &teams).await;
    let team = preferred_team
        .and_then(|id| teams.iter().find(|t| t.team_id == id))
        .or_else(|| teams.first())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("No developer teams available"))?;
    Ok(Session {
        email: email.to_string(),
        name,
        dev,
        teams,
        team,
        free_team_ids,
        token_expires_at,
        logged_in_at: crate::now(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppIdSummary {
    pub app_id_id: String,
    pub identifier: String,
    pub name: String,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppIdList {
    pub app_ids: Vec<AppIdSummary>,
    pub max_quantity: Option<u64>,
    pub available_quantity: Option<i64>,
}

fn date_secs(d: &plist::Date) -> i64 {
    std::time::SystemTime::from(*d)
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub async fn list_app_ids(s: &mut Session) -> Result<AppIdList> {
    let team = s.team.clone();
    let r = s.dev.list_app_ids(&team, None).await.rc()?;
    Ok(AppIdList {
        app_ids: r
            .app_ids
            .into_iter()
            .map(|a| AppIdSummary {
                app_id_id: a.app_id_id,
                identifier: a.identifier,
                name: a.name,
                expires_at: a.expiration_date.as_ref().map(date_secs),
            })
            .collect(),
        max_quantity: r.max_quantity,
        available_quantity: r.available_quantity,
    })
}

pub async fn delete_app_id(s: &mut Session, app_id_id: &str) -> Result<()> {
    let team = s.team.clone();
    s.dev.delete_app_id(&team, app_id_id, None).await.rc()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CertSummary {
    pub serial_number: String,
    pub name: Option<String>,
    pub machine_name: Option<String>,
    pub expires_at: Option<i64>,
    /// Created by this Loadside installation (same machine name).
    pub ours: bool,
}

pub fn summarize_certs(
    certs: &[isideload::dev::developer_session::DevelopmentCertificate],
    machine_name: &str,
) -> Vec<CertSummary> {
    certs
        .iter()
        .filter_map(|c| {
            Some(CertSummary {
                serial_number: c.serial_number.clone()?,
                name: c.name.clone(),
                machine_name: c.machine_name.clone(),
                expires_at: c.expiration_date.as_ref().map(date_secs),
                ours: c.machine_name.as_deref() == Some(machine_name),
            })
        })
        .collect()
}

pub async fn list_certs(s: &mut Session, machine_name: &str) -> Result<Vec<CertSummary>> {
    let team = s.team.clone();
    let certs = s.dev.list_ios_certs(&team).await.rc()?;
    Ok(summarize_certs(&certs, machine_name))
}

pub async fn revoke_cert(s: &mut Session, serial: &str) -> Result<()> {
    let team = s.team.clone();
    s.dev.revoke_development_cert(&team, serial, None).await.rc()
}
