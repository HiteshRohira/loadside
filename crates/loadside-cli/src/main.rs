//! `loadside` — command line client for loadsided.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use loadside_core::{
    client::Client,
    paths,
    protocol::{Event, Failure, JobInfo, JobStatus, Prompt, Snapshot},
    state::{AppRecord, ExtensionsMode, InstallOptions},
};
use serde_json::{Value, json};

#[derive(Parser)]
#[command(name = "loadside", version, about = "Sideload IPAs onto your iPhone/iPad with your Apple ID", propagate_version = true)]
struct Cli {
    /// Print raw JSON instead of tables.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum ExtArg {
    /// Register an App ID per extension (default)
    Register,
    /// Sign extensions with the main app's profile
    Reuse,
    /// Remove extensions
    Strip,
}

#[derive(Subcommand)]
enum Cmd {
    /// Daemon, account and device overview
    Status,
    /// Sign in with your Apple ID
    Login {
        #[arg(long)]
        email: Option<String>,
        /// Don't store the password in the Keychain (disables unattended refresh)
        #[arg(long)]
        no_remember: bool,
        /// Read the password from stdin; 2FA is answered in the Loadside app
        /// (or with `loadside answer-2fa`)
        #[arg(long)]
        password_stdin: bool,
    },
    /// Answer an open 2FA prompt with a code read from stdin
    #[command(hide = true, name = "answer-2fa")]
    Answer2fa,
    /// Sign out and forget the saved password
    Logout,
    /// List developer teams, or select one
    Team { team_id: Option<String> },
    /// List connected devices
    Devices,
    /// Show what's inside an IPA
    Inspect { path: PathBuf },
    /// Sign and install an IPA
    Install {
        path: PathBuf,
        /// Device UDID (defaults to the only connected device)
        #[arg(short, long)]
        device: Option<String>,
        #[arg(long)]
        bundle_id: Option<String>,
        /// Home screen name
        #[arg(long)]
        name: Option<String>,
        #[arg(long, value_enum, default_value_t = ExtArg::Register)]
        extensions: ExtArg,
        /// Keep the embedded Apple Watch app
        #[arg(long)]
        keep_watch: bool,
        /// Keep UISupportedDevices restrictions
        #[arg(long)]
        keep_device_restrictions: bool,
        /// Show the app's Documents in Files / Finder
        #[arg(long)]
        file_sharing: bool,
        #[arg(long)]
        increased_memory_limit: bool,
        /// .dylib / .framework / .deb / .bundle to inject (repeatable)
        #[arg(long = "inject", value_name = "FILE")]
        inject: Vec<PathBuf>,
        /// Also save the signed IPA here
        #[arg(long, value_name = "OUT.ipa")]
        export: Option<PathBuf>,
        /// Only sign/export, don't install
        #[arg(long)]
        no_install: bool,
        /// Don't auto-refresh this app
        #[arg(long)]
        no_auto_refresh: bool,
        /// Install even if the IPA is FairPlay encrypted
        #[arg(long)]
        allow_encrypted: bool,
        /// Let AltStore/SideStore/LiveContainer receive your signing certificate
        /// and private key (they need it to refresh themselves)
        #[arg(long)]
        allow_signing_key: bool,
        /// Queue the job and return immediately
        #[arg(long)]
        detach: bool,
    },
    /// List installed apps and their expiry
    Apps,
    /// Re-sign and reinstall an app now (or all with --all)
    Refresh {
        app_id: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        detach: bool,
    },
    /// Forget an app (optionally deleting it from the device)
    Remove {
        app_id: String,
        #[arg(long)]
        uninstall: bool,
    },
    /// Turn auto-refresh on/off for an app
    AutoRefresh { app_id: String, #[arg(value_parser = ["on", "off"])] state: String },
    /// App IDs on your account (free accounts: 10 per 7 days)
    AppIds,
    /// Development certificates on your account
    Certs,
    /// Revoke a development certificate by serial number
    RevokeCert { serial: String },
    /// Recent jobs
    Jobs,
    /// Recent daemon logs
    Logs {
        #[arg(short = 'n', default_value_t = 100)]
        limit: usize,
        /// Keep streaming new lines
        #[arg(short, long)]
        follow: bool,
    },
    /// Show or change settings: `config`, `config KEY VALUE`
    Config { key: Option<String>, value: Option<String> },
    /// Forget the anisette machine identity (fixes some sign-in errors)
    ResetAnisette,
    /// Check everything Loadside needs (account, device, Developer Mode, Wi-Fi sync)
    Doctor,
    /// Turn on "Show this iPhone when on Wi-Fi" (USB), so it works over the same Wi-Fi
    WifiSync {
        #[arg(short, long)]
        device: Option<String>,
    },
    /// Make the Developer Mode switch appear on the device (USB)
    DevMode {
        #[arg(short, long)]
        device: Option<String>,
    },
    /// Manage the background daemon
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Install the LaunchAgent so loadsided runs at login
    Install {
        /// Path to the loadsided binary (default: next to this binary)
        #[arg(long)]
        bin: Option<PathBuf>,
    },
    /// Remove the LaunchAgent and stop the daemon
    Uninstall,
    Start,
    Stop,
    Restart,
    Status,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        match e.downcast_ref::<Failure>() {
            Some(f) => {
                eprintln!("error: {}", f.message);
                if let Some(h) = &f.hint {
                    eprintln!("  → {h}");
                }
            }
            None => eprintln!("error: {e:#}"),
        }
        std::process::exit(1);
    }
}

