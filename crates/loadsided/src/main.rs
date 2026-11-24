//! loadsided — the Loadside background daemon.
//!
//! Owns the Apple session, signs & installs apps, watches for devices over
//! usbmuxd and refreshes apps before their free-account profiles expire.
//! Idles at a few MB of RAM; clients talk to it over a Unix socket.

mod engine;
mod logging;
mod checklist;
mod notify;
mod prompts;
mod server;

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use loadside_core::{device, idevice::usbmuxd::UsbmuxdListenEvent, paths};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

fn main() -> anyhow::Result<()> {
    let foreground = std::env::args().any(|a| a == "--foreground" || a == "-f");
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("loadsided {}", loadside_core::VERSION);
        return Ok(());
    }
    paths::ensure_dirs()?;
    let logs = logging::init(foreground);
    loadside_core::init();

    // Two worker threads are plenty; signing parallelism comes from rayon.
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
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
    for dir in [paths::incoming_dir(), paths::exports_dir()] {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > Duration::from_secs(3 * 24 * 3600));
                if old {
                    let p = e.path();
                    let _ = std::fs::remove_dir_all(&p).or_else(|_| std::fs::remove_file(&p));
                }
            }
        }
    }

    let (engine, jobs_rx, check_rx) = engine::Engine::new(logs)?;

    tokio::spawn(engine.clone().worker(jobs_rx));
    tokio::spawn(engine.clone().scheduler(check_rx));

    // usbmuxd's listen stream isn't Send, so it lives on its own thread.
    let (dev_tx, mut dev_rx) = mpsc::unbounded_channel::<WatchMsg>();
    std::thread::Builder::new().name("usbmuxd-watch".into()).spawn(move || watch_thread(dev_tx))?;
    let e2 = engine.clone();
    tokio::spawn(async move {
        while let Some(msg) = dev_rx.recv().await {
            match msg {
                WatchMsg::Event(UsbmuxdListenEvent::Connected(d)) => {
                    let e = e2.clone();
                    tokio::spawn(async move { e.device_attached(d).await });
                }
                WatchMsg::Event(UsbmuxdListenEvent::Disconnected(id)) => e2.device_detached(id),
                // Events may have been missed while disconnected: rebuild.
                WatchMsg::Reconnected => {
                    let e = e2.clone();
                    tokio::spawn(async move {
                        if let Err(fl) = e.refresh_devices().await {
                            warn!("Device resync failed: {}", fl.message);
                        }
                    });
                }
            }
        }
    });

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

enum WatchMsg {
    Event(UsbmuxdListenEvent),
    Reconnected,
}

fn watch_thread(tx: mpsc::UnboundedSender<WatchMsg>) {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            error!("device watcher runtime: {e}");
            return;
        }
    };
    rt.block_on(async move {
        let mut backoff = 1;
        let mut first = true;
        loop {
            if !first && tx.send(WatchMsg::Reconnected).is_err() {
                return;
            }
            first = false;
            match watch_once(&tx).await {
                Ok(()) => backoff = 1,
                Err(e) => {
                    warn!("device watcher: {e:#} (retrying in {backoff}s)");
                }
            }
            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(30);
        }
    });
}

async fn watch_once(tx: &mpsc::UnboundedSender<WatchMsg>) -> anyhow::Result<()> {
    let mut mux = device::usbmuxd().await?;
    let mut stream = mux.listen().await?;
    // usbmuxd replays an Attached event for every device already connected.
    while let Some(ev) = stream.next().await {
        let ev = ev?;
        if tx.send(WatchMsg::Event(ev)).is_err() {
            return Ok(());
        }
    }
    anyhow::bail!("usbmuxd closed the listen connection")
}
