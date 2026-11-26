//! GitHub Releases client: parse repo URLs/slugs, check for new `.ipa` releases
//! using conditional requests (ETag), pin numeric repository IDs, compare versions,
//! and stream downloads safely to disk with strict host/redirect checks and
//! SHA-256 / size verification.

use std::cmp::Ordering;
use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::errors::truncate_chars;
use crate::protocol::Failure;
use crate::state::{ReleaseAsset, ReleaseInfo};

const API_BASE: &str = "https://api.github.com";
const MAX_API_BYTES: usize = 8 * 1024 * 1024;
const MAX_IPA_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_NOTES_CHARS: usize = 6000;
const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone)]
pub enum CheckOutcome {
    /// HTTP 304 Not Modified: nothing changed on GitHub (doesn't consume rate limit).
    NotModified,
    /// Latest qualifying release with a matching `.ipa` asset.
    Found {
        release: ReleaseInfo,
        all_ipa_assets: Vec<ReleaseAsset>,
        etag: Option<String>,
    },
}

/// Normalize a user-provided GitHub repo reference or full URL into `"owner/repo"`.
///
/// Accepts:
/// - `https://github.com/owner/repo`
/// - `https://github.com/owner/repo.git`
/// - `https://github.com/owner/repo/releases`
/// - `https://github.com/owner/repo/releases/tag/v1.0`
/// - `https://github.com/owner/repo?tab=readme-ov-file#top`
/// - `github.com/owner/repo`
/// - `git@github.com:owner/repo.git`
/// - `owner/repo`
pub fn parse_repo(input: &str) -> Result<String, Failure> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(Failure::new("invalid_repo", "Enter a GitHub repository URL or owner/repo."));
    }

    let path = if let Some(rest) = raw.strip_prefix("git@github.com:") {
        rest.to_string()
    } else if raw.contains("://") {
        let u = reqwest::Url::parse(raw)
            .map_err(|_| Failure::new("invalid_repo", "That doesn't look like a valid GitHub repository URL."))?;
        if !matches!(u.scheme(), "https" | "http") {
            return Err(Failure::new("invalid_repo", "GitHub repository URLs must use https://."));
        }
        let host = u.host_str().unwrap_or("").to_ascii_lowercase();
        if host != "github.com" && host != "www.github.com" {
            return Err(Failure::new("invalid_repo", "Only github.com repositories are supported."));
        }
        u.path().trim_start_matches('/').to_string()
    } else {
        let lower = raw.to_ascii_lowercase();
        let stripped = if lower.starts_with("github.com/") {
            &raw["github.com/".len()..]
        } else if lower.starts_with("www.github.com/") {
            &raw["www.github.com/".len()..]
        } else {
            raw.trim_start_matches('/')
        };
        // Strip any query string or fragment on scheme-less URLs.
        stripped.split(['?', '#']).next().unwrap_or(stripped).to_string()
    };

    let mut parts = path.split('/').filter(|s| !s.is_empty());
    let owner = parts.next().unwrap_or("");
    let repo_raw = parts.next().unwrap_or("");
    let repo = repo_raw.strip_suffix(".git").unwrap_or(repo_raw);

    if !valid_owner(owner) || !valid_repo_name(repo) {
        return Err(Failure::new("invalid_repo", "Enter a GitHub repository URL or owner/repo.")
            .with_hint("Example: https://github.com/NuvioMedia/NuvioMobile or NuvioMedia/NuvioMobile"));
    }

    Ok(format!("{owner}/{repo}"))
}

fn valid_owner(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 39
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn valid_repo_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s != "."
        && s != ".."
        && !s.contains("..")
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

// ---------------------------------------------------------------------------
// Strict HTTPS + host / redirect safety
// ---------------------------------------------------------------------------

fn is_disallowed_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000) == 64) // CGNAT 100.64.0.0/10
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        }
    }
}

fn is_safe_https_host(u: &reqwest::Url) -> bool {
    if u.scheme() != "https" || !u.username().is_empty() || u.password().is_some() {
        return false;
    }
    match u.host() {
        Some(url::Host::Domain(d)) => {
            let l = d.to_ascii_lowercase();
            !l.is_empty() && l != "localhost" && !l.ends_with(".localhost") && !l.ends_with(".local")
        }
        Some(url::Host::Ipv4(ip)) => !is_disallowed_ip(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => !is_disallowed_ip(IpAddr::V6(ip)),
        None => false,
    }
}

/// Strictly `https://api.github.com/...` — the only host that may ever receive a GitHub token.
pub fn is_github_api_url(s: &str) -> bool {
    reqwest::Url::parse(s).is_ok_and(|u| {
        is_safe_https_host(&u)
            && matches!(u.host(), Some(url::Host::Domain(ref d)) if d.eq_ignore_ascii_case("api.github.com"))
    })
}

/// HTTPS URL on GitHub's own release / object-storage hosts.
pub fn is_github_web_or_asset_url(s: &str) -> bool {
    reqwest::Url::parse(s).is_ok_and(|u| {
        if !is_safe_https_host(&u) {
            return false;
        }
        match u.host() {
            Some(url::Host::Domain(d)) => {
                let l = d.to_ascii_lowercase();
                l == "github.com"
                    || l == "api.github.com"
                    || l.ends_with(".github.com")
                    || l == "githubusercontent.com"
                    || l.ends_with(".githubusercontent.com")
            }
            _ => false,
        }
    })
}

/// Redirect policy that validates every hop: HTTPS only, no private/loopback IPs,
/// max 10 redirects.
fn safe_redirect_policy(require_github_host: bool) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }
        let u = attempt.url();
        if !is_safe_https_host(u) {
            return attempt.error("refused redirect to non-HTTPS or local/private address");
        }
        if require_github_host && !is_github_web_or_asset_url(u.as_str()) {
            return attempt.error("refused redirect away from GitHub");
        }
        attempt.follow()
    })
}

