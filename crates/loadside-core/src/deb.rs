//! Extract jailbreak tweak packages (.deb) so their dylibs, frameworks and
//! resource bundles can be injected into an app, like Sideloadly does.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug, Default)]
pub struct DebContents {
    pub dylibs: Vec<PathBuf>,
    pub frameworks: Vec<PathBuf>,
    pub bundles: Vec<PathBuf>,
}

/// Tweak packages are small; these caps stop decompression bombs.
const MAX_DEB_BYTES: u64 = 512 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_TAR_ENTRIES: usize = 100_000;

/// A writer that refuses to grow past a limit (for decoders that only write).
struct CappedVec(Vec<u8>, u64);
impl std::io::Write for CappedVec {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.0.len() as u64 + buf.len() as u64 > self.1 {
            return Err(std::io::Error::other("tweak package expands to too much data"));
        }
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Unpack `data.tar.*` from the .deb into `dest` and collect injectable items.
pub fn extract(deb: &Path, dest: &Path) -> Result<DebContents> {
    let len = std::fs::metadata(deb).with_context(|| format!("reading {}", deb.display()))?.len();
    if len > MAX_DEB_BYTES {
        bail!("{} is too large for a tweak package ({} MB)", deb.display(), len >> 20);
    }
    let bytes = std::fs::read(deb).with_context(|| format!("reading {}", deb.display()))?;
    let (name, data) = find_data_member(&bytes)?;
    // Stream the payload straight into tar where the decoder allows it.
    let reader: Box<dyn Read + '_> = if name.ends_with(".gz") {
        Box::new(flate2::read::GzDecoder::new(data))
    } else if name.ends_with(".zst") {
        Box::new(ruzstd::decoding::StreamingDecoder::new(data).map_err(|e| anyhow::anyhow!("zstd: {e}"))?)
    } else if name.ends_with(".xz") || name.ends_with(".lzma") {
        let mut out = CappedVec(Vec::new(), MAX_UNPACKED_BYTES);
        let mut input = std::io::BufReader::new(data);
        let r = if name.ends_with(".xz") { lzma_rs::xz_decompress(&mut input, &mut out) } else { lzma_rs::lzma_decompress(&mut input, &mut out) };
        r.map_err(|e| anyhow::anyhow!("{name}: {e:?}"))?;
        Box::new(std::io::Cursor::new(out.0))
    } else if name == "data.tar" {
        Box::new(data)
    } else {
        bail!("unsupported .deb payload compression: {name}");
    };

    std::fs::create_dir_all(dest)?;
    let mut archive = tar::Archive::new(reader.take(MAX_UNPACKED_BYTES));
    archive.set_preserve_permissions(false);
    archive.set_overwrite(true);
    let (mut count, mut total) = (0usize, 0u64);
    for entry in archive.entries()? {
        let mut entry = entry?;
        count += 1;
        total += entry.header().size().unwrap_or(0);
        if count > MAX_TAR_ENTRIES || total > MAX_UNPACKED_BYTES {
            bail!("{} expands to too much data", deb.display());
        }
        // Skip anything that would escape `dest` and all symlinks/devices.
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            continue;
        }
        entry.unpack_in(dest)?;
    }

    let mut out = DebContents::default();
    walk(dest, &mut out)?;
    if out.dylibs.is_empty() && out.frameworks.is_empty() {
        bail!("{} contains no dylibs or frameworks to inject", deb.display());
    }
    Ok(out)
}

fn find_data_member(bytes: &[u8]) -> Result<(String, &[u8])> {
    if !bytes.starts_with(b"!<arch>\n") {
        bail!("not a .deb (ar) archive");
    }
    let mut off = 8;
    while off + 60 <= bytes.len() {
        let hdr = &bytes[off..off + 60];
        let name = String::from_utf8_lossy(&hdr[0..16]).trim().trim_end_matches('/').to_string();
        let size: usize = String::from_utf8_lossy(&hdr[48..58]).trim().parse().context("bad ar header")?;
        let start = off + 60;
        let end = start.checked_add(size).filter(|e| *e <= bytes.len()).context("truncated .deb")?;
        if name.starts_with("data.tar") {
            return Ok((name, &bytes[start..end]));
        }
        off = end + (size % 2);
    }
    bail!("no data.tar in .deb")
}

fn walk(dir: &Path, out: &mut DebContents) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ft.is_dir() {
            match ext {
                "framework" => out.frameworks.push(path),
                // Settings bundles only work inside the Settings app.
                "bundle" if !path.to_string_lossy().contains("PreferenceBundles") => out.bundles.push(path),
                _ => walk(&path, out)?,
            }
        } else if ft.is_file() && ext == "dylib" {
            out.dylibs.push(path);
        }
    }
    Ok(())
}
