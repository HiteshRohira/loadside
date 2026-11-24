//! Minimal Mach-O editing: add `LC_LOAD_DYLIB` / `LC_RPATH` commands into the
//! header padding, rewrite dylib load paths, and detect FairPlay encryption.
//! Handles thin and fat (32/64-bit fat headers) binaries; every slice is
//! checked and edited on its own. Code signatures are not touched — the bundle
//! is re-signed afterwards anyway.
//!
//! All offsets come from untrusted files, so every read is bounds-checked
//! against its slice and nothing here can panic on malformed input.

use anyhow::{Context, Result, bail};

const MH_MAGIC: u32 = 0xfeed_face;
const MH_MAGIC_64: u32 = 0xfeed_facf;
const FAT_MAGIC: u32 = 0xcafe_babe;
const FAT_MAGIC_64: u32 = 0xcafe_babf;
/// Real fat binaries have a handful of slices; Java class files share the magic.
const MAX_FAT_ARCHS: u32 = 32;

const LC_SEGMENT: u32 = 0x1;
const LC_SEGMENT_64: u32 = 0x19;
const LC_LOAD_DYLIB: u32 = 0xc;
const LC_LOAD_WEAK_DYLIB: u32 = 0x8000_0018;
const LC_REEXPORT_DYLIB: u32 = 0x8000_001f;
const LC_LAZY_LOAD_DYLIB: u32 = 0x20;
const LC_LOAD_UPWARD_DYLIB: u32 = 0x8000_0023;
const LC_RPATH: u32 = 0x8000_001c;
const LC_ENCRYPTION_INFO: u32 = 0x21;
const LC_ENCRYPTION_INFO_64: u32 = 0x2c;

fn is_dylib_load(cmd: u32) -> bool {
    matches!(cmd, LC_LOAD_DYLIB | LC_LOAD_WEAK_DYLIB | LC_REEXPORT_DYLIB | LC_LAZY_LOAD_DYLIB | LC_LOAD_UPWARD_DYLIB)
}

fn get(d: &[u8], o: usize, n: usize) -> Result<&[u8]> {
    o.checked_add(n).and_then(|e| d.get(o..e)).context("truncated Mach-O")
}
fn be32(d: &[u8], o: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(get(d, o, 4)?.try_into()?))
}
fn be64(d: &[u8], o: usize) -> Result<u64> {
    Ok(u64::from_be_bytes(get(d, o, 8)?.try_into()?))
}
fn le32(d: &[u8], o: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(get(d, o, 4)?.try_into()?))
}
fn le64(d: &[u8], o: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(get(d, o, 8)?.try_into()?))
}
fn put32(d: &mut [u8], o: usize, v: u32) -> Result<()> {
    o.checked_add(4)
        .and_then(|e| d.get_mut(o..e))
        .context("write out of bounds")?
        .copy_from_slice(&v.to_le_bytes());
    Ok(())
}

pub fn is_macho(d: &[u8]) -> bool {
    if d.len() < 8 {
        return false;
    }
    let le = u32::from_le_bytes(d[0..4].try_into().unwrap());
    let be = u32::from_be_bytes(d[0..4].try_into().unwrap());
    matches!(le, MH_MAGIC | MH_MAGIC_64)
        || (matches!(be, FAT_MAGIC | FAT_MAGIC_64) && be32(d, 4).map(|n| n > 0 && n <= MAX_FAT_ARCHS).unwrap_or(false))
}

/// Slice layout from the fat header alone (offsets may lie beyond `head`).
/// For thin binaries returns `[(0, u64::MAX)]`.
pub fn fat_layout(head: &[u8]) -> Result<Vec<(u64, u64)>> {
    let magic = be32(head, 0)?;
    if magic != FAT_MAGIC && magic != FAT_MAGIC_64 {
        return Ok(vec![(0, u64::MAX)]);
    }
    let n = be32(head, 4)?;
    if n == 0 || n > MAX_FAT_ARCHS {
        bail!("implausible fat header ({n} architectures)");
    }
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n as usize {
        out.push(if magic == FAT_MAGIC {
            let b = 8 + i * 20;
            (be32(head, b + 8)? as u64, be32(head, b + 12)? as u64)
        } else {
            let b = 8 + i * 32;
            (be64(head, b + 8)?, be64(head, b + 16)?)
        });
    }
    Ok(out)
}

