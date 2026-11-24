//! IPA inspection and preparation (everything that happens to the app bundle
//! before it is handed to the signer).

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::macho;
use crate::protocol::Failure;
use crate::state::{ExtensionsMode, InstallOptions};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubBundle {
    pub name: String,
    pub bundle_id: String,
    /// "extension" | "watch"
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IpaInfo {
    pub name: String,
    pub bundle_id: String,
    pub version: Option<String>,
    pub build: Option<String>,
    pub minimum_os: Option<String>,
    pub executable: String,
    pub sub_bundles: Vec<SubBundle>,
    pub restricts_devices: bool,
    /// None if it couldn't be determined.
    pub encrypted: Option<bool>,
    pub size_bytes: u64,
}

impl IpaInfo {
    pub fn extension_count(&self) -> usize {
        self.sub_bundles.iter().filter(|b| b.kind == "extension").count()
    }
    /// App IDs a fresh install will need with the given options.
    pub fn app_ids_needed(&self, opts: &InstallOptions) -> usize {
        1 + match opts.extensions {
            ExtensionsMode::RegisterAll => self.extension_count(),
            _ => 0,
        }
    }
}

fn plist_str(d: &plist::Dictionary, k: &str) -> Option<String> {
    d.get(k).and_then(|v| v.as_string()).map(str::to_string)
}

fn display_name(d: &plist::Dictionary) -> String {
    plist_str(d, "CFBundleDisplayName")
        .or_else(|| plist_str(d, "CFBundleName"))
        .or_else(|| plist_str(d, "CFBundleExecutable"))
        .unwrap_or_else(|| "App".into())
}

fn info_from_plists(
    main: &plist::Dictionary,
    subs: Vec<(String, plist::Dictionary)>,
    encrypted: Option<bool>,
    size: u64,
) -> Result<IpaInfo> {
    Ok(IpaInfo {
        name: display_name(main),
        bundle_id: plist_str(main, "CFBundleIdentifier").context("Info.plist has no CFBundleIdentifier")?,
        version: plist_str(main, "CFBundleShortVersionString"),
        build: plist_str(main, "CFBundleVersion"),
        minimum_os: plist_str(main, "MinimumOSVersion"),
        executable: plist_str(main, "CFBundleExecutable").context("Info.plist has no CFBundleExecutable")?,
        restricts_devices: main.contains_key("UISupportedDevices"),
        sub_bundles: subs
            .into_iter()
            .map(|(kind, d)| SubBundle {
                name: display_name(&d),
                bundle_id: plist_str(&d, "CFBundleIdentifier").unwrap_or_default(),
                kind,
            })
            .collect(),
        encrypted,
        size_bytes: size,
    })
}

/// Classify a path relative to the archive root. Returns the kind of bundle
/// whose Info.plist this is ("main" | "extension" | "watch").
fn classify_plist(name: &str) -> Option<(&'static str, String)> {
    let parts: Vec<&str> = name.split('/').collect();
    if parts.first() != Some(&"Payload") || parts.last() != Some(&"Info.plist") {
        return None;
    }
    let app = parts.get(1)?;
    if !app.ends_with(".app") {
        return None;
    }
    match parts.len() {
        3 => Some(("main", app.to_string())),
        5 if parts[2] == "PlugIns" && parts[3].ends_with(".appex") => Some(("extension", app.to_string())),
        5 if parts[2] == "Watch" && parts[3].ends_with(".app") => Some(("watch", app.to_string())),
        _ => None,
    }
}

pub fn inspect(path: &Path) -> Result<IpaInfo> {
    if path.is_dir() {
        return inspect_app_dir(path);
    }
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut zip = zip::ZipArchive::new(file).map_err(|e| anyhow::anyhow!("not a valid IPA (invalid zip): {e}"))?;

    let mut main: Option<(String, plist::Dictionary)> = None;
    let mut subs = Vec::new();
    if zip.len() > MAX_ENTRIES {
        bail!("not a valid IPA: too many files ({})", zip.len());
    }
    let names: Vec<String> = zip.file_names().map(str::to_string).collect();
    let mut plist_bytes: u64 = 0;
    for name in &names {
        let Some((kind, app)) = classify_plist(name) else { continue };
        let mut buf = Vec::new();
        zip.by_name(name)?.take(MAX_PLIST_BYTES).read_to_end(&mut buf)?;
        plist_bytes += buf.len() as u64;
        if plist_bytes > 4 * MAX_PLIST_BYTES {
            bail!("not a valid IPA: too many or too large Info.plist files");
        }
        let dict: plist::Dictionary = plist::from_bytes(&buf).with_context(|| format!("parsing {name}"))?;
        if kind == "main" {
            if main.is_some() {
                bail!("not a valid IPA: multiple .app directories found in Payload");
            }
            main = Some((app, dict));
        } else {
            subs.push((kind.to_string(), dict));
        }
    }
    let (app, main) = main.ok_or_else(|| anyhow::anyhow!("not a valid IPA: no Payload directory / .app found"))?;
    let exe = plist_str(&main, "CFBundleExecutable").unwrap_or_default();
    let encrypted = if valid_file_name(&exe) {
        zip.by_name(&format!("Payload/{app}/{exe}")).ok().and_then(|f| stream_encryption(f).ok())
    } else {
        None
    };
    info_from_plists(&main, subs, encrypted, size)
}

/// FairPlay check on a sequential stream (zip entries can't seek): read the
/// fat header, then only the first bytes of each slice.
fn stream_encryption(mut r: impl Read) -> Result<bool> {
    const PREFIX: u64 = 256 * 1024;
    let mut head = Vec::new();
    (&mut r).take(4096).read_to_end(&mut head)?;
    let mut layout = macho::fat_layout(&head)?;
    if layout == [(0, u64::MAX)] {
        // Thin: the header is at the start.
        (&mut r).take(PREFIX - head.len() as u64).read_to_end(&mut head)?;
        return macho::is_encrypted_from_prefixes(&[head]);
    }
    layout.sort();
    // Slice offsets come from the (untrusted) header; don't decompress more
    // than a real binary could hold just to reach them.
    const MAX_SKIP: u64 = 2 * 1024 * 1024 * 1024;
    if layout.iter().any(|(off, _)| *off > MAX_SKIP) {
        bail!("fat slice offset out of range");
    }
    let mut pos = head.len() as u64;
    let mut prefixes = Vec::new();
    for (off, size) in layout {
        if off < pos {
            bail!("overlapping fat slices");
        }
        std::io::copy(&mut (&mut r).take(off - pos), &mut std::io::sink())?;
        let mut buf = Vec::new();
        (&mut r).take(PREFIX.min(size)).read_to_end(&mut buf)?;
        pos = off + buf.len() as u64;
        prefixes.push(buf);
    }
    macho::is_encrypted_from_prefixes(&prefixes)
}

fn read_plist(p: &Path) -> Result<plist::Dictionary> {
    plist::from_file(p).with_context(|| format!("parsing {}", p.display()))
}

/// Open a file inside an untrusted bundle: a regular file (not a link, FIFO
/// or device) no bigger than `max`.
fn open_regular(p: &Path, max: u64) -> Result<std::fs::File> {
    let m = std::fs::symlink_metadata(p).with_context(|| format!("reading {}", p.display()))?;
    if !m.file_type().is_file() {
        return Err(bad_bundle(format!("{} is not a regular file.", p.display())));
    }
    if m.len() > max {
        return Err(bad_bundle(format!("{} is too large.", p.display())));
    }
    Ok(std::fs::File::open(p)?)
}

fn read_plist_untrusted(p: &Path) -> Result<plist::Dictionary> {
    let mut buf = Vec::new();
    open_regular(p, MAX_PLIST_BYTES)?.take(MAX_PLIST_BYTES).read_to_end(&mut buf)?;
    plist::from_bytes(&buf).with_context(|| format!("parsing {}", p.display()))
}

fn inspect_app_dir(app: &Path) -> Result<IpaInfo> {
    let real_dir = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
    if !real_dir(app) {
        return Err(bad_bundle("The app is a link, not a folder."));
    }
    let main = read_plist_untrusted(&app.join("Info.plist")).context("not a valid app bundle")?;
    let mut subs = Vec::new();
    let mut plist_bytes: u64 = 0;
    for (dir, kind, ext) in [("PlugIns", "extension", "appex"), ("Watch", "watch", "app")] {
        // Never follow a linked folder out of the bundle.
        if !real_dir(&app.join(dir)) {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(app.join(dir)) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == ext) && e.file_type().is_ok_and(|t| t.is_dir()) {
                let plist = p.join("Info.plist");
                plist_bytes += std::fs::symlink_metadata(&plist).map(|m| m.len()).unwrap_or(0);
                if plist_bytes > 4 * MAX_PLIST_BYTES {
                    bail!("not a valid app: too many or too large Info.plist files");
                }
                if let Ok(d) = read_plist_untrusted(&plist) {
                    subs.push((kind.to_string(), d));
                }
            }
        }
    }
    let exe = plist_str(&main, "CFBundleExecutable").unwrap_or_default();
    let encrypted = if valid_file_name(&exe) {
        open_regular(&app.join(&exe), MAX_TOTAL_BYTES).ok().and_then(|f| stream_encryption(f).ok())
    } else {
        None
    };
    info_from_plists(&main, subs, encrypted, dir_size(app))
}