async fn call(c: &Client, method: &str, params: Value) -> Result<Value> {
    c.call(method, params).await.map_err(anyhow::Error::new)
}

fn cli_for_action(a: &Value) -> Option<String> {
    let dev = a["device_udid"].as_str().map(|u| format!(" -d {u}")).unwrap_or_default();
    Some(match a["id"].as_str()? {
        "login" => "loadside login".into(),
        "enable_wifi_sync" => format!("loadside wifi-sync{dev}"),
        "reveal_developer_mode" => format!("loadside dev-mode{dev}"),
        "refresh_devices" => "loadside devices".into(),
        "open_settings" => "loadside config anisette_url https://your-anisette-server".into(),
        "open_url" => format!("open {}", a["url"].as_str()?),
        _ => return None,
    })
}

/// The device to act on: the given UDID, or the only (USB) device.
async fn pick_device(c: &Client, wanted: Option<String>, usb_only: bool) -> Result<String> {
    if let Some(u) = wanted {
        return Ok(u);
    }
    let devs = call(c, "devices", json!({})).await?.as_array().cloned().unwrap_or_default();
    let devs: Vec<Value> = devs.into_iter().filter(|d| !usb_only || d["connection"] == "usb").collect();
    match devs.as_slice() {
        [one] => Ok(one["udid"].as_str().unwrap_or("").to_string()),
        [] if usb_only => bail!("No device on USB. Connect it with a cable and unlock it."),
        [] => bail!("No device connected."),
        _ => bail!("More than one device; pick one with -d <UDID>."),
    }
}

