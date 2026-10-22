const PITCH_CLASSES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

/// Note name using the Pro Tools convention where middle C (60) is "C3" (so 0 is "C-2").
pub fn note_name(pitch: u8) -> String {
    let class = PITCH_CLASSES.get(usize::from(pitch % 12)).copied().unwrap_or("?");
    let octave = i32::from(pitch / 12) - 2;
    format!("{class}{octave}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(note_name(60), "C3");
        assert_eq!(note_name(0), "C-2");
        assert_eq!(note_name(127), "G8");
        assert_eq!(note_name(61), "C#3");
    }
}