pub fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => total += dir_size(&e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    total
}

// ---------------------------------------------------------------------------
// Untrusted bundle handling
// ---------------------------------------------------------------------------

/// Limits for untrusted archives (decompression bombs, absurd file counts).
const MAX_ENTRIES: usize = 200_000;
const MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_PLIST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SYMLINK_TARGET: u64 = 4096;

fn bad_bundle(msg: impl Into<String>) -> anyhow::Error {
    Failure::new("unsafe_bundle", msg)
        .with_hint("The app bundle contains links or paths that point outside itself. Loadside refuses to process it; get the IPA from a source you trust.")
        .into()
}

/// A plain file name: one normal path component (no `/`, `..`, absolute).
pub fn valid_file_name(name: &str) -> bool {
    let p = Path::new(name);
    !name.is_empty() && p.components().count() == 1 && matches!(p.components().next(), Some(Component::Normal(_)))
}

/// Every symlink under `root` must resolve to something inside `root`, and
/// there must be no FIFOs, sockets or device files.
pub fn verify_tree(root: &Path) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    let canon_root = std::fs::canonicalize(root)?;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir)? {
            let e = e?;
            let t = e.file_type()?;
            let p = e.path();
            if t.is_symlink() {
                let resolved = std::fs::canonicalize(&p).map_err(|_| bad_bundle(format!("Broken link in the app: {}", rel(root, &p))))?;
                if !resolved.starts_with(&canon_root) {
                    return Err(bad_bundle(format!("{} links outside the app.", rel(root, &p))));
                }
                // iOS bundles don't need folder links, and a loop (a link to a
                // parent) sends tools that walk the tree around forever.
                if resolved.is_dir() {
                    return Err(bad_bundle(format!("{} is a link to a folder.", rel(root, &p))));
                }
            } else if t.is_dir() {
                stack.push(p);
            } else if t.is_fifo() || t.is_socket() || t.is_block_device() || t.is_char_device() {
                return Err(bad_bundle(format!("{} is a special file.", rel(root, &p))));
            }
        }
    }
    Ok(())
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