/// (offset, size) of each architecture slice, validated against `d`.
fn slices(d: &[u8]) -> Result<Vec<(usize, usize)>> {
    let layout = fat_layout(d)?;
    if layout == [(0, u64::MAX)] {
        return Ok(vec![(0, d.len())]);
    }
    let mut out = Vec::with_capacity(layout.len());
    for (off, size) in layout {
        let (off, size) = (usize::try_from(off)?, usize::try_from(size)?);
        if off.checked_add(size).is_none_or(|end| end > d.len()) {
            bail!("fat slice out of bounds");
        }
        out.push((off, size));
    }
    Ok(out)
}

#[derive(Clone, Copy)]
struct Header {
    base: usize,
    /// End of this slice (exclusive), never beyond the buffer.
    limit: usize,
    is64: bool,
    ncmds: u32,
    sizeofcmds: u32,
}

impl Header {
    fn size(&self) -> usize {
        if self.is64 { 32 } else { 28 }
    }
    fn cmds_start(&self) -> usize {
        self.base + self.size()
    }
    fn cmds_end(&self) -> usize {
        self.cmds_start() + self.sizeofcmds as usize
    }
}

fn header(d: &[u8], base: usize, size: usize) -> Result<Header> {
    let limit = base.checked_add(size).filter(|l| *l <= d.len()).context("slice out of bounds")?;
    let magic = le32(d, base)?;
    let is64 = match magic {
        MH_MAGIC_64 => true,
        MH_MAGIC => false,
        _ => bail!("not a Mach-O slice (magic {magic:#x})"),
    };
    let h = Header { base, limit, is64, ncmds: le32(d, base + 16)?, sizeofcmds: le32(d, base + 20)? };
    if h.cmds_start().checked_add(h.sizeofcmds as usize).is_none_or(|e| e > limit) {
        bail!("load commands run past the end of the slice");
    }
    // Every command is at least 8 bytes.
    if h.ncmds as usize > h.sizeofcmds as usize / 8 {
        bail!("implausible load command count");
    }
    Ok(h)
}

/// Iterate load commands: (absolute offset, cmd, cmdsize).
fn commands(d: &[u8], h: &Header) -> Result<Vec<(usize, u32, u32)>> {
    let mut out = Vec::with_capacity(h.ncmds as usize);
    let mut off = h.cmds_start();
    let end = h.cmds_end();
    for _ in 0..h.ncmds {
        let cmd = le32(d, off)?;
        let size = le32(d, off + 4)?;
        if size < 8 || off.checked_add(size as usize).is_none_or(|e| e > end) {
            bail!("corrupt load command");
        }
        out.push((off, cmd, size));
        off += size as usize;
    }
    Ok(out)
}

/// NUL-terminated string stored at `cmd_off + str_off` inside a command.
fn cmd_str(d: &[u8], cmd_off: usize, cmd_size: u32, str_off: usize) -> Result<String> {
    if str_off >= cmd_size as usize {
        bail!("string offset outside load command");
    }
    let bytes = get(d, cmd_off + str_off, cmd_size as usize - str_off)?;
    let n = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    Ok(String::from_utf8_lossy(&bytes[..n]).into_owned())
}

/// Bytes available between the end of the load commands and the first
/// section/segment data in this slice.
fn free_space(d: &[u8], h: &Header) -> Result<usize> {
    let mut min_off = usize::MAX;
    for (off, cmd, size) in commands(d, h)? {
        let (fileoff, filesize, nsects, first_sect, sect_size, offset_field, flags_field) = match cmd {
            LC_SEGMENT_64 => (le64(d, off + 40)? as usize, le64(d, off + 48)? as usize, le32(d, off + 64)? as usize, 72, 80, 48, 64),
            LC_SEGMENT => (le32(d, off + 32)? as usize, le32(d, off + 36)? as usize, le32(d, off + 48)? as usize, 56, 68, 40, 56),
            _ => continue,
        };
        if first_sect + nsects.saturating_mul(sect_size) > size as usize {
            bail!("segment sections run past the load command");
        }
        for s in 0..nsects {
            let so = off + first_sect + s * sect_size;
            let sect_off = le32(d, so + offset_field)? as usize;
            let flags = le32(d, so + flags_field)?;
            let zerofill = matches!(flags & 0xff, 0x1 | 0xc | 0x12);
            if sect_off != 0 && !zerofill {
                min_off = min_off.min(sect_off);
            }
        }
        if nsects == 0 && fileoff != 0 && filesize != 0 {
            min_off = min_off.min(fileoff);
        }
    }
    if min_off == usize::MAX {
        bail!("no segments found");
    }
    let used = h.size() + h.sizeofcmds as usize; // relative to slice base
    let slice_len = h.limit - h.base;
    Ok(min_off.min(slice_len).saturating_sub(used))
}

