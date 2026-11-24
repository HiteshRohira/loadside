//! The daemon's brain: state, Apple session, job queue, device tracking and
//! the auto-refresh scheduler.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use loadside_core::{
    account::{self, CertSummary, Session, TwoFactorCallbackParams, TwoFactorCallbackResponse},
    config::Config,
    device::{self, DeviceInfo},
    errors, ipa, paths,
    pipeline::{self, CertChooser},
    protocol::{Event, Failure, InstallParams, JobInfo, JobKind, JobStatus, Snapshot},
    secrets,
    state::{AppRecord, State},
};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};

use crate::logging::LogBuffer;
use crate::notify;
use crate::prompts::Prompter;

const MAX_JOBS_KEPT: usize = 50;
const PROMPT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone)]
pub enum JobSpec {
    Install(InstallParams),
    /// `reinstall`: the user asked to put a deleted app back.
    Refresh { app_id: String, reinstall: bool },
}

#[derive(Debug)]
pub struct QueuedJob {
    pub id: String,
    pub spec: JobSpec,
    pub background: bool,
}

pub struct Engine {
    pub started_at: i64,
    pub config: Mutex<Config>,
    pub state: Mutex<State>,
    session: tokio::sync::Mutex<Option<Session>>,
    pub events: broadcast::Sender<Event>,
    pub prompts: Prompter,
    jobs: Mutex<Vec<JobInfo>>,
    queue: mpsc::UnboundedSender<QueuedJob>,
    devices: Mutex<BTreeMap<String, DeviceInfo>>,
    /// usbmuxd device id → (udid, connection)
    mux: Mutex<HashMap<u32, (String, String)>>,
    pub logs: Arc<LogBuffer>,
    check_tx: mpsc::UnboundedSender<()>,
    next_check_at: AtomicI64,
    /// Serializes state.json writes so an older snapshot never wins.
    save_lock: Mutex<()>,
    /// Mirrors whether `session` holds a session (readable while it's locked).
    logged_in: AtomicBool,
    /// A background sign-in needed 2FA (or was rejected). Don't keep trying
    /// unattended (each try can send a code to the user's devices) until the
    /// user signs in from the app.
    needs_interactive_login: AtomicBool,
}

fn f(code: &str, msg: &str) -> Failure {
    Failure::new(code, msg)
}

fn short_id() -> String {
    uuid_v4()[..8].to_string()
}

fn uuid_v4() -> String {
    loadside_core::pipeline::new_id()
}

fn fmt_date(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%a %b %-d, %H:%M").to_string())
        .unwrap_or_default()
}

