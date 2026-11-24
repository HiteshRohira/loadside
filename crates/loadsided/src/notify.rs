//! macOS user notifications (via osascript, so the daemon stays a plain
//! binary with no app bundle or entitlements).

pub fn send(title: &str, message: &str) {
    // Text goes in as arguments, never spliced into the script, so quotes
    // and newlines in app names or error hints can't break it.
    let script = "on run argv\ndisplay notification (item 1 of argv) with title \"Loadside\" subtitle (item 2 of argv)\nend run";
    let clean = |s: &str| s.replace(['\r', '\n'], " ");
    let _ = std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .arg(clean(message))
        .arg(clean(title))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|mut c| {
            std::thread::spawn(move || {
                let _ = c.wait();
            })
        });
}