fn append_command(d: &mut [u8], h: &Header, cmd_bytes: &[u8]) -> Result<()> {
    let space = free_space(d, h)?;
    if cmd_bytes.len() > space {
        bail!(
            "not enough space for new load command in header padding ({} needed, {} free)",
            cmd_bytes.len(),
            space
        );
    }
    let at = h.cmds_end();
    let end = at + cmd_bytes.len();
    if end > h.limit {
        bail!("load command would run past the slice");
    }
    d[at..end].copy_from_slice(cmd_bytes);
    put32(d, h.base + 16, h.ncmds + 1)?;
    put32(d, h.base + 20, h.sizeofcmds + cmd_bytes.len() as u32)?;
    Ok(())
}

/// Remove the command at `off` (size `size`), shifting later commands up.
fn remove_command(d: &mut [u8], h: &Header, off: usize, size: u32) -> Result<()> {
    let end = h.cmds_end();
    let size = size as usize;
    if off < h.cmds_start() || off + size > end {
        bail!("command outside load command area");
    }
    d.copy_within(off + size..end, off);
    d[end - size..end].fill(0);
    put32(d, h.base + 16, h.ncmds - 1)?;
    put32(d, h.base + 20, h.sizeofcmds - size as u32)?;
    Ok(())
}

fn padded_str_cmd(cmd: u32, header_len: usize, s: &str, align: usize, fill: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let raw = header_len + s.len() + 1;
    let size = raw.div_ceil(align) * align;
    let mut v = Vec::with_capacity(size);
    v.extend_from_slice(&cmd.to_le_bytes());
    v.extend_from_slice(&(size as u32).to_le_bytes());
    fill(&mut v);
    v.extend_from_slice(s.as_bytes());
    v.resize(size, 0);
    v
}

fn dylib_cmd(cmd: u32, path: &str, timestamp: u32, current: u32, compat: u32) -> Vec<u8> {
    padded_str_cmd(cmd, 24, path, 8, |v| {
        v.extend_from_slice(&24u32.to_le_bytes()); // name offset
        v.extend_from_slice(&timestamp.to_le_bytes());
        v.extend_from_slice(&current.to_le_bytes());
        v.extend_from_slice(&compat.to_le_bytes());
    })
}

fn slice_headers(d: &[u8]) -> Result<Vec<Header>> {
    slices(d)?.into_iter().map(|(b, s)| header(d, b, s)).collect()
}

fn slice_strings(d: &[u8], h: &Header, want: impl Fn(u32) -> bool) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (off, cmd, size) in commands(d, h)? {
        if want(cmd) {
            let so = le32(d, off + 8)? as usize;
            out.push(cmd_str(d, off, size, so)?);
        }
    }
    Ok(out)
}

