//! Device-conversion goldens (ADR 0016 slice a). `plan_parity` never reaches the device code, so
//! these pin the bytes a device buffer receives: a fixed click session rendered through the real
//! callback adapter, and a fixed synthetic render through the scratch chunking, for each sample
//! format. The fixture was captured from the code before the backend trait; the trait path must
//! reproduce it bit for bit. Set `DEVICE_GOLDEN_OUT=<file>` to print a new fixture instead.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::*;
use cpal::{FromSample, SizedSample};

const FIXTURE: &str = include_str!("golden/device_conversion.txt");

pub(super) trait Wire {
    fn wire(self, out: &mut Vec<u8>);
}
macro_rules! wire {
    ($($t:ty),*) => { $(impl Wire for $t { fn wire(self, out: &mut Vec<u8>) { out.extend_from_slice(&self.to_le_bytes()) } })* };
}
wire!(i8, i16, i32, i64, u8, u16, u32, u64, f32, f64);
impl Wire for cpal::I24 {
    fn wire(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.inner().to_le_bytes());
    }
}

/// FNV-1a, 64 bits: stable and dependency-free.
pub(super) fn digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
}

fn line(name: &str, bytes: &[u8]) -> String {
    format!("{name} {:016x} {}", digest(bytes), bytes.len())
}

/// Bytes of a click session, four 256-frame callbacks after Play, on a `channels`-wide device.
fn session_bytes<T: SizedSample + FromSample<f32> + Wire>(channels: u16) -> Vec<u8> {
    let mut session = Session::default();
    session.edit.click = true;
    let io = IoMap::for_device("WASAPI", "golden", 0, channels);
    let (player, access, _clock) = Player::prepare(Arc::new(session), 48_000, io).unwrap();
    let mut device = DeviceOutput::new(access, &player, usize::from(channels));
    let zero = T::from_sample(0.0f32);
    device.fill(&mut vec![zero; 64 * usize::from(channels)]);
    player.service().unwrap();
    player.play(0, None, None).unwrap();
    let mut bytes = Vec::new();
    for _ in 0..4 {
        let mut block = vec![zero; 256 * usize::from(channels)];
        device.fill(&mut block);
        block.into_iter().for_each(|s| s.wire(&mut bytes));
    }
    bytes
}

/// Bytes of a fixed synthetic render (beyond full scale, exact scale and tiny values) through
/// three scratch chunks of a two-channel device.
fn synthetic_bytes<T: SizedSample + FromSample<f32> + Wire>() -> Vec<u8> {
    let mut callback = crate::OutputCallback::new(2);
    let mut data = vec![T::from_sample(0.0f32); 20_000 * 2];
    let mut n = 0u32;
    callback.fill_with(&mut data, |scratch, _, _| {
        for s in scratch.iter_mut() {
            *s = match n % 97 {
                0 => 1.0,
                1 => -1.0,
                2 => 0.0,
                3 => f32::MIN_POSITIVE,
                4 => 2.5,
                5 => -2.5,
                _ => (n as f32 * 0.37).sin() * 1.2,
            };
            n += 1;
        }
    });
    let mut bytes = Vec::new();
    data.into_iter().for_each(|s| s.wire(&mut bytes));
    bytes
}

macro_rules! each_format {
    ($f:ident, $($arg:expr),*) => {
        [
            ("f32", $f::<f32>($($arg),*)), ("i16", $f::<i16>($($arg),*)), ("i32", $f::<i32>($($arg),*)),
            ("i8", $f::<i8>($($arg),*)), ("u8", $f::<u8>($($arg),*)), ("u16", $f::<u16>($($arg),*)),
            ("u32", $f::<u32>($($arg),*)), ("i64", $f::<i64>($($arg),*)), ("u64", $f::<u64>($($arg),*)),
            ("f64", $f::<f64>($($arg),*)), ("i24", $f::<cpal::I24>($($arg),*)),
        ]
    };
}

/// Every golden line, in fixture order.
pub(super) fn lines() -> Vec<String> {
    let mut lines = Vec::new();
    for (name, bytes) in each_format!(synthetic_bytes,) {
        lines.push(line(&format!("synthetic-2ch-{name}"), &bytes));
    }
    for channels in [2u16, 6, 10] {
        let formats: Vec<_> = match channels {
            2 => each_format!(session_bytes, 2).into(),
            _ => vec![("f32", session_bytes::<f32>(channels)), ("i16", session_bytes::<i16>(channels)), ("i32", session_bytes::<i32>(channels))],
        };
        for (name, bytes) in formats {
            if name == "f32" {
                assert!(bytes.iter().any(|b| *b != 0), "the click is audible");
            }
            lines.push(line(&format!("click-{channels}ch-{name}"), &bytes));
        }
    }
    lines
}