/// Write a plist, refusing to follow a symlink at `path`.
fn write_plist(path: &Path, d: &plist::Dictionary) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(bad_bundle(format!("{} is a link; refusing to write through it.", path.display())));
    }
    plist::to_file_xml(path, d)?;
    Ok(())
}

/// Write file contents, refusing to follow a symlink at `path`.
fn write_file(path: &Path, data: &[u8]) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(bad_bundle(format!("{} is a link; refusing to write through it.", path.display())));
    }
    std::fs::write(path, data)?;
    Ok(())
}

/// Copy a directory tree. Symlinks are recreated last (so nothing is ever
/// written through one) and then verified to stay inside `dst`.
pub fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    let mut links = Vec::new();
    copy_dir_inner(src, dst, &mut links)?;
    for (target, at) in links {
        let _ = std::fs::remove_file(&at);
        std::os::unix::fs::symlink(target, &at)?;
    }
    verify_tree(dst)
}

fn copy_dir_inner(src: &Path, dst: &Path, links: &mut Vec<(PathBuf, PathBuf)>) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let from = e.path();
        let to = dst.join(e.file_name());
        let t = e.file_type()?;
        if t.is_symlink() {
            links.push((std::fs::read_link(&from)?, to));
        } else if t.is_dir() {
            copy_dir_inner(&from, &to, links)?;
        } else if t.is_file() {
            std::fs::copy(&from, &to)?;
            // Signing rewrites plists and binaries: keep them writable.
            use std::os::unix::fs::PermissionsExt;
            if let Ok(m) = std::fs::metadata(&to) {
                let mode = m.permissions().mode();
                if mode & 0o600 != 0o600 {
                    let _ = std::fs::set_permissions(&to, std::fs::Permissions::from_mode((mode & 0o755) | 0o600));
                }
            }
        } else {
            return Err(bad_bundle(format!("{} is a special file.", from.display())));
        }
    }
    Ok(())
}

/// Extract an IPA into `work` without trusting it: names must stay inside
/// `work`, sizes are capped, and symlinks are created last and verified.
fn extract_zip(zip: &mut zip::ZipArchive<std::fs::File>, work: &Path) -> Result<()> {
    if zip.len() > MAX_ENTRIES {
        bail!("not a valid IPA: too many files ({})", zip.len());
    }
    let mut total: u64 = 0;
    let mut links: Vec<(PathBuf, PathBuf)> = Vec::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        let Some(rel) = f.enclosed_name() else {
            return Err(bad_bundle(format!("Archive entry \"{}\" points outside the archive.", f.name())));
        };
        let out = work.join(rel);
        if f.is_symlink() {
            let mut target = String::new();
            (&mut f).take(MAX_SYMLINK_TARGET).read_to_string(&mut target)?;
            links.push((PathBuf::from(target), out));
            continue;
        }
        if f.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut w = std::fs::File::create(&out)?;
        // Count real output bytes; the header's size field can lie.
        let budget = MAX_TOTAL_BYTES.saturating_sub(total);
        let n = std::io::copy(&mut (&mut f).take(budget + 1), &mut w)?;
        total += n;
        if total > MAX_TOTAL_BYTES {
            bail!("the IPA expands to more than {} GB", MAX_TOTAL_BYTES >> 30);
        }
        if let Some(mode) = f.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            // Keep the owner's read/write bits: signing rewrites Info.plist and binaries.
            let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode((mode & 0o755) | 0o600));
        }
    }
    for (target, at) in links {
        // An absolute target can only point outside the bundle.
        if target.is_absolute() || target.as_os_str().is_empty() {
            return Err(bad_bundle(format!("{} links outside the app.", rel(work, &at))));
        }
        // Never create, delete or link *through* an earlier link: a chain like
        // "a -> /elsewhere" then "a/b" would otherwise touch files outside
        // `work` before verify_tree gets a chance to reject the archive.
        ensure_real_dirs(work, &at)?;
        if let Some(parent) = at.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.is_dir() => return Err(bad_bundle(format!("{} is both a folder and a link.", rel(work, &at)))),
            Ok(_) => std::fs::remove_file(&at)?,
            Err(_) => {}
        }
        std::os::unix::fs::symlink(target, &at)?;
    }
    verify_tree(work)
}

/// Every existing ancestor of `at` below `root` must be a real directory.
fn ensure_real_dirs(root: &Path, at: &Path) -> Result<()> {
    let relp = at.strip_prefix(root).map_err(|_| bad_bundle("Archive entry points outside the archive."))?;
    let comps: Vec<_> = relp.components().collect();
    let mut cur = root.to_path_buf();
    for c in comps.iter().take(comps.len().saturating_sub(1)) {
        match c {
            std::path::Component::Normal(n) => cur.push(n),
            _ => return Err(bad_bundle("Archive entry points outside the archive.")),
        }
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(bad_bundle(format!("{} goes through a link.", rel(root, at))));
            }
            Ok(m) if !m.is_dir() => return Err(bad_bundle(format!("{} is inside a file.", rel(root, at)))),
            _ => {}
        }
    }
    Ok(())
}

/// Apps that `isideload` hands your signing certificate *and private key*
/// to (so they can refresh themselves): AltStore, SideStore, StikStore,
/// LiveContainer, or anything bundling the SideStore framework.
pub fn receives_signing_key(app_dir: &Path, bundle_id: &str) -> bool {
    if matches!(bundle_id, "com.rileytestut.AltStore" | "com.SideStore.SideStore" | "app.stik.store" | "com.kdt.livecontainer") {
        return true;
    }
    let Ok(rd) = std::fs::read_dir(app_dir.join("Frameworks")) else { return false };
    rd.flatten().any(|e| {
        read_plist(&e.path().join("Info.plist"))
            .ok()
            .and_then(|d| plist_str(&d, "CFBundleIdentifier"))
            .is_some_and(|id| id == "com.SideStore.SideStore")
    })
}