fn union(lists: Vec<Vec<String>>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in lists {
        for s in l {
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

/// Dylib load paths across all slices (first slice's order first).
pub fn load_paths(d: &[u8]) -> Result<Vec<String>> {
    let lists = slice_headers(d)?.iter().map(|h| slice_strings(d, h, is_dylib_load)).collect::<Result<Vec<_>>>()?;
    Ok(union(lists))
}

pub fn rpaths(d: &[u8]) -> Result<Vec<String>> {
    let lists = slice_headers(d)?.iter().map(|h| slice_strings(d, h, |c| c == LC_RPATH)).collect::<Result<Vec<_>>>()?;
    Ok(union(lists))
}

/// Add a load command for `path` to every slice that lacks it. Checks all
/// slices for room first, so a failure leaves the binary unchanged. Returns
/// false if every slice already had it.
pub fn add_load_dylib(d: &mut [u8], path: &str, weak: bool) -> Result<bool> {
    let cmd = if weak { LC_LOAD_WEAK_DYLIB } else { LC_LOAD_DYLIB };
    add_to_slices(d, &dylib_cmd(cmd, path, 2, 0x0001_0000, 0x0001_0000), |d, h| {
        Ok(slice_strings(d, h, is_dylib_load)?.iter().any(|p| p == path))
    })
    .with_context(|| format!("injecting {path}"))
}

pub fn ensure_rpath(d: &mut [u8], rpath: &str) -> Result<bool> {
    let bytes = padded_str_cmd(LC_RPATH, 12, rpath, 8, |v| v.extend_from_slice(&12u32.to_le_bytes()));
    add_to_slices(d, &bytes, |d, h| Ok(slice_strings(d, h, |c| c == LC_RPATH)?.iter().any(|p| p == rpath))).context("adding rpath")
}

fn add_to_slices(d: &mut [u8], bytes: &[u8], has: impl Fn(&[u8], &Header) -> Result<bool>) -> Result<bool> {
    let mut todo = Vec::new();
    for h in slice_headers(d)? {
        if !has(d, &h)? {
            let space = free_space(d, &h)?;
            if bytes.len() > space {
                bail!("not enough space for new load command in header padding ({} needed, {} free)", bytes.len(), space);
            }
            todo.push(h);
        }
    }
    for h in &todo {
        append_command(d, h, bytes)?;
    }
    Ok(!todo.is_empty())
}

/// Rewrite dylib load paths where `map` returns a replacement. Short enough
/// paths are rewritten in place; longer ones replace the command with a new
/// one at the end (using header padding). Returns the (old, new) pairs that
/// changed and the paths that couldn't be rewritten for lack of space.
pub fn rewrite_load_paths(
    d: &mut [u8],
    map: impl Fn(&str) -> Option<String>,
) -> Result<(Vec<(String, String)>, Vec<String>)> {
    let mut changed: Vec<(String, String)> = Vec::new();
    let mut too_long: Vec<String> = Vec::new();
    for (base, size) in slices(d)? {
        // Commands move when one is replaced, so re-read after each change.
        let mut i = 0;
        // Replaced commands are revisited at the end; bound the work in case
        // `map` keeps producing new paths.
        let mut budget = 4096;
        loop {
            budget -= 1;
            if budget == 0 {
                bail!("load path rewriting did not converge");
            }
            let h = header(d, base, size)?;
            let cmds = commands(d, &h)?;
            let Some(&(off, cmd, csize)) = cmds.get(i) else { break };
            i += 1;
            if !is_dylib_load(cmd) {
                continue;
            }
            let name_off = le32(d, off + 8)? as usize;
            let old = cmd_str(d, off, csize, name_off)?;
            let Some(new) = map(&old) else { continue };
            if new == old {
                continue;
            }
            let room = csize as usize - name_off;
            if new.len() < room {
                let start = off + name_off;
                d[start..off + csize as usize].fill(0);
                d[start..start + new.len()].copy_from_slice(new.as_bytes());
            } else {
                let (ts, cur, compat) = (le32(d, off + 12)?, le32(d, off + 16)?, le32(d, off + 20)?);
                let bytes = dylib_cmd(cmd, &new, ts, cur, compat);
                // Room after removing the old command.
                if bytes.len() > free_space(d, &h)? + csize as usize {
                    if !too_long.contains(&old) {
                        too_long.push(old);
                    }
                    continue;
                }
                remove_command(d, &h, off, csize)?;
                let h = header(d, base, size)?;
                append_command(d, &h, &bytes)?;
                i -= 1; // the next command moved into this index
            }
            if !changed.iter().any(|(o, _)| o == &old) {
                changed.push((old, new));
            }
        }
    }
    Ok((changed, too_long))
}

fn slice_encrypted(d: &[u8], h: &Header) -> Result<bool> {
    for (off, cmd, _) in commands(d, h)? {
        if (cmd == LC_ENCRYPTION_INFO || cmd == LC_ENCRYPTION_INFO_64) && le32(d, off + 16)? != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// True if any slice has `cryptid != 0` (FairPlay encrypted App Store binary).
pub fn is_encrypted(d: &[u8]) -> Result<bool> {
    let mut any = false;
    for (base, size) in slices(d)? {
        let h = header(d, base, size)?;
        any |= slice_encrypted(d, &h)?;
    }
    Ok(any)
}

/// Encryption check from the first bytes of each slice (enough to cover the
/// load commands), for when the whole binary isn't in memory.
pub fn is_encrypted_from_prefixes(prefixes: &[Vec<u8>]) -> Result<bool> {
    let mut any = false;
    for p in prefixes {
        let h = header(p, 0, p.len())?;
        any |= slice_encrypted(p, &h)?;
    }
    Ok(any)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(extra: &[&str]) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("t.c");
        std::fs::write(&src, "int main(void){return 0;}").unwrap();
        let out = dir.path().join("t");
        let st = std::process::Command::new("cc").args(extra).arg("-o").arg(&out).arg(&src).status().expect("cc");
        assert!(st.success());
        std::fs::read(out).unwrap()
    }

    /// A fat (arm64 + x86_64) binary with header padding, like real app binaries.
    fn test_binary() -> Vec<u8> {
        compile(&["-arch", "arm64", "-arch", "x86_64", "-Wl,-headerpad,0x400"])
    }

    #[test]
    fn reports_lack_of_space() {
        let mut d = std::fs::read("/bin/ls").unwrap();
        let mut err = None;
        for i in 0..64 {
            if let Err(e) = add_load_dylib(&mut d, &format!("@rpath/Long-Tweak-Name-Number-{i}.dylib"), false) {
                err = Some(format!("{e:#}"));
                break;
            }
        }
        assert!(err.unwrap().contains("not enough space"));
    }

    #[test]
    fn inject_roundtrip_on_real_binary() {
        let mut d = test_binary();
        assert_eq!(slices(&d).unwrap().len(), 2);
        assert!(is_macho(&d));
        let before = load_paths(&d).unwrap();
        assert!(add_load_dylib(&mut d, "@rpath/Tweak.dylib", false).unwrap());
        assert!(!add_load_dylib(&mut d, "@rpath/Tweak.dylib", false).unwrap());
        let after = load_paths(&d).unwrap();
        assert_eq!(after.len(), before.len() + 1);
        assert_eq!(after.last().unwrap(), "@rpath/Tweak.dylib");
        for h in slice_headers(&d).unwrap() {
            assert_eq!(slice_strings(&d, &h, is_dylib_load).unwrap().len(), after.len());
        }
        assert!(ensure_rpath(&mut d, "@executable_path/Frameworks").unwrap());
        assert!(rpaths(&d).unwrap().contains(&"@executable_path/Frameworks".to_string()));
        // In place (shorter) ...
        let (changed, _) = rewrite_load_paths(&mut d, |p| (p == "@rpath/Tweak.dylib").then(|| "@rpath/T.dylib".into())).unwrap();
        assert_eq!(changed.len(), 1);
        assert!(load_paths(&d).unwrap().contains(&"@rpath/T.dylib".to_string()));
        // ... and longer than the command (replaced via padding).
        let long = "@rpath/CydiaSubstrate.framework/CydiaSubstrate";
        let (changed, too_long) = rewrite_load_paths(&mut d, |p| (p == "@rpath/T.dylib").then(|| long.into())).unwrap();
        assert_eq!((changed.len(), too_long.len()), (1, 0));
        let paths = load_paths(&d).unwrap();
        assert!(paths.contains(&long.to_string()) && !paths.contains(&"@rpath/T.dylib".to_string()));
        assert_eq!(paths.len(), after.len());
        assert!(!is_encrypted(&d).unwrap());
        // Still a valid binary according to the system tools.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("b");
        std::fs::write(&f, &d).unwrap();
        let out = std::process::Command::new("otool").arg("-L").arg(&f).output().unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains(long));
    }

    #[test]
    fn prefixes_detect_encryption_state() {
        let d = test_binary();
        let prefixes: Vec<Vec<u8>> = fat_layout(&d)
            .unwrap()
            .into_iter()
            .map(|(o, _)| d[o as usize..(o as usize + 64 * 1024).min(d.len())].to_vec())
            .collect();
        assert!(!is_encrypted_from_prefixes(&prefixes).unwrap());
    }

    #[test]
    fn malformed_input_errors_instead_of_panicking() {
        let good = test_binary();
        // Truncations, huge counts and garbage must all return Err/Ok, never panic.
        for cut in [0, 4, 8, 20, 40, 100, 4096, good.len() / 2] {
            let d = &good[..cut.min(good.len())];
            let _ = load_paths(d);
            let _ = is_encrypted(d);
            let mut m = d.to_vec();
            let _ = add_load_dylib(&mut m, "@rpath/x.dylib", false);
        }
        let mut huge = good.clone();
        huge[4..8].copy_from_slice(&0xffff_ffffu32.to_be_bytes()); // nfat_arch
        assert!(load_paths(&huge).is_err());
        let mut thin = compile(&["-arch", "arm64"]);
        thin[16..20].copy_from_slice(&0x7fff_ffffu32.to_le_bytes()); // ncmds
        assert!(load_paths(&thin).is_err());
        let mut thin = compile(&["-arch", "arm64"]);
        thin[20..24].copy_from_slice(&0xffff_fff0u32.to_le_bytes()); // sizeofcmds
        assert!(is_encrypted(&thin).is_err());
    }
}
