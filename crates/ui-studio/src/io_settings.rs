//! The machine's I/O paths (ADR 0015), kept with the preferences in `ui.json` (`UiState.io_maps`),
//! one table per audio system plus device name. A device that opens takes its saved table (the
//! opened channel counts are stored inside it, so one device in two USB modes is one entry); the
//! engine holds the live one (`setup.io_map`), and every change reaches the audio thread as a plan,
//! never a reopen.

use crate::StudioApp;
use soundcraft_model::IoMap;

/// Tables kept in the preferences; the oldest go first.
const MAX_SAVED: usize = 64;

/// The table to use for a device that just opened: its saved one made to fit `opened` (the default
/// table for the channels the stream has), else `opened` itself. A device whose input count is
/// not known to the stream (a separate input device) keeps the saved input count.
pub fn adopt(saved: &[IoMap], opened: IoMap) -> IoMap {
    let Some(mut map) = saved.iter().find(|m| m.key() == opened.key()).cloned() else { return opened };
    let inputs = if opened.inputs > 0 { opened.inputs } else { map.inputs };
    map.reconcile(inputs, opened.outputs);
    map
}

/// Keep `map` as its device's table (most recent last).
pub fn remember(saved: &mut Vec<IoMap>, map: &IoMap) {
    saved.retain(|m| m.key() != map.key());
    saved.push(map.clone());
    let excess = saved.len().saturating_sub(MAX_SAVED);
    saved.drain(..excess);
}

impl StudioApp {
    /// Once per opened device, give the engine this machine's table for it; afterwards pass every
    /// change of the engine's table to the audio thread and the preferences. Without a real
    /// device (`--no-audio`, the silent fallback) there is nothing to open or keep.
    pub(crate) fn sync_io(&mut self) {
        let Some(player) = self.player.as_ref().filter(|p| !p.silent) else { return };
        if self.io_adopt {
            let map = adopt(&self.ui.io_maps, (*player.io_map()).clone());
            self.engine.set_io_map(map);
            self.io_adopt = false;
        } else if self.io_pushed == self.engine.io_revision() {
            return;
        }
        let map = self.engine.io_map().clone();
        if player.set_io(map.clone()).is_ok() {
            remember(&mut self.ui.io_maps, &map);
            self.io_pushed = self.engine.io_revision();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_model::{ChannelFormat, IoDirection, IoPath};

    fn tascam() -> IoMap {
        IoMap::for_device("ASIO", "TASCAM Model 12", 12, 10)
    }

    #[test]
    fn an_unknown_device_gets_its_defaults_and_a_known_one_its_saved_table() {
        let mut saved = Vec::new();
        assert_eq!(adopt(&saved, tascam()), tascam());
        let mut mine = tascam();
        mine.add_path(IoPath { name: "Cue".into(), direction: IoDirection::Output, channels: vec![8, 9], format: ChannelFormat::Stereo }).unwrap();
        remember(&mut saved, &mine);
        assert_eq!(adopt(&saved, tascam()), mine);
        assert_eq!(
            adopt(&saved, IoMap::for_device("ASIO", "Scarlett 2i2", 2, 2)).paths.len(),
            IoMap::for_device("ASIO", "Scarlett 2i2", 2, 2).paths.len()
        );
    }

    #[test]
    fn one_device_in_another_usb_mode_adapts_its_saved_table() {
        let mut mine = tascam();
        mine.add_path(IoPath { name: "Cue".into(), direction: IoDirection::Output, channels: vec![8, 9], format: ChannelFormat::Stereo }).unwrap();
        mine.add_path(IoPath { name: "Pair".into(), direction: IoDirection::Output, channels: vec![2, 3], format: ChannelFormat::Stereo }).unwrap();
        let mut saved = Vec::new();
        remember(&mut saved, &mine);
        let two = adopt(&saved, IoMap::for_device("ASIO", "TASCAM Model 12", 2, 2));
        assert_eq!((two.inputs, two.outputs), (2, 2), "the opened counts are stored inside the table");
        assert!(two.path("Cue").is_none() && two.path("Pair").is_none(), "paths beyond the opened channels are dropped");
        assert!(two.path("Out 1-2").is_some());
        // The stream knows no input count for a separate input device: the saved one stays.
        let separate = adopt(&saved, IoMap::for_device("ASIO", "TASCAM Model 12", 0, 10));
        assert_eq!((separate.inputs, separate.outputs), (12, 10));
    }

    #[test]
    fn remembering_replaces_by_device_and_is_bounded() {
        let mut saved = Vec::new();
        remember(&mut saved, &tascam());
        remember(&mut saved, &tascam());
        assert_eq!(saved.len(), 1);
        for n in 0..(MAX_SAVED + 5) {
            remember(&mut saved, &IoMap::for_device("ASIO", &format!("device {n}"), 2, 2));
        }
        assert_eq!(saved.len(), MAX_SAVED);
        assert_eq!(saved.last().map(|m| m.device.as_str()), Some("device 68"));
    }
}