#[derive(Debug)]
pub struct Prepared {
    pub app_dir: PathBuf,
    pub info: IpaInfo,
    pub warnings: Vec<String>,
}

/// Extract `src` (an .ipa or .app) into `work` and apply `opts`.
pub fn prepare(src: &Path, work: &Path, opts: &InstallOptions) -> Result<Prepared> {
    let payload = work.join("Payload");
    std::fs::create_dir_all(&payload)?;
    if src.is_dir() {
        let name = src.file_name().context("bad .app path")?;
        copy_dir(src, &payload.join(name))?;
    } else {
        let file = std::fs::File::open(src).with_context(|| format!("opening {}", src.display()))?;
        let mut zip = zip::ZipArchive::new(file).map_err(|e| anyhow::anyhow!("not a valid IPA (invalid zip): {e}"))?;
        extract_zip(&mut zip, work).context("extracting IPA")?;
    }
    let apps: Vec<PathBuf> = std::fs::read_dir(&payload)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()) && p.extension().is_some_and(|e| e == "app")
        })
        .collect();
    let app_dir = match apps.as_slice() {
        [one] => one.clone(),
        [] => bail!("not a valid IPA: no .app directory found in Payload"),
        _ => bail!("not a valid IPA: multiple .app directories found in Payload"),
    };

    let mut warnings = Vec::new();
    let info_path = app_dir.join("Info.plist");
    let mut info = read_plist(&info_path)?;
    let original_id = plist_str(&info, "CFBundleIdentifier").context("Info.plist has no CFBundleIdentifier")?;
    let exe = plist_str(&info, "CFBundleExecutable").context("Info.plist has no CFBundleExecutable")?;
    if !valid_file_name(&exe) {
        return Err(bad_bundle(format!("The app's executable name \"{exe}\" isn't a plain file name.")));
    }
    if std::fs::symlink_metadata(app_dir.join(&exe)).is_ok_and(|m| !m.is_file()) {
        return Err(bad_bundle("The app's executable isn't a regular file."));
    }

    // --- encryption check ---
    let exe_path = app_dir.join(&exe);
    let exe_bytes = std::fs::read(&exe_path).with_context(|| format!("reading main executable {exe}"))?;
    if macho::is_encrypted(&exe_bytes).unwrap_or(false) {
        if !opts.allow_encrypted {
            return Err(Failure::new("encrypted_ipa", "This IPA is still FairPlay-encrypted (taken straight from the App Store).")
                .with_hint("Encrypted apps crash on launch when sideloaded. Use a decrypted IPA, or enable \"Install anyway\" if you know what you're doing.")
                .into());
        }
        warnings.push("Main binary is FairPlay-encrypted; the app will probably not launch.".into());
    }
    drop(exe_bytes);

    // --- strip sub-bundles ---
    // The signer only signs PlugIns/ and Frameworks/. A Watch app or
    // ExtensionKit extensions (Extensions/) would be left unsigned and make
    // the install fail, so they always go.
    for d in ["Watch", "com.apple.WatchPlaceholder", "Extensions"] {
        let p = app_dir.join(d);
        if std::fs::symlink_metadata(&p).is_ok() {
            remove_any(&p)?;
            if d == "Extensions" || !opts.strip_watch_app {
                warnings.push(format!("Removed {d}/: Loadside can't sign {}.", if d == "Extensions" { "ExtensionKit extensions" } else { "Apple Watch apps" }));
            }
        }
    }
    if opts.extensions == ExtensionsMode::Strip {
        let p = app_dir.join("PlugIns");
        if std::fs::symlink_metadata(&p).is_ok() {
            remove_any(&p)?;
        }
    }

    // --- Info.plist edits ---
    let new_id = opts.bundle_id.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&original_id).to_string();
    if !valid_bundle_id(&new_id) {
        return Err(Failure::new("invalid_bundle_id", format!("\"{new_id}\" isn't a valid bundle identifier."))
            .with_hint("Use letters, digits, '-' and '.', e.g. com.example.myapp.")
            .into());
    }
    info.insert("CFBundleIdentifier".into(), plist::Value::String(new_id.clone()));
    if let Some(name) = opts.display_name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        info.insert("CFBundleDisplayName".into(), plist::Value::String(name.to_string()));
    }
    if opts.remove_supported_devices {
        info.remove("UISupportedDevices");
    }
    if opts.file_sharing {
        info.insert("UIFileSharingEnabled".into(), plist::Value::Boolean(true));
        info.insert("LSSupportsOpeningDocumentsInPlace".into(), plist::Value::Boolean(true));
    }
    write_plist(&info_path, &info)?;

    // Extensions must be prefixed with the main bundle ID or Apple rejects
    // them. Fix them up instead of failing like other tools do.
    let mut used_ids: Vec<String> = vec![new_id.clone()];
    if let Ok(rd) = std::fs::read_dir(app_dir.join("PlugIns")) {
        let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for ext in entries {
            if !std::fs::symlink_metadata(&ext).is_ok_and(|m| m.is_dir()) {
                continue;
            }
            let p = ext.join("Info.plist");
            if !p.exists() {
                continue;
            }
            let mut d = read_plist(&p)?;
            let Some(id) = plist_str(&d, "CFBundleIdentifier") else { continue };
            let mut fixed = if let Some(rest) = id.strip_prefix(&format!("{original_id}.")) {
                format!("{new_id}.{rest}")
            } else if id.starts_with(&format!("{new_id}.")) {
                id.clone()
            } else {
                let last = id.rsplit('.').next().unwrap_or("ext");
                format!("{new_id}.{last}")
            };
            // Two extensions must never end up with the same ID.
            if used_ids.contains(&fixed) {
                let base = fixed.clone();
                let mut n = 2;
                while used_ids.contains(&fixed) {
                    fixed = format!("{base}{n}");
                    n += 1;
                }
            }
            if fixed != id && !id.starts_with(&format!("{original_id}.")) {
                warnings.push(format!("Extension {id} renamed to {fixed} (it didn't share the app's bundle ID prefix)."));
            }
            used_ids.push(fixed.clone());
            if fixed != id || opts.remove_supported_devices {
                d.insert("CFBundleIdentifier".into(), plist::Value::String(fixed));
                if opts.remove_supported_devices {
                    d.remove("UISupportedDevices");
                }
                write_plist(&p, &d)?;
            }
        }
    }

    // --- injection ---
    if !opts.inject.is_empty() {
        inject(&app_dir, &exe_path, &opts.inject, &work.join("inject"), &mut warnings)?;
    }

    // Nothing above may have introduced an escaping link.
    verify_tree(&app_dir)?;
    // The signer rewrites every bundle's executable in place: each must be a
    // plain file name naming a regular file inside its own bundle.
    verify_executables(&app_dir)?;

    // isideload embeds the signing certificate + private key into some apps.
    let final_id = new_id.clone();
    if receives_signing_key(&app_dir, &final_id) && !opts.allow_signing_key {
        return Err(Failure::new("signing_key_requested", "This app asks for your signing certificate and its private key.")
            .with_hint("AltStore, SideStore, StikStore and LiveContainer refresh themselves with it, so Loadside would copy the key into the app. Only allow this for a copy you got from the official source, and never share the signed IPA. Turn on \"Allow this app to receive my signing key\" to continue.")
            .into());
    }

    let info = inspect_app_dir(&app_dir)?;
    Ok(Prepared { app_dir, info, warnings })
}

