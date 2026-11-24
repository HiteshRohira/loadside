//! Turns low-level errors (Apple's developer API, GrandSlam auth, lockdown,
//! installd, our own IPA handling) into a [`Failure`] with a plain-language
//! message and a hint that says what to actually do about it.

use isideload::SideloadError;
use rootcause::Report;

use crate::protocol::Failure;

/// An error that came out of `isideload`, flattened into a std error so it can
/// travel inside `anyhow`, while keeping Apple's numeric codes for mapping.
#[derive(Debug)]
pub struct AppleError {
    pub dev_code: Option<i64>,
    pub auth_code: Option<i64>,
    pub text: String,
}

impl std::fmt::Display for AppleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}
impl std::error::Error for AppleError {}

pub fn from_report(r: Report) -> anyhow::Error {
    let mut dev_code = None;
    let mut auth_code = None;
    for node in r.iter_reports() {
        if let Some(e) = node.downcast_current_context::<SideloadError>() {
            match e {
                SideloadError::DeveloperError(c, _) => dev_code = dev_code.or(Some(*c)),
                SideloadError::AuthWithMessage(c, _) => auth_code = auth_code.or(Some(*c)),
                _ => {}
            }
        }
    }
    // The default report rendering starts with a blank line and interleaves
    // source locations; join the context messages instead ("a: b: c").
    let mut parts: Vec<String> = Vec::new();
    for node in r.iter_reports() {
        let msg = node.format_current_context().to_string();
        let msg = msg.trim();
        if !msg.is_empty() && !parts.iter().any(|p| p == msg) {
            parts.push(msg.to_string());
        }
    }
    let text = if parts.is_empty() { format!("{r}").trim().to_string() } else { parts.join(": ") };
    anyhow::Error::new(AppleError { dev_code, auth_code, text })
}

/// First `n` characters (never splits a UTF-8 sequence).
pub fn truncate_chars(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// An HTTP status mentioned the way reqwest/isideload format it.
fn http_status(t: &str, code: u16) -> bool {
    let c = code.to_string();
    has(t, &[&format!("status {c}"), &format!("status: {c}"), &format!("http {c}"), &format!("({c} "), &format!("error {c}")])
}

/// `.rc()` converts an isideload `Result<T, Report>` into `anyhow::Result<T>`.
pub trait RcExt<T> {
    fn rc(self) -> anyhow::Result<T>;
}
impl<T> RcExt<T> for Result<T, Report> {
    fn rc(self) -> anyhow::Result<T> {
        self.map_err(from_report)
    }
}

/// Build a [`Failure`] from any error.
pub fn explain(err: &anyhow::Error) -> Failure {
    // Already a Failure (we raise these deliberately with good text).
    for cause in err.chain() {
        if let Some(f) = cause.downcast_ref::<Failure>() {
            let mut f = f.clone();
            if f.detail.is_none() {
                f.detail = Some(format!("{err:#}"));
            }
            return f;
        }
    }
    let mut dev_code = None;
    let mut auth_code = None;
    for cause in err.chain() {
        if let Some(a) = cause.downcast_ref::<AppleError>() {
            dev_code = dev_code.or(a.dev_code);
            auth_code = auth_code.or(a.auth_code);
        }
    }
    let detail = format!("{err:#}");
    let mut f = classify(&detail, dev_code, auth_code);
    f.detail = Some(detail);
    f
}

fn has(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| text.contains(n))
}

