//! The exclusive-mode open (ADR 0016 section 2, slice e) over a small trait of the client calls, so
//! the format choice, the period rule, the alignment retry and the error mapping run on a fake in
//! tests. Control-thread code: it may allocate and format; `com.rs` implements the trait.

use crate::render_loop::{E_DEVICE_IN_USE, Fault};
use transit_audio_backend as ab;

/// `AUDCLNT_E_UNSUPPORTED_FORMAT`: the device does not take this format in exclusive mode.
pub(crate) const E_UNSUPPORTED_FORMAT: i32 = 0x8889_0008_u32 as i32;
/// `AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED`: the endpoint's Advanced settings forbid exclusive use.
pub(crate) const E_EXCLUSIVE_MODE_NOT_ALLOWED: i32 = 0x8889_000E_u32 as i32;
/// `AUDCLNT_E_ENDPOINT_CREATE_FAILED`: the endpoint could not be created (unplugged, disabled).
pub(crate) const E_ENDPOINT_CREATE_FAILED: i32 = 0x8889_000F_u32 as i32;
/// `AUDCLNT_E_BUFFER_SIZE_ERROR`: the requested period is outside the range the device accepts.
pub(crate) const E_BUFFER_SIZE_ERROR: i32 = 0x8889_0016_u32 as i32;
/// `AUDCLNT_E_INVALID_DEVICE_PERIOD`: the period is outside what the device allows (some drivers
/// answer a period below their minimum with this instead of `AUDCLNT_E_BUFFER_SIZE_ERROR`).
pub(crate) const E_INVALID_DEVICE_PERIOD: i32 = 0x8889_0020_u32 as i32;
/// `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED`: the period is not a multiple of the driver's alignment.
pub(crate) const E_BUFFER_SIZE_NOT_ALIGNED: i32 = 0x8889_0019_u32 as i32;
/// `E_NOTFOUND` (`HRESULT_FROM_WIN32(ERROR_NOT_FOUND)`): no such endpoint.
pub(crate) const E_NOTFOUND: i32 = 0x8007_0490_u32 as i32;

/// A failed client call: its `HRESULT` and the system's text for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApiError {
    pub(crate) code: i32,
    pub(crate) text: String,
}

/// The `OpenError` of a failed exclusive-mode call `what`.
pub(crate) fn open_error(what: &str, error: &ApiError) -> ab::OpenError {
    let text = format!("{what}: {}", error.text);
    match error.code {
        E_DEVICE_IN_USE => ab::OpenError::InUse(format!("{text} (another app holds the device exclusively)")),
        E_EXCLUSIVE_MODE_NOT_ALLOWED => {
            ab::OpenError::ExclusiveDisallowed(format!("{text} (exclusive use is turned off in the device's Sound settings, Advanced tab)"))
        }
        E_ENDPOINT_CREATE_FAILED | E_NOTFOUND => ab::OpenError::DeviceNotAvailable(text),
        code => match Fault::from_hresult(code) {
            Fault::DeviceGone => ab::OpenError::DeviceNotAvailable(text),
            Fault::Other(_) => ab::OpenError::Other(text),
        },
    }
}

/// The client calls of an exclusive open, on one `IMMDevice`.
pub(crate) trait ExclusiveApi {
    type Client;
    /// A fresh, uninitialized client of the device (`IMMDevice::Activate`).
    fn activate(&mut self) -> Result<Self::Client, ApiError>;
    /// `GetDevicePeriod`: the default and the minimum period, in 100 ns units.
    fn device_period(&mut self, client: &Self::Client) -> Result<(i64, i64), ApiError>;
    /// `IsFormatSupported(EXCLUSIVE, format)`: `Ok(false)` for `S_FALSE`, `AUDCLNT_E_UNSUPPORTED_FORMAT`
    /// or a format that has no `WAVEFORMATEXTENSIBLE`; any other failure is an error.
    fn supports(&mut self, client: &Self::Client, format: &ab::Format) -> Result<bool, ApiError>;
    /// `Initialize(EXCLUSIVE, EVENTCALLBACK, duration, duration, format)`.
    fn initialize(&mut self, client: &Self::Client, format: &ab::Format, duration_100ns: i64) -> Result<(), ApiError>;
    /// `GetBufferSize`, in frames (also legal after an `Initialize` that failed with `NOT_ALIGNED`).
    fn buffer_size(&mut self, client: &Self::Client) -> Result<u32, ApiError>;
}