/// Every bundle under `root` (the app, extensions, frameworks, anything
/// with an Info.plist) must name its executable with a plain file name, and
/// that must be a regular file (not a link) if it exists.
fn verify_executables(root: &Path) -> Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let plist = dir.join("Info.plist");
        if std::fs::symlink_metadata(&plist).is_ok_and(|m| m.is_file()) {
            if let Ok(d) = read_plist_untrusted(&plist) {
                if let Some(exe) = plist_str(&d, "CFBundleExecutable") {
                    if !valid_file_name(&exe) {
                        return Err(bad_bundle(format!("{} names its executable \"{exe}\", which isn't a plain file name.", rel(root, &dir))));
                    }
                    match std::fs::symlink_metadata(dir.join(&exe)) {
                        Ok(m) if !m.is_file() => {
                            return Err(bad_bundle(format!("{} isn't a regular file.", rel(root, &dir.join(&exe)))));
                        }
                        _ => {}
                    }
                }
            }
        }
        for e in std::fs::read_dir(&dir)?.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(e.path());
            }
        }
    }
    Ok(())
}

fn remove_any(p: &Path) -> Result<()> {
    if std::fs::symlink_metadata(p)?.is_dir() {
        std::fs::remove_dir_all(p)?;
    } else {
        std::fs::remove_file(p)?;
    }
    Ok(())
}

fn valid_bundle_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 155
        && !id.starts_with('.')
        && !id.ends_with('.')
        && !id.contains("..")
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

const SUBSTRATE_PROVIDERS: &[&str] = &[
    "CydiaSubstrate.framework",
    "libsubstrate.dylib",
    "ElleKit.framework",
    "libellekit.dylib",
    "libsubstitute.dylib",
];

fn is_substrate_ref(path: &str) -> bool {
    path.contains("CydiaSubstrate.framework")
        || path.contains("ElleKit.framework")
        || path.ends_with("libsubstrate.dylib")
        || path.ends_with("libsubstitute.dylib")
        || path.ends_with("libellekit.dylib")
}