fn api_client() -> Result<reqwest::Client, Failure> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(25))
        .redirect(safe_redirect_policy(true))
        .user_agent(concat!("Loadside/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| Failure::new("network", format!("Couldn't initialize HTTP client: {e}")))
}

fn download_client(follow_redirects: bool) -> Result<reqwest::Client, Failure> {
    let policy = if follow_redirects {
        // GitHub redirects release downloads to GitHub/Azure/S3 CDN URLs over HTTPS;
        // enforce HTTPS + no private/loopback IP literals on every hop.
        safe_redirect_policy(false)
    } else {
        reqwest::redirect::Policy::none()
    };
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .redirect(policy)
        .user_agent(concat!("Loadside/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| Failure::new("network", format!("Couldn't initialize HTTP client: {e}")))
}

// ---------------------------------------------------------------------------
// GitHub JSON models
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GhRepo {
    id: u64,
    full_name: String,
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    id: u64,
    tag_name: String,
    name: Option<String>,
    html_url: String,
    draft: bool,
    prerelease: bool,
    published_at: Option<String>,
    body: Option<String>,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    id: u64,
    name: String,
    url: String,
    browser_download_url: String,
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

fn parse_rfc3339(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.timestamp())
}

fn to_release_asset(a: &GhAsset) -> Option<ReleaseAsset> {
    if !a.name.to_ascii_lowercase().ends_with(".ipa") {
        return None;
    }
    if !crate::ipa::valid_file_name(&a.name) {
        return None;
    }
    if !is_github_web_or_asset_url(&a.browser_download_url) || !is_github_api_url(&a.url) {
        return None;
    }
    Some(ReleaseAsset {
        id: a.id,
        name: a.name.clone(),
        download_url: a.browser_download_url.clone(),
        api_url: a.url.clone(),
        size: a.size,
        digest: a.digest.clone(),
    })
}

fn to_release_info(repo: &str, rel: &GhRelease, chosen: ReleaseAsset) -> ReleaseInfo {
    let notes = rel
        .body
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(|b| truncate_chars(b, MAX_NOTES_CHARS));
    ReleaseInfo {
        release_id: rel.id,
        tag: rel.tag_name.clone(),
        name: rel.name.clone().filter(|n| !n.trim().is_empty()),
        html_url: if is_github_web_or_asset_url(&rel.html_url) {
            rel.html_url.clone()
        } else {
            format!("https://github.com/{repo}/releases")
        },
        published_at: rel.published_at.as_deref().and_then(parse_rfc3339),
        body: notes,
        prerelease: rel.prerelease,
        asset: chosen,
    }
}

/// Fetch the repository's canonical numeric ID and full name (to guard against
/// repo/account rename takeovers).
pub async fn fetch_repo_identity(repo: &str, token: Option<&str>) -> Result<(u64, String), Failure> {
    let repo = parse_repo(repo)?;
    let url = format!("{API_BASE}/repos/{repo}");
    let client = api_client()?;
    let mut req = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(tok) = token.map(str::trim).filter(|s| !s.is_empty()) {
        req = req.bearer_auth(tok);
    }
    let mut resp = req.send().await.map_err(|e| map_network_error("checking GitHub repository", &e))?;
    if !resp.status().is_success() {
        return Err(map_github_status(resp.status(), resp.headers(), &repo, token.is_some()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| map_network_error("reading GitHub repository info", &e))?
    {
        if body.len() + chunk.len() > MAX_API_BYTES {
            return Err(Failure::new("github_response_too_large", "GitHub returned an unexpectedly large response."));
        }
        body.extend_from_slice(&chunk);
    }
    let info: GhRepo = serde_json::from_slice(&body)
        .map_err(|e| Failure::new("github_parse", format!("Couldn't read GitHub repository details: {e}")))?;
    Ok((info.id, info.full_name))
}

// ---------------------------------------------------------------------------
// Asset matching & pattern inference
// ---------------------------------------------------------------------------

/// Match an asset name against a user-supplied pattern.
/// If the pattern contains `*`, matches as a case-insensitive wildcard glob;
/// otherwise matches as a case-insensitive substring.
pub fn asset_matches(name: &str, pattern: &str) -> bool {
    let pat = pattern.trim();
    if pat.is_empty() {
        return true;
    }
    let name_l = name.to_ascii_lowercase();
    let pat_l = pat.to_ascii_lowercase();
    if !pat_l.contains('*') {
        return name_l.contains(&pat_l);
    }
    wildcard_match(&name_l, &pat_l)
}

fn wildcard_match(text: &str, pat: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pat.chars().collect();
    let (mut ti, mut pi) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && p[pi] == t[ti] {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_p = Some(pi);
            pi += 1;
            star_t = ti;
        } else if let Some(sp) = star_p {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn looks_like_simulator_or_debug(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    l.contains("simulator")
        || l.contains("-sim.")
        || l.contains("_sim.")
        || l.contains("-sim-")
        || l.contains("_sim_")
        || l.contains("-debug")
        || l.contains("_debug")
        || l.contains(".debug.")
}

/// Pick the best matching `.ipa` asset from a release's `.ipa` assets.
pub fn select_asset<'a>(assets: &'a [ReleaseAsset], pattern: Option<&str>) -> Result<&'a ReleaseAsset, Failure> {
    if assets.is_empty() {
        return Err(Failure::new("no_ipa_asset", "This release has no .ipa file attached."));
    }
    let pat = pattern.map(str::trim).filter(|s| !s.is_empty());
    if let Some(p) = pat {
        let matched: Vec<&ReleaseAsset> = assets.iter().filter(|a| asset_matches(&a.name, p)).collect();
        if let Some(exact) = matched.iter().find(|a| a.name.eq_ignore_ascii_case(p)) {
            return Ok(exact);
        }
        if matched.len() == 1 {
            return Ok(matched[0]);
        }
        if matched.len() > 1 {
            // Multiple assets match the pattern; prefer non-simulator/debug if unambiguous,
            // otherwise surface an ambiguity error so the user can tighten the filter.
            let non_sim: Vec<&ReleaseAsset> = matched.iter().copied().filter(|a| !looks_like_simulator_or_debug(&a.name)).collect();
            if non_sim.len() == 1 {
                return Ok(non_sim[0]);
            }
            let names = matched.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ");
            return Err(Failure::new("asset_ambiguous", format!("More than one .ipa asset matched \"{p}\"."))
                .with_hint(format!("Tighten the IPA filter in app details. Matched: {names}")));
        }
        let names = assets.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ");
        return Err(Failure::new("asset_not_found", format!("No .ipa asset matched \"{p}\"."))
            .with_hint(format!("Available .ipa files: {names}")));
    }
    if assets.len() == 1 {
        return Ok(&assets[0]);
    }
    // Multiple IPAs and no filter yet: prefer a device release build over simulator/debug.
    // Note: never penalize "unsigned", since sideloaded IPAs are very often named *-unsigned.ipa.
    let preferred = assets.iter().find(|a| !looks_like_simulator_or_debug(&a.name));
    Ok(preferred.unwrap_or(&assets[0]))
}

/// Derive a resilient pattern when a user picks one `.ipa` out of several
/// (e.g. `Novio-1.4.0-arm64.ipa` -> `Novio-*-arm64.ipa`, `App.1.4.0.arm64.ipa` -> `App.*.arm64.ipa`)
/// so future releases with bumped version numbers still match the same variant.
pub fn infer_asset_pattern(chosen_name: &str, all_assets: &[ReleaseAsset]) -> Option<String> {
    if all_assets.len() <= 1 {
        return None;
    }
    let generalized = generalize_version_tokens(chosen_name);
    if generalized != chosen_name {
        let matches: Vec<&ReleaseAsset> = all_assets.iter().filter(|a| asset_matches(&a.name, &generalized)).collect();
        if matches.len() == 1 && matches[0].name == chosen_name {
            return Some(generalized);
        }
        // If the only other matches are simulator/debug builds while the chosen file isn't,
        // `select_asset` disambiguates cleanly so the generalized pattern still works across upgrades!
        if !looks_like_simulator_or_debug(chosen_name) {
            let non_sim: Vec<&ReleaseAsset> = matches
                .iter()
                .copied()
                .filter(|a| !looks_like_simulator_or_debug(&a.name))
                .collect();
            if non_sim.len() == 1 && non_sim[0].name == chosen_name {
                return Some(generalized);
            }
        }
    }
    Some(chosen_name.to_string())
}

fn generalize_version_tokens(name: &str) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(idx) if name[idx..].eq_ignore_ascii_case(".ipa") => (&name[..idx], &name[idx..]),
        _ => (name, ""),
    };
    let chars: Vec<char> = stem.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let at_boundary = i == 0 || matches!(chars[i - 1], '-' | '_' | '.' | ' ');
        if at_boundary {
            let start = i;
            let mut j = i;
            if j < chars.len() && matches!(chars[j], 'v' | 'V') && j + 1 < chars.len() && chars[j + 1].is_ascii_digit() {
                j += 1;
            }
            let num_start = j;
            let mut has_dot = false;
            let mut last_digit_end = j;
            while j < chars.len() && (chars[j].is_ascii_digit() || chars[j] == '.') {
                if chars[j] == '.' {
                    // Stop before a trailing dot followed by non-digits (e.g. App.1.4.0.arm64).
                    if j + 1 < chars.len() && chars[j + 1].is_ascii_digit() {
                        has_dot = true;
                        j += 1;
                    } else {
                        break;
                    }
                } else {
                    j += 1;
                    last_digit_end = j;
                }
            }
            // Also absorb optional prerelease/build suffix attached to the version (e.g. -beta.3, +123).
            if last_digit_end > num_start && has_dot {
                let mut k = last_digit_end;
                if k < chars.len() && chars[k] == '-' {
                    let rest: String = chars[k + 1..].iter().collect();
                    let lower = rest.to_ascii_lowercase();
                    let is_pre = ["alpha", "beta", "rc", "pre", "dev", "nightly", "canary", "preview"]
                        .iter()
                        .any(|w| lower.starts_with(w));
                    if is_pre {
                        k += 1;
                        // Consume the prerelease label (e.g. beta, rc10), plus an optional
                        // numeric dot-segment like `.3`, stopping before `.arm64` etc.
                        while k < chars.len() && chars[k].is_ascii_alphanumeric() {
                            k += 1;
                        }
                        if k + 1 < chars.len() && chars[k] == '.' && chars[k + 1].is_ascii_digit() {
                            k += 1;
                            while k < chars.len() && chars[k].is_ascii_digit() {
                                k += 1;
                            }
                        }
                        last_digit_end = k;
                    }
                }
                if last_digit_end < chars.len() && chars[last_digit_end] == '+' {
                    let mut k = last_digit_end + 1;
                    while k < chars.len() && (chars[k].is_ascii_alphanumeric() || chars[k] == '.') {
                        k += 1;
                    }
                    last_digit_end = k;
                }
                let end_boundary = last_digit_end == chars.len()
                    || matches!(chars[last_digit_end], '-' | '_' | '.' | ' ');
                if end_boundary {
                    if !out.ends_with('*') {
                        out.push('*');
                    }
                    i = last_digit_end;
                    continue;
                }
            }
            out.push(chars[start]);
            i = start + 1;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out.push_str(ext);
    out
}

// ---------------------------------------------------------------------------
// Release listing & pinned lookup
// ---------------------------------------------------------------------------

/// Fetch the latest qualifying release for `repo` that contains a matching `.ipa` asset.
pub async fn fetch_latest_release(
    repo: &str,
    include_prereleases: bool,
    asset_pattern: Option<&str>,
    etag: Option<&str>,
    token: Option<&str>,
) -> Result<CheckOutcome, Failure> {
    let repo = parse_repo(repo)?;
    let url = format!("{API_BASE}/repos/{repo}/releases?per_page=20");
    let client = api_client()?;
    let mut req = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(tag) = etag.filter(|s| !s.is_empty()) {
        req = req.header("If-None-Match", tag);
    }
    if let Some(tok) = token.map(str::trim).filter(|s| !s.is_empty()) {
        req = req.bearer_auth(tok);
    }

    let mut resp = req.send().await.map_err(|e| map_network_error("checking GitHub releases", &e))?;
    let status = resp.status();

    if status == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(CheckOutcome::NotModified);
    }
    if !status.is_success() {
        return Err(map_github_status(status, resp.headers(), &repo, token.is_some()));
    }

    let new_etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| map_network_error("reading GitHub release list", &e))?
    {
        if body.len() + chunk.len() > MAX_API_BYTES {
            return Err(Failure::new("github_response_too_large", "GitHub returned an unexpectedly large response."));
        }
        body.extend_from_slice(&chunk);
    }

    let releases: Vec<GhRelease> = serde_json::from_slice(&body)
        .map_err(|e| Failure::new("github_parse", format!("Couldn't read GitHub's release list: {e}")))?;

    let mut qualifying: Vec<(GhRelease, Vec<ReleaseAsset>)> = Vec::new();
    let mut saw_qualifying = false;

    for rel in releases {
        if rel.draft {
            continue;
        }
        if rel.prerelease && !include_prereleases {
            continue;
        }
        saw_qualifying = true;
        let ipa_assets: Vec<ReleaseAsset> = rel.assets.iter().filter_map(to_release_asset).collect();
        if ipa_assets.is_empty() {
            continue;
        }
        qualifying.push((rel, ipa_assets));
    }

    if qualifying.is_empty() {
        if !saw_qualifying {
            return Err(Failure::new(
                "no_releases",
                if include_prereleases {
                    format!("No published releases were found for {repo}.")
                } else {
                    format!("No stable releases were found for {repo}.")
                },
            )
            .with_hint(if include_prereleases {
                "Check that the repository has at least one published release with an .ipa asset."
            } else {
                "If this repository only publishes prereleases, turn on \"Include prereleases\"."
            }));
        }
        return Err(Failure::new("no_ipa_asset", format!("No .ipa file was found in the releases for {repo}."))
            .with_hint("Loadside tracks GitHub releases that attach an .ipa asset."));
    }

    // Pick the highest semantic version among qualifying releases with .ipa assets
    // (so a backport release published after a newer major version doesn't hide the
    // newer version). When tags don't parse as versions, preserve GitHub's recency order.
    let best_idx = pick_highest_release_index(&qualifying);
    let (best_rel, best_assets) = &qualifying[best_idx];

    // If the newest/highest qualifying release has .ipa files but none match the
    // configured asset filter, return that error directly rather than silently
    // falling back to an older release and claiming "up to date".
    let chosen = select_asset(best_assets, asset_pattern)?.clone();
    let info = to_release_info(&repo, best_rel, chosen);
    Ok(CheckOutcome::Found {
        release: info,
        all_ipa_assets: best_assets.clone(),
        etag: new_etag,
    })
}

fn pick_highest_release_index(qualifying: &[(GhRelease, Vec<ReleaseAsset>)]) -> usize {
    let mut best_idx = 0usize;
    let mut best_ver = parse_version(&qualifying[0].0.tag_name, qualifying[0].0.prerelease);
    for (idx, (rel, _)) in qualifying.iter().enumerate().skip(1) {
        let cand_ver = parse_version(&rel.tag_name, rel.prerelease);
        if let (Some(bv), Some(cv)) = (&best_ver, &cand_ver) {
            if cv > bv {
                best_idx = idx;
                best_ver = cand_ver;
            }
        }
    }
    best_idx
}

/// Fetch one exact GitHub release by ID and select one exact asset by ID, guaranteeing
/// that what the user previewed and trusted is exactly what gets downloaded.
pub async fn fetch_pinned_release(
    repo: &str,
    release_id: u64,
    asset_id: u64,
    token: Option<&str>,
) -> Result<(ReleaseInfo, Vec<ReleaseAsset>), Failure> {
    let repo = parse_repo(repo)?;
    let url = format!("{API_BASE}/repos/{repo}/releases/{release_id}");
    let client = api_client()?;
    let mut req = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(tok) = token.map(str::trim).filter(|s| !s.is_empty()) {
        req = req.bearer_auth(tok);
    }
    let mut resp = req.send().await.map_err(|e| map_network_error("fetching the selected GitHub release", &e))?;
    if !resp.status().is_success() {
        return Err(map_github_status(resp.status(), resp.headers(), &repo, token.is_some()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| map_network_error("reading the selected GitHub release", &e))?
    {
        if body.len() + chunk.len() > MAX_API_BYTES {
            return Err(Failure::new("github_response_too_large", "GitHub returned an unexpectedly large response."));
        }
        body.extend_from_slice(&chunk);
    }
    let rel: GhRelease = serde_json::from_slice(&body)
        .map_err(|e| Failure::new("github_parse", format!("Couldn't read GitHub release: {e}")))?;
    let ipa_assets: Vec<ReleaseAsset> = rel.assets.iter().filter_map(to_release_asset).collect();
    let chosen = ipa_assets
        .iter()
        .find(|a| a.id == asset_id)
        .cloned()
        .ok_or_else(|| {
            Failure::new("asset_not_found", "The selected .ipa asset is no longer attached to that release.")
                .with_hint("Look up the repository again to refresh the release list.")
        })?;
    Ok((to_release_info(&repo, &rel, chosen), ipa_assets))
}

// ---------------------------------------------------------------------------
// Safe streaming download
// ---------------------------------------------------------------------------

/// Stream a GitHub release `.ipa` asset safely to `dest`, reporting `(downloaded, total)` bytes.
///
/// - Restricts bearer-authenticated requests strictly to `api.github.com`.
/// - Enforces HTTPS and rejects private/loopback IPs on every redirect hop.
/// - Re-validates the asset filename and enforces a total transfer deadline.
/// - Uses async file writes (never blocks tokio worker threads).
/// - Verifies SHA-256 checksum when provided (case-insensitive prefix) or exact size otherwise.
pub async fn download_asset(
    asset: &ReleaseAsset,
    dest: &Path,
    token: Option<&str>,
    on_progress: impl Fn(u64, u64) + Send + Sync,
) -> Result<(), Failure> {
    if !crate::ipa::valid_file_name(&asset.name) {
        return Err(Failure::new("invalid_asset", "GitHub release asset has an unsafe filename."));
    }
    if asset.size > MAX_IPA_BYTES {
        return Err(Failure::new("asset_too_large", "This .ipa file exceeds Loadside's 4 GB download limit."));
    }
    let tok = token.map(str::trim).filter(|s| !s.is_empty());

    // Resolve the actual download URL without ever forwarding our GitHub token to
    // any host other than api.github.com.
    let mut resp = if let Some(tok) = tok {
        if !is_github_api_url(&asset.api_url) {
            return Err(Failure::new("invalid_url", "Refused to send GitHub token to a non-GitHub API URL."));
        }
        let no_redir = download_client(false)?;
        let initial = no_redir
            .get(&asset.api_url)
            .header("Accept", "application/octet-stream")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(tok)
            .send()
            .await
            .map_err(|e| map_network_error("requesting the release download", &e))?;

        let status = initial.status();
        if status.is_redirection() {
            let loc = initial
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| Failure::new("github_download", "GitHub returned a redirect with no download location."))?;
            let loc_url = reqwest::Url::parse(loc)
                .map_err(|_| Failure::new("invalid_url", "GitHub returned an invalid download redirect URL."))?;
            if !is_safe_https_host(&loc_url) {
                return Err(Failure::new("invalid_url", "Refused to follow a non-HTTPS or local download redirect."));
            }
            let dl = download_client(true)?;
            dl.get(loc_url)
                .send()
                .await
                .map_err(|e| map_network_error("downloading the release .ipa", &e))?
        } else if status.is_success() {
            initial
        } else {
            return Err(map_github_status(status, initial.headers(), &asset.name, true));
        }
    } else {
        if !is_github_web_or_asset_url(&asset.download_url) {
            return Err(Failure::new("invalid_url", "GitHub asset URL isn't a valid GitHub HTTPS URL."));
        }
        let dl = download_client(true)?;
        dl.get(&asset.download_url)
            .send()
            .await
            .map_err(|e| map_network_error("downloading the release .ipa", &e))?
    };

    if !resp.status().is_success() {
        return Err(map_github_status(resp.status(), resp.headers(), &asset.name, tok.is_some()));
    }
    if !is_safe_https_host(resp.url()) {
        return Err(Failure::new("invalid_url", "Refused to download over an unencrypted or local connection."));
    }

    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| Failure::new("io", format!("Couldn't prepare download folder: {e}")))?;
    }
    let partial = dest.with_extension(format!("partial-{}", uuid::Uuid::new_v4().simple()));
    let mut file = tokio::fs::File::create(&partial)
        .await
        .map_err(|e| Failure::new("io", format!("Couldn't create download file: {e}")))?;

    let total = resp.content_length().unwrap_or(asset.size);
    let mut downloaded: u64 = 0;
    let mut hasher = Sha256::new();
    let expected_sha256 = asset
        .digest
        .as_deref()
        .map(str::trim)
        .and_then(|d| {
            let lower = d.to_ascii_lowercase();
            lower.strip_prefix("sha256:").map(|h| h.trim().to_string())
        })
        .filter(|h| !h.is_empty());

    let stream_fut = async {
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| map_network_error("downloading the release .ipa", &e))?
        {
            downloaded = downloaded.saturating_add(chunk.len() as u64);
            if downloaded > MAX_IPA_BYTES {
                return Err(Failure::new("asset_too_large", "The download exceeded Loadside's 4 GB safety limit."));
            }
            if expected_sha256.is_some() {
                hasher.update(&chunk);
            }
            file.write_all(&chunk)
                .await
                .map_err(|e| Failure::new("io", format!("Couldn't write downloaded .ipa: {e}")))?;
            on_progress(downloaded, total);
        }
        file.flush()
            .await
            .map_err(|e| Failure::new("io", format!("Couldn't finish writing downloaded .ipa: {e}")))?;
        Ok(())
    };

    let stream_res = match tokio::time::timeout(DOWNLOAD_DEADLINE, stream_fut).await {
        Ok(r) => r,
        Err(_) => Err(Failure::new("network", "Downloading the release .ipa timed out.")
            .with_hint("Check your internet connection and try again.")),
    };

    drop(file);
    if let Err(e) = stream_res {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(e);
    }

    if let Some(expected) = expected_sha256 {
        let actual = hex::encode(hasher.finalize());
        if actual != expected {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(Failure::new("checksum_mismatch", "The downloaded .ipa didn't match GitHub's checksum.")
                .with_hint("Try again; the download may have been corrupted in transit."));
        }
    } else if asset.size > 0 && downloaded != asset.size {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(Failure::new("download_incomplete", "The downloaded .ipa was incomplete.")
            .with_hint("The transfer ended early. Try again."));
    }

    tokio::fs::rename(&partial, dest).await.map_err(|e| {
        let p = partial.clone();
        tokio::spawn(async move {
            let _ = tokio::fs::remove_file(&p).await;
        });
        Failure::new("io", format!("Couldn't save downloaded .ipa: {e}"))
    })?;
    Ok(())
}

