//! The command line.

use crate::{Config, Kind, fake, session};
use std::path::PathBuf;

pub const USAGE: &str = "\
usage: audio-bench [options]
  --backend cpal|wasapi|fake  backend to measure (default cpal; wasapi is the native backend, built with --features native-wasapi,
                         render only: no --loopback until slice d)
  --system ASIO|WASAPI   audio system of the cpal backend (default: the platform default)
  --device <name>        output device (and input, with --loopback); default device when absent
  --in-device <name>     input device when it differs from --device
  --period <frames>      requested period: 32, 64, 128, ... (default: the driver's)
  --rate <Hz>            sample rate (default 48000)
  --seconds <N>          measurement time (default 20)
  --loopback             output-to-input cable: measure the round trip
  --out-ch <n>           1-based output channel of the cable (default 1)
  --in-ch <n>            1-based input channel of the cable (default 1)
  --out <path.md>        write the table here (rows are appended when the table is there)
  --list-devices         list the devices of --system and exit (opens no stream)
  fake mode only:
  --fake-delay <frames>  software loopback delay (default 144)
  --fake-jitter-us <us>  peak arrival jitter (default 100)
  --fake-gap-at <sec>    one arrival gap at this time, or `none` (default: half way)";

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
    let mut request = session::Request { device: None, in_device: None, rate: 48_000, period: None, loopback: false, out_ch: 1, in_ch: 1 };
    let (mut seconds, mut out) = (20.0f64, None);
    let (mut delay, mut jitter, mut gap) = (None, None, None::<Option<f64>>);
    let mut list = false;
    while let Some(flag) = args.next() {
        let mut value = || args.next();
        match flag.as_str() {
            "--backend" => {
                kind = match value().as_deref() {
                    Some("cpal") => Kind::Cpal,
                    Some("fake") => Kind::Fake,
                    Some("wasapi") => Kind::Wasapi,
                    other => return Err(format!("--backend: expected cpal, wasapi or fake, got {other:?}")),
                }
            }
            "--system" => system = Some(value().ok_or("--system needs a value")?),
            "--device" => request.device = Some(value().ok_or("--device needs a value")?),
            "--in-device" => request.in_device = Some(value().ok_or("--in-device needs a value")?),
            "--period" => request.period = Some(number(&flag, value())?),
            "--rate" => request.rate = number(&flag, value())?,
            "--seconds" => seconds = number(&flag, value())?,
            "--loopback" => request.loopback = true,
            "--out-ch" => request.out_ch = number(&flag, value())?,
            "--in-ch" => request.in_ch = number(&flag, value())?,
            "--out" => out = Some(PathBuf::from(value().ok_or("--out needs a path")?)),
            "--list-devices" => list = true,
            "--fake-delay" => delay = Some(number(&flag, value())?),
            "--fake-jitter-us" => jitter = Some(number(&flag, value())?),
            "--fake-gap-at" => {
                gap = Some(match value().as_deref() {
                    Some("none") => None,
                    other => Some(number(&flag, other.map(str::to_string))?),
                })
            }
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
    if kind == Kind::Wasapi && request.loopback {
        return Err("--backend wasapi renders only until capture arrives (slice d); drop --loopback".into());
    }
    if kind != Kind::Fake && (delay.is_some() || jitter.is_some() || gap.is_some()) {
        return Err("--fake-* options need --backend fake".into());
    }
    let mut script = fake::Script::new(seconds);
    script.delay_frames = delay.unwrap_or(script.delay_frames);
    script.jitter_us = jitter.unwrap_or(script.jitter_us);
    script.gap_at_s = gap.unwrap_or(script.gap_at_s);
    Ok(Command::Run(Config { kind, system, request, seconds, fake: script, out }))
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
        assert!(parse_str("--backend wasapi --loopback").unwrap_err().contains("slice d"));
        assert!(parse_str("--backend nope").is_err());
        assert!(parse_str("--period abc").is_err());
        assert!(parse_str("--seconds 0").is_err());
        assert!(parse_str("--fake-delay 5").is_err(), "fake options need the fake backend");
        assert!(matches!(parse_str("--list-devices --system WASAPI"), Ok(Command::List(Some(_)))));
        let Ok(Command::Run(c)) = parse_str("--backend fake --fake-gap-at none") else { panic!("did not parse") };
        assert_eq!(c.fake.gap_at_s, None);
    }
}
