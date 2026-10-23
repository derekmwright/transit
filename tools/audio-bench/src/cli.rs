//! The command line.

use crate::{Config, Kind, fake, session};
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
  fake mode only:
  --fake-delay <frames>  software loopback delay (default 144)
  --fake-jitter-us <us>  peak arrival jitter (default 100)
  --fake-gap-at <sec>    one true xrun at this time (every later callback one period late), or `none` (default: half way)
  --fake-burst           deliver callbacks in pairs: a two-period wait, then two blocks back to back, no frames lost (not an xrun)";

/// What the command line asks for.
#[derive(Debug)]
pub enum Command {
    Run(Config),
    List(Option<String>),
    Help,
}

fn number<T: std::str::FromStr>(flag: &str, value: Option<String>) -> Result<T, String> {
    let value = value.ok_or_else(|| format!("{flag} needs a value"))?;
    value.parse().map_err(|_| format!("{flag}: `{value}` is not a valid number"))
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
            "--period" => request.period = Some(number(&flag, value())?),
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