#[test]
fn device_conversion_bytes_match_the_captured_goldens() {
    let lines = lines();
    if let Some(path) = std::env::var_os("DEVICE_GOLDEN_OUT") {
        std::fs::write(path, lines.join("\n") + "\n").unwrap();
        return;
    }
    let expected: Vec<&str> = FIXTURE.lines().collect();
    assert_eq!(lines.len(), expected.len(), "fixture has {} lines", expected.len());
    for (got, want) in lines.iter().zip(expected) {
        assert_eq!(got, want);
    }
}

/// The click session through the real render callback, opened on the fake backend and driven
/// block by block, with the cpal adapter's conversion as the sink.
fn trait_path_bytes<T: crate::backend::cpal::Sample + Wire>(channels: u16) -> Vec<u8> {
    use ab::Backend;
    let mut session = Session::default();
    session.edit.click = true;
    let io = IoMap::for_device("WASAPI", "golden", 0, channels);
    let (player, access, clock) = Player::prepare(Arc::new(session), 48_000, io).unwrap();
    let format = ab::Format { rate: 48_000, channels, sample: ab::SampleFormat::F32 };
    let fake = ab::fake::FakeBackend::new(format);
    let plan = fake.plan_render(&ab::OutputWish::default()).unwrap();
    let (render, on_error) = render_callbacks(&player, access, clock, usize::from(channels), 48_000);
    let (stream, achieved) = fake.open_render(&plan.endpoint, &plan.attempts[0], render, on_error).unwrap();
    assert_eq!(achieved.format, format);
    stream.start().unwrap();
    let mut convert = crate::OutputCallback::new(usize::from(channels));
    let mut pump = |frames: usize| {
        let mut block = vec![T::from_sample(0.0f32); frames * usize::from(channels)];
        let mut sink = crate::backend::cpal::ConvertSink { convert: &mut convert, data: &mut block };
        assert!(fake.pump_render(frames, ab::Stamp::INVALID, &mut sink));
        block
    };
    pump(64);
    player.service().unwrap();
    player.play(0, None, None).unwrap();
    let mut bytes = Vec::new();
    for _ in 0..4 {
        pump(256).into_iter().for_each(|s| s.wire(&mut bytes));
    }
    bytes
}

#[test]
fn the_backend_trait_path_reproduces_the_captured_device_goldens() {
    let want: std::collections::HashMap<&str, &str> = FIXTURE.lines().filter_map(|l| l.split_once(' ')).collect();
    let mut checked = 0;
    let mut check = |name: &str, bytes: Vec<u8>| {
        assert_eq!(Some(line(name, &bytes).split_once(' ').unwrap().1), want.get(name).copied(), "{name}");
        checked += 1;
    };
    for (name, bytes) in each_format!(trait_path_bytes, 2) {
        check(&format!("click-2ch-{name}"), bytes);
    }
    for channels in [6u16, 10] {
        check(&format!("click-{channels}ch-f32"), trait_path_bytes::<f32>(channels));
        check(&format!("click-{channels}ch-i16"), trait_path_bytes::<i16>(channels));
        check(&format!("click-{channels}ch-i32"), trait_path_bytes::<i32>(channels));
    }
    assert_eq!(checked, 17);
}

#[test]
fn a_scripted_device_loss_reaches_the_fault_path_and_a_failed_open_is_reported() {
    use ab::Backend;
    let (player, access, clock) = Player::prepare(Arc::new(Session::default()), 48_000, IoMap::default()).unwrap();
    let fake = ab::fake::FakeBackend::new(ab::Format { rate: 48_000, channels: 2, sample: ab::SampleFormat::F32 });
    let plan = fake.plan_render(&ab::OutputWish::default()).unwrap();
    fake.fail_next_open(ab::OpenError::DeviceNotAvailable("scripted".into()));
    let (render, on_error) = render_callbacks(&player, access, clock, 2, 48_000);
    assert_eq!(fake.open_render(&plan.endpoint, &plan.attempts[0], render, on_error).err().map(|e| e.to_string()), Some("scripted".into()));
    let (player, access, clock) = Player::prepare(Arc::new(Session::default()), 48_000, IoMap::default()).unwrap();
    let (render, on_error) = render_callbacks(&player, access, clock, 2, 48_000);
    let (_stream, _) = fake.open_render(&plan.endpoint, &plan.attempts[0], render, on_error).unwrap();
    assert!(fake.inject_error(ab::Direction::Output, ab::StreamError::DeviceNotAvailable));
    assert_eq!(player.callback_telemetry().device_lost_errors, 1);
    assert!(player.fault.gone.load(Ordering::Acquire));
}
