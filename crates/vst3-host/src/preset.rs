//! VST3 states as files: Steinberg's `.vstpreset` container, so a preset saved in transit loads in
//! any VST3 host and the other way round (issue #68), and transit's own in-session state blob
//! (component state plus controller state, see [`join_state`]).
//!
//! `.vstpreset` layout (VST3 SDK `public.sdk/source/vst/vstpresetfile.h`), little-endian:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 4 | `"VST3"` |
//! | 4 | 4 | format version, 1 (`i32`) |
//! | 8 | 32 | the class id, ASCII hex as `FUID::toString` writes it |
//! | 40 | 8 | offset of the chunk list (`i64`) |
//! | 48 | … | chunk data: `"Comp"` the component state, `"Cont"` the controller state |
//! | list | 8 + 20 n | `"List"`, entry count (`i32`), then per chunk: id (4), offset (`i64`), size (`i64`) |
//!
//! Safe code only; every input is hostile (sizes and offsets are checked against the file).

/// The file extension of VST3 presets.
pub const EXTENSION: &str = "vstpreset";

/// Largest component or controller state accepted (as the instance accepts).
pub const MAX_STATE_BYTES: usize = 64 << 20;

/// transit's in-session state container: magic, then the component and controller states, each
/// with its length (`u32`, little-endian).
pub const STATE_MAGIC: &[u8; 4] = b"SCV3";

const HEADER: usize = 48;
const VERSION: i32 = 1;
const MAX_CHUNKS: usize = 1024;

/// Component and controller state in transit's container (what a session stores).
#[must_use]
pub fn join_state(component: &[u8], controller: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(12 + component.len() + controller.len());
    out.extend_from_slice(STATE_MAGIC);
    out.extend_from_slice(&u32::try_from(component.len()).ok()?.to_le_bytes());
    out.extend_from_slice(component);
    out.extend_from_slice(&u32::try_from(controller.len()).ok()?.to_le_bytes());
    out.extend_from_slice(controller);
    Some(out)
}

/// Splits a state blob into (component, controller). A blob without the container's magic is
/// taken as a bare component state.
#[must_use]
pub fn split_state(data: &[u8]) -> Option<(&[u8], &[u8])> {
    if data.len() > MAX_STATE_BYTES.saturating_mul(2) {
        return None;
    }
    let Some(rest) = data.strip_prefix(STATE_MAGIC) else { return (!data.is_empty()).then_some((data, &[][..])) };
    let take = |b: &[u8]| -> Option<(usize, usize)> {
        let n = u32::from_le_bytes(b.get(..4)?.try_into().ok()?) as usize;
        (n <= MAX_STATE_BYTES && b.len() >= 4 + n).then_some((4, 4 + n))
    };
    let (a, b) = take(rest)?;
    let comp = rest.get(a..b)?;
    let rest = rest.get(b..)?;
    let (c, d) = take(rest)?;
    let ctrl = rest.get(c..d)?;
    (d == rest.len()).then_some((comp, ctrl))
}

/// The class id as `FUID::toString` writes it on this platform: on Windows the 16 bytes are a
/// COM GUID (the first three fields little-endian), elsewhere they are written in order. Either
/// way the same plug-in gets the same string, which is what other hosts compare.
#[must_use]
pub fn class_id_string(cid: &[u8; 16]) -> String {
    let order = byte_order();
    order.iter().map(|&i| format!("{:02X}", cid[i])).collect()
}

