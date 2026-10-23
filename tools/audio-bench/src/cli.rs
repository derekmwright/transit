//! The command line.

use crate::{Config, Kind, fake, midi_roundtrip, placement, session};
use std::path::PathBuf;
use transit_audio_backend as ab;

pub const USAGE: &str = "\
usage: audio-bench [options]
  --backend cpal|wasapi|asio|fake  backend to measure (default cpal; wasapi and asio are the native backends, built with
                         --features native-wasapi or native-asio)
  --system ASIO|WASAPI   audio system of the cpal backend (default: the platform default)
  --device <name>        output device (and input, with --loopback); default device when absent (alias --out-device);
                         with --backend asio the ASIO driver's registry name (Focusrite USB ASIO; a unique part,
                         any case, also does: model mixer; an unknown name lists the installed drivers)
  --in-device <name>     input device when it differs from --device (not with --backend asio: capture is the driver's own)
  --mode shared|exclusive  device sharing of --backend wasapi (default shared); exclusive holds the device and silences other apps
  --in-mode shared|exclusive  mode of the capture stream when it differs from --mode (the calibration stays keyed on --mode)
  --period <frames>      requested period: 32, 64, 128, ... (default: the driver's); with --backend asio the buffer size
                         asked of createBuffers, snapped to the driver's steps (the table shows what it granted)
  --rate <Hz>            sample rate (default 48000)
  --seconds <N>          measurement time (default 20)
  --loopback             output-to-input cable: measure the round trip
  --out-ch <n>           1-based output channel of the cable (default 1)
  --in-ch <n>            1-based input channel of the cable (default 1)
  --out <path.md>        write the table here (rows are appended when the table is there)
  --settings <ui.json>   with --loopback on --backend wasapi or asio: merge the measured calibration into audio.calibrations
                         of this settings file (close the app first: it rewrites its audio block when it saves); on asio
                         it is the click loop by sample position, as the app's Calibration Measure stores it
  --list-devices         list the devices of --system and exit (opens no stream)
  --buffer <frames>      the same as --period
  MIDI-to-audio onset latency (--backend asio only; a hardware synth's MIDI in and its audio into an input):
  --midi-roundtrip       send notes to --midi-out and time their onset on --in by the driver's sample positions
                         (MIDI delivery + synth response + audio input; not MIDI transport accuracy)
  --midi-out <name>      MIDI output port: its name or a unique part of it, any case
  --midi-channel <1-16>  MIDI channel the synth listens on
  --in <ch>[,<ch>]       1-based input channel(s) of the synth's audio, summed (`1`, or `1,2` for a stereo pair)
  --note <0-127>         note to play (default 60); --velocity <1-127> (default 100)
  --trials <n>           notes to time (default 10)
  --gap-ms <ms>          silence after each note-off (default 1500; longer than the synth's release)
  --max-ms <ms>          an onset must come within this of the send (default 1000)
  --max-fit-error-ms <ms>  skip a trial while the position clock fit's standard error at the send is above this
                         (default 0.5); more than half skipped refuses the run (replaces --max-fit-residual-ms: the
                         fit's scatter is reported, but it is not a bound on the send position)
  --no-warmup            do not send the unmeasured warm-up note before the first trial
  --json <path>          also write the result, every trial and the command line, as JSON
  Placement against the grid (--backend asio only; the same MIDI options except --trials):
  --placement            schedule a note pattern at intended sample positions, send each the compensation early, and
                         report each onset's error against its position; with --compensation, uncompensated and compensated
  --compensation <n|<ms>ms>  how early each note is sent: samples (`1070`) or milliseconds (`24.3ms`); default none
  --pattern-ms <ms>,<ms>,...  the intended onsets in ms from the pattern's start (increasing, at least 50 ms apart)
  --pattern-count <n>    or a grid: this many notes (default 8) ...
  --pattern-spacing-ms <ms>  ... this far apart (default 500)
  --lead-ms <ms>         the first onset this long after scheduling (default 250); |compensation| must be below it
  fake mode only:
  --fake-delay <frames>  software loopback delay (default 144)
  --fake-jitter-us <us>  peak arrival jitter (default 100)
  --fake-gap-at <sec>    one true xrun at this time (every later callback one period late), or `none` (default: half way)
  --fake-burst           deliver callbacks in pairs: a two-period wait, then two blocks back to back, no frames lost (not an xrun)";

/// What the command line asks for.
#[derive(Debug)]
pub enum Command {
    Run(Config),
    /// `--midi-roundtrip`: the device and the MIDI options.
    MidiRoundTrip {
        config: Config,
        midi: midi_roundtrip::Options,
    },
    /// `--placement`: the device, the MIDI options and the pattern.
    Placement {
        config: Config,
        placement: placement::Options,
    },
    List(Option<String>),
    Help,
}

fn number<T: std::str::FromStr>(flag: &str, value: Option<String>) -> Result<T, String> {
    let value = value.ok_or_else(|| format!("{flag} needs a value"))?;
    value.parse().map_err(|_| format!("{flag}: `{value}` is not a valid number"))
}

/// `--in`: 1-based channels separated by commas (or `+`), as 0-based indices.
fn channels(value: Option<String>) -> Result<Vec<u16>, String> {
    let value = value.ok_or("--in needs an input channel")?;
    let parsed: Option<Vec<u16>> = value.split([',', '+']).map(|c| c.trim().parse::<u16>().ok().and_then(|c| c.checked_sub(1))).collect();
    match parsed {
        Some(list) if !list.is_empty() => Ok(list),
        _ => Err(format!("--in: `{value}` is not a list of 1-based input channels (`1`, or `1,2`)")),
    }
}

/// `--pattern-ms`: onsets in ms separated by commas.
fn pattern_list(value: Option<String>) -> Result<Vec<u32>, String> {
    let value = value.ok_or("--pattern-ms needs a list of ms")?;
    value.split(',').map(|v| v.trim().parse::<u32>().map_err(|_| format!("--pattern-ms: `{value}` is not a list of ms (`0,500,1250`)"))).collect()
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut args = args.into_iter();
    let mut kind = Kind::Cpal;
    let mut system = None;
    let mut request = session::Request {
        device: None,
        in_device: None,
        rate: 48_000,
        period: None,
        loopback: false,
        mode: ab::Mode::Shared,
        in_mode: None,
        out_ch: 1,
        in_ch: 1,
    };
    let (mut seconds, mut out, mut settings) = (20.0f64, None, None);
    let (mut delay, mut jitter, mut gap, mut burst) = (None, None, None::<Option<f64>>, false);
    let mut list = false;
    let (mut midi, mut placing, mut no_warmup) = (false, false, false);
    let mut max_error = None::<f64>;
    let (mut compensation, mut pattern, mut count, mut spacing, mut lead) = (None, None::<Vec<u32>>, None::<u32>, None::<u32>, None::<u32>);
    let (mut port, mut channel, mut inputs, mut json) = (None::<String>, None::<u8>, None::<Vec<u16>>, None);
    let (mut note, mut velocity, mut trials, mut gap_ms, mut max) = (None::<u8>, None::<u8>, None::<u32>, None::<u32>, None::<u32>);
    while let Some(flag) = args.next() {
        let mut value = || args.next();
        match flag.as_str() {
            "--backend" => {
                kind = match value().as_deref() {
                    Some("cpal") => Kind::Cpal,
                    Some("fake") => Kind::Fake,
                    Some("wasapi") => Kind::Wasapi,
                    Some("asio") => Kind::Asio,
                    other => return Err(format!("--backend: expected cpal, wasapi, asio or fake, got {other:?}")),
                }
            }
            "--system" => system = Some(value().ok_or("--system needs a value")?),
            "--device" | "--out-device" => request.device = Some(value().ok_or("--device needs a value")?),
            "--in-device" => request.in_device = Some(value().ok_or("--in-device needs a value")?),
            "--mode" => {
                request.mode = match value().as_deref() {
                    Some("shared") => ab::Mode::Shared,
                    Some("exclusive") => ab::Mode::Exclusive,
                    other => return Err(format!("--mode: expected shared or exclusive, got {other:?}")),
                }
            }
            "--in-mode" => {
                request.in_mode = Some(match value().as_deref() {
                    Some("shared") => ab::Mode::Shared,
                    Some("exclusive") => ab::Mode::Exclusive,
                    other => return Err(format!("--in-mode: expected shared or exclusive, got {other:?}")),
                })
            }
            "--period" | "--buffer" => request.period = Some(number(&flag, value())?),
            "--midi-roundtrip" => midi = true,
            "--midi-out" => port = Some(value().ok_or("--midi-out needs a port name")?),
            "--midi-channel" => channel = Some(number(&flag, value())?),
            "--in" => inputs = Some(channels(value())?),
            "--note" => note = Some(number(&flag, value())?),
            "--velocity" => velocity = Some(number(&flag, value())?),
            "--trials" => trials = Some(number(&flag, value())?),
            "--gap-ms" => gap_ms = Some(number(&flag, value())?),
            "--max-ms" => max = Some(number(&flag, value())?),
            "--max-fit-error-ms" => max_error = Some(number(&flag, value())?),
            "--max-fit-residual-ms" => {
                return Err("--max-fit-residual-ms was replaced by --max-fit-error-ms: the limit is the clock fit's standard error at the send; its scatter is reported but is not a bound on the send position".into());
            }
            "--no-warmup" => no_warmup = true,
            "--placement" => placing = true,
            "--compensation" => compensation = Some(placement::Compensation::parse(&value().ok_or("--compensation needs a value")?)?),
            "--pattern-ms" => pattern = Some(pattern_list(value())?),
            "--pattern-count" => count = Some(number(&flag, value())?),
            "--pattern-spacing-ms" => spacing = Some(number(&flag, value())?),
            "--lead-ms" => lead = Some(number(&flag, value())?),
            "--json" => json = Some(PathBuf::from(value().ok_or("--json needs a path")?)),
            "--rate" => request.rate = number(&flag, value())?,
            "--seconds" => seconds = number(&flag, value())?,
            "--loopback" => request.loopback = true,
            "--out-ch" => request.out_ch = number(&flag, value())?,
            "--in-ch" => request.in_ch = number(&flag, value())?,
            "--out" => out = Some(PathBuf::from(value().ok_or("--out needs a path")?)),
            "--settings" => settings = Some(PathBuf::from(value().ok_or("--settings needs a path")?)),
            "--list-devices" => list = true,
            "--fake-delay" => delay = Some(number(&flag, value())?),
            "--fake-jitter-us" => jitter = Some(number(&flag, value())?),
            "--fake-gap-at" => {
                gap = Some(match value().as_deref() {
                    Some("none") => None,
                    other => Some(number(&flag, other.map(str::to_string))?),
                })
            }
            "--fake-burst" => burst = true,
            "--help" | "-h" => return Ok(Command::Help),
            other => return Err(format!("unknown option {other}\n{USAGE}")),
        }
    }
    if list {
        return Ok(Command::List(system));
    }
    let midi_given = port.is_some() || channel.is_some() || inputs.is_some() || json.is_some() || max_error.is_some() || no_warmup;
    if !midi && !placing && (midi_given || note.is_some() || velocity.is_some() || trials.is_some() || gap_ms.is_some() || max.is_some()) {
        return Err(
            "--midi-out, --midi-channel, --in, --note, --velocity, --trials, --gap-ms, --max-ms, --max-fit-error-ms, --no-warmup and --json need --midi-roundtrip or --placement".into(),
        );
    }
    let placement_given = compensation.is_some() || pattern.is_some() || count.is_some() || spacing.is_some() || lead.is_some();
    if !placing && placement_given {
        return Err("--compensation, --pattern-ms, --pattern-count, --pattern-spacing-ms and --lead-ms need --placement".into());
    }
    if midi && placing {
        return Err("--midi-roundtrip and --placement are separate measurements; run one at a time".into());
    }
    if placing && trials.is_some() {
        return Err("--placement plays its pattern, not --trials (use --pattern-count or --pattern-ms)".into());
    }
    if pattern.is_some() && (count.is_some() || spacing.is_some()) {
        return Err("--pattern-ms is the whole pattern; it does not take --pattern-count or --pattern-spacing-ms".into());
    }
    if !(seconds.is_finite() && seconds > 0.0) {
        return Err("--seconds must be positive".into());
    }
    if request.period == Some(0) || request.rate == 0 {
        return Err("--period and --rate must be positive".into());
    }
    if kind == Kind::Wasapi && system.as_deref().is_some_and(|s| !s.eq_ignore_ascii_case("WASAPI")) {
        return Err("--backend wasapi is the native WASAPI system; --system applies to the cpal backend".into());
    }
    if kind == Kind::Asio && system.as_deref().is_some_and(|s| !s.eq_ignore_ascii_case("ASIO")) {
        return Err("--backend asio is the native ASIO system; --system applies to the cpal backend".into());
    }
    if (request.mode == ab::Mode::Exclusive || request.in_mode == Some(ab::Mode::Exclusive)) && kind == Kind::Cpal {
        return Err("--mode exclusive needs --backend wasapi (or fake): cpal opens shared streams only".into());
    }
    if (request.mode == ab::Mode::Exclusive || request.in_mode == Some(ab::Mode::Exclusive)) && kind == Kind::Asio {
        return Err("--mode does not apply to --backend asio: an ASIO driver has one mode".into());
    }
    if request.in_device.is_some() && kind == Kind::Asio {
        return Err("--in-device does not apply to --backend asio: capture opens on the --device driver (one ASIO driver per process)".into());
    }
    if settings.is_some() && !request.loopback {
        return Err("--settings needs --loopback: a calibration is a measured round trip".into());
    }
    if settings.is_some() && kind == Kind::Cpal {
        return Err("--settings needs --backend wasapi or asio: the frame times of cpal streams are not comparable".into());
    }
    if kind != Kind::Fake && (delay.is_some() || jitter.is_some() || gap.is_some() || burst) {
        return Err("--fake-* options need --backend fake".into());
    }
    if midi || placing {
        let mode = if midi { "--midi-roundtrip" } else { "--placement" };
        if kind != Kind::Asio {
            return Err(format!("{mode} needs --backend asio: it places the note and the onset by the driver's sample positions"));
        }
        if request.loopback || settings.is_some() || out.is_some() {
            return Err(format!("{mode} does not take --loopback, --settings or --out (use --json)"));
        }
        let port = port.ok_or(format!("{mode} needs --midi-out <port name>"))?;
        let channel = channel.ok_or(format!("{mode} needs --midi-channel <1-16>"))?;
        if !(1..=16).contains(&channel) {
            return Err("--midi-channel counts 1 to 16".into());
        }
        let inputs = inputs.ok_or(format!("{mode} needs --in <input channel> (1-based; `1,2` sums a stereo pair)"))?;
        let mut settings = midi_roundtrip::Settings::new(channel - 1, inputs);
        settings.note = note.unwrap_or(settings.note);
        settings.velocity = velocity.unwrap_or(settings.velocity);
        settings.trials = trials.unwrap_or(settings.trials);
        settings.gap_ms = gap_ms.unwrap_or(settings.gap_ms);
        settings.max_ms = max.unwrap_or(settings.max_ms);
        settings.warmup = !no_warmup;
        if let Some(ms) = max_error {
            if !(ms.is_finite() && ms > 0.0 && ms <= 100.0) {
                return Err("--max-fit-error-ms must be above 0 and at most 100".into());
            }
            settings.max_fit_error_ns = (ms * 1e6).round() as u64;
        }
        if settings.note > 127 || !(1..=127).contains(&settings.velocity) {
            return Err("--note counts 0 to 127 and --velocity 1 to 127".into());
        }
        if settings.trials == 0 || settings.max_ms == 0 {
            return Err("--trials and --max-ms must be positive".into());
        }
        let config = Config { kind, system, request, seconds, fake: fake::Script::new(seconds), out, settings: None };
        if placing {
            let mut placed = placement::Settings::new(0, Vec::new());
            placed.compensation = compensation;
            placed.lead_ms = lead.unwrap_or(placed.lead_ms);
            placed.pattern_ms = pattern.unwrap_or_else(|| {
                placement::grid(count.unwrap_or(placement::DEFAULT_PATTERN_COUNT), spacing.unwrap_or(placement::DEFAULT_PATTERN_SPACING_MS))
            });
            placed.base = settings;
            placed.check(config.request.rate)?;
            return Ok(Command::Placement { config, placement: placement::Options { port, settings: placed, json } });
        }
        return Ok(Command::MidiRoundTrip { config, midi: midi_roundtrip::Options { port, settings, json } });
    }
    let mut script = fake::Script::new(seconds);
    script.delay_frames = delay.unwrap_or(script.delay_frames);
    script.jitter_us = jitter.unwrap_or(script.jitter_us);
    script.gap_at_s = gap.unwrap_or(script.gap_at_s);
    script.burst = burst;
    Ok(Command::Run(Config { kind, system, request, seconds, fake: script, out, settings }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(line: &str) -> Result<Command, String> {
        parse(line.split_whitespace().map(str::to_string))
    }

    #[test]
    fn a_full_command_line() {
        let Ok(Command::Run(c)) =
            parse_str("--backend cpal --system ASIO --device Scarlett --period 64 --seconds 5 --loopback --out-ch 2 --in-ch 2 --out x.md")
        else {
            panic!("did not parse");
        };
        assert_eq!((c.kind, c.system.as_deref(), c.request.device.as_deref()), (Kind::Cpal, Some("ASIO"), Some("Scarlett")));
        assert_eq!((c.request.period, c.request.loopback, c.request.out_ch, c.request.in_ch, c.seconds), (Some(64), true, 2, 2, 5.0));
        assert_eq!(c.out, Some(PathBuf::from("x.md")));
    }

    #[test]
    fn defaults_and_errors() {
        let Ok(Command::Run(c)) = parse_str("--backend fake") else { panic!("did not parse") };
        assert_eq!((c.seconds, c.request.rate, c.fake.gap_at_s), (20.0, 48_000, Some(10.0)));
        let Ok(Command::Run(c)) = parse_str("--backend wasapi --seconds 5 --period 64") else { panic!("did not parse") };
        assert_eq!((c.kind, c.request.period), (Kind::Wasapi, Some(64)));
        assert!(parse_str("--backend wasapi --system ASIO").is_err());
        let Ok(Command::Run(c)) = parse_str("--backend wasapi --loopback --device Out --in-device In --settings ui.json") else {
            panic!("did not parse")
        };
        assert_eq!(
            (c.kind, c.request.loopback, c.request.in_device.as_deref(), c.settings),
            (Kind::Wasapi, true, Some("In"), Some(PathBuf::from("ui.json")))
        );
        assert!(parse_str("--backend wasapi --settings ui.json").unwrap_err().contains("--loopback"));
        assert!(parse_str("--backend cpal --loopback --settings ui.json").unwrap_err().contains("--backend wasapi"));
        assert!(parse_str("--backend nope").is_err());
        assert!(parse_str("--period abc").is_err());
        assert!(parse_str("--seconds 0").is_err());
        assert!(parse_str("--fake-delay 5").is_err(), "fake options need the fake backend");
        assert!(matches!(parse_str("--list-devices --system WASAPI"), Ok(Command::List(Some(_)))));
        let Ok(Command::Run(c)) = parse_str("--backend fake --fake-gap-at none") else { panic!("did not parse") };
        assert_eq!(c.fake.gap_at_s, None);
        let Ok(Command::Run(c)) = parse_str("--backend fake --fake-burst") else { panic!("did not parse") };
        assert!(c.fake.burst);
        assert!(parse_str("--fake-burst").is_err(), "fake options need the fake backend");
    }

    #[test]
    fn the_native_asio_backend_takes_a_driver_name_and_a_buffer_size() {
        let Ok(Command::Run(c)) = parse_str("--backend asio --device Scarlett --period 64 --loopback --out-ch 2 --in-ch 2") else {
            panic!("did not parse")
        };
        assert_eq!((c.kind, c.request.device.as_deref(), c.request.period), (Kind::Asio, Some("Scarlett"), Some(64)));
        assert_eq!((c.request.out_ch, c.request.in_ch), (2, 2));
        assert!(parse_str("--backend asio --system WASAPI").is_err());
        assert!(matches!(parse_str("--backend asio --system ASIO"), Ok(Command::Run(_))));
        assert!(parse_str("--backend asio --mode exclusive").unwrap_err().contains("one mode"));
        let Ok(Command::Run(c)) = parse_str("--backend asio --loopback --settings ui.json") else { panic!("an ASIO loop writes its calibration") };
        assert_eq!(c.settings.as_deref(), Some(std::path::Path::new("ui.json")));
        let refused = parse_str("--backend asio --device Scarlett --in-device Other --loopback").unwrap_err();
        assert!(refused.contains("--in-device does not apply"), "{refused}");
        assert!(matches!(parse_str("--backend wasapi --in-device Mic --loopback"), Ok(Command::Run(_))), "the other backends keep it");
    }

    #[test]
    fn the_midi_roundtrip_takes_a_port_a_channel_and_an_input() {
        let Ok(Command::MidiRoundTrip { config, midi }) = parse_str(
            "--midi-roundtrip --midi-out Minilogue --midi-channel 2 --in 1,2 --backend asio --device Scarlett --rate 44100 --buffer 256 --trials 5 --gap-ms 900 --json r.json",
        ) else {
            panic!("did not parse")
        };
        assert_eq!((config.kind, config.request.rate, config.request.period), (Kind::Asio, 44_100, Some(256)));
        assert_eq!((midi.port.as_str(), midi.settings.channel, midi.settings.returns.as_slice()), ("Minilogue", 1, &[0u16, 1][..]));
        assert_eq!((midi.settings.note, midi.settings.velocity, midi.settings.trials, midi.settings.gap_ms), (60, 100, 5, 900));
        assert_eq!((midi.settings.max_ms, midi.json), (1000, Some(PathBuf::from("r.json"))));
        let Ok(Command::MidiRoundTrip { midi, .. }) = parse_str("--midi-roundtrip --midi-out x --midi-channel 16 --in 3 --backend asio") else {
            panic!("did not parse")
        };
        assert_eq!((midi.settings.channel, midi.settings.returns.as_slice(), midi.settings.gap_ms), (15, &[2u16][..], 1500));
        assert_eq!((midi.settings.max_fit_error_ns, midi.settings.warmup), (500_000, true));
        let Ok(Command::MidiRoundTrip { midi, .. }) =
            parse_str("--midi-roundtrip --midi-out x --midi-channel 1 --in 1 --backend asio --max-fit-error-ms 0.75 --no-warmup")
        else {
            panic!("did not parse")
        };
        assert_eq!((midi.settings.max_fit_error_ns, midi.settings.warmup), (750_000, false));
        assert!(parse_str("--midi-roundtrip --midi-out x --midi-channel 1 --in 1 --backend asio --max-fit-error-ms 0").is_err());
        let replaced = parse_str("--midi-roundtrip --midi-out x --midi-channel 1 --in 1 --backend asio --max-fit-residual-ms 2").unwrap_err();
        assert!(replaced.contains("--max-fit-error-ms"), "{replaced}");
        let base = "--midi-roundtrip --backend asio --midi-out x";
        assert!(parse_str(&format!("{base} --in 1")).unwrap_err().contains("--midi-channel"));
        assert!(parse_str(&format!("{base} --midi-channel 1")).unwrap_err().contains("--in"));
        assert!(parse_str(&format!("{base} --midi-channel 17 --in 1")).unwrap_err().contains("1 to 16"));
        assert!(parse_str(&format!("{base} --midi-channel 1 --in 0")).unwrap_err().contains("1-based"));
        assert!(parse_str(&format!("{base} --midi-channel 1 --in 1 --velocity 0")).unwrap_err().contains("--velocity"));
        assert!(parse_str(&format!("{base} --midi-channel 1 --in 1 --loopback")).unwrap_err().contains("--loopback"));
        assert!(parse_str("--midi-roundtrip --backend wasapi --midi-out x --midi-channel 1 --in 1").unwrap_err().contains("--backend asio"));
        assert!(parse_str("--backend asio --midi-out x").unwrap_err().contains("need --midi-roundtrip"));
    }

    #[test]
    fn the_placement_takes_a_compensation_and_a_pattern() {
        let base = "--placement --midi-out MODEL --midi-channel 1 --in 3 --backend asio --rate 44100 --buffer 256";
        let Ok(Command::Placement { config, placement }) = parse_str(&format!("{base} --compensation 1070 --json p.json")) else {
            panic!("did not parse")
        };
        assert_eq!((config.kind, config.request.rate), (Kind::Asio, 44_100));
        let s = &placement.settings;
        assert_eq!((s.compensation, s.pattern_ms.len(), s.lead_ms), (Some(placement::Compensation::Samples(1070)), 8, 250));
        assert_eq!((s.base.returns.as_slice(), s.base.warmup, placement.json), (&[2u16][..], true, Some(PathBuf::from("p.json"))));
        let Ok(Command::Placement { placement, .. }) = parse_str(&format!("{base} --compensation 24.3ms --pattern-ms 0,250,1000 --no-warmup")) else {
            panic!("did not parse")
        };
        assert_eq!((placement.settings.pattern_ms.as_slice(), placement.settings.base.warmup), (&[0u32, 250, 1000][..], false));
        let Ok(Command::Placement { placement, .. }) = parse_str(&format!("{base} --pattern-count 4 --pattern-spacing-ms 1500 --lead-ms 400")) else {
            panic!("did not parse")
        };
        assert_eq!((placement.settings.pattern_ms.as_slice(), placement.settings.compensation), (&[0u32, 1500, 3000, 4500][..], None));
        assert!(parse_str(&format!("{base} --trials 4")).unwrap_err().contains("--pattern-count"));
        assert!(parse_str(&format!("{base} --pattern-ms 0,500 --pattern-count 2")).is_err());
        assert!(parse_str(&format!("{base} --compensation -20000")).unwrap_err().contains("negative by more than the lead"));
        assert!(parse_str(&format!("{base} --midi-roundtrip")).unwrap_err().contains("one at a time"));
        assert!(
            parse_str("--midi-roundtrip --midi-out x --midi-channel 1 --in 1 --backend asio --compensation 5")
                .unwrap_err()
                .contains("need --placement")
        );
        assert!(parse_str("--placement --midi-out x --midi-channel 1 --in 1 --backend wasapi").unwrap_err().contains("--backend asio"));
    }

    #[test]
    fn the_mode_defaults_to_shared_and_exclusive_needs_a_native_or_fake_backend() {
        let Ok(Command::Run(c)) = parse_str("--backend wasapi") else { panic!("did not parse") };
        assert_eq!(c.request.mode, ab::Mode::Shared);
        let Ok(Command::Run(c)) = parse_str("--backend wasapi --mode exclusive --period 128") else { panic!("did not parse") };
        assert_eq!((c.request.mode, c.request.period), (ab::Mode::Exclusive, Some(128)));
        let Ok(Command::Run(c)) = parse_str("--backend fake --loopback --mode exclusive") else { panic!("did not parse") };
        assert_eq!(c.request.mode, ab::Mode::Exclusive);
        assert!(parse_str("--backend cpal --mode exclusive").unwrap_err().contains("--backend wasapi"));
        assert!(parse_str("--backend wasapi --mode raw").unwrap_err().contains("--mode"));
    }

    #[test]
    fn the_capture_mode_defaults_to_the_render_mode_and_can_differ() {
        let Ok(Command::Run(c)) = parse_str("--backend wasapi --loopback --mode exclusive") else { panic!("did not parse") };
        assert_eq!(c.request.in_mode, None);
        let Ok(Command::Run(c)) = parse_str("--backend wasapi --loopback --mode exclusive --in-mode shared") else { panic!("did not parse") };
        assert_eq!((c.request.mode, c.request.in_mode), (ab::Mode::Exclusive, Some(ab::Mode::Shared)));
        let Ok(Command::Run(c)) = parse_str("--backend wasapi --loopback --in-mode exclusive") else { panic!("did not parse") };
        assert_eq!((c.request.mode, c.request.in_mode), (ab::Mode::Shared, Some(ab::Mode::Exclusive)));
        assert!(parse_str("--backend wasapi --in-mode raw").unwrap_err().contains("--in-mode"));
        assert!(parse_str("--backend cpal --in-mode exclusive").unwrap_err().contains("--backend wasapi"));
    }
}