/// `frames` at `rate` in 100 ns units, rounded to nearest (Microsoft's alignment sample).
pub(crate) fn frames_to_100ns(frames: u32, rate: u32) -> i64 {
    let rate = u64::from(rate.max(1));
    ((u64::from(frames) * 10_000_000 + rate / 2) / rate) as i64
}

/// A 100 ns duration at `rate` in frames, rounded to nearest.
pub(crate) fn frames_of_100ns(duration: i64, rate: u32) -> u32 {
    let d = duration.unsigned_abs();
    u32::try_from((d * u64::from(rate) + 5_000_000) / 10_000_000).unwrap_or(u32::MAX)
}

/// The candidates `client` accepts, in order. In-use and disallowed abort the probe.
pub(crate) fn probe<A: ExclusiveApi>(api: &mut A, client: &A::Client, candidates: &[ab::Format]) -> Result<Vec<ab::Format>, ab::OpenError> {
    let mut accepted = Vec::new();
    for format in candidates {
        if api.supports(client, format).map_err(|e| open_error("IsFormatSupported(exclusive)", &e))? {
            accepted.push(*format);
        }
    }
    Ok(accepted)
}

/// The format to open: `requested` when the device takes it, else the first of
/// [`ab::exclusive_candidates`] at its rate and channel count that it takes.
fn choose<A: ExclusiveApi>(api: &mut A, client: &A::Client, requested: &ab::Format) -> Result<ab::Format, ab::OpenError> {
    if api.supports(client, requested).map_err(|e| open_error("IsFormatSupported(exclusive)", &e))? {
        return Ok(*requested);
    }
    let candidates: Vec<ab::Format> =
        ab::exclusive_candidates(requested.rate, &[requested.channels]).into_iter().filter(|f| f.rate == requested.rate && f != requested).collect();
    probe(api, client, &candidates)?.into_iter().next().ok_or_else(|| {
        ab::OpenError::Other(format!(
            "the device takes no {} Hz, {}-channel format in exclusive mode (tried float, 32-bit, 24-in-32, 24 and 16-bit PCM)",
            requested.rate, requested.channels
        ))
    })
}

/// An initialized exclusive client: the format it runs and its buffer (one period) in frames.
pub(crate) struct Opened<C> {
    pub(crate) client: C,
    pub(crate) format: ab::Format,
    pub(crate) buffer_frames: u32,
    /// The device's minimum period (`GetDevicePeriod`), in frames at the format's rate: reported
    /// so a user can choose it, never used by default.
    pub(crate) min_period: u32,
}

/// Activate, choose the format, and initialize an event-driven exclusive client. The period is
/// `Period::Frames(n)` at the format's rate, else the device's default period, and the buffer
/// duration equals it (the exclusive event-driven rule). A period the driver cannot align is
/// retried once at the aligned size `GetBufferSize` reports, on a freshly activated client.
pub(crate) fn open<A: ExclusiveApi>(api: &mut A, requested: &ab::Format, period: ab::Period) -> Result<Opened<A::Client>, ab::OpenError> {
    let client = api.activate().map_err(|e| open_error("Activate(IAudioClient)", &e))?;
    let format = choose(api, &client, requested)?;
    let (default_100ns, min_100ns) = api.device_period(&client).map_err(|e| open_error("GetDevicePeriod", &e))?;
    // The default is the device's default period, not its minimum: the Focusrite Scarlett 2i2
    // reports a 3 ms minimum but signals only every other period below its 10 ms default (K25).
    // An explicit `Period::Frames(n)` is the user's choice and is honored as asked.
    let duration = match period {
        ab::Period::Frames(n) => frames_to_100ns(n, format.rate),
        ab::Period::Default => default_100ns,
    };
    let client = match api.initialize(&client, &format, duration) {
        Ok(()) => client,
        Err(e) if e.code == E_BUFFER_SIZE_NOT_ALIGNED => {
            let aligned = api.buffer_size(&client).map_err(|e| open_error("GetBufferSize (alignment)", &e))?;
            // A failed `Initialize` cannot be retried on the same client: release it, activate a
            // new one and initialize that once at the aligned duration.
            drop(client);
            let client = api.activate().map_err(|e| open_error("Activate(IAudioClient) for the aligned period", &e))?;
            let duration = frames_to_100ns(aligned, format.rate);
            log::info!("WASAPI exclusive: the period is not aligned; retrying at {aligned} frames");
            api.initialize(&client, &format, duration).map_err(|e| initialize_error(&e, format.rate, duration, default_100ns, min_100ns))?;
            client
        }
        Err(e) => return Err(initialize_error(&e, format.rate, duration, default_100ns, min_100ns)),
    };
    let buffer_frames = api.buffer_size(&client).map_err(|e| open_error("GetBufferSize", &e))?;
    Ok(Opened { client, format, buffer_frames, min_period: frames_of_100ns(min_100ns, format.rate) })
}

