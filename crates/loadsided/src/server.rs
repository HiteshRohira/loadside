//! Unix socket server: newline-delimited JSON requests → engine methods.

use std::path::PathBuf;
use std::sync::Arc;

use loadside_core::protocol::{Failure, InstallParams, Request, ServerMessage};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast::error::RecvError, mpsc};
use tracing::{info, warn};

use crate::engine::{Engine, JobSpec};

pub async fn serve(engine: Arc<Engine>, listener: UnixListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                // Only this user may talk to the daemon, whatever the socket
                // file's permissions end up being.
                let me = unsafe { libc::geteuid() };
                match stream.peer_cred() {
                    Ok(c) if c.uid() == me => {}
                    Ok(c) => {
                        warn!("Rejected IPC connection from another user (uid {})", c.uid());
                        continue;
                    }
                    // The client hung up before we looked (e.g. a readiness
                    // probe). Nothing to serve, and nothing suspicious.
                    Err(e) => {
                        tracing::debug!("IPC client went away before the peer check: {e}");
                        continue;
                    }
                }
                let engine = engine.clone();
                tokio::spawn(async move { connection(engine, stream).await });
            }
            Err(e) => {
                warn!("accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

struct InteractiveGuard(Arc<Engine>);
impl Drop for InteractiveGuard {
    fn drop(&mut self) {
        self.0.prompts.client_disconnected();
    }
}

/// Requests are small JSON objects; anything bigger is a broken or hostile client.
const MAX_REQUEST_BYTES: u64 = 1024 * 1024;
/// Events queued for a client that isn't reading; past this it's dropped.
const MAX_QUEUED_MESSAGES: usize = 2048;

async fn connection(engine: Arc<Engine>, stream: UnixStream) {
    let (r, mut w) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<String>(MAX_QUEUED_MESSAGES);
    let writer = tokio::spawn(async move {
        while let Some(mut line) = rx.recv().await {
            line.push('\n');
            if w.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let mut reader = BufReader::new(r);
    let mut subscribed = false;
    let mut _guard: Option<InteractiveGuard> = None;
    let mut forwarder: Option<tokio::task::JoinHandle<()>> = None;
    // Set when the client missed events: close so it reconnects and gets a
    // fresh snapshot, instead of waiting forever on an event it never sees.
    let kill = std::sync::Arc::new(tokio::sync::Notify::new());

    loop {
        let mut buf = Vec::new();
        let mut limited = (&mut reader).take(MAX_REQUEST_BYTES + 1);
        let read = tokio::select! {
            r = limited.read_until(b'\n', &mut buf) => r,
            _ = kill.notified() => {
                warn!("IPC client fell behind on events; closing its connection");
                break;
            }
        };
        match read {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if buf.len() as u64 > MAX_REQUEST_BYTES {
            warn!("IPC request too large; closing connection");
            break;
        }
        let Ok(line) = String::from_utf8(buf) else { break };
        if line.trim().is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.try_send(encode(&ServerMessage::Response { id: 0, result: None, error: Some(Failure::new("bad_request", format!("invalid JSON: {e}"))) }));
                continue;
            }
        };
        if req.method == "subscribe" {
            if !subscribed {
                subscribed = true;
                let interactive = req.params.get("interactive").and_then(Value::as_bool).unwrap_or(true);
                if interactive {
                    engine.prompts.client_connected();
                    _guard = Some(InteractiveGuard(engine.clone()));
                }
                let mut brx = engine.events.subscribe();
                let tx2 = tx.clone();
                let kill2 = kill.clone();
                forwarder = Some(tokio::spawn(async move {
                    loop {
                        match brx.recv().await {
                            Ok(event) => {
                                // A client that stops reading gets disconnected
                                // instead of growing an unbounded queue.
                                if tx2.try_send(encode(&ServerMessage::Event { event })).is_err() {
                                    kill2.notify_one();
                                    break;
                                }
                            }
                            Err(RecvError::Lagged(_)) => {
                                kill2.notify_one();
                                break;
                            }
                            Err(RecvError::Closed) => break,
                        }
                    }
                }));
            }
            let snap = engine.snapshot().await;
            let _ = tx.send(encode(&ServerMessage::Response { id: req.id, result: Some(json!(snap)), error: None })).await;
            continue;
        }
        let engine = engine.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let id = req.id;
            let shutdown = req.method == "shutdown";
            // Its own task, so a panic still produces a reply.
            let (e2, method) = (engine.clone(), req.method.clone());
            let outcome = tokio::spawn(async move { dispatch(&e2, &method, req.params).await }).await;
            let (result, error) = match outcome {
                Ok(Ok(v)) => (Some(v), None),
                Ok(Err(e)) => (None, Some(e)),
                Err(e) => {
                    tracing::error!("request {} crashed: {e}", req.method);
                    (None, Some(Failure::new("internal", "Loadside hit an internal error handling that request.")))
                }
            };
            let _ = tx.send(encode(&ServerMessage::Response { id, result, error })).await;
            if shutdown {
                info!("Shutdown requested");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                std::process::exit(0);
            }
        });
    }
    if let Some(f) = forwarder {
        f.abort();
    }
    drop(tx);
    // A client that stopped reading can leave the writer stuck in write_all;
    // don't keep the connection (and its interactive flag) alive for it.
    let abort = writer.abort_handle();
    if tokio::time::timeout(std::time::Duration::from_secs(2), writer).await.is_err() {
        abort.abort();
    }
}

fn encode(m: &ServerMessage) -> String {
    serde_json::to_string(m).unwrap_or_else(|_| "{}".into())
}

fn s<'a>(p: &'a Value, k: &str) -> Result<&'a str, Failure> {
    p.get(k).and_then(Value::as_str).ok_or_else(|| Failure::new("bad_request", format!("missing parameter: {k}")))
}