fn inject(app_dir: &Path, exe_path: &Path, items: &[PathBuf], scratch: &Path, warnings: &mut Vec<String>) -> Result<()> {
    let mut dylibs: Vec<PathBuf> = Vec::new();
    let mut frameworks: Vec<PathBuf> = Vec::new();
    let mut bundles: Vec<PathBuf> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        if !item.exists() {
            return Err(Failure::new("inject_missing", format!("Injection file not found: {}", item.display()))
                .with_hint("It may have been moved or deleted. Remove it from the injection list or add it again.")
                .into());
        }
        let ext = item.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        match ext.as_str() {
            "dylib" => dylibs.push(item.clone()),
            "framework" => frameworks.push(item.clone()),
            "bundle" => bundles.push(item.clone()),
            "deb" => {
                let c = crate::deb::extract(item, &scratch.join(format!("deb{i}")))?;
                dylibs.extend(c.dylibs);
                frameworks.extend(c.frameworks);
                bundles.extend(c.bundles);
            }
            _ => {
                return Err(Failure::new("inject_unsupported", format!("Can't inject {}", item.display()))
                    .with_hint("Supported: .dylib, .framework, .deb and .bundle.")
                    .into());
            }
        }
    }

    let fw_dir = app_dir.join("Frameworks");
    if std::fs::symlink_metadata(&fw_dir).is_ok_and(|m| !m.is_dir()) {
        return Err(bad_bundle("The app's Frameworks folder is a link."));
    }
    std::fs::create_dir_all(&fw_dir)?;
    let mut binaries: Vec<(PathBuf, String)> = Vec::new(); // (path in app, @rpath load path)
    let mut provided: Vec<String> = Vec::new();

    for d in &dylibs {
        let name = d.file_name().context("bad dylib path")?.to_string_lossy().to_string();
        let dst = fw_dir.join(&name);
        if std::fs::symlink_metadata(&dst).is_ok_and(|m| m.file_type().is_symlink()) {
            std::fs::remove_file(&dst)?;
        }
        std::fs::copy(d, &dst)?;
        provided.push(name.clone());
        binaries.push((dst, format!("@rpath/{name}")));
    }
    for f in &frameworks {
        let name = f.file_name().context("bad framework path")?.to_string_lossy().to_string();
        let stem = name.trim_end_matches(".framework").to_string();
        let dst = fw_dir.join(&name);
        if std::fs::symlink_metadata(&dst).is_ok() {
            remove_any(&dst)?;
        }
        copy_dir(f, &dst)?;
        let plist_path = dst.join("Info.plist");
        let exe = if plist_path.exists() {
            let exe = read_plist(&plist_path).ok().and_then(|d| plist_str(&d, "CFBundleExecutable")).unwrap_or(stem.clone());
            if !valid_file_name(&exe) {
                return Err(bad_bundle(format!("{name} declares an executable \"{exe}\" that isn't a plain file name.")));
            }
            exe
        } else {
            // The signer only treats directories with an Info.plist as bundles.
            let mut d = plist::Dictionary::new();
            d.insert("CFBundleExecutable".into(), stem.clone().into());
            d.insert("CFBundleIdentifier".into(), format!("dev.loadside.inject.{}", stem.replace(|c: char| !c.is_ascii_alphanumeric(), "-")).into());
            d.insert("CFBundleName".into(), stem.clone().into());
            d.insert("CFBundlePackageType".into(), "FMWK".into());
            d.insert("CFBundleInfoDictionaryVersion".into(), "6.0".into());
            d.insert("CFBundleShortVersionString".into(), "1.0".into());
            d.insert("CFBundleVersion".into(), "1".into());
            write_plist(&plist_path, &d)?;
            stem.clone()
        };
        provided.push(name.clone());
        binaries.push((dst.join(&exe), format!("@rpath/{name}/{exe}")));
    }
    for b in &bundles {
        let name = b.file_name().context("bad bundle path")?;
        let dst = app_dir.join(name);
        if std::fs::symlink_metadata(&dst).is_ok() {
            remove_any(&dst)?;
        }
        copy_dir(b, &dst)?;
    }

    let substrate_target = SUBSTRATE_PROVIDERS
        .iter()
        .find(|p| provided.iter().any(|x| x == *p))
        .and_then(|p| binaries.iter().find(|(path, _)| path.to_string_lossy().contains(*p)).map(|(_, l)| l.clone()));

    // Map a dependency path to one of our injected copies.
    let map = |path: &str| -> Option<String> {
        if is_substrate_ref(path) {
            if let Some(t) = &substrate_target {
                return Some(t.clone());
            }
        }
        let p = path.strip_prefix("/var/jb").unwrap_or(path);
        if let Some(idx) = p.find(".framework/") {
            let before = &p[..idx];
            let fw = before.rsplit('/').next().unwrap_or(before);
            let full = format!("{fw}.framework");
            if provided.contains(&full) {
                return Some(format!("@rpath/{full}/{}", &p[idx + ".framework/".len()..]));
            }
            return None;
        }
        let file = p.rsplit('/').next().unwrap_or(p);
        provided.iter().any(|x| x == file).then(|| format!("@rpath/{file}"))
    };

    let mut dependencies: Vec<String> = Vec::new();
    let mut missing_substrate = false;
    let mut valid: Vec<(PathBuf, String)> = Vec::new();
    for (path, load) in &binaries {
        if !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file()) {
            warnings.push(format!("{} is missing or not a regular file; skipped.", rel(app_dir, path)));
            continue;
        }
        let mut data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        if !macho::is_macho(&data) {
            warnings.push(format!("{} is not a Mach-O binary; skipped.", rel(app_dir, path)));
            continue;
        }
        valid.push((path.clone(), load.clone()));
        let deps = macho::load_paths(&data)?;
        if deps.iter().any(|d| is_substrate_ref(d)) && substrate_target.is_none() {
            missing_substrate = true;
        }
        let (changed, too_long) = macho::rewrite_load_paths(&mut data, &map)?;
        for (_, new) in &changed {
            dependencies.push(new.clone());
        }
        for d in &deps {
            if let Some(m) = map(d) {
                dependencies.push(m);
            }
        }
        for t in too_long {
            warnings.push(format!("Couldn't rewrite dependency {t} in {} (no room in its header).", rel(app_dir, path)));
        }
        write_file(path, &data)?;
    }
    if missing_substrate {
        warnings.push(
            "A tweak depends on CydiaSubstrate but none was provided. Add ElleKit (libellekit.dylib) or CydiaSubstrate.framework to the injection list, otherwise the app will crash on launch.".into(),
        );
    }

    // Load everything that isn't merely a dependency of something else.
    let mut exe = std::fs::read(exe_path)?;
    macho::ensure_rpath(&mut exe, "@executable_path/Frameworks")?;
    for (path, load) in &valid {
        let is_dep = dependencies.iter().any(|d| d == load);
        let is_substrate = SUBSTRATE_PROVIDERS.iter().any(|p| path.to_string_lossy().contains(*p));
        if is_dep || is_substrate {
            continue;
        }
        macho::add_load_dylib(&mut exe, load, false)?;
    }
    write_file(exe_path, &exe)?;
    Ok(())
}