/// "128 frames (2.67 ms)": a 100 ns duration at `rate`.
fn period_text(duration_100ns: i64, rate: u32) -> String {
    format!("{} frames ({:.2} ms)", frames_of_100ns(duration_100ns, rate), duration_100ns as f64 / 10_000.0)
}

/// A failed exclusive `Initialize` at `requested_100ns`. An out-of-range period names the
/// requested period and the range the device reported; endpoint-create-failed names the requested
/// period too, because some USB drivers answer an unsupported period with it.
fn initialize_error(error: &ApiError, rate: u32, requested_100ns: i64, default_100ns: i64, min_100ns: i64) -> ab::OpenError {
    let requested = period_text(requested_100ns, rate);
    match error.code {
        E_INVALID_DEVICE_PERIOD => ab::OpenError::PeriodRefused {
            requested_frames: frames_of_100ns(requested_100ns, rate),
            min_frames: Some(frames_of_100ns(min_100ns, rate)),
            default_frames: Some(frames_of_100ns(default_100ns, rate)),
            message: format!(
                "Initialize(exclusive): the period of {} frames is below what the device allows (minimum {}, default {} at {rate} Hz)",
                frames_of_100ns(requested_100ns, rate),
                frames_of_100ns(min_100ns, rate),
                frames_of_100ns(default_100ns, rate)
            ),
        },
        E_BUFFER_SIZE_ERROR => ab::OpenError::PeriodRefused {
            requested_frames: frames_of_100ns(requested_100ns, rate),
            min_frames: Some(frames_of_100ns(min_100ns, rate)),
            default_frames: Some(frames_of_100ns(default_100ns, rate)),
            message: format!(
                "Initialize(exclusive): the period of {requested} is outside the range the device accepts: at least {} at {rate} Hz, \
                 default {} ({})",
                period_text(min_100ns, rate),
                period_text(default_100ns, rate),
                error.text
            ),
        },
        E_ENDPOINT_CREATE_FAILED => ab::OpenError::DeviceNotAvailable(format!(
            "Initialize(exclusive) at a period of {requested}, {rate} Hz: {} (some drivers answer an unsupported period this way; \
             the device default is {})",
            error.text,
            period_text(default_100ns, rate)
        )),
        _ => open_error("Initialize(exclusive)", error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use transit_audio_backend::{Format, SampleFormat};

    /// A scripted device: the formats it takes, the `Initialize` results in order, the sizes
    /// `GetBufferSize` reports, and a log of every call.
    #[derive(Default)]
    struct Fake {
        accepts: Vec<Format>,
        supports_error: Option<i32>,
        activate_error: Option<i32>,
        inits: VecDeque<Result<(), i32>>,
        sizes: VecDeque<u32>,
        /// (default, minimum) in 100 ns.
        period: (i64, i64),
        next_client: u32,
        log: Vec<String>,
    }

    fn err(code: i32) -> ApiError {
        ApiError { code, text: format!("{code:#x}") }
    }

    impl ExclusiveApi for Fake {
        type Client = u32;
        fn activate(&mut self) -> Result<u32, ApiError> {
            if let Some(code) = self.activate_error {
                return Err(err(code));
            }
            self.next_client += 1;
            self.log.push(format!("activate {}", self.next_client));
            Ok(self.next_client)
        }
        fn device_period(&mut self, _client: &u32) -> Result<(i64, i64), ApiError> {
            Ok(self.period)
        }
        fn supports(&mut self, _client: &u32, format: &Format) -> Result<bool, ApiError> {
            if let Some(code) = self.supports_error {
                return Err(err(code));
            }
            Ok(self.accepts.contains(format))
        }
        fn initialize(&mut self, client: &u32, format: &Format, duration_100ns: i64) -> Result<(), ApiError> {
            self.log.push(format!("initialize {client} {:?} {duration_100ns}", format.sample));
            self.inits.pop_front().unwrap_or(Ok(())).map_err(err)
        }
        fn buffer_size(&mut self, client: &u32) -> Result<u32, ApiError> {
            let size = self.sizes.pop_front().unwrap_or(0);
            self.log.push(format!("buffer size {client} = {size}"));
            Ok(size)
        }
    }

    fn i32_24(rate: u32) -> Format {
        Format { valid_bits: Some(24), ..Format::new(rate, 2, SampleFormat::I32) }
    }

    fn scarlett() -> Fake {
        // 3 ms minimum, 10 ms default; takes 24-in-32 and 16-bit stereo at 44.1 and 48 kHz.
        Fake {
            accepts: vec![i32_24(48_000), Format::new(48_000, 2, SampleFormat::I16), i32_24(44_100)],
            period: (100_000, 30_000),
            ..Default::default()
        }
    }

    #[test]
    fn the_requested_format_is_kept_when_the_device_takes_it() {
        let mut fake = scarlett();
        fake.sizes.push_back(480);
        let opened = open(&mut fake, &i32_24(48_000), ab::Period::Default).unwrap();
        assert_eq!((opened.format, opened.buffer_frames, opened.client), (i32_24(48_000), 480, 1));
        assert_eq!(fake.log, ["activate 1", "initialize 1 I32 100000", "buffer size 1 = 480"], "default is the device default period");
        assert_eq!(opened.min_period, 144, "the minimum is reported, not used");
    }

    #[test]
    fn an_explicit_period_below_the_default_is_honored_as_asked() {
        let mut fake = scarlett();
        fake.sizes.push_back(144);
        let opened = open(&mut fake, &i32_24(48_000), ab::Period::Frames(144)).unwrap();
        assert_eq!((opened.buffer_frames, opened.min_period), (144, 144));
        assert_eq!(fake.log[1], "initialize 1 I32 30000");
    }

    #[test]
    fn an_unsupported_request_takes_the_first_supported_candidate_at_its_rate_and_channels() {
        let mut fake = scarlett();
        fake.sizes.push_back(480);
        // The shared mix format (float, front-pair mask) is not taken: 24-in-32 is the first candidate.
        let mix = Format { channel_mask: Some(3), ..Format::new(48_000, 2, SampleFormat::F32) };
        let opened = open(&mut fake, &mix, ab::Period::Frames(480)).unwrap();
        assert_eq!(opened.format, i32_24(48_000));
        assert_eq!(fake.log[1], "initialize 1 I32 100000", "Period::Frames(480) at 48 kHz is 10 ms");
        let mut none = scarlett();
        let error = open(&mut none, &Format::new(96_000, 2, SampleFormat::F32), ab::Period::Default).err().unwrap();
        assert!(matches!(&error, ab::OpenError::Other(t) if t.contains("96000 Hz, 2-channel")), "{error:?}");
    }

    #[test]
    fn a_not_aligned_period_is_retried_once_on_a_fresh_client_at_the_aligned_size() {
        let mut fake = scarlett();
        fake.inits = VecDeque::from([Err(E_BUFFER_SIZE_NOT_ALIGNED), Ok(())]);
        fake.sizes = VecDeque::from([132, 132]);
        let opened = open(&mut fake, &i32_24(48_000), ab::Period::Frames(128)).unwrap();
        assert_eq!((opened.client, opened.buffer_frames), (2, 132), "the second client runs at the aligned size");
        // 128 frames is 26_667 hns; the aligned 132 frames is 27_500 hns.
        assert_eq!(
            fake.log,
            ["activate 1", "initialize 1 I32 26667", "buffer size 1 = 132", "activate 2", "initialize 2 I32 27500", "buffer size 2 = 132"]
        );
    }

    #[test]
    fn a_second_alignment_failure_is_not_retried_again() {
        let mut fake = scarlett();
        fake.inits = VecDeque::from([Err(E_BUFFER_SIZE_NOT_ALIGNED), Err(E_BUFFER_SIZE_NOT_ALIGNED), Ok(())]);
        fake.sizes = VecDeque::from([132]);
        assert!(matches!(open(&mut fake, &i32_24(48_000), ab::Period::Frames(128)), Err(ab::OpenError::Other(_))));
        assert_eq!(fake.log.iter().filter(|l| l.starts_with("initialize")).count(), 2);
    }

    #[test]
    fn in_use_disallowed_and_gone_map_to_their_open_errors() {
        for (code, want) in [
            (E_DEVICE_IN_USE, "InUse"),
            (E_EXCLUSIVE_MODE_NOT_ALLOWED, "ExclusiveDisallowed"),
            (E_ENDPOINT_CREATE_FAILED, "DeviceNotAvailable"),
            (E_NOTFOUND, "DeviceNotAvailable"),
            (0x8889_0004_u32 as i32, "DeviceNotAvailable"),
            (0x8000_4005_u32 as i32, "Other"),
        ] {
            // From Initialize, from the format query, and from activation.
            let mut init = scarlett();
            init.inits.push_back(Err(code));
            let mut query = scarlett();
            query.supports_error = Some(code);
            let mut activate = scarlett();
            activate.activate_error = Some(code);
            for fake in [&mut init, &mut query, &mut activate] {
                let error = open(fake, &i32_24(48_000), ab::Period::Default).err().unwrap();
                let got = format!("{error:?}");
                assert!(got.starts_with(want), "{code:#x}: {got}");
            }
        }
        let error = open_error("Initialize", &err(E_EXCLUSIVE_MODE_NOT_ALLOWED));
        assert!(error.to_string().contains("Advanced"), "the message says where to allow it: {error}");
    }

    #[test]
    fn an_out_of_range_period_names_the_range_the_device_reported() {
        let mut fake = scarlett();
        fake.inits.push_back(Err(E_BUFFER_SIZE_ERROR));
        let error = open(&mut fake, &i32_24(48_000), ab::Period::Frames(16)).err().unwrap();
        let ab::OpenError::PeriodRefused { requested_frames, min_frames, default_frames, message: text } = error else { panic!("{error:?}") };
        assert_eq!((requested_frames, min_frames, default_frames), (16, Some(144), Some(480)));
        assert!(text.contains("the period of 16 frames (0.33 ms) is outside"), "{text}");
        assert!(text.contains("at least 144 frames (3.00 ms) at 48000 Hz, default 480 frames (10.00 ms)"), "{text}");
    }

    #[test]
    fn an_invalid_device_period_is_a_refused_period_with_the_range() {
        let mut fake = scarlett();
        fake.inits.push_back(Err(E_INVALID_DEVICE_PERIOD));
        let error = open(&mut fake, &i32_24(48_000), ab::Period::Frames(128)).err().unwrap();
        let ab::OpenError::PeriodRefused { requested_frames, min_frames, default_frames, message: text } = error else { panic!("{error:?}") };
        assert_eq!((requested_frames, min_frames, default_frames), (128, Some(144), Some(480)));
        assert_eq!(text, "Initialize(exclusive): the period of 128 frames is below what the device allows (minimum 144, default 480 at 48000 Hz)");
    }

    #[test]
    fn endpoint_create_failed_on_initialize_names_the_requested_period() {
        let mut fake = scarlett();
        fake.inits.push_back(Err(E_ENDPOINT_CREATE_FAILED));
        let error = open(&mut fake, &i32_24(48_000), ab::Period::Frames(64)).err().unwrap();
        let ab::OpenError::DeviceNotAvailable(text) = error else { panic!("{error:?}") };
        assert!(text.contains("at a period of 64 frames (1.33 ms), 48000 Hz"), "{text}");
        assert!(text.contains("the device default is 480 frames (10.00 ms)"), "{text}");
    }

    #[test]
    fn the_probe_keeps_accepted_candidates_in_order_and_aborts_on_in_use() {
        let mut fake = scarlett();
        let candidates = ab::exclusive_candidates(48_000, &[2]);
        let got = probe(&mut fake, &0, &candidates).unwrap();
        assert_eq!(got, [i32_24(48_000), Format::new(48_000, 2, SampleFormat::I16), i32_24(44_100)]);
        fake.supports_error = Some(E_DEVICE_IN_USE);
        assert!(matches!(probe(&mut fake, &0, &candidates), Err(ab::OpenError::InUse(_))));
        fake.supports_error = Some(E_EXCLUSIVE_MODE_NOT_ALLOWED);
        assert!(matches!(probe(&mut fake, &0, &candidates), Err(ab::OpenError::ExclusiveDisallowed(_))));
    }

    #[test]
    fn durations_and_frames_round_to_nearest() {
        assert_eq!(frames_to_100ns(480, 48_000), 100_000);
        assert_eq!(frames_to_100ns(128, 48_000), 26_667);
        assert_eq!(frames_to_100ns(441, 44_100), 100_000);
        assert_eq!(frames_of_100ns(26_667, 48_000), 128);
        assert_eq!(frames_of_100ns(30_000, 48_000), 144);
    }
}
