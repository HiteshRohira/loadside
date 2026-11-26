//! Apple ID passwords live in the macOS login Keychain, never on disk.
//! Keeping the password lets the daemon sign in again unattended for refreshes
//! (Apple usually doesn't ask for 2FA again on an already-trusted machine
//! identity).

use anyhow::Result;

const SERVICE: &str = "dev.loadside.appleid";
const GITHUB_SERVICE: &str = "dev.loadside.github";
const GITHUB_ACCOUNT: &str = "token";
/// Service name for isideload's own data (anisette state, signing key).
pub const STORAGE_SERVICE: &str = "dev.loadside.isideload";

pub fn save_password(email: &str, password: &str) -> Result<()> {
    keyring::Entry::new(SERVICE, email)?.set_password(password)?;
    Ok(())
}

pub fn password(email: &str) -> Result<Option<String>> {
    match keyring::Entry::new(SERVICE, email)?.get_password() {
        Ok(p) => Ok(Some(p)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Whether a password is saved, without reading it. Reading the secret can
/// raise a Keychain approval dialog (and blocks until it's answered); looking
/// up only the item's attributes never does.
pub fn has_password(email: &str) -> bool {
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit};
    ItemSearchOptions::new()
        .class(ItemClass::generic_password())
        .service(SERVICE)
        .account(email)
        .load_attributes(true)
        .limit(Limit::Max(1))
        .search()
        .map(|r| !r.is_empty())
        .unwrap_or(false)
}

pub fn delete_password(email: &str) -> Result<()> {
    match keyring::Entry::new(SERVICE, email)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub fn save_github_token(token: &str) -> Result<()> {
    keyring::Entry::new(GITHUB_SERVICE, GITHUB_ACCOUNT)?.set_password(token)?;
    Ok(())
}

pub fn github_token() -> Result<Option<String>> {
    match keyring::Entry::new(GITHUB_SERVICE, GITHUB_ACCOUNT)?.get_password() {
        Ok(t) => Ok(Some(t)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn has_github_token() -> bool {
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit};
    ItemSearchOptions::new()
        .class(ItemClass::generic_password())
        .service(GITHUB_SERVICE)
        .account(GITHUB_ACCOUNT)
        .load_attributes(true)
        .limit(Limit::Max(1))
        .search()
        .map(|r| !r.is_empty())
        .unwrap_or(false)
}

pub fn delete_github_token() -> Result<()> {
    match keyring::Entry::new(GITHUB_SERVICE, GITHUB_ACCOUNT)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