/// Zip a signed `.app` back into an .ipa.
pub fn package(app_dir: &Path, out: &Path) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use zip::write::SimpleFileOptions;

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = out.with_extension("ipa.partial");
    let mut zw = zip::ZipWriter::new(std::fs::File::create(&tmp)?);
    let app_name = app_dir.file_name().context("bad app path")?.to_string_lossy().to_string();
    fn walk(zw: &mut zip::ZipWriter<std::fs::File>, prefix: &str, dir: &Path) -> Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            let rel = format!("{prefix}/{}", e.file_name().to_string_lossy());
            let t = e.file_type()?;
            if t.is_symlink() {
                let target = std::fs::read_link(&p)?;
                zw.add_symlink(rel, target.to_string_lossy(), SimpleFileOptions::default())?;
            } else if t.is_dir() {
                zw.add_directory(format!("{rel}/"), SimpleFileOptions::default())?;
                walk(zw, &rel, &p)?;
            } else {
                let mode = e.metadata()?.permissions().mode();
                let opts = SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated)
                    .unix_permissions(mode & 0o777)
                    .large_file(e.metadata()?.len() > u32::MAX as u64);
                zw.start_file(rel, opts)?;
                let mut f = std::fs::File::open(&p)?;
                std::io::copy(&mut f, zw)?;
            }
        }
        Ok(())
    }
    let res = (|| -> Result<()> {
        zw.add_directory("Payload/", SimpleFileOptions::default())?;
        walk(&mut zw, &format!("Payload/{app_name}"), app_dir)?;
        zw.finish()?.flush()?;
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return res;
    }
    std::fs::rename(&tmp, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn cc(args: &[&str]) {
        assert!(Command::new("cc").args(args).status().unwrap().success());
    }

    fn plist_file(p: &Path, pairs: &[(&str, &str)]) {
        let mut d = plist::Dictionary::new();
        for (k, v) in pairs {
            d.insert((*k).into(), (*v).into());
        }
        plist::to_file_xml(p, &d).unwrap();
    }

    /// A link to the outside followed by an entry *under* that link must not
    /// create, delete or link anything outside the extraction directory.
    #[test]
    fn chained_symlinks_cannot_escape() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let dir = std::env::temp_dir().join(format!("ls-chain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("victim"), "keep me").unwrap();
        let ipa = dir.join("evil.ipa");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&ipa).unwrap());
            let o = SimpleFileOptions::default();
            z.add_symlink("Payload/A.app/esc", outside.to_str().unwrap(), o).unwrap();
            z.add_symlink("Payload/A.app/esc/victim", "x", o).unwrap();
            z.start_file("Payload/A.app/Info.plist", o).unwrap();
            z.write_all(b"<plist/>").unwrap();
            z.finish().unwrap();
        }
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&ipa).unwrap()).unwrap();
        assert!(extract_zip(&mut zip, &work).is_err());
        assert_eq!(std::fs::read_to_string(outside.join("victim")).unwrap(), "keep me");
        assert!(std::fs::symlink_metadata(outside.join("victim")).unwrap().is_file());
        // Same chain with a relative escape.
        let ipa2 = dir.join("evil2.ipa");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&ipa2).unwrap());
            let o = SimpleFileOptions::default();
            z.add_symlink("Payload/A.app/esc", "../../../outside", o).unwrap();
            z.add_symlink("Payload/A.app/esc/victim", "x", o).unwrap();
            z.finish().unwrap();
        }
        let work2 = dir.join("work2");
        std::fs::create_dir_all(&work2).unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&ipa2).unwrap()).unwrap();
        assert!(extract_zip(&mut zip, &work2).is_err());
        assert!(std::fs::symlink_metadata(outside.join("victim")).unwrap().is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Build Payload/Demo.app with an extension and a watch app, zipped as an IPA.
    fn fake_ipa(dir: &Path) -> PathBuf {
        let app = dir.join("src/Payload/Demo.app");
        std::fs::create_dir_all(app.join("PlugIns/Widget.appex")).unwrap();
        std::fs::create_dir_all(app.join("Watch/DemoWatch.app")).unwrap();
        let c = dir.join("m.c");
        std::fs::write(&c, "int main(void){return 0;}").unwrap();
        cc(&["-arch", "arm64", "-Wl,-headerpad,0x800", "-o", app.join("Demo").to_str().unwrap(), c.to_str().unwrap()]);
        plist_file(&app.join("Info.plist"), &[
            ("CFBundleIdentifier", "com.example.demo"),
            ("CFBundleExecutable", "Demo"),
            ("CFBundleDisplayName", "Demo"),
            ("CFBundleShortVersionString", "1.2"),
        ]);
        std::fs::write(app.join("UISupportedDevices"), "").unwrap();
        plist_file(&app.join("PlugIns/Widget.appex/Info.plist"), &[("CFBundleIdentifier", "com.other.widget"), ("CFBundleExecutable", "Widget")]);
        plist_file(&app.join("Watch/DemoWatch.app/Info.plist"), &[("CFBundleIdentifier", "com.example.demo.watchkitapp")]);
        let ipa = dir.join("Demo.ipa");
        package(&app, &ipa).unwrap();
        ipa
    }

    #[test]
    fn inspect_prepare_and_inject() {
        let dir = tempfile::tempdir().unwrap();
        let ipa = fake_ipa(dir.path());
        let info = inspect(&ipa).unwrap();
        assert_eq!(info.bundle_id, "com.example.demo");
        assert_eq!(info.version.as_deref(), Some("1.2"));
        assert_eq!(info.extension_count(), 1);
        assert_eq!(info.sub_bundles.len(), 2);
        assert_eq!(info.encrypted, Some(false));

        let tweak_c = dir.path().join("t.c");
        std::fs::write(&tweak_c, "__attribute__((constructor)) static void f(void){}").unwrap();
        let tweak = dir.path().join("MyTweak.dylib");
        cc(&["-arch", "arm64", "-dynamiclib", "-o", tweak.to_str().unwrap(), tweak_c.to_str().unwrap()]);

        let opts = InstallOptions {
            bundle_id: Some("com.me.demo".into()),
            display_name: Some("My Demo".into()),
            inject: vec![tweak],
            file_sharing: true,
            ..Default::default()
        };
        let work = dir.path().join("work");
        let p = prepare(&ipa, &work, &opts).unwrap();
        assert_eq!(p.info.bundle_id, "com.me.demo");
        assert_eq!(p.info.name, "My Demo");
        // watch app stripped by default, extension kept and re-prefixed
        assert_eq!(p.info.sub_bundles.len(), 1);
        assert_eq!(p.info.sub_bundles[0].bundle_id, "com.me.demo.widget");
        assert!(p.warnings.iter().any(|w| w.contains("renamed")));
        assert!(p.app_dir.join("Frameworks/MyTweak.dylib").exists());
        let exe = std::fs::read(p.app_dir.join("Demo")).unwrap();
        assert!(macho::load_paths(&exe).unwrap().contains(&"@rpath/MyTweak.dylib".to_string()));
        // otool agrees the binary is still well-formed
        let out = Command::new("otool").arg("-L").arg(p.app_dir.join("Demo")).output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("@rpath/MyTweak.dylib"));

        let strip = InstallOptions { extensions: ExtensionsMode::Strip, ..Default::default() };
        let p2 = prepare(&ipa, &dir.path().join("work2"), &strip).unwrap();
        assert!(p2.info.sub_bundles.is_empty());
    }

    fn zip_with(dir: &Path, build: impl FnOnce(&mut zip::ZipWriter<std::fs::File>)) -> PathBuf {
        use zip::write::SimpleFileOptions;
        let out = dir.join("evil.ipa");
        let mut zw = zip::ZipWriter::new(std::fs::File::create(&out).unwrap());
        let mut info = Vec::new();
        plist::to_writer_xml(&mut info, &plist::Dictionary::from_iter([
            ("CFBundleIdentifier".to_string(), plist::Value::from("com.evil.app")),
            ("CFBundleExecutable".to_string(), plist::Value::from("Evil")),
        ]))
        .unwrap();
        zw.start_file("Payload/Evil.app/Info.plist", SimpleFileOptions::default()).unwrap();
        std::io::Write::write_all(&mut zw, &info).unwrap();
        build(&mut zw);
        zw.finish().unwrap();
        out
    }

    #[test]
    fn rejects_symlinks_escaping_the_bundle() {
        use zip::write::SimpleFileOptions;
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, "untouched").unwrap();
        // A plugin folder that is really a link to somewhere else on disk.
        let target = dir.path().to_string_lossy().to_string();
        let ipa = zip_with(dir.path(), |zw| {
            zw.add_symlink("Payload/Evil.app/PlugIns", target.clone(), SimpleFileOptions::default()).unwrap();
        });
        let err = prepare(&ipa, &dir.path().join("work"), &InstallOptions::default()).unwrap_err();
        assert_eq!(crate::errors::explain(&err).code, "unsafe_bundle");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");

        // The `.` + `..` trick that fools lexical checks.
        let ipa = zip_with(dir.path(), |zw| {
            zw.add_symlink("Payload/Evil.app/L", ".", SimpleFileOptions::default()).unwrap();
            zw.add_symlink("Payload/Evil.app/X", "L/../../../..", SimpleFileOptions::default()).unwrap();
        });
        assert!(prepare(&ipa, &dir.path().join("work2"), &InstallOptions::default()).is_err());

        // Internal links (like framework Versions/Current) are fine to keep.
        assert!(valid_file_name("Evil") && !valid_file_name("../Evil") && !valid_file_name("/bin/ls") && !valid_file_name(""));
    }

    #[test]
    fn rejects_executable_path_traversal() {
        use zip::write::SimpleFileOptions;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("trav.ipa");
        let mut zw = zip::ZipWriter::new(std::fs::File::create(&out).unwrap());
        let mut info = Vec::new();
        plist::to_writer_xml(&mut info, &plist::Dictionary::from_iter([
            ("CFBundleIdentifier".to_string(), plist::Value::from("com.evil.app")),
            ("CFBundleExecutable".to_string(), plist::Value::from("../../../victim")),
        ]))
        .unwrap();
        zw.start_file("Payload/Evil.app/Info.plist", SimpleFileOptions::default()).unwrap();
        std::io::Write::write_all(&mut zw, &info).unwrap();
        zw.finish().unwrap();
        let err = prepare(&out, &dir.path().join("work"), &InstallOptions::default()).unwrap_err();
        assert_eq!(crate::errors::explain(&err).code, "unsafe_bundle");
    }

    #[test]
    fn signing_key_apps_need_consent() {
        use zip::write::SimpleFileOptions;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("ss.ipa");
        let mut zw = zip::ZipWriter::new(std::fs::File::create(&out).unwrap());
        let mut info = Vec::new();
        plist::to_writer_xml(&mut info, &plist::Dictionary::from_iter([
            ("CFBundleIdentifier".to_string(), plist::Value::from("com.SideStore.SideStore")),
            ("CFBundleExecutable".to_string(), plist::Value::from("SideStore")),
        ]))
        .unwrap();
        zw.start_file("Payload/SideStore.app/Info.plist", SimpleFileOptions::default()).unwrap();
        std::io::Write::write_all(&mut zw, &info).unwrap();
        zw.finish().unwrap();
        // Make the executable exist (a tiny Mach-O) by preparing from a dir instead.
        let app = dir.path().join("SideStore.app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("Info.plist"), &info).unwrap();
        let c = dir.path().join("m.c");
        std::fs::write(&c, "int main(void){return 0;}").unwrap();
        cc(&["-arch", "arm64", "-o", app.join("SideStore").to_str().unwrap(), c.to_str().unwrap()]);
        let err = prepare(&app, &dir.path().join("w1"), &InstallOptions::default()).unwrap_err();
        assert_eq!(crate::errors::explain(&err).code, "signing_key_requested");
        let ok = InstallOptions { allow_signing_key: true, ..Default::default() };
        assert!(prepare(&app, &dir.path().join("w2"), &ok).is_ok());
        // Renaming the bundle ID means it no longer gets the key.
        let renamed = InstallOptions { bundle_id: Some("com.me.store".into()), ..Default::default() };
        assert!(prepare(&app, &dir.path().join("w3"), &renamed).is_ok());
    }
}