fn map_network_error(action: &str, e: &reqwest::Error) -> Failure {
    if e.is_timeout() {
        Failure::new("network", format!("Timed out while {action}."))
            .with_hint("Check your internet connection and try again.")
    } else {
        Failure::new("network", format!("Network error while {action}."))
            .with_hint("Check your internet connection and try again.")
    }
}

fn map_github_status(status: reqwest::StatusCode, headers: &reqwest::header::HeaderMap, target: &str, has_token: bool) -> Failure {
    let remaining = headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok());
    let retry_after = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());
    let reset = headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());

    let is_rate_limit = status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || (status == reqwest::StatusCode::FORBIDDEN && (remaining == Some("0") || retry_after.is_some()));

    if is_rate_limit {
        let wait_msg = if let Some(secs) = retry_after {
            let mins = ((secs.max(60) + 59) / 60).max(1);
            format!("Wait about {mins} minute{}. ", if mins == 1 { "" } else { "s" })
        } else if let Some(r) = reset {
            let mins = ((r - crate::now()).max(60) + 59) / 60;
            format!("Resets in about {mins} minute{}. ", if mins == 1 { "" } else { "s" })
        } else {
            String::new()
        };
        let hint = if has_token {
            format!("{wait_msg}Wait a bit and try again.")
        } else {
            format!("{wait_msg}Add a GitHub personal access token in Settings to raise the limit from 60 to 5,000 requests/hour.")
        };
        return Failure::new("github_rate_limit", "GitHub's API rate limit was reached.").with_hint(hint);
    }

    match status.as_u16() {
        401 => Failure::new("github_auth", "GitHub rejected your personal access token.")
            .with_hint("Update or remove your GitHub token in Settings."),
        403 => Failure::new("github_forbidden", format!("GitHub refused access to {target} (HTTP 403)."))
            .with_hint(if has_token {
                "Check that your token has permission to read this repository's releases."
            } else {
                "If this is a private repository, add a GitHub personal access token in Settings."
            }),
        404 => Failure::new("github_not_found", format!("Couldn't find {target} on GitHub."))
            .with_hint(if has_token {
                "Check the owner/repo spelling and that your token has access to it."
            } else {
                "Check the owner/repo spelling. For private repositories, add a GitHub token in Settings."
            }),
        code => Failure::new("github_http", format!("GitHub returned HTTP {code} for {target}."))
            .with_hint("GitHub may be having a temporary hiccup. Try again in a moment."),
    }
}

