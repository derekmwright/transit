//! CLAP presets as files (issue #68). CLAP defines no preset file format of its own (its preset
//! discovery extension indexes files each plug-in defines), so transit stores a CLAP preset in a
//! small documented container around the plug-in's `clap.state` blob, the same state a session
//! stores:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 8 | `"TRCLAPPR"` |
//! | 8 | 4 | format version, 1 (`u32`, little-endian) |
//! | 12 | 4 | length `n` of the plug-in id (`u32`, at most 1024) |
//! | 16 | n | the CLAP plug-in id, UTF-8 (`com.vendor.plugin`, without transit's `clap:`) |
//! | 16 + n | 8 | length `m` of the state (`u64`, at most 64 MiB) |
//! | 24 + n | m | the plug-in's `clap.state` blob, as its `save` wrote it |
//!
//! Files use the extension [`EXTENSION`]. Safe code only; every input is hostile.

/// The file extension of transit's CLAP presets.
pub const EXTENSION: &str = "clappreset";

/// The container's magic.
pub const MAGIC: &[u8; 8] = b"TRCLAPPR";

const VERSION: u32 = 1;
const MAX_ID: usize = 1024;
/// Largest state accepted.
pub const MAX_STATE_BYTES: usize = 64 << 20;

/// A preset file for the plug-in `id` (transit's `clap:<id>`) holding `state`.
///
/// # Errors
/// An id that is not a CLAP id, or a state or id too large for the container.
pub fn file_from_state(id: &str, state: &[u8]) -> Result<Vec<u8>, String> {
    let plugin = crate::parse_id(id).ok_or_else(|| format!("`{id}` is not a CLAP plug-in"))?;
    if plugin.len() > MAX_ID || state.len() > MAX_STATE_BYTES {
        return Err("the plug-in's id or state is too large for a preset".into());
    }
    let mut out = Vec::with_capacity(24 + plugin.len() + state.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&u32::try_from(plugin.len()).map_err(|e| e.to_string())?.to_le_bytes());
    out.extend_from_slice(plugin.as_bytes());
    out.extend_from_slice(&u64::try_from(state.len()).map_err(|e| e.to_string())?.to_le_bytes());
    out.extend_from_slice(state);
    Ok(out)
}

/// The plug-in id (without `clap:`) and the state a preset file holds.
///
/// # Errors
/// A file that is not a transit CLAP preset, a newer version, or one that ends early.
pub fn decode(file: &[u8]) -> Result<(String, Vec<u8>), String> {
    let bad = |why: &str| format!("not a valid CLAP preset: {why}");
    if file.get(..8) != Some(&MAGIC[..]) {
        return Err(bad("no transit CLAP preset header"));
    }
    let u32_at = |at: usize| file.get(at..at + 4).and_then(|b| b.try_into().ok()).map(u32::from_le_bytes);
    let version = u32_at(8).ok_or_else(|| bad("short header"))?;
    if version != VERSION {
        return Err(format!("CLAP preset version {version} is not one this build reads ({VERSION})"));
    }
    let n = u32_at(12).ok_or_else(|| bad("short header"))? as usize;
    if n == 0 || n > MAX_ID {
        return Err(bad("plug-in id length"));
    }
    let id = std::str::from_utf8(file.get(16..16 + n).ok_or_else(|| bad("ends in the plug-in id"))?).map_err(|_| bad("plug-in id is not UTF-8"))?;
    let m = file.get(16 + n..24 + n).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes).ok_or_else(|| bad("ends before the state"))?;
    let m = usize::try_from(m).ok().filter(|&m| m <= MAX_STATE_BYTES).ok_or_else(|| bad("state too large"))?;
    let state = file.get(24 + n..24 + n + m).ok_or_else(|| bad("ends in the state"))?;
    if file.len() != 24 + n + m {
        return Err(bad("trailing bytes"));
    }
    Ok((id.to_owned(), state.to_vec()))
}

/// The state a preset file holds for the plug-in `id` (transit's `clap:<id>`).
///
/// # Errors
/// A file that is not a transit CLAP preset, or one for another plug-in.
pub fn state_from_file(id: &str, file: &[u8]) -> Result<Vec<u8>, String> {
    let plugin = crate::parse_id(id).ok_or_else(|| format!("`{id}` is not a CLAP plug-in"))?;
    let (theirs, state) = decode(file)?;
    if theirs != plugin {
        return Err(format!("that preset is for `{theirs}`"));
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_file_is_the_documented_layout() {
        let file = file_from_state("clap:com.example.gain", b"\x01\x02state").unwrap();
        let mut want = b"TRCLAPPR".to_vec();
        want.extend_from_slice(&1u32.to_le_bytes());
        want.extend_from_slice(&16u32.to_le_bytes());
        want.extend_from_slice(b"com.example.gain");
        want.extend_from_slice(&7u64.to_le_bytes());
        want.extend_from_slice(b"\x01\x02state");
        assert_eq!(file, want);
        assert_eq!(state_from_file("clap:com.example.gain", &file).unwrap(), b"\x01\x02state");
        assert!(state_from_file("clap:com.example.other", &file).unwrap_err().contains("com.example.gain"));
        assert!(file_from_state("vst3:00", b"x").is_err());
        // An empty state is a valid preset (a plug-in with nothing to say).
        assert_eq!(state_from_file("clap:a", &file_from_state("clap:a", b"").unwrap()).unwrap(), b"");
    }

    #[test]
    fn hostile_files_are_refused() {
        let good = file_from_state("clap:a.b", b"xyz").unwrap();
        assert!(decode(b"").is_err());
        assert!(decode(&good[..good.len() - 1]).is_err(), "ends early");
        let mut f = good.clone();
        f.push(0);
        assert!(decode(&f).is_err(), "trailing bytes");
        let mut f = good.clone();
        f[8] = 2;
        assert!(decode(&f).unwrap_err().contains("version 2"));
        let mut f = good.clone();
        f[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&f).is_err(), "absurd id length");
        let mut f = good.clone();
        f[19..27].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode(&f).is_err(), "absurd state length");
        let mut f = good;
        f[16] = 0xFF;
        assert!(decode(&f).is_err(), "id not UTF-8");
    }
}
