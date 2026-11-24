use std::path::PathBuf;

/// Root data directory. Defaults to `~/Library/Application Support/Loadside`,
/// overridable with `LOADSIDE_HOME` (handy for tests and development).
pub fn data_dir() -> PathBuf {
    if let Ok(p) = std::env::var("LOADSIDE_HOME") {
        return PathBuf::from(p);
    }
    home().join("Library/Application Support/Loadside")
}

pub fn home() -> PathBuf {
    if let Ok(h) = std::env::var("HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    // No HOME (a stripped environment): ask the user database. Never fall
    // back to a shared directory like /tmp.
    unsafe {
        let pw = libc::getpwuid(libc::geteuid());
        if !pw.is_null() && !(*pw).pw_dir.is_null() {
            let dir = std::ffi::CStr::from_ptr((*pw).pw_dir).to_string_lossy().into_owned();
            if !dir.is_empty() {
                return PathBuf::from(dir);
            }
        }
    }
    panic!("can't find the home directory (HOME is unset and the user database has none)");
}

pub fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("LOADSIDE_SOCKET") {
        return PathBuf::from(p);
    }
    data_dir().join("loadsided.sock")
}

pub fn state_path() -> PathBuf {
    data_dir().join("state.json")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

/// Where original IPAs are kept so apps can be re-signed on refresh even if the
/// user deletes or moves the file they originally installed from.
pub fn library_dir() -> PathBuf {
    data_dir().join("Library")
}

/// Scratch space for extracting and signing.
pub fn work_dir() -> PathBuf {
    data_dir().join("Work")
}

/// Files the app hands to the daemon (copied here so the daemon never needs
/// access to the user's protected folders like Downloads or Desktop).
pub fn incoming_dir() -> PathBuf {
    data_dir().join("Incoming")
}

/// Signed IPAs waiting for the app to move them where the user chose.
pub fn exports_dir() -> PathBuf {
    data_dir().join("Exports")
}

pub fn log_dir() -> PathBuf {
    if std::env::var("LOADSIDE_HOME").is_ok() {
        return data_dir().join("Logs");
    }
    home().join("Library/Logs/Loadside")
}

pub fn log_file() -> PathBuf {
    log_dir().join("loadsided.log")
}

pub const LAUNCH_AGENT_LABEL: &str = "dev.loadside.daemon";

pub fn launch_agent_path() -> PathBuf {
    home().join(format!("Library/LaunchAgents/{LAUNCH_AGENT_LABEL}.plist"))
}

/// Create our directories and make sure only this user can get into them.
/// Fails (rather than carrying on) if a directory belongs to someone else or
/// its permissions can't be tightened, since it holds the IPC socket,
/// signing state and logs.
pub fn ensure_dirs() -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    for d in [data_dir(), library_dir(), work_dir(), incoming_dir(), exports_dir(), log_dir()] {
        std::fs::create_dir_all(&d)?;
    }
    let uid = unsafe { libc::geteuid() };
    for d in [data_dir(), log_dir()] {
        let m = std::fs::symlink_metadata(&d)?;
        if !m.is_dir() || m.uid() != uid {
            return Err(std::io::Error::other(format!("{} isn't a directory owned by you", d.display())));
        }
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
