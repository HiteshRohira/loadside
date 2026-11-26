//! GitHub release tracking inside `loadsided`: inspecting repos, staging
//! pinned downloads for fresh installs, checking tracked apps on a schedule,
//! verifying app identity and repo ownership, and running in-place update jobs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use loadside_core::{
    errors,
    github::{self, CheckOutcome},
    ipa, paths, pipeline,
    protocol::{Event, Failure},
    secrets,
    state::{AppRecord, GitHubSource, InstallOptions, ReleaseInfo, UpdatePolicy},
};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::engine::{Engine, fmt_date};

fn f(code: &str, msg: &str) -> Failure {
    Failure::new(code, msg)
}

/// Removes an owned temporary download directory inside `work_dir` when dropped.
pub struct TempDownload {
    file: PathBuf,
    owned_dir: PathBuf,
}

impl TempDownload {
    pub fn path(&self) -> &Path {
        &self.file
    }
}

impl Drop for TempDownload {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.owned_dir);
    }
}

/// A pre-staged copy inside `library_dir` that can be atomically renamed over
/// `library_dir/{record_id}.ipa` once device installation succeeds.
pub struct StagedLibraryFile {
    staged: PathBuf,
    dest: PathBuf,
    committed: bool,
}

impl StagedLibraryFile {
    pub fn stage(src: &Path, record_id: &str) -> anyhow::Result<Self> {
        let lib = paths::library_dir();
        std::fs::create_dir_all(&lib)?;
        let dest = lib.join(format!("{record_id}.ipa"));
        let staged = lib.join(format!("{record_id}.new-{}.ipa", pipeline::new_id()));
        std::fs::copy(src, &staged)?;
        Ok(Self { staged, dest, committed: false })
    }

    pub fn commit(mut self) -> anyhow::Result<PathBuf> {
        std::fs::rename(&self.staged, &self.dest)?;
        let other = paths::library_dir().join(
            self.dest
                .file_stem()
                .map(|s| format!("{}.app", s.to_string_lossy()))
                .unwrap_or_default(),
        );
        if !other.as_os_str().is_empty() && other != self.dest {
            let _ = std::fs::remove_dir_all(&other);
            let _ = std::fs::remove_file(&other);
        }
        self.committed = true;
        Ok(self.dest.clone())
    }
}

impl Drop for StagedLibraryFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.staged);
        }
    }
}

impl Engine {
    /// Whether a GitHub token is saved in the macOS Keychain (cached; never prompts).
    pub async fn github_token_configured(&self) -> bool {
        if let Some(v) = *self.github_token_cached.lock().unwrap() {
            return v;
        }
        let v = tokio::task::spawn_blocking(secrets::has_github_token).await.unwrap_or(false);
        *self.github_token_cached.lock().unwrap() = Some(v);
        v
    }

    /// Read the GitHub token from the Keychain only if one actually exists.
    /// Self-heals the cached flag if the token was removed externally via Keychain Access.
    async fn read_github_token(&self) -> Option<String> {
        if !self.github_token_configured().await {
            return None;
        }
        let res = tokio::task::spawn_blocking(secrets::github_token).await;
        match res {
            Ok(Ok(Some(t))) if !t.trim().is_empty() => Some(t),
            Ok(Ok(_)) => {
                *self.github_token_cached.lock().unwrap() = Some(false);
                self.emit(Event::GitHubToken { configured: false });
                None
            }
            Ok(Err(e)) => {
                warn!("Couldn't read GitHub token from Keychain: {e:#}");
                None
            }
            Err(e) => {
                warn!("Keychain task failed: {e}");
                None
            }
        }
    }

    pub async fn set_github_token(&self, token: Option<String>) -> Result<Value, Failure> {
        let cleaned = token.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let configured = cleaned.is_some();
        let res = tokio::task::spawn_blocking(move || match cleaned {
            Some(tok) => secrets::save_github_token(&tok),
            None => secrets::delete_github_token(),
        })
        .await
        .map_err(|e| f("internal", &e.to_string()))?;

        if let Err(e) = res {
            return Err(f("keychain", "Couldn't update the GitHub token in the Keychain.")
                .with_hint(format!("{e:#}")));
        }
        *self.github_token_cached.lock().unwrap() = Some(configured);
        self.emit(Event::GitHubToken { configured });
        Ok(json!({ "configured": configured }))
    }

    /// Inspect a GitHub repository's latest qualifying release and `.ipa` assets.
    pub async fn github_inspect(
        &self,
        repo_input: &str,
        include_prereleases: bool,
        asset_pattern: Option<&str>,
    ) -> Result<Value, Failure> {
        let repo = github::parse_repo(repo_input)?;
        let token = self.read_github_token().await;
        let (rel_res, id_res) = tokio::join!(
            github::fetch_latest_release(&repo, include_prereleases, asset_pattern, None, token.as_deref()),
            github::fetch_repo_identity(&repo, token.as_deref()),
        );
        let outcome = rel_res?;
        let (repo_id, canonical_repo) = match id_res {
            Ok((id, full)) => (Some(id), full),
            Err(_) => (None, repo),
        };
        match outcome {
            CheckOutcome::Found { release, all_ipa_assets, .. } => {
                let suggested_patterns: std::collections::HashMap<String, String> = all_ipa_assets
                    .iter()
                    .filter_map(|a| {
                        github::infer_asset_pattern(&a.name, &all_ipa_assets).map(|p| (a.name.clone(), p))
                    })
                    .collect();
                Ok(json!({
                    "repo": canonical_repo,
                    "repo_id": repo_id,
                    "release": release,
                    "assets": all_ipa_assets,
                    "suggested_patterns": suggested_patterns,
                }))
            }
            CheckOutcome::NotModified => Err(f("internal", "Unexpected 304 without ETag.")),
        }
    }