// ---------------------------------------------------------------------------
// Version comparison
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct ParsedVersion {
    nums: Vec<u64>,
    pre: Vec<PrePart>,
}

#[derive(Debug, PartialEq, Eq)]
enum PrePart {
    Num(u64),
    Text(String),
}

impl Ord for PrePart {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (PrePart::Num(a), PrePart::Num(b)) => a.cmp(b),
            (PrePart::Num(_), PrePart::Text(_)) => Ordering::Less,
            (PrePart::Text(_), PrePart::Num(_)) => Ordering::Greater,
            (PrePart::Text(a), PrePart::Text(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for PrePart {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ParsedVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        let len = self.nums.len().max(other.nums.len());
        for i in 0..len {
            let a = self.nums.get(i).copied().unwrap_or(0);
            let b = other.nums.get(i).copied().unwrap_or(0);
            match a.cmp(&b) {
                Ordering::Equal => continue,
                ord => return ord,
            }
        }
        // Per SemVer: a normal version has higher precedence than a prerelease.
        match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => self.pre.cmp(&other.pre),
        }
    }
}

impl PartialOrd for ParsedVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn tokenize_pre(s: &str) -> Vec<PrePart> {
    let mut out = Vec::new();
    for seg in s.split(['.', '-', '_']).filter(|t| !t.is_empty()) {
        // Split transitions between letters and digits so "rc10" > "rc2".
        let mut cur = String::new();
        let mut cur_digit: Option<bool> = None;
        for ch in seg.chars() {
            let is_d = ch.is_ascii_digit();
            if cur_digit.is_some_and(|d| d != is_d) {
                push_pre_token(&mut out, &cur);
                cur.clear();
            }
            cur.push(ch);
            cur_digit = Some(is_d);
        }
        if !cur.is_empty() {
            push_pre_token(&mut out, &cur);
        }
    }
    out
}

fn push_pre_token(out: &mut Vec<PrePart>, tok: &str) {
    if let Ok(n) = tok.parse::<u64>() {
        out.push(PrePart::Num(n));
    } else {
        out.push(PrePart::Text(tok.to_ascii_lowercase()));
    }
}

fn looks_like_prerelease_suffix(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    ["alpha", "beta", "rc", "pre", "dev", "nightly", "canary", "preview", "snapshot", "b", "a"]
        .iter()
        .any(|w| {
            l == *w
                || l.starts_with(&format!("{w}."))
                || l.starts_with(&format!("{w}-"))
                || (w.len() >= 2 && l.starts_with(w) && l[w.len()..].chars().next().is_some_and(|c| c.is_ascii_digit()))
        })
}

/// Extract a numeric/semver version from strings like `v1.2.3`, `1.2`,
/// `release-2.0.1-beta.2`, `ios2-v1.10.0`, `Novio-1.4.0+build5`.
fn parse_version(raw: &str, is_prerelease: bool) -> Option<ParsedVersion> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // Strip build metadata (+...).
    let s = s.split('+').next().unwrap_or(s);