async fn run(cli: Cli) -> Result<()> {
    if let Cmd::Daemon { cmd } = &cli.cmd {
        return daemon_cmd(cmd).await;
    }
    let c = connect().await?;
    let json_out = cli.json;
    let print = |v: &Value| println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());

    match cli.cmd {
        Cmd::Status => {
            let snap: Snapshot = serde_json::from_value(call(&c, "status", json!({})).await?)?;
            if json_out {
                print(&json!(snap));
                return Ok(());
            }
            println!("loadsided {} (pid {})", snap.version, snap.pid);
            match &snap.account {
                Some(a) => println!(
                    "Account:  {} — {}{}",
                    a.email,
                    a.team_name.clone().unwrap_or_default(),
                    if snap.logged_in { "" } else { "  (session will be renewed on next use)" }
                ),
                None => println!("Account:  not signed in (run `loadside login`)"),
            }
            println!("Devices:  {}", if snap.devices.is_empty() { "none connected".to_string() } else { snap.devices.iter().map(|d| format!("{} [{}]", d.name, d.connection)).collect::<Vec<_>>().join(", ") });
            println!("Apps:     {}", snap.apps.len());
            println!("Auto-refresh: {} (when < {}h left){}", if snap.config.auto_refresh { "on" } else { "off" }, snap.config.refresh_threshold_hours,
                snap.next_check_at.map(|t| format!(", next check {}", rel(t))).unwrap_or_default());
            let running: Vec<_> = snap.jobs.iter().filter(|j| matches!(j.status, JobStatus::Queued | JobStatus::Running)).collect();
            for j in running {
                println!("Job:      {} — {} {:.0}%", j.title, j.stage, j.progress * 100.0);
            }
        }
        Cmd::Login { email, no_remember, password_stdin: true } => {
            let email = email.context("--email is required with --password-stdin")?;
            use std::io::IsTerminal;
            if std::io::stdin().is_terminal() {
                bail!("--password-stdin reads a pipe or file (it would echo a typed password). Run `loadside login` to type it instead.");
            }
            let mut password = String::new();
            std::io::stdin().read_line(&mut password)?;
            // Register as a client that can answer prompts, so a 2FA request
            // stays open for the app or `loadside answer-2fa` instead of
            // being cancelled when no window is open.
            subscribe(&c).await?;
            let password = password.trim_end_matches(['\r', '\n']).to_string();
            let v = call(&c, "login", json!({ "email": email.trim(), "password": password, "remember": !no_remember })).await?;
            let name = v.pointer("/account/team_name").and_then(Value::as_str).unwrap_or("");
            println!("✓ Signed in ({name})");
        }
        Cmd::Answer2fa => {
            let mut code = String::new();
            std::io::stdin().read_line(&mut code)?;
            let code: String = code.chars().filter(|ch| ch.is_ascii_digit()).collect();
            if code.is_empty() {
                bail!("no code on stdin");
            }
            let snap: Snapshot = serde_json::from_value(call(&c, "status", json!({})).await?)?;
            let p = snap.prompts.iter().find(|p| p.kind == "two_factor").context("no 2FA prompt is open")?;
            call(&c, "respond", json!({ "prompt_id": p.id, "value": { "SubmitCode": code } })).await?;
            println!("✓ Code sent");
        }
        Cmd::Login { email, no_remember, password_stdin: false } => {
            let email = match email {
                Some(e) => e,
                None => prompt_line("Apple ID email: ")?,
            };
            let password = rpassword::prompt_password("Password: ")?;
            subscribe(&c).await?;
            let fut = c.call("login", json!({ "email": email, "password": password, "remember": !no_remember }));
            let v = with_prompts(&c, fut).await?;
            let name = v.pointer("/account/team_name").and_then(Value::as_str).unwrap_or("");
            println!("✓ Signed in ({name})");
            if let Some(teams) = v.get("teams").and_then(Value::as_array).filter(|t| t.len() > 1) {
                println!("This account has {} teams; switch with `loadside team <TEAM_ID>`:", teams.len());
                for t in teams {
                    println!("  {}  {}", t["team_id"].as_str().unwrap_or(""), t["name"].as_str().unwrap_or(""));
                }
            }
        }
        Cmd::Logout => {
            call(&c, "logout", json!({})).await?;
            println!("Signed out.");
        }
        Cmd::Team { team_id } => match team_id {
            Some(id) => {
                call(&c, "select_team", json!({ "team_id": id })).await?;
                println!("Team selected.");
            }
            None => {
                subscribe(&c).await?;
                let v = with_prompts(&c, c.call("teams", json!({}))).await?;
                if json_out {
                    print(&v);
                } else {
                    let sel = v["selected"].as_str().unwrap_or("");
                    for t in v["teams"].as_array().cloned().unwrap_or_default() {
                        let id = t["team_id"].as_str().unwrap_or("");
                        println!("{} {}  {}{}", if id == sel { "*" } else { " " }, id, t["name"].as_str().unwrap_or(""), if t["is_free"].as_bool() == Some(true) { "  (free)" } else { "" });
                    }
                }
            }
        },
        Cmd::Devices => {
            let v = call(&c, "devices", json!({ "refresh": true })).await?;
            if json_out {
                print(&v);
                return Ok(());
            }
            let devs = v.as_array().cloned().unwrap_or_default();
            if devs.is_empty() {
                println!("No devices. Connect your iPhone/iPad with a cable, unlock it and tap Trust.");
            }
            for d in devs {
                let dm = match d["developer_mode"].as_bool() {
                    Some(true) => "Developer Mode on",
                    Some(false) => "Developer Mode OFF (Settings → Privacy & Security)",
                    None => "",
                };
                let conn = d["connection"].as_str().unwrap_or("").to_string();
                println!(
                    "{}  {}  {} iOS {}  [{}]  {}{}",
                    d["udid"].as_str().unwrap_or(""),
                    d["name"].as_str().unwrap_or(""),
                    d["model"].as_str().unwrap_or(""),
                    d["ios_version"].as_str().unwrap_or("?"),
                    conn,
                    dm,
                    match d["wifi_sync"].as_bool() {
                        Some(true) => "  · Wi-Fi sync on",
                        Some(false) => "  · Wi-Fi sync off (`loadside wifi-sync`)",
                        None => "",
                    }
                );
                if let Some(p) = d["problem"].as_str() {
                    println!("    ⚠ {p}");
                }
            }
        }
        Cmd::Doctor => {
            let v = call(&c, "setup_status", json!({ "force": true })).await?;
            if json_out {
                print(&v);
                return Ok(());
            }
            let mut group = String::new();
            for chk in v["checks"].as_array().cloned().unwrap_or_default() {
                let g = chk["group"].as_str().unwrap_or("");
                if g != group {
                    group = g.to_string();
                    println!("\n{}", if g == "optional" { "Optional" } else { "Essentials" });
                }
                let icon = match chk["status"].as_str().unwrap_or("") {
                    "ok" => "✓",
                    "action" => "→",
                    "warning" => "!",
                    "error" => "✗",
                    "pending" => "…",
                    _ => "○",
                };
                println!("  {icon} {:<34} {}", chk["title"].as_str().unwrap_or(""), chk["summary"].as_str().unwrap_or(""));
                if matches!(chk["status"].as_str(), Some("action" | "warning" | "error")) {
                    if let Some(d) = chk["detail"].as_str() {
                        for line in d.lines() {
                            println!("      {line}");
                        }
                    }
                    for a in chk["actions"].as_array().cloned().unwrap_or_default() {
                        if let Some(cmd) = cli_for_action(&a) {
                            println!("      $ {cmd}");
                        }
                    }
                }
            }
            println!();
            if v["ready"].as_bool() == Some(true) {
                println!("Ready to install.");
            } else {
                println!("Fix the items marked → or ✗ above, then run `loadside doctor` again.");
            }
        }
        Cmd::WifiSync { device } => {
            let udid = pick_device(&c, device, true).await?;
            call(&c, "enable_wifi_sync", json!({ "udid": udid })).await?;
            println!("✓ Wi-Fi sync on. The device shows up over Wi-Fi while it's on the same network as this Mac.");
        }
        Cmd::DevMode { device } => {
            let udid = pick_device(&c, device, true).await?;
            call(&c, "reveal_developer_mode", json!({ "udid": udid })).await?;
            println!("✓ On the device: Settings → Privacy & Security → Developer Mode → On, then restart and confirm.");
        }
        Cmd::Inspect { path } => {
            let staged = stage(&path)?;
            let v = call(&c, "inspect", json!({ "path": staged })).await;
            // Only ever delete the per-call folder we created inside Incoming.
            let staged = Path::new(&staged);
            if let Some(dir) = staged.parent().filter(|d| d.parent() == Some(paths::incoming_dir().as_path())) {
                let _ = std::fs::remove_dir_all(dir);
            }
            let v = v?;
            if json_out {
                print(&v);
                return Ok(());
            }
            let i = &v["info"];
            println!("{} {} ({})", i["name"].as_str().unwrap_or(""), i["version"].as_str().unwrap_or(""), i["bundle_id"].as_str().unwrap_or(""));
            println!("Minimum iOS: {}", i["minimum_os"].as_str().unwrap_or("?"));
            println!("Size: {:.1} MB", i["size_bytes"].as_f64().unwrap_or(0.0) / 1e6);
            for b in i["sub_bundles"].as_array().cloned().unwrap_or_default() {
                println!("  {} {} ({})", if b["kind"] == "watch" { "⌚" } else { "◦" }, b["name"].as_str().unwrap_or(""), b["bundle_id"].as_str().unwrap_or(""));
            }
            if i["encrypted"].as_bool() == Some(true) {
                println!("⚠ FairPlay encrypted — will not launch when sideloaded.");
            }
            println!("App IDs needed (default options): {}", v["app_ids_needed"]);
        }
        Cmd::Install {
            path, device, bundle_id, name, extensions, keep_watch, keep_device_restrictions, file_sharing,
            increased_memory_limit, inject, export, no_install, no_auto_refresh, allow_encrypted, allow_signing_key, detach,
        } => {
            let inject = inject.iter().map(|p| stage(p).map(PathBuf::from)).collect::<Result<Vec<_>>>()?;
            let options = InstallOptions {
                bundle_id,
                display_name: name,
                remove_supported_devices: !keep_device_restrictions,
                strip_watch_app: !keep_watch,
                extensions: match extensions {
                    ExtArg::Register => ExtensionsMode::RegisterAll,
                    ExtArg::Reuse => ExtensionsMode::ReuseMain,
                    ExtArg::Strip => ExtensionsMode::Strip,
                },
                file_sharing,
                increased_memory_limit,
                inject,
                auto_refresh: !no_auto_refresh,
                allow_encrypted,
                allow_signing_key,
            };
            let export = export.map(|p| abs(&p)).transpose()?;
            if !detach {
                subscribe(&c).await?;
            }
            let v = call(&c, "install", json!({
                "path": stage(&path)?, "device_udid": device, "options": options,
                "export_path": export, "install": !no_install,
            })).await?;
            let job = v["job_id"].as_str().unwrap_or("").to_string();
            if detach {
                println!("Queued job {job}");
            } else {
                follow_job(&c, &job).await?;
            }
        }
        Cmd::Apps => {
            let v = call(&c, "apps", json!({})).await?;
            if json_out {
                print(&v);
                return Ok(());
            }
            let apps: Vec<AppRecord> = serde_json::from_value(v)?;
            if apps.is_empty() {
                println!("No apps yet. Install one with `loadside install App.ipa`.");
            }
            for a in apps {
                let exp = match a.expires_at {
                    Some(e) => format!("expires {}", rel(e)),
                    None => "expiry unknown".into(),
                };
                let flags = [
                    (!a.options.auto_refresh).then_some("auto-refresh off"),
                    (a.on_device == Some(false)).then_some("not on device"),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(", ");
                println!("{}  {} {}  on {}  {}{}", a.id, a.name, a.version.unwrap_or_default(), a.device_name, exp, if flags.is_empty() { String::new() } else { format!("  [{flags}]") });
                if let Some(e) = a.last_error {
                    println!("          ⚠ last refresh failed: {}", e.message);
                }
            }
        }
        Cmd::Refresh { app_id, all, detach } => {
            if !detach {
                subscribe(&c).await?;
            }
            let jobs: Vec<String> = if all {
                serde_json::from_value(call(&c, "refresh_all", json!({})).await?["job_ids"].clone())?
            } else {
                let id = app_id.context("give an app id (see `loadside apps`) or --all")?;
                vec![call(&c, "refresh", json!({ "app_id": id })).await?["job_id"].as_str().unwrap_or("").to_string()]
            };
            if jobs.is_empty() {
                println!("Nothing to refresh (are the devices connected?).");
            }
            for j in jobs {
                if detach {
                    println!("Queued job {j}");
                } else {
                    follow_job(&c, &j).await?;
                }
            }
        }
        Cmd::Remove { app_id, uninstall } => {
            call(&c, "remove_app", json!({ "app_id": app_id, "uninstall": uninstall })).await?;
            println!("Removed.");
        }
        Cmd::AutoRefresh { app_id, state } => {
            call(&c, "update_app", json!({ "app_id": app_id, "auto_refresh": state == "on" })).await?;
            println!("Auto-refresh {state}.");
        }
        Cmd::AppIds => {
            subscribe(&c).await?;
            let v = with_prompts(&c, c.call("app_ids", json!({}))).await?;
            if json_out {
                print(&v);
                return Ok(());
            }
            if let (Some(a), Some(m)) = (v["available_quantity"].as_i64(), v["max_quantity"].as_u64()) {
                println!("{a} of {m} App IDs available");
            }
            for a in v["app_ids"].as_array().cloned().unwrap_or_default() {
                println!("  {}  {}{}", a["identifier"].as_str().unwrap_or(""), a["name"].as_str().unwrap_or(""),
                    a["expires_at"].as_i64().map(|e| format!("  (expires {})", rel(e))).unwrap_or_default());
            }
        }
        Cmd::Certs => {
            subscribe(&c).await?;
            let v = with_prompts(&c, c.call("certs", json!({}))).await?;
            if json_out {
                print(&v);
                return Ok(());
            }
            for cert in v.as_array().cloned().unwrap_or_default() {
                println!("{}  {}  machine: {}{}{}", cert["serial_number"].as_str().unwrap_or(""), cert["name"].as_str().unwrap_or(""),
                    cert["machine_name"].as_str().unwrap_or("?"),
                    cert["expires_at"].as_i64().map(|e| format!("  expires {}", rel(e))).unwrap_or_default(),
                    if cert["ours"].as_bool() == Some(true) { "  (Loadside)" } else { "" });
            }
        }
        Cmd::RevokeCert { serial } => {
            call(&c, "revoke_cert", json!({ "serial_number": serial })).await?;
            println!("Revoked.");
        }
        Cmd::Jobs => {
            let snap: Snapshot = serde_json::from_value(call(&c, "status", json!({})).await?)?;
            if json_out {
                print(&json!(snap.jobs));
                return Ok(());
            }
            for j in snap.jobs.iter().rev() {
                println!("{}  {:<9} {}  {}", j.id, format!("{:?}", j.status).to_lowercase(), j.title,
                    j.error.as_ref().map(|e| e.message.clone()).unwrap_or_else(|| j.stage.clone()));
            }
        }
        Cmd::Logs { limit, follow } => {
            if follow {
                c.call("subscribe", json!({ "interactive": false })).await.map_err(anyhow::Error::new)?;
            }
            let v = call(&c, "logs", json!({ "limit": limit })).await?;
            for l in v.as_array().cloned().unwrap_or_default() {
                println!("{} {:>5} {}", fmt_ts(l["ts"].as_i64().unwrap_or(0)), l["level"].as_str().unwrap_or(""), l["message"].as_str().unwrap_or(""));
            }
            if follow {
                let mut rx = c.take_events().await.context("no event stream")?;
                while let Some(ev) = rx.recv().await {
                    if let Event::Log { line } = ev {
                        println!("{} {:>5} {}", fmt_ts(line.ts), line.level, line.message);
                    }
                }
            }
        }
        Cmd::Config { key, value } => match (key, value) {
            (None, _) => print(&call(&c, "config", json!({})).await?),
            (Some(k), None) => println!("{}", call(&c, "config", json!({})).await?.get(&k).cloned().unwrap_or(Value::Null)),
            (Some(k), Some(v)) => {
                let val: Value = serde_json::from_str(&v).unwrap_or(Value::String(v));
                print(&call(&c, "set_config", json!({ "config": { k: val } })).await?);
            }
        },
        Cmd::ResetAnisette => {
            call(&c, "reset_anisette", json!({})).await?;
            println!("Machine identity reset. Sign in again with `loadside login`.");
        }
        Cmd::Daemon { .. } => unreachable!(),
    }
    Ok(())
}