impl Engine {
    #[allow(clippy::type_complexity)]
    pub fn new(logs: Arc<LogBuffer>) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<QueuedJob>, mpsc::UnboundedReceiver<()>)> {
        let (events, _) = broadcast::channel(512);
        logs.set_sink(events.clone());
        let (queue, qrx) = mpsc::unbounded_channel();
        let (check_tx, crx) = mpsc::unbounded_channel();
        let engine = Arc::new(Self {
            started_at: loadside_core::now(),
            config: Mutex::new(Config::load().sanitized()),
            state: Mutex::new(State::load()?),
            session: tokio::sync::Mutex::new(None),
            prompts: Prompter::new(events.clone()),
            events,
            jobs: Mutex::default(),
            queue,
            devices: Mutex::default(),
            mux: Mutex::default(),
            logs,
            check_tx,
            next_check_at: AtomicI64::new(0),
            save_lock: Mutex::new(()),
            logged_in: AtomicBool::new(false),
            needs_interactive_login: AtomicBool::new(false),
        });
        Ok((engine, qrx, crx))
    }

    pub fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    pub(crate) fn config(&self) -> Config {
        self.config.lock().unwrap().clone()
    }

    /// Write state.json. Snapshot and write happen under one lock, so the
    /// last writer always writes the newest state.
    pub(crate) fn persist(&self) -> Result<(), Failure> {
        let _g = self.save_lock.lock().unwrap();
        let st = self.state.lock().unwrap().clone();
        st.save().map_err(|e| {
            error!("failed to save state: {e:#}");
            f("save_failed", "Couldn't save Loadside's app list.")
                .with_hint(format!("Check free disk space and permissions of {}. ({e})", paths::data_dir().display()))
        })
    }

    /// Like `persist`, for paths that can't return an error: tell clients.
    pub(crate) fn save_state(&self) {
        if let Err(fl) = self.persist() {
            self.emit(Event::Notice { level: "error".into(), title: fl.message, message: fl.hint.unwrap_or_default() });
        }
    }

    pub(crate) fn emit_apps(&self) {
        let apps = self.state.lock().unwrap().apps.clone();
        self.emit(Event::Apps { apps });
    }

    fn emit_account(&self, logged_in: bool) {
        let account = self.state.lock().unwrap().account.clone();
        self.emit(Event::Account { account, logged_in });
    }

    fn notify(&self, title: &str, msg: &str) {
        if self.config().notifications {
            notify::send(title, msg);
        }
    }

    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.devices.lock().unwrap().values().cloned().collect()
    }

    pub(crate) fn device_connected(&self, udid: &str) -> bool {
        self.devices.lock().unwrap().contains_key(udid)
    }

    pub(crate) fn has_session(&self) -> bool {
        self.logged_in.load(Ordering::SeqCst)
    }

    pub async fn snapshot(&self) -> Snapshot {
        let logged_in = self.has_session();
        let st = self.state.lock().unwrap().clone();
        let next = self.next_check_at.load(Ordering::Relaxed);
        Snapshot {
            version: loadside_core::VERSION.into(),
            pid: std::process::id(),
            started_at: self.started_at,
            account: st.account,
            logged_in,
            devices: self.devices(),
            apps: st.apps,
            jobs: self.jobs.lock().unwrap().clone(),
            prompts: self.prompts.open(),
            config: self.config(),
            next_check_at: (next > 0).then_some(next),
        }
    }

    // ------------------------------------------------------------------
    // Session
    // ------------------------------------------------------------------

    fn two_factor_callback(
        self: &Arc<Self>,
        needed_without_client: Arc<std::sync::atomic::AtomicBool>,
    ) -> impl Fn(TwoFactorCallbackParams) -> futures::future::BoxFuture<'static, Result<TwoFactorCallbackResponse, loadside_core::rootcause::Report>>
    + Send
    + Sync
    + 'static {
        let engine = self.clone();
        move |params: TwoFactorCallbackParams| {
            let engine = engine.clone();
            let flag = needed_without_client.clone();
            Box::pin(async move {
                if !engine.prompts.has_interactive_client() {
                    flag.store(true, Ordering::SeqCst);
                    return Ok(TwoFactorCallbackResponse::Abort);
                }
                let payload = serde_json::to_value(&params).unwrap_or(Value::Null);
                match engine.prompts.ask("two_factor", payload, PROMPT_TIMEOUT).await {
                    Ok(v) => Ok(serde_json::from_value::<TwoFactorCallbackResponse>(v)
                        .unwrap_or(TwoFactorCallbackResponse::Abort)),
                    Err(_) => Ok(TwoFactorCallbackResponse::Abort),
                }
            })
        }
    }

    /// Sign in (explicitly, from a client).
    pub async fn login(self: &Arc<Self>, email: &str, password: &str, remember: bool) -> Result<Value, Failure> {
        crate::checklist::forget_password_cache();
        let email = email.trim().to_string();
        if email.is_empty() || password.is_empty() {
            return Err(f("bad_request", "Enter your Apple ID email and password."));
        }
        account::validate_email(&email).map_err(|e| errors::explain(&e))?;
        let mut guard = self.session.lock().await;
        let preferred = self.state.lock().unwrap().account.as_ref().filter(|a| a.email == email).and_then(|a| a.team_id.clone());
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        info!("Signing in to Apple ID");
        let session = account::login(&self.config(), &email, password, preferred.as_deref(), self.two_factor_callback(flag.clone()))
            .await
            .map_err(|e| {
                let fl = errors::explain(&e);
                warn!("Sign-in failed: {} ({})", fl.message, fl.detail.clone().unwrap_or_default());
                if flag.load(Ordering::SeqCst) {
                    return f("needs_2fa", "Apple asked for a two-factor code, but no Loadside window was open to enter it.")
                        .with_hint("Sign in from the Loadside app, or keep it open while running `loadside login`.");
                }
                fl
            })?;
        // Keychain calls can block on a system dialog; keep them off the runtime.
        let previous = self.state.lock().unwrap().account.as_ref().map(|a| a.email.clone()).filter(|e| *e != email);
        let (em, pw) = (email.clone(), password.to_string());
        let kc = tokio::task::spawn_blocking(move || -> Result<(), String> {
            if let Some(old) = previous {
                // Switching Apple IDs: don't leave the old password behind.
                let _ = secrets::delete_password(&old);
            }
            if remember { secrets::save_password(&em, &pw).map_err(|e| format!("{e:#}")) } else { secrets::delete_password(&em).map(|_| ()).map_err(|e| format!("{e:#}")) }
        })
        .await;
        if let Ok(Err(e)) | Err(e) = kc.map_err(|e| e.to_string()) {
            warn!("Keychain update failed: {e}");
            self.emit(Event::Notice {
                level: "error".into(),
                title: "Couldn't update the saved password".into(),
                message: format!("Automatic refresh may not be able to sign in. ({e})"),
            });
        }
        self.needs_interactive_login.store(false, Ordering::SeqCst);
        let info = session.account_info();
        let teams: Vec<_> = session.team_summaries();
        {
            let mut st = self.state.lock().unwrap();
            st.account = Some(info.clone());
        }
        self.save_state();
        *guard = Some(session);
        self.logged_in.store(true, Ordering::SeqCst);
        drop(guard);
        info!("Signed in");
        self.emit_account(true);
        self.trigger_check();
        Ok(json!({ "account": info, "teams": teams }))
    }

    pub async fn logout(&self) -> Result<Value, Failure> {
        crate::checklist::forget_password_cache();
        *self.session.lock().await = None;
        self.logged_in.store(false, Ordering::SeqCst);
        let email = self.state.lock().unwrap().account.take().map(|a| a.email);
        self.persist()?;
        self.emit_account(false);
        if let Some(e) = email {
            let r = tokio::task::spawn_blocking(move || secrets::delete_password(&e)).await;
            if let Ok(Err(e)) = r {
                return Err(f("keychain", "Signed out, but the saved password couldn't be removed from the Keychain.")
                    .with_hint(format!("Remove \"dev.loadside.appleid\" in Keychain Access. ({e:#})")));
            }
        }
        Ok(Value::Null)
    }

    /// Get a usable session, signing in again with the saved password if
    /// needed. `interactive` = a human is waiting on this.
    async fn session(self: &Arc<Self>, force_relogin: bool) -> Result<Session, Failure> {
        let mut guard = self.session.lock().await;
        if let Some(s) = guard.as_ref() {
            if !force_relogin && !s.is_stale() {
                return Ok(s.clone());
            }
        }
        let account = self.state.lock().unwrap().account.clone().ok_or_else(|| {
            f("needs_login", "You're not signed in.").with_hint("Sign in with your Apple ID in Account.")
        })?;
        if self.needs_interactive_login.load(Ordering::SeqCst) && !self.prompts.has_interactive_client() {
            return Err(f("needs_2fa", "Sign in again from the Loadside app.")
                .with_hint("Apple asked for verification during an automatic sign-in. Loadside won't retry on its own (each try can send a code to your devices)."));
        }
        let em = account.email.clone();
        let password = tokio::task::spawn_blocking(move || secrets::password(&em))
            .await
            .map_err(|e| f("internal", &e.to_string()))?
            .map_err(|e| {
                f("keychain", "Couldn't read the saved password from the Keychain.").with_hint(format!(
                    "If macOS asked whether loadsided may use it, choose Always Allow. ({e:#})"
                ))
            })?
            .ok_or_else(|| {
                f("needs_login", "Your Apple ID session expired and no saved password was found.")
                    .with_hint("Sign in again in Account (tick \"Remember password\" to enable unattended refresh).")
            })?;
        info!("Session missing or stale; signing in again");
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let res = account::login(&self.config(), &account.email, &password, account.team_id.as_deref(), self.two_factor_callback(flag.clone())).await;
        match res {
            Ok(s) => {
                *guard = Some(s.clone());
                self.logged_in.store(true, Ordering::SeqCst);
                drop(guard);
                self.needs_interactive_login.store(false, Ordering::SeqCst);
                // Keep the saved account in step with what Apple reports now
                // (name, team, free/paid).
                let info = s.account_info();
                let changed = {
                    let mut st = self.state.lock().unwrap();
                    let changed = st.account.as_ref() != Some(&info);
                    st.account = Some(info);
                    changed
                };
                if changed {
                    self.save_state();
                }
                self.emit_account(true);
                Ok(s)
            }
            Err(e) => {
                *guard = None;
                self.logged_in.store(false, Ordering::SeqCst);
                drop(guard);
                self.emit_account(false);
                let fl = errors::explain(&e);
                let stop = flag.load(Ordering::SeqCst)
                    || matches!(fl.code.as_str(), "bad_credentials" | "account_locked" | "two_factor_blocked" | "login_cancelled");
                if stop {
                    self.needs_interactive_login.store(true, Ordering::SeqCst);
                    self.notify("Sign in to Loadside again", "Apple needs you to sign in again before apps can be refreshed. Open Loadside.");
                }
                if flag.load(Ordering::SeqCst) {
                    return Err(f("needs_2fa", "Apple is asking for a two-factor code.")
                        .with_hint("Open Loadside and sign in again to enter the code."));
                }
                Err(fl)
            }
        }
    }

    async fn invalidate_session(&self) {
        *self.session.lock().await = None;
        self.logged_in.store(false, Ordering::SeqCst);
    }

    /// Keep token/anisette updates a pipeline run made to its session copy.
    async fn store_session_dev(&self, s: &Session) {
        let mut g = self.session.lock().await;
        if let Some(cur) = g.as_mut() {
            if cur.email == s.email && cur.team.team_id == s.team.team_id {
                cur.dev = s.dev.clone();
            }
        }
    }

    pub async fn teams(self: &Arc<Self>) -> Result<Value, Failure> {
        let s = self.session(false).await?;
        Ok(json!({
            "selected": s.team.team_id,
            "teams": s.team_summaries(),
        }))
    }

    pub async fn select_team(self: &Arc<Self>, team_id: &str) -> Result<Value, Failure> {
        self.session(false).await?;
        let mut guard = self.session.lock().await;
        let s = guard.as_mut().ok_or_else(|| f("needs_login", "You're not signed in."))?;
        s.select_team(team_id).map_err(|e| errors::explain(&e))?;
        let info = s.account_info();
        drop(guard);
        self.state.lock().unwrap().account = Some(info.clone());
        self.save_state();
        self.emit_account(true);
        Ok(json!(info))
    }

    /// Run an account operation, re-authenticating once if the session expired.
    async fn with_session<T, F, Fut>(self: &Arc<Self>, op: F) -> Result<T, Failure>
    where
        F: Fn(Session) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let s = self.session(false).await?;
        match op(s).await {
            Ok(v) => Ok(v),
            Err(e) => {
                let fl = errors::explain(&e);
                if fl.code == "session_expired" {
                    let s = self.session(true).await?;
                    op(s).await.map_err(|e| errors::explain(&e))
                } else {
                    Err(fl)
                }
            }
        }
    }

    pub async fn app_ids(self: &Arc<Self>) -> Result<Value, Failure> {
        self.with_session(|mut s| async move { account::list_app_ids(&mut s).await }).await.map(|v| json!(v))
    }

    pub async fn delete_app_id(self: &Arc<Self>, id: &str) -> Result<Value, Failure> {
        let id = id.to_string();
        self.with_session(|mut s| {
            let id = id.clone();
            async move { account::delete_app_id(&mut s, &id).await }
        })
        .await
        .map_err(|fl| {
            if fl.code == "unknown" {
                fl.with_hint("Free accounts can't delete App IDs; they expire on their own 7 days after creation.")
            } else {
                fl
            }
        })?;
        Ok(Value::Null)
    }

    pub async fn certs(self: &Arc<Self>) -> Result<Value, Failure> {
        let machine = self.config().machine_name;
        self.with_session(|mut s| {
            let m = machine.clone();
            async move { account::list_certs(&mut s, &m).await }
        })
        .await
        .map(|v| json!(v))
    }

    pub async fn revoke_cert(self: &Arc<Self>, serial: &str) -> Result<Value, Failure> {
        let serial = serial.to_string();
        self.with_session(|mut s| {
            let serial = serial.clone();
            async move { account::revoke_cert(&mut s, &serial).await }
        })
        .await?;
        Ok(Value::Null)
    }

    // ------------------------------------------------------------------
    // Jobs
    // ------------------------------------------------------------------

    fn update_job(&self, id: &str, upd: impl FnOnce(&mut JobInfo)) {
        let job = {
            let mut jobs = self.jobs.lock().unwrap();
            let Some(j) = jobs.iter_mut().find(|j| j.id == id) else { return };
            upd(j);
            j.clone()
        };
        self.emit(Event::Job { job });
    }

    pub(crate) fn job_active_for_app(&self, app_id: &str) -> bool {
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .any(|j| j.app_id.as_deref() == Some(app_id) && matches!(j.status, JobStatus::Queued | JobStatus::Running))
    }

    pub fn enqueue(&self, spec: JobSpec, background: bool) -> Result<String, Failure> {
        let id = short_id();
        let (kind, title, app_id, udid) = match &spec {
            JobSpec::Install(p) => {
                let name = Path::new(&p.path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                let install = p.install.unwrap_or(true);
                let kind = if install { JobKind::Install } else { JobKind::Export };
                let verb = if install { "Install" } else { "Export" };
                (kind, format!("{verb} {name}"), None, p.device_udid.clone())
            }
            JobSpec::Refresh { app_id, reinstall } => {
                let st = self.state.lock().unwrap();
                let app = st.app(app_id).ok_or_else(|| f("not_found", "Unknown app."))?;
                let verb = if *reinstall { "Reinstall" } else { "Refresh" };
                (JobKind::Refresh, format!("{verb} {}", app.name), Some(app_id.clone()), Some(app.device_udid.clone()))
            }
        };
        let job = JobInfo {
            id: id.clone(),
            kind,
            title,
            app_id,
            device_udid: udid,
            status: JobStatus::Queued,
            stage: "Waiting".into(),
            progress: 0.0,
            background,
            created_at: loadside_core::now(),
            finished_at: None,
            error: None,
            output_path: None,
            warnings: vec![],
        };
        {
            // Check-and-insert under one lock so two refreshes can't both pass.
            let mut jobs = self.jobs.lock().unwrap();
            if let Some(aid) = &job.app_id {
                if jobs.iter().any(|j| j.app_id.as_deref() == Some(aid) && matches!(j.status, JobStatus::Queued | JobStatus::Running)) {
                    return Err(f("already_queued", "A job for this app is already queued."));
                }
            }
            jobs.push(job.clone());
            while jobs.len() > MAX_JOBS_KEPT {
                match jobs.iter().position(|j| j.finished_at.is_some()) {
                    Some(pos) => {
                        jobs.remove(pos);
                    }
                    None => break,
                }
            }
        }
        self.emit(Event::Job { job });
        self.queue
            .send(QueuedJob { id: id.clone(), spec, background })
            .map_err(|_| f("internal", "job queue closed"))?;
        Ok(id)
    }

    pub fn cancel_job(&self, id: &str) -> Result<Value, Failure> {
        // Status changes happen under the jobs lock, so the worker either sees
        // Cancelled or has already claimed the job (then it's not Queued here).
        let mut ok = false;
        self.update_job(id, |j| {
            if j.status == JobStatus::Queued {
                j.status = JobStatus::Cancelled;
                j.finished_at = Some(loadside_core::now());
                j.stage = "Cancelled".into();
                ok = true;
            }
        });
        if ok { Ok(Value::Null) } else { Err(f("not_cancellable", "Only queued jobs can be cancelled.")) }
    }

    pub fn clear_jobs(&self) {
        self.jobs.lock().unwrap().retain(|j| j.finished_at.is_none());
    }

    pub async fn worker(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<QueuedJob>) {
        while let Some(job) = rx.recv().await {
            // Atomically claim the job: only a Queued job (not cancelled, not
            // cleared from history) may start.
            let claimed = {
                let mut jobs = self.jobs.lock().unwrap();
                match jobs.iter_mut().find(|j| j.id == job.id) {
                    Some(j) if j.status == JobStatus::Queued => {
                        j.status = JobStatus::Running;
                        j.stage = "Starting".into();
                        Some(j.clone())
                    }
                    _ => None,
                }
            };
            let Some(claimed) = claimed else { continue };
            self.emit(Event::Job { job: claimed });
            // Run on its own task so a panic fails this job instead of
            // silently killing the queue.
            let engine = self.clone();
            let (id, spec, background) = (job.id.clone(), job.spec.clone(), job.background);
            let handle = tokio::spawn(async move {
                match &spec {
                    JobSpec::Install(p) => engine.run_install(&id, p, background).await,
                    JobSpec::Refresh { app_id, reinstall } => engine.run_refresh(&id, app_id, background, *reinstall).await,
                }
            });
            let result = handle.await.unwrap_or_else(|e| {
                error!("Job {} crashed: {e}", job.id);
                Err(f("internal", "Loadside hit an internal error while running this job.")
                    .with_hint("Please report it with the log (Activity → Logs). Other jobs keep running."))
            });
            let now = loadside_core::now();
            match result {
                Ok(()) => self.update_job(&job.id, |j| {
                    j.status = JobStatus::Succeeded;
                    j.progress = 1.0;
                    j.stage = "Done".into();
                    j.finished_at = Some(now);
                }),
                Err(fl) => {
                    warn!("Job {} failed: {} | {}", job.id, fl.message, fl.detail.clone().unwrap_or_default());
                    let mut shown = fl.clone();
                    shown.detail = shown.detail.map(|d| d.chars().take(4000).collect());
                    self.update_job(&job.id, |j| {
                        j.status = JobStatus::Failed;
                        j.stage = "Failed".into();
                        j.error = Some(shown);
                        j.finished_at = Some(now);
                    });
                }
            }
        }
    }

    fn progress_for(self: &Arc<Self>, job_id: &str) -> pipeline::Progress {
        let engine = self.clone();
        let id = job_id.to_string();
        let last = Arc::new(Mutex::new((String::new(), -1.0f32)));
        Arc::new(move |stage: &str, p: f32| {
            {
                let mut l = last.lock().unwrap();
                if l.0 == stage && (p - l.1).abs() < 0.01 {
                    return;
                }
                *l = (stage.to_string(), p);
            }
            engine.update_job(&id, |j| {
                j.stage = stage.to_string();
                j.progress = p.clamp(0.0, 1.0);
            });
        })
    }

    fn cert_chooser(self: &Arc<Self>, background: bool) -> Option<CertChooser> {
        let engine = self.clone();
        let auto_revoke = self.config().auto_revoke_own_certs;
        if background && !auto_revoke {
            return None;
        }
        Some(Arc::new(move |certs: Vec<CertSummary>| {
            let engine = engine.clone();
            Box::pin(async move {
                if background || !engine.prompts.has_interactive_client() {
                    // Unattended: revoke at most one of our own (the one that
                    // expires first), never more, never someone else's.
                    let oldest = certs
                        .iter()
                        .filter(|c| c.ours)
                        .min_by_key(|c| c.expires_at.unwrap_or(i64::MAX))
                        .map(|c| c.serial_number.clone());
                    return if auto_revoke { oldest.map(|s| vec![s]) } else { None };
                }
                let offered: Vec<String> = certs.iter().map(|c| c.serial_number.clone()).collect();
                let v = engine.prompts.ask("max_certs", json!({ "certificates": certs }), PROMPT_TIMEOUT).await.ok()?;
                let serials: Vec<String> = serde_json::from_value(v.get("revoke").cloned().unwrap_or(Value::Null)).ok()?;
                // Only certificates we actually showed may be revoked.
                if serials.is_empty() || serials.iter().any(|s| !offered.contains(s)) {
                    return None;
                }
                Some(serials)
            })
        }))
    }

    /// Run the pipeline, re-authenticating and retrying once if the session expired.
    async fn sign_with_retry<'r>(
        self: &Arc<Self>,
        req: impl Fn() -> pipeline::Request<'r>,
        job_id: &str,
        background: bool,
    ) -> Result<pipeline::Outcome, Failure> {
        let config = self.config();
        let mut force = false;
        for attempt in 0..2 {
            let mut session = self.session(force).await?;
            let res = pipeline::run(&mut session, &config, req(), self.progress_for(job_id), self.cert_chooser(background)).await;
            self.store_session_dev(&session).await;
            match res {
                Ok(o) => return Ok(o),
                Err(e) => {
                    let fl = errors::explain(&e);
                    if fl.code == "session_expired" && attempt == 0 {
                        self.invalidate_session().await;
                        force = true;
                        continue;
                    }
                    return Err(fl);
                }
            }
        }
        unreachable!()
    }

    fn pick_device(&self, wanted: Option<&str>) -> Result<String, Failure> {
        if let Some(u) = wanted.filter(|u| !u.is_empty()) {
            if !self.device_connected(u) {
                return Err(f("device_not_found", "The selected device isn't connected.")
                    .with_hint(crate::checklist::NOT_CONNECTED_HINT));
            }
            return Ok(u.to_string());
        }
        let devs = self.devices();
        match devs.as_slice() {
            [one] => Ok(one.udid.clone()),
            [] => {
                let known = self.state.lock().unwrap().known_devices.iter().max_by_key(|k| k.last_seen).map(|k| k.name.clone());
                Err(match known {
                    Some(name) => f("device_not_found", &format!("{name} isn't reachable right now."))
                        .with_hint("Unlock it and keep it on the same Wi-Fi as this Mac (an asleep iPhone drops off Wi-Fi), or plug it in. Then try again."),
                    None => f("device_not_found", "No device connected.")
                        .with_hint("Connect your iPhone/iPad with a cable, unlock it and tap \"Trust\". With Wi-Fi sync on, it also shows up over the same Wi-Fi network."),
                })
            }
            _ => Err(f("choose_device", "More than one device is connected.").with_hint("Pick which device to install to.")),
        }
    }

    fn library_copy(&self, src: &Path, record_id: &str) -> Result<PathBuf> {
        let lib = paths::library_dir();
        std::fs::create_dir_all(&lib)?;
        let dest = if src.is_dir() { lib.join(format!("{record_id}.app")) } else { lib.join(format!("{record_id}.ipa")) };
        if src == dest {
            return Ok(dest);
        }
        let tmp = dest.with_extension("partial");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_file(&tmp);
        if src.is_dir() {
            ipa::copy_dir(src, &tmp)?;
            // Swap directories without a moment where neither exists.
            let old = dest.with_extension("old");
            let _ = std::fs::remove_dir_all(&old);
            if dest.exists() {
                std::fs::rename(&dest, &old)?;
            }
            std::fs::rename(&tmp, &dest)?;
            let _ = std::fs::remove_dir_all(&old);
        } else {
            std::fs::copy(src, &tmp).context("copying IPA into the library")?;
            // rename() replaces the old file atomically.
            std::fs::rename(&tmp, &dest)?;
        }
        Ok(dest)
    }

    /// Copy injected tweaks next to the library IPA so refresh keeps working
    /// after the user deletes the originals.
    fn library_inject(&self, items: &[PathBuf], record_id: &str) -> Result<Vec<PathBuf>> {
        if items.is_empty() {
            return Ok(vec![]);
        }
        let dir = paths::library_dir().join(format!("{record_id}-inject"));
        if items.iter().all(|i| i.starts_with(&dir)) {
            return Ok(items.to_vec());
        }
        let tmp = dir.with_extension("partial");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        let mut out = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let name = item.file_name().context("bad injection path")?;
            // Prefix keeps two tweaks with the same file name apart.
            let sub = tmp.join(i.to_string());
            std::fs::create_dir_all(&sub)?;
            let to = sub.join(name);
            if item.is_dir() {
                ipa::copy_dir(item, &to)?;
            } else {
                std::fs::copy(item, &to)?;
            }
            out.push(dir.join(i.to_string()).join(name));
        }
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::rename(&tmp, &dir)?;
        Ok(out)
    }

    async fn run_install(self: &Arc<Self>, job_id: &str, p: &InstallParams, background: bool) -> Result<(), Failure> {
        let src = PathBuf::from(&p.path);
        if !src.exists() {
            return Err(f("file_not_found", "The IPA file doesn't exist anymore.").with_hint("It may have been moved or deleted."));
        }
        let install = p.install.unwrap_or(true);
        let udid = if install {
            Some(self.pick_device(p.device_udid.as_deref())?)
        } else {
            p.device_udid.clone().filter(|u| !u.is_empty() && self.device_connected(u))
        };

        // Heads-up about the free-account 3 app limit.
        if install {
            let st = self.state.lock().unwrap();
            if st.account.as_ref().and_then(|a| a.is_free) == Some(true) {
                let now = loadside_core::now();
                let active = st
                    .apps
                    .iter()
                    .filter(|a| Some(&a.device_udid) == udid.as_ref())
                    .filter(|a| a.on_device != Some(false) && a.expires_at.is_none_or(|e| e > now))
                    .count();
                if active >= 3 {
                    let w = "This device already has 3 active apps from Loadside; free accounts allow 3 at a time.".to_string();
                    self.update_job(job_id, |j| j.warnings.push(w));
                }
            }
        }

        let export = p.export_path.as_ref().map(PathBuf::from);
        let target = udid.as_deref().map(|u| self.target(u));
        let outcome = self
            .sign_with_retry(
                || pipeline::Request {
                    source: &src,
                    options: &p.options,
                    device: target.as_ref(),
                    export_to: export.as_deref(),
                    install,
                    expect_bundle_id: None,
                },
                job_id,
                background,
            )
            .await?;
        let warnings = outcome.warnings.clone();
        let out = outcome.exported.as_ref().map(|p| p.display().to_string());
        self.update_job(job_id, |j| {
            j.warnings.extend(warnings);
            j.output_path = out;
            j.app_id = None;
        });

        if !install {
            if let Some(path) = &outcome.exported {
                self.notify("Signed IPA ready", &format!("{} → {}", outcome.name, path.display()));
            }
            return Ok(());
        }

        let udid = udid.unwrap_or_default();
        let now = loadside_core::now();
        let existing_id = {
            let st = self.state.lock().unwrap();
            st.apps.iter().find(|a| a.bundle_id == outcome.bundle_id && a.device_udid == udid).map(|a| a.id.clone())
        };
        let record_id = existing_id.clone().unwrap_or_else(short_id);
        let mut options = p.options.clone();
        let source_path = if self.config().keep_library_copy {
            let (eng, s2, rid, inj) = (self.clone(), src.clone(), record_id.clone(), options.inject.clone());
            let copied = tokio::task::spawn_blocking(move || -> Result<(PathBuf, Vec<PathBuf>)> {
                Ok((eng.library_copy(&s2, &rid)?, eng.library_inject(&inj, &rid)?))
            })
            .await;
            match copied {
                Ok(Ok((path, inject))) => {
                    options.inject = inject;
                    path
                }
                Ok(Err(e)) => {
                    warn!("Couldn't keep a library copy ({e:#}); refreshes will use the original files");
                    src.clone()
                }
                Err(e) => {
                    warn!("Library copy task failed: {e}");
                    src.clone()
                }
            }
        } else {
            src.clone()
        };
        {
            let mut st = self.state.lock().unwrap();
            let rec = AppRecord {
                id: record_id.clone(),
                name: outcome.name.clone(),
                version: outcome.version.clone(),
                original_bundle_id: outcome.original_bundle_id.clone(),
                bundle_id: outcome.bundle_id.clone(),
                source_path,
                options,
                device_udid: udid.clone(),
                device_name: outcome.device_name.clone().unwrap_or_default(),
                // The identity that actually signed it, not whatever is selected now.
                team_id: outcome.team_id.clone(),
                email: outcome.email.clone(),
                installed_at: now,
                last_refreshed_at: Some(now),
                expires_at: outcome.expires_at,
                last_attempt_at: Some(now),
                last_error: None,
                consecutive_failures: 0,
                on_device: Some(true),
            };
            match st.app_mut(&record_id) {
                Some(r) => *r = AppRecord { installed_at: r.installed_at, ..rec },
                None => st.apps.push(rec),
            }
        }
        self.persist().map_err(|fl| fl.with_hint("The app was installed, but Loadside couldn't record it, so it won't be refreshed automatically. Free some disk space and install it again."))?;
        self.emit_apps();
        self.update_job(job_id, |j| j.app_id = Some(record_id.clone()));
        if background || !self.prompts.has_interactive_client() {
            let until = outcome.expires_at.map(|e| format!(" Valid until {}.", fmt_date(e))).unwrap_or_default();
            self.notify("Installed", &format!("{} is on {}.{until}", outcome.name, outcome.device_name.unwrap_or_default()));
        }
        Ok(())
    }

    async fn run_refresh(self: &Arc<Self>, job_id: &str, app_id: &str, background: bool, reinstall: bool) -> Result<(), Failure> {
        let rec = self.state.lock().unwrap().app(app_id).cloned().ok_or_else(|| f("not_found", "Unknown app."))?;
        let now = loadside_core::now();
        self.mutate_app(app_id, |a| a.last_attempt_at = Some(now));

        let result = async {
            // Refreshing under another Apple ID / team would sign a different
            // bundle ID and install a second copy.
            if !rec.team_id.is_empty() || !rec.email.is_empty() {
                let s = self.session(false).await?;
                let email_ok = rec.email.is_empty() || rec.email.eq_ignore_ascii_case(&s.email);
                let team_ok = rec.team_id.is_empty() || rec.team_id == s.team.team_id;
                if !email_ok || !team_ok {
                    return Err(f("team_mismatch", &format!("{} was installed with a different Apple ID or team.", rec.name))
                        .with_hint(format!(
                            "Switch back to {} (team {}) in Account to refresh it, or remove it and install it again with the current account.",
                            if rec.email.is_empty() { "that account" } else { &rec.email },
                            if rec.team_id.is_empty() { "?" } else { &rec.team_id }
                        )));
                }
            }
            if !rec.source_path.exists() {
                return Err(f("source_missing", "The original IPA for this app is gone.")
                    .with_hint("Install it again from an IPA file (Install → choose file)."));
            }
            if !self.device_connected(&rec.device_udid) {
                return Err(f("device_not_found", format!("{} isn't connected.", if rec.device_name.is_empty() { "The device" } else { &rec.device_name }).as_str())
                    .with_hint(crate::checklist::NOT_CONNECTED_HINT));
            }
            self.progress_for(job_id)("Checking device", 0.01);
            // Unattended refreshes must not resurrect an app the user deleted;
            // a click on Refresh/Reinstall should.
            if !reinstall {
                if let Ok(mut c) = self.connect(&rec.device_udid).await {
                    if let Ok(set) = c.installed(vec![rec.bundle_id.clone()]).await {
                        if !set.contains(&rec.bundle_id) {
                            self.mutate_app(app_id, |a| a.on_device = Some(false));
                            self.emit_apps();
                            if background {
                                return Err(f("not_on_device", "The app was deleted from the device, so it wasn't refreshed.")
                                    .with_hint("Use Reinstall to put it back, or Remove to forget it."));
                            }
                        }
                    }
                }
            }
            let target = self.target(&rec.device_udid);
            self.sign_with_retry(
                || pipeline::Request {
                    source: &rec.source_path,
                    options: &rec.options,
                    device: Some(&target),
                    export_to: None,
                    install: true,
                    expect_bundle_id: Some(&rec.bundle_id),
                },
                job_id,
                background,
            )
            .await
        }
        .await;

        match result {
            Ok(outcome) => {
                let now = loadside_core::now();
                self.mutate_app(app_id, |a| {
                    a.last_refreshed_at = Some(now);
                    a.expires_at = outcome.expires_at.or(a.expires_at);
                    a.last_error = None;
                    a.consecutive_failures = 0;
                    a.on_device = Some(true);
                    a.version = outcome.version.clone().or(a.version.clone());
                    a.bundle_id = outcome.bundle_id.clone();
                });
                self.save_state();
                self.emit_apps();
                let ws = outcome.warnings.clone();
                self.update_job(job_id, |j| j.warnings.extend(ws));
                if background {
                    let until = outcome.expires_at.map(|e| format!(" Valid until {}.", fmt_date(e))).unwrap_or_default();
                    self.notify("Refreshed", &format!("{}.{until}", rec.name));
                }
                Ok(())
            }
            Err(fl) => {
                // Device-away / sign-in-needed aren't signing failures: they
                // shouldn't push the next attempt out with backoff.
                let transient = matches!(fl.code.as_str(), "not_on_device" | "device_not_found" | "needs_2fa" | "needs_login");
                self.mutate_app(app_id, |a| {
                    a.last_error = Some(Failure { detail: None, ..fl.clone() });
                    if !transient {
                        a.consecutive_failures += 1;
                    }
                });
                self.save_state();
                self.emit_apps();
                if background && !matches!(fl.code.as_str(), "not_on_device" | "device_not_found" | "needs_2fa") {
                    self.notify(&format!("Couldn't refresh {}", rec.name), &fl.hint.clone().unwrap_or(fl.message.clone()));
                }
                Err(fl)
            }
        }
    }

    fn mutate_app(&self, id: &str, m: impl FnOnce(&mut AppRecord)) {
        let mut st = self.state.lock().unwrap();
        if let Some(a) = st.app_mut(id) {
            m(a);
        }
    }

    pub fn update_app(&self, id: &str, auto_refresh: Option<bool>) -> Result<Value, Failure> {
        let mut found = false;
        self.mutate_app(id, |a| {
            found = true;
            if let Some(v) = auto_refresh {
                a.options.auto_refresh = v;
            }
        });
        if !found {
            return Err(f("not_found", "Unknown app."));
        }
        self.persist()?;
        self.emit_apps();
        Ok(Value::Null)
    }

    pub async fn remove_app(&self, id: &str, uninstall: bool) -> Result<Value, Failure> {
        let rec = self.state.lock().unwrap().app(id).cloned().ok_or_else(|| f("not_found", "Unknown app."))?;
        // A running refresh would put the app back right after we delete it.
        if self.job_active_for_app(id) {
            return Err(f("busy", "This app is being refreshed right now.")
                .with_hint("Wait for the job to finish (or cancel it in Activity), then remove it."));
        }
        if uninstall {
            let mut c = self.connect(&rec.device_udid).await?;
            if let Err(e) = c.uninstall(&rec.bundle_id).await {
                let fl = errors::explain(&e);
                // Already gone is fine.
                if !fl.detail.as_deref().unwrap_or("").contains("not installed") {
                    return Err(fl);
                }
            }
            info!("Uninstalled {}", rec.bundle_id);
        }
        self.state.lock().unwrap().apps.retain(|a| a.id != id);
        self.persist()?;
        if rec.source_path.starts_with(paths::library_dir()) {
            let _ = std::fs::remove_file(&rec.source_path);
            let _ = std::fs::remove_dir_all(&rec.source_path);
            let _ = std::fs::remove_dir_all(paths::library_dir().join(format!("{id}-inject")));
        }
        self.emit_apps();
        Ok(Value::Null)
    }

    // ------------------------------------------------------------------
    // Devices
    // ------------------------------------------------------------------

    pub async fn device_attached(self: &Arc<Self>, dev: loadside_core::idevice::usbmuxd::UsbmuxdDevice) {
        let conn = match dev.connection_type {
            loadside_core::idevice::usbmuxd::Connection::Network(_) => "wifi",
            _ => "usb",
        };
        self.mux.lock().unwrap().insert(dev.device_id, (dev.udid.clone(), conn.to_string()));
        let already_usb = self.devices.lock().unwrap().get(&dev.udid).is_some_and(|d| d.connection == "usb");
        if already_usb && conn == "wifi" {
            return;
        }
        // Give the device a moment (pairing/lockdown can lag the attach event).
        tokio::time::sleep(Duration::from_millis(800)).await;
        let info = device::describe(&dev).await;
        let paired = info.paired;
        {
            // It may have gone (or a USB connection may have appeared) while
            // we were talking to it.
            let mux = self.mux.lock().unwrap();
            if !mux.contains_key(&dev.device_id) {
                return;
            }
            let usb_present = mux.values().any(|(u, c)| *u == dev.udid && c == "usb");
            let mut devs = self.devices.lock().unwrap();
            if conn == "wifi" && usb_present && devs.get(&dev.udid).is_some_and(|d| d.connection == "usb") {
                return;
            }
            let mut info = info;
            if usb_present {
                info.connection = "usb".into();
            }
            info!("Device connected: {} ({}, {})", info.name, info.udid, info.connection);
            devs.insert(info.udid.clone(), info);
        }
        self.emit(Event::Devices { devices: self.devices() });
        self.remember_devices();
        if paired {
            self.sync_on_device(&dev.udid).await;
        }
        self.trigger_check();
    }

    pub fn device_detached(&self, mux_id: u32) {
        let Some((udid, _)) = self.mux.lock().unwrap().remove(&mux_id) else { return };
        let remaining: Option<String> = self.mux.lock().unwrap().values().find(|(u, _)| *u == udid).map(|(_, c)| c.clone());
        {
            let mut devs = self.devices.lock().unwrap();
            match remaining {
                Some(conn) => {
                    if let Some(d) = devs.get_mut(&udid) {
                        d.connection = conn;
                    }
                }
                None => {
                    if let Some(d) = devs.remove(&udid) {
                        info!("Device disconnected: {}", d.name);
                    }
                }
            }
        }
        self.emit(Event::Devices { devices: self.devices() });
    }

    pub async fn refresh_devices(self: &Arc<Self>) -> Result<Vec<DeviceInfo>, Failure> {
        // Every transport goes into the mux map (so unplugging USB falls back
        // to Wi-Fi); one description per device, preferring USB.
        let all = device::list_all().await.map_err(|e| errors::explain(&e))?;
        let raw = device::list_raw().await.map_err(|e| errors::explain(&e))?;
        let mut map = BTreeMap::new();
        let mut mux = HashMap::new();
        for d in &all {
            mux.insert(d.device_id, (d.udid.clone(), if matches!(d.connection_type, loadside_core::idevice::usbmuxd::Connection::Network(_)) { "wifi".into() } else { "usb".into() }));
        }
        for d in &raw {
            map.insert(d.udid.clone(), device::describe(d).await);
        }
        *self.devices.lock().unwrap() = map;
        *self.mux.lock().unwrap() = mux;
        let list = self.devices();
        self.emit(Event::Devices { devices: list.clone() });
        self.remember_devices();
        Ok(list)
    }

    /// Record paired devices (and what we learned about them) in the state so
    /// Setup knows a phone that's merely asleep has already been set up.
    pub(crate) fn remember_devices(&self) {
        let now = loadside_core::now();
        let seen: Vec<DeviceInfo> = self.devices().into_iter().filter(|d| d.paired && d.problem.is_none()).collect();
        if seen.is_empty() {
            return;
        }
        let mut changed = false;
        {
            let mut st = self.state.lock().unwrap();
            for d in seen {
                let wifi_sync = if d.connection == "wifi" { Some(true) } else { d.wifi_sync };
                let k = match st.known_devices.iter_mut().position(|k| k.udid == d.udid) {
                    Some(i) => &mut st.known_devices[i],
                    None => {
                        st.known_devices.push(loadside_core::state::KnownDevice { udid: d.udid.clone(), ..Default::default() });
                        changed = true;
                        st.known_devices.last_mut().unwrap()
                    }
                };
                let before = k.clone();
                k.name = d.name.clone();
                k.model = d.model.clone().or(k.model.take());
                k.ios_version = d.ios_version.clone().or(k.ios_version.take());
                k.developer_mode = d.developer_mode.or(k.developer_mode);
                k.wifi_sync = wifi_sync.or(k.wifi_sync);
                // Don't rewrite state.json on every attach just for the time.
                if now - k.last_seen > 3600 {
                    k.last_seen = now;
                }
                changed |= *k != before;
            }
        }
        if changed {
            self.save_state();
        }
    }

    /// Update `on_device` for our records on this device.
    pub(crate) async fn sync_on_device(&self, udid: &str) {
        let ids: Vec<String> = self.state.lock().unwrap().apps.iter().filter(|a| a.device_udid == udid).map(|a| a.bundle_id.clone()).collect();
        if ids.is_empty() {
            return;
        }
        let Ok(mut c) = self.connect(udid).await else { return };
        let Ok(set) = c.installed(ids).await else { return };
        let mut changed = false;
        {
            let mut st = self.state.lock().unwrap();
            for a in st.apps.iter_mut().filter(|a| a.device_udid == udid) {
                let now_on = Some(set.contains(&a.bundle_id));
                if a.on_device != now_on {
                    a.on_device = now_on;
                    changed = true;
                }
            }
        }
        if changed {
            self.save_state();
            self.emit_apps();
        }
    }

    // ------------------------------------------------------------------
    // Auto refresh
    // ------------------------------------------------------------------

    pub(crate) fn set_devices(&self, upd: impl FnOnce(&mut BTreeMap<String, DeviceInfo>) -> bool) {
        let changed = upd(&mut self.devices.lock().unwrap());
        if changed {
            self.emit(Event::Devices { devices: self.devices() });
        }
    }

    pub fn trigger_check(&self) {
        let _ = self.check_tx.send(());
    }

    pub async fn scheduler(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<()>) {
        loop {
            let interval = self.config().check_interval_minutes as u64 * 60;
            self.next_check_at.store(loadside_core::now() + interval as i64, Ordering::Relaxed);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
                Some(_) = rx.recv() => {
                    // Debounce bursts (Wi-Fi devices flap, several attach events...).
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    while rx.try_recv().is_ok() {}
                }
            }
            self.check_refreshes();
        }
    }

    pub fn due_apps(&self) -> Vec<String> {
        let cfg = self.config();
        if !cfg.auto_refresh {
            return vec![];
        }
        let now = loadside_core::now();
        let threshold = cfg.refresh_threshold_hours as i64 * 3600;
        // Waiting for the user to sign in again: queuing jobs would just fail.
        if self.needs_interactive_login.load(Ordering::SeqCst) && !self.prompts.has_interactive_client() {
            return vec![];
        }
        let st = self.state.lock().unwrap();
        if st.account.is_none() {
            return vec![];
        }
        st.apps
            .iter()
            .filter(|a| a.options.auto_refresh && a.on_device != Some(false))
            .filter(|a| match a.expires_at {
                Some(e) => e - now < threshold,
                // Unknown expiry: assume the free-account 7 days and refresh
                // a day early, instead of every 30 minutes.
                None => now - a.last_refreshed_at.unwrap_or(a.installed_at) > 6 * 24 * 3600,
            })
            .filter(|a| {
                // Backoff after failures: 30m, 1h, 2h, 4h ... capped at 12h.
                let Some(last) = a.last_attempt_at else { return true };
                let backoff = match a.consecutive_failures {
                    0 => 1800,
                    n => (1800i64 << (n - 1).min(5)).min(12 * 3600),
                };
                now - last > backoff
            })
            .filter(|a| self.device_connected(&a.device_udid))
            .filter(|a| !self.job_active_for_app(&a.id))
            .map(|a| a.id.clone())
            .collect()
    }

    fn check_refreshes(&self) {
        for id in self.due_apps() {
            match self.enqueue(JobSpec::Refresh { app_id: id.clone(), reinstall: false }, true) {
                Ok(j) => info!("Auto-refresh queued for app {id} (job {j})"),
                Err(e) => warn!("Couldn't queue auto-refresh for {id}: {}", e.message),
            }
        }
    }

    pub fn refresh_all(&self) -> Vec<String> {
        let ids: Vec<String> = self.state.lock().unwrap().apps.iter().filter(|a| self.device_connected(&a.device_udid)).map(|a| a.id.clone()).collect();
        ids.into_iter().filter_map(|id| self.enqueue(JobSpec::Refresh { app_id: id, reinstall: false }, false).ok()).collect()
    }

    pub fn set_config(self: &Arc<Self>, patch: Value) -> Result<Value, Failure> {
        if let Some(url) = patch.get("anisette_url").and_then(Value::as_str) {
            let url = url.trim().trim_end_matches('/');
            if !loadside_core::config::anisette_url_ok(url) {
                return Err(f("bad_request", "The anisette server must use https:// (plain http only for localhost)."));
            }
        }
        // Merge, save and publish under one lock so concurrent patches don't
        // overwrite each other.
        let (old, new) = {
            let mut cfg = self.config.lock().unwrap();
            let mut cur = serde_json::to_value(&*cfg).unwrap_or(Value::Null);
            if let (Value::Object(c), Value::Object(p)) = (&mut cur, patch) {
                for (k, v) in p {
                    c.insert(k, v);
                }
            }
            let new: Config = serde_json::from_value(cur).map_err(|e| f("bad_request", &format!("Invalid settings: {e}")))?;
            let new = new.sanitized();
            new.save().map_err(|e| errors::explain(&e))?;
            let old = std::mem::replace(&mut *cfg, new.clone());
            (old, new)
        };
        if old.anisette_url != new.anisette_url || old.modern_client_identity != new.modern_client_identity {
            // The live session was made with the old machine identity.
            info!("Sign-in identity changed; the next Apple request signs in again");
            let engine = self.clone();
            tokio::spawn(async move { engine.invalidate_session().await });
        }
        self.trigger_check();
        Ok(json!(new))
    }
}