    // Find the start of the dotted version (preferring a dotted segment like v1.2.3
    // or 1.10.0 over an earlier prefix digit like "ios2-v1.10.0").
    let candidate = find_version_start(s)?;
    let (core, pre_str) = match candidate.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (candidate, None),
    };
    let mut nums = Vec::new();
    for part in core.split('.') {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        nums.push(part.parse::<u64>().ok()?);
    }
    if nums.is_empty() {
        return None;
    }
    let pre = match pre_str {
        Some(p) if !p.is_empty() && (is_prerelease || looks_like_prerelease_suffix(p)) => tokenize_pre(p),
        _ => Vec::new(),
    };
    Some(ParsedVersion { nums, pre })
}

fn find_version_start(s: &str) -> Option<&str> {
    // Direct match at start (optionally preceded by v/V).
    let direct = s.strip_prefix(['v', 'V']).unwrap_or(s);
    if direct.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return Some(direct);
    }
    // Otherwise look after '-', '_', '/' separators for the first segment starting
    // with digits (or 'v'+digits), preferring one containing a dot.
    let mut fallback = None;
    for (idx, ch) in s.char_indices() {
        if matches!(ch, '-' | '_' | '/' | ' ') {
            let rest = &s[idx + ch.len_utf8()..];
            let trimmed = rest.strip_prefix(['v', 'V']).unwrap_or(rest);
            if trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                let core = trimmed.split('-').next().unwrap_or(trimmed);
                if core.contains('.') {
                    return Some(trimmed);
                }
                if fallback.is_none() {
                    fallback = Some(trimmed);
                }
            }
        }
    }
    fallback
}