async fn subscribe(c: &Client) -> Result<()> {
    c.call("subscribe", json!({ "interactive": true })).await.map_err(anyhow::Error::new)?;
    Ok(())
}

/// Await `fut` while answering any prompts the daemon raises on the terminal.
async fn with_prompts<F>(c: &Client, fut: F) -> Result<Value>
where
    F: std::future::Future<Output = Result<Value, Failure>>,
{
    let mut rx = c.take_events().await.context("no event stream")?;
    tokio::pin!(fut);
    loop {
        tokio::select! {
            r = &mut fut => return r.map_err(anyhow::Error::new),
            Some(ev) = rx.recv() => {
                if let Event::Prompt { prompt } = ev {
                    answer_prompt(c, &prompt).await?;
                }
            }
        }
    }
}

async fn answer_prompt(c: &Client, p: &Prompt) -> Result<()> {
    let value = match p.kind.as_str() {
        "two_factor" => {
            if let Some(e) = p.payload["lastError"].as_str().or(p.payload["last_error"].as_str()) {
                eprintln!("⚠ {e}");
            }
            let numbers = p.payload["numbers"].as_array().cloned().unwrap_or_default();
            let sms = p.payload["sms"].as_bool().unwrap_or(false);
            let unknown = p.payload["unknown"].as_bool().unwrap_or(false);
            eprintln!("{}", if sms { "Enter the code sent by SMS." } else if unknown { "Choose how to receive a verification code." } else { "Enter the 6-digit code shown on your trusted Apple device." });
            if !numbers.is_empty() {
                let list = numbers.iter().map(|n| format!("{} (••{})", n["id"], n["lastTwoDigits"].as_str().unwrap_or(""))).collect::<Vec<_>>().join(", ");
                eprintln!("  Type `sms <id>` to get a text instead: {list}");
            }
            let line = tokio::task::spawn_blocking(|| prompt_line("Code: ")).await??;
            let line = line.trim();
            if let Some(id) = line.strip_prefix("sms ") {
                json!({ "SendSms": id.trim().parse::<u32>().unwrap_or(0) })
            } else if line.eq_ignore_ascii_case("resend") {
                json!("ResendCode")
            } else if line.is_empty() {
                json!("Abort")
            } else {
                json!({ "SubmitCode": line })
            }
        }
        "max_certs" => {
            let certs = p.payload["certificates"].as_array().cloned().unwrap_or_default();
            eprintln!("Your account is at its development certificate limit. Revoking one makes apps signed with it stop launching until refreshed.");
            for (i, cert) in certs.iter().enumerate() {
                eprintln!("  [{}] {}  machine: {}{}", i + 1, cert["serial_number"].as_str().unwrap_or(""), cert["machine_name"].as_str().unwrap_or("?"),
                    if cert["ours"].as_bool() == Some(true) { " (Loadside)" } else { "" });
            }
            let line = tokio::task::spawn_blocking(|| prompt_line("Revoke which? (number, empty to abort): ")).await??;
            match line.trim().parse::<usize>().ok().and_then(|n| certs.get(n.wrapping_sub(1))) {
                Some(cert) => json!({ "revoke": [cert["serial_number"]] }),
                None => json!({ "revoke": [] }),
            }
        }
        _ => return Ok(()),
    };
    c.call("respond", json!({ "prompt_id": p.id, "value": value })).await.map_err(anyhow::Error::new)?;
    Ok(())
}