async fn dispatch(engine: &Arc<Engine>, method: &str, p: Value) -> Result<Value, Failure> {
    match method {
        "ping" => Ok(json!({ "version": loadside_core::VERSION, "pid": std::process::id() })),
        "status" => Ok(json!(engine.snapshot().await)),
        "login" => {
            let remember = p.get("remember").and_then(Value::as_bool).unwrap_or(true);
            engine.login(s(&p, "email")?, s(&p, "password")?, remember).await
        }
        "logout" => engine.logout().await,
        "teams" => engine.teams().await,
        "select_team" => engine.select_team(s(&p, "team_id")?).await,
        "devices" => {
            if p.get("refresh").and_then(Value::as_bool).unwrap_or(false) {
                Ok(json!(engine.refresh_devices().await?))
            } else {
                Ok(json!(engine.devices()))
            }
        }
        "inspect" => {
            let path = PathBuf::from(s(&p, "path")?);
            let info = tokio::task::spawn_blocking(move || loadside_core::ipa::inspect(&path))
                .await
                .map_err(|e| Failure::new("internal", e.to_string()))?
                .map_err(|e| loadside_core::errors::explain(&e))?;
            let opts: loadside_core::state::InstallOptions =
                serde_json::from_value(p.get("options").cloned().unwrap_or(json!({}))).unwrap_or_default();
            let needed = info.app_ids_needed(&opts);
            Ok(json!({ "info": info, "app_ids_needed": needed }))
        }
        "install" => {
            let params: InstallParams = serde_json::from_value(p).map_err(|e| Failure::new("bad_request", e.to_string()))?;
            if params.path.is_empty() {
                return Err(Failure::new("bad_request", "missing parameter: path"));
            }
            // Sign-only without somewhere to save it would spend App IDs for nothing.
            if params.install == Some(false) && params.export_path.as_deref().is_none_or(str::is_empty) {
                return Err(Failure::new("bad_request", "Signing without installing needs an export path."));
            }
            let id = engine.enqueue(JobSpec::Install(params), false)?;
            Ok(json!({ "job_id": id }))
        }
        "refresh" => {
            let reinstall = p.get("reinstall").and_then(Value::as_bool).unwrap_or(false);
            let skip_update = p.get("skip_update").and_then(Value::as_bool).unwrap_or(false);
            let id = engine.enqueue(JobSpec::Refresh { app_id: s(&p, "app_id")?.to_string(), reinstall, skip_update }, false)?;
            Ok(json!({ "job_id": id }))
        }
        "refresh_all" => Ok(json!({ "job_ids": engine.refresh_all().await })),
        "apps" => Ok(json!(engine.state.lock().unwrap().apps.clone())),
        "update_app" => engine.update_app(s(&p, "app_id")?, p.get("auto_refresh").and_then(Value::as_bool)),
        "remove_app" => engine.remove_app(s(&p, "app_id")?, p.get("uninstall").and_then(Value::as_bool).unwrap_or(false)).await,
        "github_inspect" => {
            let repo = s(&p, "repo")?;
            let include_prereleases = p.get("include_prereleases").and_then(Value::as_bool).unwrap_or(false);
            let asset_pattern = p.get("asset_pattern").and_then(Value::as_str);
            engine.github_inspect(repo, include_prereleases, asset_pattern).await
        }
        "github_download" => {
            let repo = s(&p, "repo")?;
            let include_prereleases = p.get("include_prereleases").and_then(Value::as_bool).unwrap_or(false);
            let asset_pattern = p.get("asset_pattern").and_then(Value::as_str);
            let asset_name = p.get("asset_name").and_then(Value::as_str);
            let release_id = p.get("release_id").and_then(Value::as_u64);
            let asset_id = p.get("asset_id").and_then(Value::as_u64);
            engine
                .github_download(repo, include_prereleases, asset_pattern, asset_name, release_id, asset_id)
                .await
        }
        "set_app_github" => {
            let app_id = s(&p, "app_id")?;
            let disconnect = p.get("disconnect").and_then(Value::as_bool).unwrap_or(false);
            let repo = p.get("repo").and_then(Value::as_str);
            let policy = match p.get("policy") {
                Some(v) if !v.is_null() => Some(
                    serde_json::from_value(v.clone())
                        .map_err(|e| Failure::new("bad_request", format!("invalid update policy: {e}")))?,
                ),
                _ => None,
            };
            let include_prereleases = p.get("include_prereleases").and_then(Value::as_bool);
            let asset_pattern = p.get("asset_pattern").map(|v| v.as_str().map(str::to_string));
            engine
                .set_app_github(app_id, disconnect, repo, policy, include_prereleases, asset_pattern)
                .await
        }
        "check_updates" => {
            let app_id = p.get("app_id").and_then(Value::as_str);
            let force = p.get("force").and_then(Value::as_bool).unwrap_or(true);
            engine.check_updates(app_id, force).await
        }
        "update_release" => {
            let id = engine.enqueue(JobSpec::Update { app_id: s(&p, "app_id")?.to_string() }, false)?;
            Ok(json!({ "job_id": id }))
        }
        "set_github_token" => {
            let token = p.get("token").and_then(Value::as_str).map(str::to_string);
            engine.set_github_token(token).await
        }
        "app_ids" => engine.app_ids().await,
        "delete_app_id" => engine.delete_app_id(s(&p, "app_id_id")?).await,
        "certs" => engine.certs().await,
        "revoke_cert" => engine.revoke_cert(s(&p, "serial_number")?).await,
        "respond" => {
            let ok = engine.prompts.respond(s(&p, "prompt_id")?, p.get("value").cloned().unwrap_or(Value::Null));
            if ok { Ok(Value::Null) } else { Err(Failure::new("not_found", "That prompt is no longer open.")) }
        }
        "cancel_job" => engine.cancel_job(s(&p, "job_id")?),
        "clear_jobs" => {
            engine.clear_jobs();
            Ok(Value::Null)
        }
        "check_now" => {
            engine.trigger_check();
            Ok(json!({ "due": engine.due_apps() }))
        }
        "logs" => {
            let limit = p.get("limit").and_then(Value::as_u64).unwrap_or(200) as usize;
            Ok(json!(engine.logs.recent(limit)))
        }
        "config" => Ok(json!(engine.config.lock().unwrap().clone())),
        "set_config" => engine.set_config(p.get("config").cloned().unwrap_or(p)),
        "reset_anisette" => {
            let cfg = engine.config();
            // Keychain access can block on a system dialog: keep it off the runtime.
            tokio::task::spawn_blocking(move || loadside_core::account::reset_anisette(&cfg))
                .await
                .map_err(|e| loadside_core::errors::explain(&anyhow::anyhow!(e.to_string())))?
                .map_err(|e| loadside_core::errors::explain(&e))?;
            Ok(Value::Null)
        }
        "setup_status" => Ok(json!(engine.setup_status(p.get("force").and_then(Value::as_bool).unwrap_or(false)).await)),
        "reveal_developer_mode" => engine.reveal_developer_mode(s(&p, "udid")?).await,
        "enable_wifi_sync" => engine.enable_wifi_sync(s(&p, "udid")?).await,
        "shutdown" => Ok(Value::Null),
        other => Err(Failure::new("unknown_method", format!("unknown method: {other}"))),
    }
}