/// The 16 class-id bytes of this platform for a `FUID::toString` string (`None`: not 32 hex
/// digits).
#[must_use]
pub fn parse_class_id_string(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 16];
    for (k, &i) in byte_order().iter().enumerate() {
        out[i] = u8::from_str_radix(s.get(k * 2..k * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Which byte of the class id each pair of the string is.
fn byte_order() -> [usize; 16] {
    if cfg!(windows) { [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15] } else { [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15] }
}

/// A `.vstpreset` file for class `cid` with these states (the controller chunk is left out when
/// `controller` is empty, as the SDK does).
#[must_use]
pub fn encode(cid: &[u8; 16], component: &[u8], controller: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + component.len() + controller.len() + 48);
    out.extend_from_slice(b"VST3");
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(class_id_string(cid).as_bytes());
    out.extend_from_slice(&0i64.to_le_bytes()); // patched below
    let mut chunks: Vec<([u8; 4], usize, usize)> = Vec::new();
    for (id, data) in [(*b"Comp", component), (*b"Cont", controller)] {
        if id == *b"Cont" && data.is_empty() {
            continue;
        }
        chunks.push((id, out.len(), data.len()));
        out.extend_from_slice(data);
    }
    let list = out.len();
    out.extend_from_slice(b"List");
    out.extend_from_slice(&i32::try_from(chunks.len()).unwrap_or(0).to_le_bytes());
    for (id, offset, size) in chunks {
        out.extend_from_slice(&id);
        out.extend_from_slice(&i64::try_from(offset).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&i64::try_from(size).unwrap_or(0).to_le_bytes());
    }
    let at = i64::try_from(list).unwrap_or(0).to_le_bytes();
    if let Some(slot) = out.get_mut(40..48) {
        slot.copy_from_slice(&at);
    }
    out
}

/// What a `.vstpreset` file holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    /// The class id bytes (this platform's order).
    pub class_id: [u8; 16],
    /// The component state.
    pub component: Vec<u8>,
    /// The controller state (empty when the file has none).
    pub controller: Vec<u8>,
}

fn read_i64(b: &[u8], at: usize) -> Option<i64> {
    Some(i64::from_le_bytes(b.get(at..at.checked_add(8)?)?.try_into().ok()?))
}

fn read_i32(b: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

/// Reads a `.vstpreset` file.
///
/// # Errors
/// A file that is not a VST3 preset, or whose chunk list or chunks lie outside it.
pub fn decode(file: &[u8]) -> Result<Decoded, String> {
    let bad = |why: &str| format!("not a valid VST3 preset: {why}");
    if file.len() < HEADER || file.get(..4) != Some(b"VST3") {
        return Err(bad("no VST3 header"));
    }
    if file.len() > MAX_STATE_BYTES.saturating_mul(2).saturating_add(4096) {
        return Err(bad("too large"));
    }
    let cid = std::str::from_utf8(file.get(8..40).ok_or_else(|| bad("short header"))?).map_err(|_| bad("class id is not text"))?;
    let class_id = parse_class_id_string(cid).ok_or_else(|| bad("class id is not 32 hex digits"))?;
    let list = usize::try_from(read_i64(file, 40).ok_or_else(|| bad("short header"))?).map_err(|_| bad("chunk list offset"))?;
    if file.get(list..list.saturating_add(4)) != Some(b"List") {
        return Err(bad("no chunk list"));
    }
    let n = usize::try_from(read_i32(file, list + 4).ok_or_else(|| bad("short chunk list"))?).map_err(|_| bad("chunk count"))?;
    if n > MAX_CHUNKS {
        return Err(bad("too many chunks"));
    }
    let (mut component, mut controller) = (None, Vec::new());
    for k in 0..n {
        let e = list + 8 + k * 20;
        let id = file.get(e..e + 4).ok_or_else(|| bad("short chunk list"))?;
        let offset = usize::try_from(read_i64(file, e + 4).ok_or_else(|| bad("short chunk list"))?).map_err(|_| bad("chunk offset"))?;
        let size = usize::try_from(read_i64(file, e + 12).ok_or_else(|| bad("short chunk list"))?).map_err(|_| bad("chunk size"))?;
        if size > MAX_STATE_BYTES {
            return Err(bad("chunk too large"));
        }
        let data = file.get(offset..offset.checked_add(size).ok_or_else(|| bad("chunk size"))?).ok_or_else(|| bad("chunk outside the file"))?;
        match id {
            b"Comp" => component = Some(data.to_vec()),
            b"Cont" => controller = data.to_vec(),
            _ => {}
        }
    }
    let component = component.ok_or_else(|| bad("no component state"))?;
    Ok(Decoded { class_id, component, controller })
}

/// A `.vstpreset` file for the plug-in `id` (`vst3:<class id>`) from a state blob as a session
/// stores it.
///
/// # Errors
/// An id that is not a VST3 id, or a blob that is not a VST3 state.
pub fn file_from_state(id: &str, state: &[u8]) -> Result<Vec<u8>, String> {
    let cid = crate::parse_id(id).ok_or_else(|| format!("`{id}` is not a VST3 plug-in"))?;
    let (comp, ctrl) = split_state(state).ok_or_else(|| "the plug-in's state is not a VST3 state".to_string())?;
    Ok(encode(&cid, comp, ctrl))
}

/// The state blob (as a session stores it) of a `.vstpreset` file for the plug-in `id`.
///
/// # Errors
/// A file that is not a VST3 preset, or one for another plug-in.
pub fn state_from_file(id: &str, file: &[u8]) -> Result<Vec<u8>, String> {
    let cid = crate::parse_id(id).ok_or_else(|| format!("`{id}` is not a VST3 plug-in"))?;
    let d = decode(file)?;
    if d.class_id != cid {
        return Err(format!("that preset is for another plug-in (class {})", class_id_string(&d.class_id)));
    }
    join_state(&d.component, &d.controller).ok_or_else(|| "the preset's state is too large".to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const CID: [u8; 16] = [0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0xFE, 0xDC, 0xBA, 0x98, 0x76, 0x54, 0x32, 0x10];

    /// A `.vstpreset` built by hand from the SDK's layout, field by field, for comparison.
    fn reference(cid_text: &str, comp: &[u8], ctrl: &[u8]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(b"VST3"); // chunk id
        f.extend_from_slice(&[1, 0, 0, 0]); // version 1
        f.extend_from_slice(cid_text.as_bytes()); // 32 ASCII hex digits
        let list = 48 + comp.len() + ctrl.len();
        f.extend_from_slice(&(list as u64).to_le_bytes()); // chunk list offset
        f.extend_from_slice(comp); // at 48
        f.extend_from_slice(ctrl); // at 48 + comp
        f.extend_from_slice(b"List");
        f.extend_from_slice(&[2, 0, 0, 0]);
        f.extend_from_slice(b"Comp");
        f.extend_from_slice(&48u64.to_le_bytes());
        f.extend_from_slice(&(comp.len() as u64).to_le_bytes());
        f.extend_from_slice(b"Cont");
        f.extend_from_slice(&((48 + comp.len()) as u64).to_le_bytes());
        f.extend_from_slice(&(ctrl.len() as u64).to_le_bytes());
        f
    }

    #[test]
    fn the_file_matches_the_sdk_layout_byte_for_byte() {
        let text = if cfg!(windows) { "67452301AB89EFCDFEDCBA9876543210" } else { "0123456789ABCDEFFEDCBA9876543210" };
        assert_eq!(class_id_string(&CID), text, "FUID::toString order on this platform");
        let file = encode(&CID, b"component!", b"ctrl");
        assert_eq!(file, reference(text, b"component!", b"ctrl"));
        assert_eq!(decode(&file).unwrap(), Decoded { class_id: CID, component: b"component!".to_vec(), controller: b"ctrl".to_vec() });
        // No controller state: one chunk, as the SDK writes it.
        let lone = encode(&CID, b"c", b"");
        assert_eq!(&lone[49..53], b"List");
        assert_eq!(read_i32(&lone, 53), Some(1));
        assert_eq!(decode(&lone).unwrap().controller, Vec::<u8>::new());
    }

    #[test]
    fn a_session_state_round_trips_through_a_file() {
        let id = format!("vst3:{}", CID.iter().map(|b| format!("{b:02X}")).collect::<String>());
        let blob = join_state(b"processor", b"editor").unwrap();
        let file = file_from_state(&id, &blob).unwrap();
        assert_eq!(state_from_file(&id, &file).unwrap(), blob);
        // A bare component state (older sessions) becomes a file with no controller chunk.
        let bare = file_from_state(&id, b"just the component").unwrap();
        assert_eq!(split_state(&state_from_file(&id, &bare).unwrap()).unwrap(), (&b"just the component"[..], &b""[..]));
        let other = "vst3:00000000000000000000000000000000";
        assert!(state_from_file(other, &file).unwrap_err().contains("another plug-in"));
        assert!(file_from_state("clap:x", &blob).is_err());
    }

    #[test]
    fn hostile_files_are_refused() {
        let good = encode(&CID, b"abc", b"de");
        assert!(decode(b"").is_err());
        assert!(decode(&good[..47]).is_err());
        let mut f = good.clone();
        f[0] = b'X';
        assert!(decode(&f).is_err(), "no magic");
        let mut f = good.clone();
        f[40..48].copy_from_slice(&i64::MAX.to_le_bytes());
        assert!(decode(&f).is_err(), "list outside the file");
        let mut f = good.clone();
        f[40..48].copy_from_slice(&(-8i64).to_le_bytes());
        assert!(decode(&f).is_err(), "negative offset");
        let list = good.len() - 48;
        let mut f = good.clone();
        f[list + 4..list + 8].copy_from_slice(&i32::MAX.to_le_bytes());
        assert!(decode(&f).is_err(), "absurd chunk count");
        let mut f = good.clone();
        f[list + 8 + 12..list + 8 + 20].copy_from_slice(&1_000_000i64.to_le_bytes());
        assert!(decode(&f).is_err(), "chunk past the end");
        let mut f = good.clone();
        f[list + 8 + 4..list + 8 + 12].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode(&f).is_err(), "chunk offset overflow");
        let mut f = good;
        f[8] = b'Z';
        assert!(decode(&f).is_err(), "class id not hex");
        assert_eq!(parse_class_id_string(&class_id_string(&CID)), Some(CID));
    }
}