async fn follow_job(c: &Client, job_id: &str) -> Result<()> {
    let mut rx = c.take_events().await.context("no event stream")?;
    let mut last_len = 0;
    loop {
        let Some(ev) = rx.recv().await else { bail!("lost connection to loadsided") };
        match ev {
            Event::Prompt { prompt } => {
                eprintln!();
                answer_prompt(c, &prompt).await?;
            }
            Event::Job { job } if job.id == job_id => {
                if render_job(&job, &mut last_len) {
                    return finish_job(&job);
                }
            }
            _ => {}
        }
    }
}

fn render_job(j: &JobInfo, last_len: &mut usize) -> bool {
    let done = matches!(j.status, JobStatus::Succeeded | JobStatus::Failed | JobStatus::Cancelled);
    let width = 24;
    let filled = ((j.progress.clamp(0.0, 1.0)) * width as f32) as usize;
    let line = format!("{}  [{}{}] {:>3.0}%  {}", j.title, "█".repeat(filled), "░".repeat(width - filled), j.progress * 100.0, j.stage);
    let pad = last_len.saturating_sub(line.chars().count());
    eprint!("\r{line}{}", " ".repeat(pad));
    *last_len = line.chars().count();
    let _ = std::io::stderr().flush();
    if done {
        eprintln!();
    }
    done
}

