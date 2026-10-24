//! Musical key and scale shared by note editing and piano-roll views.

/// Seven-note scales. Minor means natural minor (Aeolian).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scale {
    #[default]
    Major,
    Minor,
}

/// A tonic pitch class (C = 0) and scale. Defaults to C major.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct KeyScale {
    pub root: u8,
    pub scale: Scale,
}

impl KeyScale {
    /// Whether a MIDI pitch belongs to this key. Tonic values normalize modulo 12.
    pub fn contains(self, pitch: u8) -> bool {
        let degree = (pitch % 12 + 12 - self.root % 12) % 12;
        match self.scale {
            Scale::Major => [0, 2, 4, 5, 7, 9, 11].contains(&degree),
            Scale::Minor => [0, 2, 3, 5, 7, 8, 10].contains(&degree),
        }
    }

    /// Closest scale tone in MIDI range; equal distances choose the lower pitch.
    pub fn nearest(self, pitch: u8) -> u8 {
        (0..=127).filter(|&candidate| self.contains(candidate)).min_by_key(|&candidate| candidate.abs_diff(pitch)).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn major_minor_nearest_ties_and_boundaries() {
        let c = KeyScale::default();
        assert_eq!((0..12).filter(|&p| c.contains(p)).collect::<Vec<_>>(), [0, 2, 4, 5, 7, 9, 11]);
        assert_eq!([61, 63, 66, 68, 70].map(|p| c.nearest(p)), [60, 62, 65, 67, 69]);
        let a = KeyScale { root: 9, scale: Scale::Minor };
        assert_eq!((0..12).filter(|&p| a.contains(p)).collect::<Vec<_>>(), [0, 2, 4, 5, 7, 9, 11]);
        let cm = KeyScale { root: 0, scale: Scale::Minor };
        assert_eq!([61, 64, 66, 69, 71].map(|p| cm.nearest(p)), [60, 63, 65, 68, 70]);
        let d = KeyScale { root: 1, scale: Scale::Major };
        assert_eq!(d.nearest(127), 126);
        assert_eq!(KeyScale { root: 2, scale: Scale::Major }.nearest(0), 1);
        for root in 0..=255 {
            for scale in [Scale::Major, Scale::Minor] {
                let key = KeyScale { root, scale };
                for pitch in 0..=127 {
                    let nearest = key.nearest(pitch);
                    assert!(nearest <= 127 && key.contains(nearest));
                    assert!((0..=127).filter(|&p| key.contains(p)).all(|p| p.abs_diff(pitch) >= nearest.abs_diff(pitch)));
                }
            }
        }
    }
}