    /// Download a GitHub release `.ipa` into Loadside's staging directory and inspect it,
    /// ready to hand off to the install flow. If `release_id` and `asset_id` are supplied,
    /// fetches that exact pinned release/asset so what the user approved is what downloads.
    pub async fn github_download(
        &self,
        repo_input: &str,
        include_prereleases: bool,
        asset_pattern: Option<&str>,
        asset_name: Option<&str>,
        release_id: Option<u64>,
        asset_id: Option<u64>,
    ) -> Result<Value, Failure> {
        let repo = github::parse_repo(repo_input)?;
        let token = self.read_github_token().await;
        let (repo_id, canonical_repo) = match github::fetch_repo_identity(&repo, token.as_deref()).await {
            Ok((id, full)) => (Some(id), full),
            Err(_) => (None, repo.clone()),
        };

        let (release, all_assets) = match (release_id, asset_id) {
            (Some(rid), Some(aid)) => github::fetch_pinned_release(&canonical_repo, rid, aid, token.as_deref()).await?,
            _ => {
                let selector = asset_name.or(asset_pattern);
                match github::fetch_latest_release(&canonical_repo, include_prereleases, selector, None, token.as_deref()).await? {
                    CheckOutcome::Found { release, all_ipa_assets, .. } => (release, all_ipa_assets),
                    CheckOutcome::NotModified => return Err(f("internal", "Unexpected 304 without ETag.")),
                }
            }
        };

        if !ipa::valid_file_name(&release.asset.name) {
            return Err(f("invalid_asset", "GitHub release asset has an unsafe filename."));
        }
        let stage_dir = paths::incoming_dir().join(pipeline::new_uuid());
        std::fs::create_dir_all(&stage_dir)
            .map_err(|e| f("io", &format!("Couldn't prepare staging folder: {e}")))?;
        let dest = stage_dir.join(&release.asset.name);

        if let Err(fl) = github::download_asset(&release.asset, &dest, token.as_deref(), |_, _| {}).await {
            let _ = std::fs::remove_dir_all(&stage_dir);
            return Err(fl);
        }

        let inspect_path = dest.clone();
        let info = match tokio::task::spawn_blocking(move || ipa::inspect(&inspect_path))
            .await
            .map_err(|e| f("internal", &e.to_string()))?
        {
            Ok(i) => i,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&stage_dir);
                return Err(errors::explain(&e));
            }
        };
        let app_ids_needed = info.app_ids_needed(&InstallOptions::default());
        let inferred_pattern = asset_pattern
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| github::infer_asset_pattern(&release.asset.name, &all_assets));

        Ok(json!({
            "repo": canonical_repo,
            "repo_id": repo_id,
            "path": dest.display().to_string(),
            "info": info,
            "app_ids_needed": app_ids_needed,
            "release": release,
            "asset_pattern": inferred_pattern,
        }))
    }

    /// Connect, configure, or disconnect GitHub release tracking on an existing app.
    pub async fn set_app_github(
        self: &Arc<Self>,
        app_id: &str,
        disconnect: bool,
        repo: Option<&str>,
        policy: Option<UpdatePolicy>,
        include_prereleases: Option<bool>,
        asset_pattern: Option<Option<String>>,
    ) -> Result<Value, Failure> {
        if disconnect {
            let mut found = false;
            self.mutate_app(app_id, |a| {
                found = true;
                a.github = None;
            });
            if !found {
                return Err(f("not_found", "Unknown app."));
            }
            self.persist()?;
            self.emit_apps();
            return Ok(Value::Null);
        }

        let normalized_repo = match repo {
            Some(r) if !r.trim().is_empty() => Some(github::parse_repo(r)?),
            _ => None,
        };

        // Determine whether we need to fetch & pin the numeric repository ID.
        let need_repo_id_for: Option<String> = {
            let st = self.state.lock().unwrap();
            let app = st.app(app_id).ok_or_else(|| f("not_found", "Unknown app."))?;
            match (&app.github, &normalized_repo) {
                (None, Some(r)) => Some(r.clone()),
                (Some(gh), Some(r)) if !gh.repo.eq_ignore_ascii_case(r) || gh.repo_id.is_none() => Some(r.clone()),
                (Some(gh), None) if gh.repo_id.is_none() => Some(gh.repo.clone()),
                _ => None,
            }
        };

        let fetched_identity = if let Some(ref r) = need_repo_id_for {
            let token = self.read_github_token().await;
            match github::fetch_repo_identity(r, token.as_deref()).await {
                Ok((id, full)) => Some((id, full)),
                Err(fl) if normalized_repo.is_some() => return Err(fl),
                Err(_) => None,
            }
        } else {
            None
        };

        let mut needs_check = false;
        let mut became_immediate = false;
        let mut found = false;
        let mut missing_repo = false;

        self.mutate_app(app_id, |a| {
            found = true;
            let gh = match a.github.as_mut() {
                Some(existing) => existing,
                None => {
                    let Some(ref r) = normalized_repo else {
                        missing_repo = true;
                        return;
                    };
                    let canonical = fetched_identity.as_ref().map(|(_, full)| full.clone()).unwrap_or_else(|| r.clone());
                    a.github = Some(GitHubSource {
                        repo: canonical,
                        repo_id: fetched_identity.as_ref().map(|(id, _)| *id),
                        ..Default::default()
                    });
                    needs_check = true;
                    a.github.as_mut().unwrap()
                }
            };

            if let Some(ref new_repo) = normalized_repo {
                if !gh.repo.eq_ignore_ascii_case(new_repo) {
                    let canonical = fetched_identity.as_ref().map(|(_, full)| full.clone()).unwrap_or_else(|| new_repo.clone());
                    gh.repo = canonical;
                    gh.repo_id = fetched_identity.as_ref().map(|(id, _)| *id);
                    gh.etag = None;
                    gh.available_release = None;
                    gh.last_check_error = None;
                    gh.last_checked_at = None;
                    gh.installed_release_id = None;
                    gh.installed_tag = None;
                    gh.installed_asset_name = None;
                    gh.last_update_attempt_at = None;
                    gh.consecutive_update_failures = 0;
                    needs_check = true;
                } else if let Some((id, full)) = &fetched_identity {
                    gh.repo = full.clone();
                    if gh.repo_id.is_none() {
                        gh.repo_id = Some(*id);
                    }
                }
            } else if gh.repo_id.is_none() {
                if let Some((id, _)) = &fetched_identity {
                    gh.repo_id = Some(*id);
                }
            }
            if let Some(pre) = include_prereleases {
                if gh.include_prereleases != pre {
                    gh.include_prereleases = pre;
                    gh.etag = None;
                    gh.available_release = None;
                    gh.last_check_error = None;
                    gh.consecutive_update_failures = 0;
                    gh.last_update_attempt_at = None;
                    needs_check = true;
                }
            }
            if let Some(pat_opt) = asset_pattern.clone() {
                let cleaned = pat_opt.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
                if gh.asset_pattern != cleaned {
                    gh.asset_pattern = cleaned;
                    gh.etag = None;
                    gh.available_release = None;
                    gh.last_check_error = None;
                    gh.consecutive_update_failures = 0;
                    gh.last_update_attempt_at = None;
                    needs_check = true;
                }
            }
            if let Some(pol) = policy {
                if gh.policy != pol {
                    gh.policy = pol;
                    if pol == UpdatePolicy::Immediate && gh.available_release.is_some() {
                        became_immediate = true;
                    }
                }
            }
        });

        if !found {
            return Err(f("not_found", "Unknown app."));
        }
        if missing_repo {
            return Err(f("invalid_repo", "Enter a GitHub repository URL or owner/repo."));
        }

        self.persist()?;
        self.emit_apps();

        if needs_check {
            let _ = self.check_updates(Some(app_id), true).await;
        } else if became_immediate {
            self.trigger_check();
        }
        Ok(Value::Null)
    }

    /// Background scheduler entry point: check tracked apps whose check interval elapsed.
    pub async fn check_updates_due(self: &Arc<Self>) {
        let has_tracked = self.state.lock().unwrap().apps.iter().any(|a| a.github.is_some());
        if !has_tracked {
            return;
        }
        let _ = self.check_updates(None, false).await;
    }

    /// Check GitHub for newer releases on one app or all tracked apps.
    pub async fn check_updates(self: &Arc<Self>, app_id: Option<&str>, force: bool) -> Result<Value, Failure> {
        let _guard = self.github_check_lock.lock().await;
        let interval = self.config().release_check_interval_minutes as i64 * 60;
        let now = loadside_core::now();

        let targets: Vec<(String, String, GitHubSource, String)> = {
            let st = self.state.lock().unwrap();
            if let Some(id) = app_id {
                let app = st.app(id).ok_or_else(|| f("not_found", "Unknown app."))?;
                let gh = app.github.clone().ok_or_else(|| f("no_github", "This app isn't connected to a GitHub repository."))?;
                vec![(app.id.clone(), app.name.clone(), gh, app.device_udid.clone())]
            } else {
                st.apps
                    .iter()
                    .filter_map(|a| {
                        let gh = a.github.clone()?;
                        if !force {
                            if let Some(last) = gh.last_checked_at {
                                if now - last < interval {
                                    return None;
                                }
                            }
                        }
                        Some((a.id.clone(), a.name.clone(), gh, a.device_udid.clone()))
                    })
                    .collect()
            }
        };

        if targets.is_empty() {
            let avail_count = self
                .state
                .lock()
                .unwrap()
                .apps
                .iter()
                .filter(|a| a.github.as_ref().and_then(|g| g.available_release.as_ref()).is_some())
                .count();
            return Ok(json!({ "checked": 0, "updates_available": avail_count, "failed": 0 }));
        }

        let token = self.read_github_token().await;
        let mut checked = 0usize;
        let mut failed = 0usize;
        let mut changed = false;
        let mut trigger_immediate = false;

        for (id, app_name, snap_gh, device_udid) in targets {
            let etag = if force { None } else { snap_gh.etag.as_deref() };
            let res = github::fetch_latest_release(
                &snap_gh.repo,
                snap_gh.include_prereleases,
                snap_gh.asset_pattern.as_deref(),
                etag,
                token.as_deref(),
            )
            .await;
            let checked_at = loadside_core::now();
            checked += 1;

            match res {
                Ok(CheckOutcome::NotModified) => {
                    self.mutate_app(&id, |a| {
                        let Some(g) = a.github.as_mut() else { return };
                        // Revalidate: don't overwrite if the user edited this app's GitHub settings while in flight.
                        if !g.repo.eq_ignore_ascii_case(&snap_gh.repo)
                            || g.include_prereleases != snap_gh.include_prereleases
                            || g.asset_pattern != snap_gh.asset_pattern
                        {
                            return;
                        }
                        g.last_checked_at = Some(checked_at);
                        g.last_check_error = None;
                        changed = true;
                    });
                }
                Ok(CheckOutcome::Found { release, etag, .. }) => {
                    let dev_connected = self.device_connected(&device_udid);
                    let mut notify_info: Option<(String, String)> = None;

                    self.mutate_app(&id, |a| {
                        let Some(g) = a.github.as_mut() else { return };
                        if !g.repo.eq_ignore_ascii_case(&snap_gh.repo)
                            || g.include_prereleases != snap_gh.include_prereleases
                            || g.asset_pattern != snap_gh.asset_pattern
                        {
                            return;
                        }
                        // Compute against LIVE state so a concurrent install/update can't be undone!
                        let newer = github::is_newer_release(
                            g.installed_release_id,
                            g.installed_tag.as_deref(),
                            a.version.as_deref(),
                            &release,
                        );
                        let prev_avail_id = g.available_release.as_ref().map(|r| r.release_id);
                        let newly_discovered = newer && prev_avail_id != Some(release.release_id);

                        g.last_checked_at = Some(checked_at);
                        g.etag = etag.clone().or(g.etag.clone());
                        g.last_check_error = None;
                        changed = true;

                        if newer {
                            if prev_avail_id != Some(release.release_id) {
                                g.consecutive_update_failures = 0;
                                g.last_update_attempt_at = None;
                            }
                            g.available_release = Some(release.clone());
                            info!("GitHub update found for {app_name}: {} ({})", release.tag, g.repo);
                            match g.policy {
                                UpdatePolicy::Immediate => {
                                    trigger_immediate = true;
                                    if newly_discovered && !dev_connected {
                                        let dev = if a.device_name.is_empty() {
                                            "your device".to_string()
                                        } else {
                                            a.device_name.clone()
                                        };
                                        notify_info = Some((
                                            format!("Update ready for {}", a.name),
                                            format!("{} will install automatically when {dev} connects.", release.tag),
                                        ));
                                    }
                                }
                                UpdatePolicy::OnRefresh if newly_discovered => {
                                    notify_info = Some((
                                        format!("Update available for {}", a.name),
                                        format!("{} is out on GitHub and will install on the next refresh.", release.tag),
                                    ));
                                }
                                UpdatePolicy::Manual if newly_discovered => {
                                    notify_info = Some((
                                        format!("Update available for {}", a.name),
                                        format!("{} is out on GitHub. Open Loadside to update.", release.tag),
                                    ));
                                }
                                _ => {}
                            }
                        } else {
                            g.available_release = None;
                            if g.installed_release_id.is_none() {
                                let ver_eq = a.version.as_deref().is_some_and(|v| {
                                    github::compare_versions(&release.tag, v) == Some(std::cmp::Ordering::Equal)
                                        || release
                                            .tag
                                            .trim_start_matches(['v', 'V'])
                                            .eq_ignore_ascii_case(v.trim_start_matches(['v', 'V']))
                                });
                                if ver_eq {
                                    g.installed_release_id = Some(release.release_id);
                                    g.installed_tag = Some(release.tag.clone());
                                }
                            }
                        }
                    });

                    if let Some((title, body)) = notify_info {
                        self.notify(&title, &body);
                    }
                }
                Err(fl) => {
                    failed += 1;
                    warn!("GitHub check failed for {app_name} ({}): {}", snap_gh.repo, fl.message);
                    let rate_limited = fl.code == "github_rate_limit";
                    self.mutate_app(&id, |a| {
                        let Some(g) = a.github.as_mut() else { return };
                        if !g.repo.eq_ignore_ascii_case(&snap_gh.repo)
                            || g.include_prereleases != snap_gh.include_prereleases
                            || g.asset_pattern != snap_gh.asset_pattern
                        {
                            return;
                        }
                        g.last_checked_at = Some(checked_at);
                        g.last_check_error = Some(Failure { detail: None, ..fl.clone() });
                        changed = true;
                    });
                    if rate_limited {
                        break;
                    }
                }
            }
        }

        if changed {
            self.save_state();
            self.emit_apps();
        }
        if trigger_immediate {
            self.check_refreshes();
        }

        let updates_available = self
            .state
            .lock()
            .unwrap()
            .apps
            .iter()
            .filter(|a| a.github.as_ref().and_then(|g| g.available_release.as_ref()).is_some())
            .count();
        Ok(json!({ "checked": checked, "updates_available": updates_available, "failed": failed }))
    }

    pub(crate) fn update_backoff_active(gh: &GitHubSource, now: i64) -> bool {
        let Some(last) = gh.last_update_attempt_at else { return false };
        let backoff = match gh.consecutive_update_failures {
            0 => 300,
            n => (1800i64 << (n - 1).min(5)).min(12 * 3600),
        };
        now - last <= backoff
    }

    /// Apps configured for immediate GitHub updates that have a newer release ready
    /// and whose device is connected.
    pub fn due_updates(&self) -> Vec<String> {
        let now = loadside_core::now();
        if self.needs_interactive_login.load(Ordering::SeqCst) {
            return vec![];
        }
        let st = self.state.lock().unwrap();
        if st.account.is_none() {
            return vec![];
        }
        st.apps
            .iter()
            .filter(|a| a.on_device != Some(false))
            .filter(|a| {
                let Some(gh) = &a.github else { return false };
                if gh.policy != UpdatePolicy::Immediate || gh.available_release.is_none() {
                    return false;
                }
                !Self::update_backoff_active(gh, now)
            })
            .filter(|a| self.device_usable(&a.device_udid))
            .filter(|a| !self.job_active_for_app(&a.id))
            .map(|a| a.id.clone())
            .collect()
    }

    /// Verify that the GitHub repository's numeric ID matches the pinned ID (if any),
    /// protecting against account/repo rename hijack attacks.
    async fn verify_repo_ownership(&self, app_id: &str, gh: &GitHubSource) -> Result<(), Failure> {
        let Some(expected_id) = gh.repo_id else { return Ok(()) };
        let token = self.read_github_token().await;
        let (actual_id, _) = github::fetch_repo_identity(&gh.repo, token.as_deref()).await?;
        if actual_id != expected_id {
            let fl = f(
                "repo_hijacked",
                &format!("{} now points to a different GitHub repository than when you connected it.", gh.repo),
            )
            .with_hint(
                "Loadside paused automatic updates for your safety. Verify the repository owner on GitHub, then disconnect and reconnect it if this change was intentional.",
            );
            self.mutate_app(app_id, |a| {
                if let Some(g) = a.github.as_mut() {
                    g.last_check_error = Some(fl.clone());
                }
            });
            self.save_state();
            self.emit_apps();
            return Err(fl);
        }
        Ok(())
    }

    /// Called by `run_refresh` for apps with `UpdatePolicy::OnRefresh` (or `Immediate`).
    /// If a newer GitHub release is available (and not in update failure backoff), downloads,
    /// verifies bundle identity & encryption, and returns it.
    pub(crate) async fn maybe_fetch_release_for_refresh(
        self: &Arc<Self>,
        job_id: &str,
        rec: &AppRecord,
        background: bool,
    ) -> Option<(ReleaseInfo, Result<(TempDownload, StagedLibraryFile), Failure>)> {
        let gh = rec.github.clone()?;
        if !matches!(gh.policy, UpdatePolicy::OnRefresh | UpdatePolicy::Immediate) {
            return None;
        }
        let now = loadside_core::now();
        // During background refreshes, don't keep re-downloading a release that already failed.
        if background && gh.consecutive_update_failures > 0 && Self::update_backoff_active(&gh, now) {
            return None;
        }

        // If no update is cached yet and we haven't checked recently, do a quick check now.
        let mut release = gh.available_release.clone();
        let stale = gh.last_checked_at.is_none_or(|t| now - t > 300);
        if release.is_none() && stale {
            self.progress_for(job_id)("Checking GitHub for updates", 0.02);
            let token = self.read_github_token().await;
            let check = github::fetch_latest_release(
                &gh.repo,
                gh.include_prereleases,
                gh.asset_pattern.as_deref(),
                gh.etag.as_deref(),
                token.as_deref(),
            )
            .await;
            let ts = loadside_core::now();
            match check {
                Ok(CheckOutcome::NotModified) => {
                    self.mutate_app(&rec.id, |a| {
                        if let Some(g) = a.github.as_mut() {
                            if g.repo.eq_ignore_ascii_case(&gh.repo)
                                && g.include_prereleases == gh.include_prereleases
                                && g.asset_pattern == gh.asset_pattern
                            {
                                g.last_checked_at = Some(ts);
                                g.last_check_error = None;
                            }
                        }
                    });
                    self.save_state();
                    self.emit_apps();
                }
                Ok(CheckOutcome::Found { release: rel, etag, .. }) => {
                    let mut is_newer_live = false;
                    self.mutate_app(&rec.id, |a| {
                        if let Some(g) = a.github.as_mut() {
                            if !g.repo.eq_ignore_ascii_case(&gh.repo)
                                || g.include_prereleases != gh.include_prereleases
                                || g.asset_pattern != gh.asset_pattern
                            {
                                return;
                            }
                            let newer = github::is_newer_release(
                                g.installed_release_id,
                                g.installed_tag.as_deref(),
                                a.version.as_deref(),
                                &rel,
                            );
                            g.last_checked_at = Some(ts);
                            g.etag = etag.clone().or(g.etag.clone());
                            g.last_check_error = None;
                            if newer {
                                g.available_release = Some(rel.clone());
                                is_newer_live = true;
                            }
                        }
                    });
                    self.save_state();
                    self.emit_apps();
                    if is_newer_live {
                        release = Some(rel);
                    }
                }
                Err(fl) => {
                    warn!("Refresh-time GitHub check failed for {}: {}", rec.name, fl.message);
                    self.mutate_app(&rec.id, |a| {
                        if let Some(g) = a.github.as_mut() {
                            if g.repo.eq_ignore_ascii_case(&gh.repo) {
                                g.last_checked_at = Some(ts);
                                g.last_check_error = Some(Failure { detail: None, ..fl.clone() });
                            }
                        }
                    });
                    self.save_state();
                    self.emit_apps();
                }
            }
        }

        let release = release?;
        let res = async {
            self.verify_repo_ownership(&rec.id, &gh).await?;
            let tmp = self.download_and_validate_release(job_id, rec, &release).await?;
            let staged = tokio::task::spawn_blocking({
                let p = tmp.path().to_path_buf();
                let rid = rec.id.clone();
                move || StagedLibraryFile::stage(&p, &rid)
            })
            .await
            .map_err(|e| f("internal", &e.to_string()))?
            .map_err(|e| f("io", &format!("Couldn't stage updated IPA in library: {e:#}")))?;
            Ok((tmp, staged))
        }
        .await;
        Some((release, res))
    }

    /// Download a release `.ipa` into `work_dir` with job progress (0.02..0.28) and
    /// validate its IPA structure, original bundle ID, and encryption BEFORE signing
    /// (so we never waste free App IDs or allow a swapped app to hide behind a custom bundle ID).
    async fn download_and_validate_release(
        self: &Arc<Self>,
        job_id: &str,
        rec: &AppRecord,
        release: &ReleaseInfo,
    ) -> Result<TempDownload, Failure> {
        if !ipa::valid_file_name(&release.asset.name) {
            return Err(f("invalid_asset", "GitHub release asset has an unsafe filename."));
        }
        let owned_dir = paths::work_dir().join(format!("gh-{}", pipeline::new_id()));
        std::fs::create_dir_all(&owned_dir)
            .map_err(|e| f("io", &format!("Couldn't prepare download directory: {e}")))?;
        let dest = owned_dir.join(&release.asset.name);
        let tmp = TempDownload { file: dest.clone(), owned_dir };

        let token = self.read_github_token().await;
        let prog = self.scaled_progress_for(job_id, 0.02, 0.27);
        let stage = format!("Downloading {}", release.tag);
        prog(&stage, 0.0);
        let stage_cb = stage.clone();
        github::download_asset(&release.asset, &dest, token.as_deref(), move |done, total| {
            let frac = if total > 0 { (done as f32 / total as f32).clamp(0.0, 1.0) } else { 0.5 };
            prog(&stage_cb, frac);
        })
        .await?;

        self.progress_for(job_id)("Verifying downloaded IPA", 0.27);
        let check_path = dest.clone();
        let info = tokio::task::spawn_blocking(move || ipa::inspect(&check_path))
            .await
            .map_err(|e| f("internal", &e.to_string()))?
            .map_err(|e| errors::explain(&e))?;

        if !rec.original_bundle_id.is_empty() && info.bundle_id != rec.original_bundle_id {
            return Err(f(
                "bundle_id_mismatch",
                &format!(
                    "The downloaded release has bundle ID {} instead of {}.",
                    info.bundle_id, rec.original_bundle_id
                ),
            )
            .with_hint(
                "Loadside refused to overwrite this app with a different bundle identifier. Check that the right repository and .ipa filter are connected.",
            ));
        }

        if info.encrypted == Some(true) && !rec.options.allow_encrypted {
            return Err(f("encrypted_ipa", "The downloaded release IPA is FairPlay-encrypted.")
                .with_hint("Encrypted apps crash on launch when sideloaded."));
        }

        Ok(tmp)
    }

    /// Execute an in-place GitHub release update job for `app_id`.
    pub(crate) async fn run_update(self: &Arc<Self>, job_id: &str, app_id: &str, background: bool) -> Result<(), Failure> {
        let rec = self
            .state
            .lock()
            .unwrap()
            .app(app_id)
            .cloned()
            .ok_or_else(|| f("not_found", "Unknown app."))?;
        let gh = rec
            .github
            .clone()
            .ok_or_else(|| f("no_github", "This app isn't connected to a GitHub repository."))?;

        // Revalidate policy when a queued background update starts (the user may have
        // switched the app to On Refresh or Manual while the job sat in the queue).
        if background && gh.policy != UpdatePolicy::Immediate {
            info!("Skipping queued background update for {}: policy is no longer Immediate", rec.name);
            return Ok(());
        }

        let now = loadside_core::now();
        self.mutate_app(app_id, |a| {
            if let Some(g) = a.github.as_mut() {
                g.last_update_attempt_at = Some(now);
            }
        });

        let result: Result<(pipeline::Outcome, ReleaseInfo, StagedLibraryFile), Failure> = async {
            if !rec.team_id.is_empty() || !rec.email.is_empty() {
                let s = self.session(false, !background).await?;
                let email_ok = rec.email.is_empty() || rec.email.eq_ignore_ascii_case(&s.email);
                let team_ok = rec.team_id.is_empty() || rec.team_id == s.team.team_id;
                if !email_ok || !team_ok {
                    return Err(f("team_mismatch", &format!("{} was installed with a different Apple ID or team.", rec.name))
                        .with_hint(format!(
                            "Switch back to {} (team {}) in Account to update it, or remove it and install it again with the current account.",
                            if rec.email.is_empty() { "that account" } else { &rec.email },
                            if rec.team_id.is_empty() { "?" } else { &rec.team_id }
                        )));
                }
            }
            if !self.ensure_device_connected(&rec.device_udid).await {
                return Err(f(
                    "device_not_found",
                    &format!("{} isn't connected.", if rec.device_name.is_empty() { "The device" } else { &rec.device_name }),
                )
                .with_hint(crate::checklist::NOT_CONNECTED_HINT));
            }

            self.progress_for(job_id)("Checking device", 0.01);
            let present = tokio::time::timeout(Duration::from_secs(60), async {
                match self.connect(&rec.device_udid).await {
                    Ok(mut c) => c.installed(vec![rec.bundle_id.clone()]).await.ok().map(|set| set.contains(&rec.bundle_id)),
                    Err(_) => None,
                }
            })
            .await
            .ok()
            .flatten();
            match present {
                Some(false) => {
                    self.mutate_app(app_id, |a| a.on_device = Some(false));
                    self.emit_apps();
                    return Err(f("not_on_device", "The app was deleted from the device, so it wasn't updated.")
                        .with_hint("Use Reinstall to put it back, or Remove to forget it."));
                }
                None => {
                    return Err(f("device_not_found", "Couldn't check the device, so the update was skipped.")
                        .with_hint(crate::checklist::NOT_CONNECTED_HINT));
                }
                _ => {}
            }

            self.verify_repo_ownership(app_id, &gh).await?;

            // Pick the release to install: cached available_release, or fetch fresh.
            let mut release = match gh.available_release.clone() {
                Some(r) => r,
                None => self.fetch_fresh_newer_release(job_id, &rec, &gh).await?,
            };

            self.update_job(job_id, |j| j.title = format!("Update {} to {}", rec.name, release.tag));
            // If a cached release asset was deleted/replaced on GitHub (404), refresh once.
            let tmp = match self.download_and_validate_release(job_id, &rec, &release).await {
                Ok(t) => t,
                Err(fl) if fl.code == "github_not_found" && gh.available_release.is_some() => {
                    warn!("Cached release {} vanished on GitHub (404); re-fetching latest", release.tag);
                    release = self.fetch_fresh_newer_release(job_id, &rec, &gh).await?;
                    self.update_job(job_id, |j| j.title = format!("Update {} to {}", rec.name, release.tag));
                    self.download_and_validate_release(job_id, &rec, &release).await?
                }
                Err(fl) => return Err(fl),
            };

            // Pre-stage into library_dir before installing on device so committing after
            // install is an atomic same-filesystem rename (can't fail due to disk space).
            let staged_lib = tokio::task::spawn_blocking({
                let p = tmp.path().to_path_buf();
                let rid = app_id.to_string();
                move || StagedLibraryFile::stage(&p, &rid)
            })
            .await
            .map_err(|e| f("internal", &e.to_string()))?
            .map_err(|e| f("io", &format!("Couldn't stage updated IPA in library: {e:#}")))?;

            let target = self.target(&rec.device_udid);
            let source_path = tmp.path().to_path_buf();
            let outcome = self
                .sign_with_retry_progress(
                    || pipeline::Request {
                        source: &source_path,
                        options: &rec.options,
                        device: Some(&target),
                        export_to: None,
                        install: true,
                        expect_bundle_id: Some(&rec.bundle_id),
                    },
                    self.scaled_progress_for(job_id, 0.28, 1.0),
                    background,
                )
                .await?;
            Ok((outcome, release, staged_lib))
        }
        .await;

        match result {
            Ok((outcome, release, staged_lib)) => {
                let now = loadside_core::now();
                let new_source_path = tokio::task::spawn_blocking(move || staged_lib.commit())
                    .await
                    .ok()
                    .and_then(|r| match r {
                        Ok(p) => Some(p),
                        Err(e) => {
                            warn!("Couldn't finalize library IPA after update: {e:#}");
                            None
                        }
                    });
                let mut lib_warning: Option<String> = None;
                if new_source_path.is_none() {
                    lib_warning = Some("Updated on the device, but couldn't update Loadside's local library copy.".into());
                }
                self.mutate_app(app_id, |a| {
                    a.name = outcome.name.clone();
                    a.version = outcome.version.clone().or(a.version.clone());
                    a.original_bundle_id = outcome.original_bundle_id.clone();
                    a.bundle_id = outcome.bundle_id.clone();
                    if let Some(p) = &new_source_path {
                        a.source_path = p.clone();
                    }
                    a.last_refreshed_at = Some(now);
                    a.expires_at = outcome.expires_at.or(a.expires_at);
                    a.last_error = None;
                    a.consecutive_failures = 0;
                    a.on_device = Some(true);
                    // Only advance GitHub tracking metadata if the app still tracks the same repo
                    // AND the library copy succeeded (so refreshes won't silently downgrade!).
                    if new_source_path.is_some() {
                        if let Some(g) = a.github.as_mut() {
                            if g.repo.eq_ignore_ascii_case(&gh.repo) {
                                g.installed_release_id = Some(release.release_id);
                                g.installed_tag = Some(release.tag.clone());
                                g.installed_asset_name = Some(release.asset.name.clone());
                                if g.available_release.as_ref().map(|r| r.release_id) == Some(release.release_id) {
                                    g.available_release = None;
                                }
                                g.consecutive_update_failures = 0;
                                g.last_check_error = None;
                            }
                        }
                    }
                });
                self.save_state();
                self.emit_apps();
                let mut ws = outcome.warnings.clone();
                if let Some(w) = lib_warning {
                    ws.push(w);
                }
                self.update_job(job_id, |j| j.warnings.extend(ws));
                if background {
                    let until = outcome.expires_at.map(|e| format!(" Valid until {}.", fmt_date(e))).unwrap_or_default();
                    self.notify(
                        "Updated",
                        &format!("{} {} is on {}.{until}", outcome.name, release.tag, rec.device_name),
                    );
                }
                Ok(())
            }
            Err(fl) => {
                let transient = matches!(
                    fl.code.as_str(),
                    "not_on_device" | "device_not_found" | "device_locked" | "timeout" | "needs_2fa" | "needs_login" | "network"
                );
                if transient && fl.code != "not_on_device" {
                    let tx = self.check_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(305)).await;
                        let _ = tx.send(());
                    });
                }
                self.mutate_app(app_id, |a| {
                    a.last_error = Some(Failure { detail: None, ..fl.clone() });
                    if !transient {
                        if let Some(g) = a.github.as_mut() {
                            g.consecutive_update_failures += 1;
                        }
                    }
                });
                self.save_state();
                self.emit_apps();
                let repeat = rec.last_error.as_ref().is_some_and(|e| e.code == fl.code);
                if background
                    && !repeat
                    && !matches!(fl.code.as_str(), "not_on_device" | "device_not_found" | "needs_2fa" | "up_to_date")
                {
                    self.notify(
                        &format!("Couldn't update {}", rec.name),
                        &fl.hint.clone().unwrap_or(fl.message.clone()),
                    );
                }
                Err(fl)
            }
        }
    }

    async fn fetch_fresh_newer_release(
        self: &Arc<Self>,
        job_id: &str,
        rec: &AppRecord,
        gh: &GitHubSource,
    ) -> Result<ReleaseInfo, Failure> {
        self.progress_for(job_id)("Checking GitHub", 0.015);
        let token = self.read_github_token().await;
        match github::fetch_latest_release(
            &gh.repo,
            gh.include_prereleases,
            gh.asset_pattern.as_deref(),
            None,
            token.as_deref(),
        )
        .await?
        {
            CheckOutcome::Found { release, etag, .. } => {
                let newer = github::is_newer_release(
                    gh.installed_release_id,
                    gh.installed_tag.as_deref(),
                    rec.version.as_deref(),
                    &release,
                );
                self.mutate_app(&rec.id, |a| {
                    if let Some(g) = a.github.as_mut() {
                        if g.repo.eq_ignore_ascii_case(&gh.repo) {
                            g.last_checked_at = Some(loadside_core::now());
                            g.etag = etag.clone().or(g.etag.clone());
                            g.last_check_error = None;
                            if newer {
                                g.available_release = Some(release.clone());
                            } else {
                                g.available_release = None;
                            }
                        }
                    }
                });
                self.save_state();
                self.emit_apps();
                if !newer {
                    return Err(f("up_to_date", &format!("{} is already on the latest release ({}).", rec.name, release.tag)));
                }
                Ok(release)
            }
            CheckOutcome::NotModified => {
                Err(f("up_to_date", &format!("{} is already up to date.", rec.name)))
            }
        }
    }
}