fn finish_job(j: &JobInfo) -> Result<()> {
    for w in &j.warnings {
        eprintln!("⚠ {w}");
    }
    match j.status {
        JobStatus::Succeeded => {
            if let Some(o) = &j.output_path {
                println!("✓ Signed IPA written to {o}");
            }
            println!("✓ {}", j.title);
            Ok(())
        }
        JobStatus::Cancelled => bail!("cancelled"),
        _ => Err(anyhow::Error::new(j.error.clone().unwrap_or_else(|| Failure::new("unknown", "job failed")))),
    }
}

fn prompt_line(label: &str) -> Result<String> {
    eprint!("{label}");
    std::io::stderr().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim_end_matches(['\r', '\n']).to_string())
}

fn abs(p: &Path) -> Result<String> {
    let p = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir()?.join(p) };
    Ok(p.display().to_string())
}

/// Copy an input file into Loadside's folder so the daemon never reads the
/// user's protected folders (macOS would prompt to allow "loadsided").
fn stage(p: &Path) -> Result<String> {
    let src = PathBuf::from(abs(p)?);
    if !src.exists() {
        bail!("{} doesn't exist", src.display());
    }
    if src.starts_with(paths::data_dir()) {
        return Ok(src.display().to_string());
    }
    let dir = paths::incoming_dir().join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&dir)?;
    let dst = dir.join(src.file_name().context("bad path")?);
    if src.is_dir() {
        loadside_core::ipa::copy_dir(&src, &dst)?;
    } else {
        std::fs::copy(&src, &dst).with_context(|| format!("copying {}", src.display()))?;
    }
    Ok(dst.display().to_string())
}

fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.with_timezone(&chrono::Local).format("%m-%d %H:%M:%S").to_string()).unwrap_or_default()
}

fn rel(ts: i64) -> String {
    let d = ts - loadside_core::now();
    let (past, a) = (d < 0, d.unsigned_abs());
    let s = if a < 3600 { format!("{}m", a / 60) } else if a < 86400 * 2 { format!("{}h", a / 3600) } else { format!("{}d {}h", a / 86400, (a % 86400) / 3600) };
    if past { format!("{s} ago") } else { format!("in {s}") }
}

// ---------------------------------------------------------------------------
// Daemon management
// ---------------------------------------------------------------------------

async fn connect() -> Result<Client> {
    if let Ok(c) = Client::connect_default().await {
        return Ok(c);
    }
    // Not running: start it (via launchd if installed, else directly).
    if paths::launch_agent_path().exists() {
        let _ = launchctl(&["kickstart", &format!("gui/{}/{}", uid(), paths::LAUNCH_AGENT_LABEL)]);
    } else {
        let bin = sibling_daemon()?;
        std::process::Command::new(&bin)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("starting {}", bin.display()))?;
    }
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(c) = Client::connect_default().await {
            return Ok(c);
        }
    }
    bail!("loadsided isn't running and couldn't be started. Try `loadside daemon install`.")
}

fn sibling_daemon() -> Result<PathBuf> {
    let exe = std::fs::canonicalize(std::env::current_exe()?)?;
    let dir = exe.parent().context("no parent dir")?;
    for cand in [dir.join("loadsided"), dir.join("../Resources/loadsided"), dir.join("../MacOS/loadsided")] {
        if cand.exists() {
            return Ok(std::fs::canonicalize(cand)?);
        }
    }
    bail!("couldn't find the loadsided binary next to {}", exe.display())
}

fn uid() -> u32 {
    // SAFETY: getuid never fails.
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    Ok(std::process::Command::new("/bin/launchctl").args(args).output()?)
}

pub fn launch_agent_plist(bin: &Path) -> Result<Vec<u8>> {
    use plist::{Dictionary, Value as P};
    let log = paths::log_dir().join("launchd.log").display().to_string();
    let mut keep_alive = Dictionary::new();
    keep_alive.insert("SuccessfulExit".into(), P::Boolean(false));
    let mut d = Dictionary::new();
    d.insert("Label".into(), P::String(paths::LAUNCH_AGENT_LABEL.into()));
    d.insert("ProgramArguments".into(), P::Array(vec![P::String(bin.display().to_string())]));
    d.insert("RunAtLoad".into(), P::Boolean(true));
    d.insert("KeepAlive".into(), P::Dictionary(keep_alive));
    d.insert("ThrottleInterval".into(), P::Integer(10.into()));
    d.insert("ProcessType".into(), P::String("Background".into()));
    d.insert("StandardOutPath".into(), P::String(log.clone()));
    d.insert("StandardErrorPath".into(), P::String(log));
    // A real plist writer escapes paths containing &, < and friends.
    let mut out = Vec::new();
    plist::to_writer_xml(&mut out, &P::Dictionary(d))?;
    Ok(out)
}

