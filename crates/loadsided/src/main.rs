//! loadsided — the Loadside background daemon.
//!
//! Owns the Apple session, signs & installs apps, watches for devices over
//! usbmuxd and refreshes apps before their free-account profiles expire.
//! Idles at a few MB of RAM; clients talk to it over a Unix socket.

mod engine;
mod github;
mod logging;
mod checklist;
mod notify;
mod prompts;
mod server;

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use loadside_core::{
    device::{self, WatchEvent},
    paths,
};
use tracing::{debug, info, warn};

fn main() -> anyhow::Result<()> {
    let foreground = std::env::args().any(|a| a == "--foreground" || a == "-f");
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("loadsided {}", loadside_core::VERSION);
        return Ok(());
    }
    paths::ensure_dirs()?;
    let logs = logging::init(foreground);
    loadside_core::init();

    // Four worker threads are plenty; signing parallelism comes from rayon.
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build()?;
    rt.block_on(run(logs))
}

/// Hold an exclusive lock for the life of the process, so two daemons can
/// never run at once (both would refresh apps and write state).
fn single_instance() -> anyhow::Result<Option<std::fs::File>> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths::data_dir().join("loadsided.lock"))?;
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    Ok((r == 0).then_some(f))
}

async fn run(logs: Arc<logging::LogBuffer>) -> anyhow::Result<()> {
    let Some(_lock) = single_instance()? else {
        // Exit 0: launchd's KeepAlive only restarts on failure, so this
        // doesn't turn into a restart loop.
        info!("loadsided is already running; exiting");
        std::process::exit(0);
    };
    let sock = paths::socket_path();
    let _ = std::fs::remove_file(&sock);
    // Create the socket with a private umask: no window where others can connect.
    let old = unsafe { libc::umask(0o077) };
    let listener = tokio::net::UnixListener::bind(&sock);
    unsafe { libc::umask(old) };
    let listener = listener?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    info!("loadsided {} listening on {}", loadside_core::VERSION, sock.display());

    // Clean up scratch space left by a crash, and stale hand-off files.
    if let Ok(rd) = std::fs::read_dir(paths::work_dir()) {
        for e in rd.flatten() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }

    let (engine, jobs_rx, check_rx) = engine::Engine::new(logs)?;
    // Stale hand-off files, but never ones an installed app refreshes from.
    engine.sweep_staging(Duration::from_secs(3 * 24 * 3600));

    tokio::spawn(engine.clone().worker(jobs_rx));
    tokio::spawn(engine.clone().scheduler(check_rx));
    tokio::spawn(watch_usbmuxd(engine.clone()));
    tokio::spawn(engine.clone().device_reconciler());

    let e3 = engine.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        info!("Stopping");
        let _ = std::fs::remove_file(paths::socket_path());
        drop(e3);
        std::process::exit(0);
    });
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = server::serve(engine.clone(), listener) => {}
        _ = term.recv() => {
            info!("Stopping (SIGTERM)");
            let _ = std::fs::remove_file(paths::socket_path());
        }
    }
    Ok(())
}

/// Continuously watch usbmuxd's v3 event stream (USB + Wi-Fi), reconciling
/// device state after each successful handshake so missed events never stick.
async fn watch_usbmuxd(engine: Arc<engine::Engine>) {
    let mut backoff: u64 = 1;
    loop {
        match device::UsbmuxdListener::connect().await {
            Ok(mut listener) => {
                let connected_at = Instant::now();
                info!("Connected to usbmuxd v3 event stream (USB + Wi-Fi)");
                // Reconcile immediately after the handshake succeeds so any
                // already-connected devices are picked up even if replay races.
                let eng = engine.clone();
                tokio::spawn(async move {
                    if let Err(fl) = eng.reconcile_devices(false).await {
                        warn!("Initial usbmuxd device sync failed: {}", fl.message);
                    }
                });

                loop {
                    match listener.next_event().await {
                        Ok(Some(WatchEvent::Attached(dev))) => {
                            debug!("usbmuxd Attached: {} (id {}, {})", dev.udid, dev.device_id, dev.connection);
                            let eng = engine.clone();
                            tokio::spawn(async move { eng.device_attached(dev).await });
                        }
                        Ok(Some(WatchEvent::Detached(id))) => {
                            debug!("usbmuxd Detached: id {id}");
                            engine.device_detached(id);
                        }
                        Ok(None) => {
                            warn!("usbmuxd closed the event stream");
                            break;
                        }
                        Err(e) => {
                            warn!("usbmuxd event stream error: {e:#}");
                            break;
                        }
                    }
                }

                // A healthy connection resets reconnect backoff instead of ratcheting to 30s forever.
                if connected_at.elapsed() >= Duration::from_secs(10) {
                    backoff = 1;
                }
            }
            Err(e) => {
                warn!("usbmuxd watcher connect failed: {e:#} (retrying in {backoff}s)");
            }
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}
