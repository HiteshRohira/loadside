//! Read the interesting bits out of an `embedded.mobileprovision` (a CMS
//! envelope around an XML plist) without a full CMS parser.

use std::path::Path;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct ProfileInfo {
    pub name: Option<String>,
    pub team_name: Option<String>,
    pub expires_at: Option<i64>,
    pub created_at: Option<i64>,
    pub provisioned_devices: Vec<String>,
}

pub fn parse(bytes: &[u8]) -> Result<ProfileInfo> {
    let start = find(bytes, b"<?xml").context("no plist in profile")?;
    let end = find(&bytes[start..], b"</plist>").context("unterminated plist in profile")? + start + 8;
    let dict: plist::Dictionary = plist::from_bytes(&bytes[start..end])?;
    let date = |k: &str| {
        dict.get(k).and_then(|v| v.as_date()).map(|d| {
            std::time::SystemTime::from(d)
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        })
    };
    Ok(ProfileInfo {
        name: dict.get("Name").and_then(|v| v.as_string()).map(str::to_string),
        team_name: dict.get("TeamName").and_then(|v| v.as_string()).map(str::to_string),
        expires_at: date("ExpirationDate"),
        created_at: date("CreationDate"),
        provisioned_devices: dict
            .get("ProvisionedDevices")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_string().map(str::to_string)).collect())
            .unwrap_or_default(),
    })
}

pub fn parse_file(path: &Path) -> Result<ProfileInfo> {
    parse(&std::fs::read(path).with_context(|| format!("reading {}", path.display()))?)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