async fn daemon_cmd(cmd: &DaemonCmd) -> Result<()> {
    let target = format!("gui/{}/{}", uid(), paths::LAUNCH_AGENT_LABEL);
    let domain = format!("gui/{}", uid());
    let plist = paths::launch_agent_path();
    match cmd {
        DaemonCmd::Install { bin } => {
            let bin = match bin {
                Some(b) => std::fs::canonicalize(b)?,
                None => sibling_daemon()?,
            };
            paths::ensure_dirs()?;
            // Stop a manually started daemon first.
            if let Ok(c) = Client::connect_default().await {
                let _ = c.call("shutdown", json!({})).await;
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            if !bin.exists() {
                bail!("[missing_binary] {} doesn't exist. Reinstall Loadside.", bin.display());
            }
            let _ = launchctl(&["bootout", &target]);
            // Undo a `launchctl disable` (doesn't override System Settings → Login Items).
            let _ = launchctl(&["enable", &target]);
            std::fs::create_dir_all(plist.parent().unwrap())?;
            std::fs::write(&plist, launch_agent_plist(&bin)?)?;
            // bootout is asynchronous; bootstrap right after it often fails
            // with "Input/output error". Retry for a few seconds.
            let mut out = launchctl(&["bootstrap", &domain, &plist.display().to_string()])?;
            for _ in 0..10 {
                if out.status.success() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                out = launchctl(&["bootstrap", &domain, &plist.display().to_string()])?;
            }
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let disabled = launchctl(&["print-disabled", &domain])
                    .map(|o| {
                        let t = String::from_utf8_lossy(&o.stdout).to_string();
                        t.lines().any(|l| l.contains(paths::LAUNCH_AGENT_LABEL) && (l.contains("disabled") || l.contains("true")))
                    })
                    .unwrap_or(false);
                if disabled || err.contains("Operation not permitted") || err.contains("not permitted") {
                    bail!(
                        "[login_item_disabled] macOS blocked the background service. Open System Settings → General → Login Items & Extensions and allow \"loadsided\" (Allow in the Background), then try again. ({err})"
                    );
                }
                bail!("[bootstrap_failed] launchctl bootstrap failed: {err}");
            }
            // Make sure it actually came up.
            let mut up = false;
            for _ in 0..25 {
                if Client::connect_default().await.is_ok() {
                    up = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            if !up {
                bail!(
                    "[not_responding] The background service was registered but isn't answering. See {}",
                    paths::log_dir().join("loadsided.log").display()
                );
            }
            println!("✓ loadsided will run at login ({})", bin.display());
        }
        DaemonCmd::Uninstall => {
            let _ = launchctl(&["bootout", &target]);
            match std::fs::remove_file(&plist) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => bail!("couldn't remove {}: {e}", plist.display()),
            }
            if let Ok(c) = Client::connect_default().await {
                let _ = c.call("shutdown", json!({})).await;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            if launchctl(&["print", &target]).is_ok_and(|o| o.status.success()) {
                bail!("the service is still registered with launchd; run `launchctl bootout {target}`");
            }
            if Client::connect_default().await.is_ok() {
                bail!("loadsided is still running; quit it with `pkill loadsided`");
            }
            println!("✓ Background service removed (app, settings and apps list are kept)");
        }
        DaemonCmd::Start => {
            connect().await?;
            println!("✓ running");
        }
        DaemonCmd::Stop => {
            if plist.exists() {
                // KeepAlive only restarts on failure, so a clean shutdown sticks.
            }
            match Client::connect_default().await {
                Ok(c) => {
                    let _ = c.call("shutdown", json!({})).await;
                    println!("✓ stopped");
                }
                Err(_) => println!("not running"),
            }
        }
        DaemonCmd::Restart => {
            if let Ok(c) = Client::connect_default().await {
                let _ = c.call("shutdown", json!({})).await;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            connect().await?;
            println!("✓ restarted");
        }
        DaemonCmd::Status => {
            println!("LaunchAgent: {}", if plist.exists() { plist.display().to_string() } else { "not installed".into() });
            match Client::connect_default().await {
                Ok(c) => {
                    let v = c.call("ping", json!({})).await.map_err(anyhow::Error::new)?;
                    let pid = v["pid"].as_u64().unwrap_or(0);
                    let rss = std::process::Command::new("/bin/ps").args(["-o", "rss=", "-p", &pid.to_string()]).output().ok()
                        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok());
                    println!("Daemon: running, version {}, pid {pid}{}", v["version"].as_str().unwrap_or(""),
                        rss.map(|k| format!(", {:.1} MB RAM", k as f64 / 1024.0)).unwrap_or_default());
                }
                Err(_) => println!("Daemon: not running"),
            }
        }
    }
    Ok(())
}