pub fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    Some(parse_version(a, false)?.cmp(&parse_version(b, false)?))
}

/// Decide whether `candidate` represents a newer release than what is currently installed.
pub fn is_newer_release(
    installed_release_id: Option<u64>,
    installed_tag: Option<&str>,
    installed_version: Option<&str>,
    candidate: &ReleaseInfo,
) -> bool {
    if installed_release_id == Some(candidate.release_id) {
        return false;
    }
    let cand_ver = parse_version(&candidate.tag, candidate.prerelease);
    if let Some(tag) = installed_tag.map(str::trim).filter(|s| !s.is_empty()) {
        let inst_ver = parse_version(tag, false);
        if let (Some(cv), Some(iv)) = (&cand_ver, &inst_ver) {
            return cv > iv;
        }
        // Non-numeric / rolling tags (e.g. "nightly"): a different release_id is newer.
        if installed_release_id.is_some() {
            return true;
        }
        return !tag.eq_ignore_ascii_case(candidate.tag.trim());
    }
    if let Some(ver) = installed_version.map(str::trim).filter(|s| !s.is_empty()) {
        let cand_clean = candidate.tag.trim().trim_start_matches(['v', 'V']);
        let ver_clean = ver.trim_start_matches(['v', 'V']);
        if cand_clean.eq_ignore_ascii_case(ver_clean) {
            return false;
        }
        if let (Some(cv), Some(iv)) = (&cand_ver, &parse_version(ver, false)) {
            return cv > iv;
        }
    }
    // Unknown local IPA + non-numeric tag: don't blindly claim an update when neither parses
    // if we have an installed version; let the user trigger Check/Update or connect from GitHub.
    cand_ver.is_some() || installed_version.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_asset(name: &str) -> ReleaseAsset {
        ReleaseAsset {
            id: 1,
            name: name.into(),
            download_url: format!("https://github.com/o/r/releases/download/v1/{name}"),
            api_url: "https://api.github.com/repos/o/r/releases/assets/1".into(),
            size: 100,
            digest: None,
        }
    }

    #[test]
    fn parses_repo_variants() {
        assert_eq!(parse_repo("novio-app/novio").unwrap(), "novio-app/novio");
        assert_eq!(parse_repo("https://github.com/novio-app/novio").unwrap(), "novio-app/novio");
        assert_eq!(parse_repo("https://github.com/novio-app/novio.git").unwrap(), "novio-app/novio");
        assert_eq!(parse_repo("https://github.com/novio-app/novio/releases/tag/v1.0").unwrap(), "novio-app/novio");
        assert_eq!(parse_repo("https://github.com/novio-app/novio?tab=readme-ov-file#top").unwrap(), "novio-app/novio");
        assert_eq!(parse_repo("github.com/novio-app/novio?foo=1").unwrap(), "novio-app/novio");
        assert_eq!(parse_repo("github.com/novio-app/novio/").unwrap(), "novio-app/novio");
        assert_eq!(parse_repo("git@github.com:novio-app/novio.git").unwrap(), "novio-app/novio");

        assert!(parse_repo("").is_err());
        assert!(parse_repo("justone").is_err());
        assert!(parse_repo("https://gitlab.com/a/b").is_err());
        assert!(parse_repo("../etc/passwd").is_err());
        assert!(parse_repo("owner/..").is_err());
        assert!(parse_repo("-bad/repo").is_err());
    }

    #[test]
    fn enforces_host_and_url_safety() {
        assert!(is_github_api_url("https://api.github.com/repos/o/r/releases/assets/1"));
        assert!(!is_github_api_url("https://evil.example.com/repos/o/r/releases/assets/1"));
        assert!(!is_github_api_url("http://api.github.com/repos/o/r/releases/assets/1"));
        assert!(!is_github_api_url("https://127.0.0.1/repos/o/r"));

        assert!(is_github_web_or_asset_url("https://github.com/o/r/releases/download/v1/App.ipa"));
        assert!(is_github_web_or_asset_url("https://objects.githubusercontent.com/foo"));
        assert!(!is_github_web_or_asset_url("https://evil.com/App.ipa"));
        assert!(!is_github_web_or_asset_url("https://192.168.1.1/App.ipa"));
    }

    #[test]
    fn compares_versions_semantically() {
        assert_eq!(compare_versions("v1.2.3", "1.2.3"), Some(Ordering::Equal));
        assert_eq!(compare_versions("v1.10.0", "v1.9.9"), Some(Ordering::Greater));
        assert_eq!(compare_versions("1.2", "1.2.0"), Some(Ordering::Equal));
        assert_eq!(compare_versions("1.2.0", "1.2.0-beta.2"), Some(Ordering::Greater));
        assert_eq!(compare_versions("1.2.0-beta.2", "1.2.0-beta.1"), Some(Ordering::Greater));
        assert_eq!(compare_versions("1.2.0-beta.10", "1.2.0-beta.2"), Some(Ordering::Greater));
        assert_eq!(compare_versions("1.2.0-rc10", "1.2.0-rc2"), Some(Ordering::Greater));
        assert_eq!(compare_versions("release-2.0.0", "1.9.5"), Some(Ordering::Greater));
        assert_eq!(compare_versions("ios2-v1.10.0", "ios2-v1.9.0"), Some(Ordering::Greater));
        // Non-prerelease suffix like -arm64 does not count as prerelease.
        assert_eq!(compare_versions("1.2.0-arm64", "1.2.0"), Some(Ordering::Equal));
    }

    #[test]
    fn detects_newer_release() {
        let rel = ReleaseInfo {
            release_id: 42,
            tag: "v1.3.0".into(),
            asset: sample_asset("Novio.ipa"),
            ..Default::default()
        };
        assert!(!is_newer_release(Some(42), Some("v1.0.0"), Some("1.0.0"), &rel));
        assert!(!is_newer_release(None, Some("v1.3.0"), Some("1.3.0"), &rel));
        assert!(!is_newer_release(None, None, Some("1.3.0"), &rel));
        assert!(is_newer_release(Some(41), Some("v1.2.0"), Some("1.2.0"), &rel));
        assert!(is_newer_release(None, None, Some("1.2.9"), &rel));
        assert!(!is_newer_release(None, None, Some("1.4.0"), &rel));

        // Rolling nightly tag with a new release ID counts as newer.
        let nightly = ReleaseInfo {
            release_id: 99,
            tag: "nightly".into(),
            asset: sample_asset("Novio.ipa"),
            ..Default::default()
        };
        assert!(is_newer_release(Some(98), Some("nightly"), Some("1.0.0"), &nightly));
        assert!(!is_newer_release(Some(99), Some("nightly"), Some("1.0.0"), &nightly));
    }

    #[test]
    fn matches_and_infers_asset_patterns() {
        let assets = vec![
            sample_asset("Novio-1.4.0-arm64.ipa"),
            sample_asset("Novio-1.4.0-simulator.ipa"),
        ];
        assert!(asset_matches("Novio-1.4.0-arm64.ipa", "arm64"));
        assert!(asset_matches("Novio-1.4.0-arm64.ipa", "Novio-*-arm64.ipa"));
        assert!(!asset_matches("Novio-1.4.0-simulator.ipa", "Novio-*-arm64.ipa"));

        let inferred = infer_asset_pattern("Novio-1.4.0-arm64.ipa", &assets);
        assert_eq!(inferred.as_deref(), Some("Novio-*-arm64.ipa"));
        assert!(asset_matches("Novio-2.0.1-arm64.ipa", inferred.as_deref().unwrap()));

        // Dot-delimited and prerelease filenames generalize properly too.
        let dot_assets = vec![
            sample_asset("App.1.4.0-beta.3.arm64.ipa"),
            sample_asset("App.1.4.0-beta.3.simulator.ipa"),
        ];
        assert_eq!(
            infer_asset_pattern("App.1.4.0-beta.3.arm64.ipa", &dot_assets).as_deref(),
            Some("App.*.arm64.ipa")
        );

        // App-1.4.0.ipa + App-1.4.0-simulator.ipa generalizes to App-*.ipa and disambiguates.
        let sim_pair = vec![
            sample_asset("App-1.4.0.ipa"),
            sample_asset("App-1.4.0-simulator.ipa"),
        ];
        let pat = infer_asset_pattern("App-1.4.0.ipa", &sim_pair);
        assert_eq!(pat.as_deref(), Some("App-*.ipa"));
        let next_pair = vec![
            sample_asset("App-1.5.0.ipa"),
            sample_asset("App-1.5.0-simulator.ipa"),
        ];
        assert_eq!(select_asset(&next_pair, pat.as_deref()).unwrap().name, "App-1.5.0.ipa");

        // Single asset needs no pattern.
        assert_eq!(infer_asset_pattern("Novio-1.4.0.ipa", &assets[..1]), None);
    }
}