pub fn classify(detail: &str, dev_code: Option<i64>, auth_code: Option<i64>) -> Failure {
    let t = detail.to_lowercase();
    let f = |code: &str, msg: &str, hint: &str| Failure::new(code, msg).with_hint(hint);

    // ---- Apple ID / authentication ----
    if matches!(auth_code, Some(-22406)) || has(&t, &["-22406", "your apple id or password was incorrect"]) {
        return f("bad_credentials", "Apple rejected the Apple ID or password.",
            "Check the email and password. If you use Sign in with Apple-only accounts, set a password at appleid.apple.com first.");
    }
    if matches!(auth_code, Some(-20209)) || has(&t, &["-20209", "account has been locked", "this apple id has been locked"]) {
        return f("account_locked", "This Apple ID is locked for security reasons.",
            "Unlock it at iforgot.apple.com, then sign in again.");
    }
    if matches!(auth_code, Some(-36607)) || has(&t, &["-36607"]) {
        return f("anisette_rejected", "Apple refused the sign-in from this machine identity.",
            "Wait a minute and retry. If it keeps happening: Settings → Reset machine identity, or switch to another anisette server.");
    }
    if has(&t, &["-21669", "incorrect verification code", "verification code"]) && has(&t, &["incorrect", "invalid", "wrong"]) {
        return f("bad_2fa_code", "The verification code was not accepted.",
            "Request a new code and enter it within a couple of minutes.");
    }
    if has(&t, &["no 2fa code provided", "no 2fa method selected"]) {
        return f("login_cancelled", "Sign-in was cancelled.", "Sign in again when you're ready.");
    }
    if has(&t, &["too many requests"]) || http_status(&t, 429) {
        return f("rate_limited", "Apple is rate-limiting requests from this account.",
            "Wait 10–15 minutes before trying again; repeated retries extend the cooldown.");
    }
    if has(&t, &["provision"]) && (http_status(&t, 401) || http_status(&t, 403) || has(&t, &["unauthorized"])) {
        return f("anisette_rejected", "Apple rejected this anisette server's machine provisioning.",
            "The server isn't working with Apple right now. Pick another anisette server in Settings → Anisette server (or the default), then sign in again.");
    }
    if (has(&t, &["sms 2fa", "trusted phone numbers", "verify/phone"]) && http_status(&t, 403))
        || (has(&t, &["2fa"]) && http_status(&t, 403))
    {
        return f("two_factor_blocked", "Apple refused to send a verification code for this sign-in (HTTP 403).",
            "This usually means Apple doesn't trust the client identity used for sign-in. Wait 15–30 minutes and try again; avoid resetting the machine identity repeatedly. If it persists, try a different anisette server.");
    }
    if has(&t, &["gsservice", "grandslam", "login"]) && http_status(&t, 503) {
        return f("apple_unavailable", "Apple's sign-in service refused the request (HTTP 503).",
            "Apple sometimes answers 503 to clients it considers outdated. Wait a few minutes and retry.");
    }
    if has(&t, &["anisette"]) && has(&t, &["websocket", "connect", "provision", "server", "failed"]) {
        return f("anisette_unavailable", "Couldn't get machine provisioning (anisette) data.",
            "The anisette server may be down. Try again later or pick a different server in Settings → Anisette server.");
    }

    // ---- Developer services ----
    if dev_code == Some(7460) || has(&t, &["maximum number of certificates"]) {
        return f("max_certs", "Your Apple ID has reached its development certificate limit.",
            "Revoke an unused certificate in Account → Certificates. Apps signed with a revoked certificate stop launching until refreshed.");
    }
    if has(&t, &["not enough available app ids", "maximum app id", "app id limit", "you may create up to 10 app ids"])
        || matches!(dev_code, Some(9400..=9402))
    {
        return f("app_id_limit", "No free App IDs left this week.",
            "Free accounts can create 10 App IDs per rolling 7 days, and every app extension needs its own. Set Extensions to \"Strip\" or \"Reuse main\", or wait until older App IDs expire (Account → App IDs shows when).");
    }
    if dev_code == Some(1100) || has(&t, &["session has expired"]) || (has(&t, &["developer", "services", "qh65b2"]) && (http_status(&t, 401) || has(&t, &["unauthorized"]))) {
        return f("session_expired", "Your Apple developer session expired.",
            "Loadside will sign in again automatically; if this repeats, sign out and back in.");
    }
    if has(&t, &["no developer teams available"]) {
        return f("no_team", "This Apple ID has no developer team.",
            "Sign in to developer.apple.com once with this Apple ID and accept the agreement, then retry.");
    }
    if has(&t, &["program license agreement", "agree to the latest"]) {
        return f("agreement_required", "Apple requires you to accept an updated developer agreement.",
            "Sign in to developer.apple.com, accept the agreement, then retry.");
    }

    // ---- Device / installd ----
    if has(&t, &["passwordprotected", "password protected", "device is locked"]) {
        return f("device_locked", "Your device is locked.",
            "Unlock it and keep the screen on until installation finishes.");
    }
    if has(&t, &["pairingdialogresponsepending"]) {
        return f("trust_pending", "Waiting for you to trust this Mac.",
            "Tap \"Trust\" on the device and enter your passcode, then retry.");
    }
    if has(&t, &["userdeniedpairing"]) {
        return f("trust_denied", "The device refused to trust this Mac.",
            "Unplug and reconnect the device, then tap \"Trust\".");
    }
    if has(&t, &["invalidhostid", "pairing file", "pair record", "pairrecord", "nopairingrecord"]) {
        return f("not_paired", "This Mac isn't paired with the device.",
            "Connect over USB, unlock the device and tap \"Trust\". Opening Finder once with the device connected also creates the pairing.");
    }
    if has(&t, &["maximum number of apps for free development profiles", "0xe8008021"]) {
        return f("free_app_limit", "Free Apple IDs can only have 3 sideloaded apps on a device at once.",
            "Delete one (Apps → Remove, or delete it on the device — apps from other sideloading tools count too), then retry.");
    }
    if has(&t, &["valid provisioning profile for this executable was not found", "0xe8008015"]) {
        return f("profile_mismatch", "The device rejected the provisioning profile.",
            "Retry once (a just-registered device can take a moment). If the app has extensions, set Extensions to \"Register all\" or \"Strip\".");
    }
    if has(&t, &["could not be verified", "0xe800801c", "0xe8008018", "applicationverificationfailed"]) {
        return f("verification_failed", "iOS couldn't verify the app's signature.",
            "Usually a damaged or unusual IPA. Try Extensions → \"Strip\", remove injected tweaks, or re-download the IPA. The log has the exact installd reason.");
    }
    if has(&t, &["0xe8000067", "minimumosversion", "requires a newer version of ios"]) {
        return f("ios_too_old", "This app requires a newer iOS version than the device has.",
            "Update the device or use an older version of the app.");
    }
    if has(&t, &["deviceosversiontoolow"]) {
        return f("ios_too_old", "This app requires a newer iOS version than the device has.", "Update the device or use an older build of the app.");
    }
    if has(&t, &["0xe8008001", "deviceisnotsupported", "uisupporteddevices"]) {
        return f("device_unsupported", "The app says it doesn't support this device model.",
            "Turn on \"Allow any device\" (removes UISupportedDevices) and retry.");
    }
    if has(&t, &["no space left", "insufficient storage", "0xe8000079"]) {
        return f("device_full", "Not enough storage on the device.", "Free up space on the device and retry.");
    }
    if has(&t, &["devicenotfound", "device not found", "no device"]) {
        return f("device_not_found", "The device isn't connected.",
            "Connect it with a cable (or enable \"Show this iPhone when on Wi-Fi\" in Finder), unlock it, and retry.");
    }
    if has(&t, &["usbmuxd"]) && has(&t, &["connection refused", "no such file", "connect"]) {
        return f("usbmuxd", "Couldn't talk to the macOS device service (usbmuxd).",
            "Reconnect the device. If it persists, run `sudo killall -9 usbmuxd` (macOS restarts it automatically).");
    }

    // ---- IPA problems ----
    if has(&t, &["fairplay", "still encrypted"]) {
        return f("encrypted_ipa", "This IPA is still FairPlay-encrypted (taken straight from the App Store).",
            "Encrypted apps crash on launch when sideloaded. Use a decrypted IPA.");
    }
    if has(&t, &["no payload directory", "no .app directory", "invalid bundle", "not a valid ipa", "invalid zip", "could not find eocd"]) {
        return f("invalid_ipa", "That file isn't a valid IPA.",
            "Make sure it's an .ipa (a zip with a Payload/*.app folder) and that the download completed.");
    }
    if has(&t, &["not enough space for new load command", "header padding"]) {
        return f("inject_no_space", "The app binary has no room to inject another library.",
            "Inject fewer libraries, or combine tweaks into one dylib.");
    }

    // ---- Network ----
    if has(&t, &["dns error", "failed to lookup address", "network is unreachable", "timed out", "connection reset", "error sending request"]) {
        return f("network", "Network request failed.", "Check your internet connection and retry.");
    }

    let first_line = detail.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let msg = truncate_chars(first_line, 220);
    Failure::new("unknown", if msg.is_empty() { "Something went wrong.".to_string() } else { msg })
        .with_hint("Open Activity → Logs for the full details.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_errors() {
        assert_eq!(classify("Developer error 7460: x", Some(7460), None).code, "max_certs");
        assert_eq!(classify("Not enough available app IDs. 3 are required", None, None).code, "app_id_limit");
        assert_eq!(classify("installation failed: ApplicationVerificationFailed The maximum number of apps for free development profiles has been reached.", None, None).code, "free_app_limit");
        assert_eq!(classify("lockdown: PasswordProtected", None, None).code, "device_locked");
        assert_eq!(classify("Auth error -22406: Your Apple ID or password was incorrect", None, Some(-22406)).code, "bad_credentials");
        assert_eq!(classify("weird", None, None).code, "unknown");
        // The two sign-in failures seen in the wild.
        assert_eq!(classify("Failed to complete SMS 2FA: SMS 2FA request failed with http status 403:", None, None).code, "two_factor_blocked");
        assert_eq!(classify("Failed to provision: Failed to send end provisioning request: HTTP status client error (401 Unauthorized) for url (https://gsa.apple.com/grandslam/MidService/finishMachineProvisioning)", None, None).code, "anisette_rejected");
        // Loose numbers no longer misfire (a UDID containing 401, a path with 7460).
        assert_eq!(classify("device 00008120-000E5401EC3A44201E failed", None, None).code, "unknown");
        // Multi-byte characters at the cut point don't panic.
        let long = "é".repeat(400);
        assert!(classify(&long, None, None).message.chars().count() <= 221);
        // Leading blank lines are skipped.
        assert_eq!(classify("\n\nreal message", None, None).message, "real message");
    }
}
