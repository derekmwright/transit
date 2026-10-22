//! Audio device selection: which audio system (cpal host), output and input device, sample rate
//! and buffer size to open. The choice is per machine (the app keeps it with its preferences, not
//! in sessions). Anything that cannot be honoured falls back to the system default: a device that
//! is unplugged, a rate or buffer size the device does not offer.

use cpal::traits::{DeviceTrait, HostTrait};
use std::sync::{Mutex, PoisonError};

/// Rates offered when a device supports them (any rate inside a supported range is possible, but
/// these are the ones interfaces actually run at).
pub const STANDARD_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
/// Buffer sizes offered when the device reports a range that contains them.
pub const STANDARD_BUFFERS: [u32; 9] = [32, 64, 128, 256, 512, 1024, 2048, 4096, 8192];

/// Sample formats the streams convert to and from, best first (ASIO drivers usually run in
/// 32-bit integers, WASAPI and Core Audio in float).
pub(crate) const FORMATS: [cpal::SampleFormat; 3] = [cpal::SampleFormat::F32, cpal::SampleFormat::I32, cpal::SampleFormat::I16];

/// Preference of a sample format (lower is better); `None` when it is not supported.
pub(crate) fn format_rank(f: cpal::SampleFormat) -> Option<usize> {
    FORMATS.iter().position(|x| *x == f)
}

/// Which implementation talks to the audio system (ADR 0016).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// The app's choice: cpal in this slice.
    #[default]
    Auto,
    Cpal,
    /// The native WASAPI backend (shared mode, `IAudioClient3`). Used only on the WASAPI system
    /// of a build with the `native-wasapi` feature; otherwise, or if it fails to open, cpal.
    NativeWasapi,
}

impl Backend {
    pub const ALL: [Backend; 3] = [Backend::Auto, Backend::Cpal, Backend::NativeWasapi];

    /// The label of the Audio Devices selector.
    pub const fn label(self) -> &'static str {
        match self {
            Backend::Auto => "Auto",
            Backend::Cpal => "cpal",
            Backend::NativeWasapi => "Native WASAPI (experimental)",
        }
    }
}

/// Whether this build can open the native WASAPI backend (Windows with the `native-wasapi` feature).
pub const NATIVE_WASAPI_COMPILED: bool = cfg!(all(windows, feature = "native-wasapi"));

/// Whether `system` (the choice's audio system; `None` is the platform default) is WASAPI.
pub fn is_wasapi_system(system: Option<&str>) -> bool {
    system.map_or(cfg!(windows), |s| s.eq_ignore_ascii_case("WASAPI"))
}

/// Whether the Backend selector applies: the feature is compiled in and the system is WASAPI.
pub fn native_wasapi_offered(system: Option<&str>) -> bool {
    NATIVE_WASAPI_COMPILED && is_wasapi_system(system)
}

/// The user's audio device choice. Every field is optional: `None` means "the system default".
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Which backend opens the device on the WASAPI system (`Auto` is cpal in this slice).
    pub backend: Backend,
    /// Audio system (cpal host name: "WASAPI", "ASIO", "CoreAudio", "ALSA", "JACK", ...).
    pub system: Option<String>,
    pub output: Option<String>,
    pub input: Option<String>,
    /// The native WASAPI backend opens this input (endpoint id or name) together with the output
    /// for duplex recording, for interfaces whose endpoints share no container id. `None` pairs
    /// by container id alone.
    pub input_pairing: Option<String>,
    /// Device sample rate; the mix resamples when it differs from the session's.
    pub sample_rate: Option<u32>,
    /// Device buffer in frames.
    pub buffer_size: Option<u32>,
    /// Loopback calibrations of device pairs (`audio-bench --loopback --settings`). Only the
    /// native WASAPI backend applies one, and only on an exact key match.
    pub calibrations: Vec<LoopbackCalibration>,
    /// The native WASAPI backend opens the device shared (through the Windows mixer) instead of
    /// exclusive, which is its default (ADR 0016, amendment of 2026-10-22). Missing in a saved
    /// config reads as alse: exclusive.
    pub share_device: bool,
}

/// The `mode` key of a shared-mode calibration.
pub const MODE_SHARED: &str = "shared";
/// The `mode` key of an exclusive-mode calibration (`audio-bench --mode exclusive`).
pub const MODE_EXCLUSIVE: &str = "exclusive";

/// One measured loopback of an output/input pair. The key is `(device, input, mode, rate,
/// period)`; a calibration applies to a stream pair only when every part of the key is equal,
/// because the round trip changes with any of them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LoopbackCalibration {
    /// The output device name, as the backend lists it.
    pub device: String,
    /// The paired input as the user named it (`input_pairing`); `None` pairs by container id.
    pub input: Option<String>,
    /// How the device was opened ([`MODE_SHARED`] or [`MODE_EXCLUSIVE`]).
    pub mode: String,
    pub rate: u32,
    /// The requested period in frames; 0 is the driver's default.
    pub period: u32,
    /// The median round trip, in frames at `rate`.
    pub round_trip_frames: u32,
    /// The uncertainty of `round_trip_frames`, in frames: the measured spread plus one frame of
    /// position granularity.
    pub spread_frames: u32,
    /// UTC date of the measurement (`YYYY-MM-DD`).
    pub measured_at: String,
}

impl LoopbackCalibration {
    /// Whether `other` measured the same device pair under the same conditions.
    pub fn same_key(&self, other: &Self) -> bool {
        self.device.eq_ignore_ascii_case(&other.device)
            && self.input == other.input
            && self.mode == other.mode
            && self.rate == other.rate
            && self.period == other.period
    }

    /// The calibration the backend applies: the round trip and its bound in nanoseconds.
    pub fn to_backend(&self) -> Option<transit_audio_backend::Calibration> {
        if self.rate == 0 {
            return None;
        }
        let ns = |frames: u32| u64::from(frames) * 1_000_000_000 / u64::from(self.rate);
        Some(transit_audio_backend::Calibration { round_trip_ns: ns(self.round_trip_frames), uncertainty_ns: ns(self.spread_frames) })
    }
}

impl AudioConfig {
    /// The saved calibration of the pair opened on output `device` in `mode` ([`MODE_SHARED`] or
    /// [`MODE_EXCLUSIVE`]) at `rate` and the requested `period` (0 is the driver's default), under
    /// this config's paired input.
    pub fn find_calibration(&self, device: &str, mode: &str, rate: u32, period: u32) -> Option<&LoopbackCalibration> {
        let want = LoopbackCalibration {
            device: device.to_owned(),
            input: self.input_pairing.clone(),
            mode: mode.to_owned(),
            rate,
            period,
            round_trip_frames: 0,
            spread_frames: 0,
            measured_at: String::new(),
        };
        self.calibrations.iter().find(|c| c.same_key(&want))
    }

    /// The calibration for the pair opened on output `device` with `request`; `None` when no saved
    /// calibration has exactly that key.
    pub fn calibration_for(&self, device: &str, request: &transit_audio_backend::OpenRequest) -> Option<transit_audio_backend::Calibration> {
        let mode = match request.mode {
            transit_audio_backend::Mode::Shared => MODE_SHARED,
            transit_audio_backend::Mode::Exclusive => MODE_EXCLUSIVE,
            _ => return None,
        };
        self.find_calibration(device, mode, request.format.rate, request.period.frames().unwrap_or(0))?.to_backend()
    }
}

/// One device as the settings window lists it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DeviceInfo {
    pub name: String,
    pub is_default: bool,
    /// Most channels any usable configuration offers.
    pub max_channels: u16,
    /// Standard rates the device accepts (in a format the streams convert).
    pub sample_rates: Vec<u32>,
    /// Buffer range the device reports (`None` when it does not say).
    pub buffer_min: Option<u32>,
    pub buffer_max: Option<u32>,
}

impl DeviceInfo {
    /// Standard buffer sizes inside the device's range (all of them when the range is unknown).
    pub fn buffer_sizes(&self) -> Vec<u32> {
        STANDARD_BUFFERS.iter().copied().filter(|b| self.buffer_min.is_none_or(|m| *b >= m) && self.buffer_max.is_none_or(|m| *b <= m)).collect()
    }
}

/// An audio system and its devices.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AudioSystem {
    pub name: String,
    pub is_default: bool,
    pub outputs: Vec<DeviceInfo>,
    pub inputs: Vec<DeviceInfo>,
}

#[allow(deprecated)]
fn device_name(d: &cpal::Device) -> Option<String> {
    d.name().ok()
}

fn describe(d: &cpal::Device, default_name: Option<&str>, output: bool) -> Option<DeviceInfo> {
    let name = device_name(d)?;
    let ranges: Vec<cpal::SupportedStreamConfigRange> = if output {
        d.supported_output_configs().ok()?.filter(|c| format_rank(c.sample_format()).is_some()).collect()
    } else {
        d.supported_input_configs().ok()?.filter(|c| format_rank(c.sample_format()).is_some()).collect()
    };
    let max_channels = ranges.iter().map(cpal::SupportedStreamConfigRange::channels).max().unwrap_or(0);
    let sample_rates =
        STANDARD_RATES.iter().copied().filter(|r| ranges.iter().any(|c| c.min_sample_rate().0 <= *r && c.max_sample_rate().0 >= *r)).collect();
    let (mut buffer_min, mut buffer_max) = (None::<u32>, None::<u32>);
    for c in &ranges {
        if let cpal::SupportedBufferSize::Range { min, max } = c.buffer_size() {
            buffer_min = Some(buffer_min.map_or(*min, |m| m.min(*min)));
            buffer_max = Some(buffer_max.map_or(*max, |m| m.max(*max)));
        }
    }
    Some(DeviceInfo { is_default: default_name == Some(name.as_str()), name, max_channels, sample_rates, buffer_min, buffer_max })
}

/// cpal's name for its ASIO host (Windows builds with the `asio` feature).
pub const ASIO_SYSTEM: &str = "ASIO";
/// Whether this build can open an ASIO driver's control panel.
pub const ASIO_CONTROL_PANEL: bool = soundcraft_asio_panel::AVAILABLE;

/// Opens the control panel of the ASIO driver that is open (ASIO4ALL's device choice, an
/// interface's buffer size and clock...). Drivers do not reliably report what changed there:
/// reopen the device afterwards.
pub fn asio_control_panel() -> Result<(), String> {
    soundcraft_asio_panel::open_control_panel()
}

/// Every audio system on this machine with its devices. Enumerating can take a moment (some
/// systems probe each device): call it when the settings window opens, not every frame.
pub fn audio_systems() -> Vec<AudioSystem> {
    let default_id = cpal::default_host().id();
    cpal::available_hosts()
        .into_iter()
        .filter_map(|id| {
            let host = shared_host(id)?;
            let def_out = host.default_output_device().as_ref().and_then(device_name);
            let def_in = host.default_input_device().as_ref().and_then(device_name);
            let mut outputs: Vec<DeviceInfo> =
                host.output_devices().map(|ds| ds.filter_map(|d| describe(&d, def_out.as_deref(), true)).collect()).unwrap_or_default();
            let mut inputs: Vec<DeviceInfo> =
                host.input_devices().map(|ds| ds.filter_map(|d| describe(&d, def_in.as_deref(), false)).collect()).unwrap_or_default();
            if id.name() == ASIO_SYSTEM {
                let installed = soundcraft_asio_panel::driver_names();
                add_unlisted(&mut outputs, &installed);
                add_unlisted(&mut inputs, &installed);
            }
            Some(AudioSystem { name: id.name().to_string(), is_default: id == default_id, outputs, inputs })
        })
        .collect()
}

/// Adds the installed ASIO drivers cpal could not list. cpal loads each driver to describe it, and
/// the SDK cannot load one while another is open: without this, only the open driver showed and
/// switching to another meant going through a different audio system first. Like REAPER, every
/// installed driver is offered; one that turns out to be absent falls back to the default.
fn add_unlisted(list: &mut Vec<DeviceInfo>, installed: &[String]) {
    for name in installed {
        if !list.iter().any(|d| &d.name == name) {
            list.push(DeviceInfo {
                name: name.clone(),
                is_default: false,
                max_channels: 0,
                // Not known until the driver is open: offer the standard choices.
                sample_rates: STANDARD_RATES.to_vec(),
                buffer_min: None,
                buffer_max: None,
            });
        }
    }
}

/// One cpal host per audio system for the whole process, created on first use (a handful at most,
/// kept for good). ASIO needs this: the SDK holds a single global driver and loading any driver
/// first unloads it, while each cpal ASIO host only knows the drivers it loaded itself. With a
/// fresh host per call, listing devices or opening a new stream unloaded the driver a running
/// stream used (its control panel then reported no driver). One host returns the open driver
/// instead, and refuses to load another while it is in use.
fn shared_host(id: cpal::HostId) -> Option<&'static cpal::Host> {
    static HOSTS: Mutex<Vec<&'static cpal::Host>> = Mutex::new(Vec::new());
    let mut hosts = HOSTS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(h) = hosts.iter().find(|h| h.id() == id) {
        return Some(h);
    }
    let host: &'static cpal::Host = Box::leak(Box::new(cpal::host_from_id(id).ok()?));
    hosts.push(host);
    Some(host)
}

/// The named audio system, else the default one.
pub(crate) fn host_named(system: Option<&str>) -> Option<&'static cpal::Host> {
    let chosen = system.and_then(|want| cpal::available_hosts().into_iter().find(|id| id.name().eq_ignore_ascii_case(want))).and_then(shared_host);
    let host = chosen.or_else(|| {
        if let Some(s) = system {
            log::warn!("audio system `{s}` is not available; using the default");
        }
        shared_host(cpal::default_host().id())
    });
    if host.is_none_or(|h| h.id().name() != ASIO_SYSTEM) {
        release_asio_device_cache();
    }
    host
}

/// The chosen audio system, else the default one.
#[cfg(test)]
pub(crate) fn host(cfg: &AudioConfig) -> Option<&'static cpal::Host> {
    host_named(cfg.system.as_deref())
}

pub use crate::backend::cpal::release_asio_device_cache;

pub(crate) fn cached_device<D: Clone>(
    open: &mut Option<D>,
    asio: bool,
    name: Option<&str>,
    output: bool,
    device_name: impl Fn(&D) -> Option<String>,
    load: impl FnOnce() -> Option<D>,
) -> Option<D> {
    let changed_driver = name.is_some_and(|n| open.as_ref().is_some_and(|d| device_name(d).as_deref() != Some(n)));
    if !asio || output || changed_driver {
        // Release BEFORE loading: assigning the new value after load keeps the old driver alive.
        *open = None;
    }
    if asio
        && !output
        && let Some(d) = open.as_ref()
        && (name.is_none() || device_name(d).as_deref() == name)
    {
        return Some(d.clone());
    }
    let d = load();
    if asio {
        open.clone_from(&d);
    }
    d
}

pub(crate) fn find_device(host: &cpal::Host, name: Option<&str>, output: bool) -> Option<cpal::Device> {
    if let Some(want) = name {
        let found = if output { host.output_devices().ok() } else { host.input_devices().ok() }
            .and_then(|mut ds| ds.find(|d| device_name(d).as_deref() == Some(want)));
        if found.is_some() {
            return found;
        }
        log::warn!("audio device `{want}` is not available; using the default");
    }
    if output { host.default_output_device() } else { host.default_input_device() }
}

/// The buffer to request: the chosen size when the device's range allows it, else the default.
pub(crate) fn buffer_size(want: Option<u32>, supported: &cpal::SupportedBufferSize) -> cpal::BufferSize {
    match (want, supported) {
        (Some(n), cpal::SupportedBufferSize::Range { min, max }) if (*min..=*max).contains(&n) => cpal::BufferSize::Fixed(n),
        // The device does not say: ask anyway (cpal reports an error if it cannot, and the caller
        // falls back to the default).
        (Some(n), cpal::SupportedBufferSize::Unknown) if (16..=8192).contains(&n) => cpal::BufferSize::Fixed(n),
        _ => cpal::BufferSize::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asio_cache_releases_before_loading_another_driver_and_leaving_asio() {
        use std::sync::{Arc, Weak};
        // Like asio-sys, the loader refuses B while any strong reference to A survives.
        let mut loaded = Weak::<String>::new();
        let mut cache = None;
        let mut load = |name: &str| {
            if let Some(d) = loaded.upgrade() {
                return (d.as_str() == name).then_some(d);
            }
            let d = Arc::new(name.to_string());
            loaded = Arc::downgrade(&d);
            Some(d)
        };
        let name = |d: &Arc<String>| Some(d.as_ref().clone());
        let a = cached_device(&mut cache, true, Some("A"), true, name, || load("A")).unwrap();
        let input = cached_device(&mut cache, true, None, false, name, || panic!("input must share the output's buffers")).unwrap();
        assert!(Arc::ptr_eq(&a, &input));
        drop(a);
        assert!(cache.is_some(), "the input still uses A");
        drop(input);
        let b = cached_device(&mut cache, true, Some("B"), true, name, || load("B")).expect("A must unload BEFORE loading B");
        assert_eq!(b.as_str(), "B");
        drop(b);
        let _ = cached_device(&mut cache, false, None, true, name, || None);
        assert!(cache.is_none(), "WASAPI must not retain the ASIO driver");
        let a = load("A").expect("B must also unload when switching through WASAPI");
        assert_eq!(a.as_str(), "A");
        drop(a);
        let a = cached_device(&mut cache, true, Some("A"), false, name, || load("A")).unwrap();
        drop(a);
        let b = cached_device(&mut cache, true, Some("B"), false, name, || load("B")).expect("recorder-only driver changes must unload A too");
        assert_eq!(b.as_str(), "B");
    }

    fn sample_calibration() -> LoopbackCalibration {
        LoopbackCalibration {
            device: "Speakers (Scarlett 2i2)".into(),
            input: Some("Line (Scarlett 2i2)".into()),
            mode: MODE_SHARED.into(),
            rate: 48_000,
            period: 128,
            round_trip_frames: 480,
            spread_frames: 25,
            measured_at: "2026-10-21".into(),
        }
    }

    fn request(rate: u32, period: transit_audio_backend::Period) -> transit_audio_backend::OpenRequest {
        use transit_audio_backend as ab;
        ab::OpenRequest {
            format: ab::Format::new(rate, 2, ab::SampleFormat::F32),
            period,
            mode: ab::Mode::Shared,
            origin: ab::HostOrigin::default(),
            calibration: None,
        }
    }

    #[test]
    fn a_calibration_applies_only_to_its_exact_key() {
        use transit_audio_backend::Period;
        let cal = sample_calibration();
        let cfg = AudioConfig { input_pairing: cal.input.clone(), calibrations: vec![cal.clone()], ..Default::default() };
        let found = cfg.calibration_for("speakers (scarlett 2i2)", &request(48_000, Period::Frames(128))).expect("the key matches");
        assert_eq!((found.round_trip_ns, found.uncertainty_ns), (10_000_000, 520_833));
        let device = "Speakers (Scarlett 2i2)";
        assert!(cfg.calibration_for(device, &request(44_100, Period::Frames(128))).is_none(), "another rate");
        assert!(cfg.calibration_for(device, &request(48_000, Period::Frames(64))).is_none(), "another period");
        assert!(cfg.calibration_for(device, &request(48_000, Period::Default)).is_none(), "the default period is not 128");
        assert!(cfg.calibration_for("Headphones", &request(48_000, Period::Frames(128))).is_none(), "another device");
        let other_input = AudioConfig { input_pairing: Some("Mic".into()), ..cfg.clone() };
        assert!(other_input.calibration_for(device, &request(48_000, Period::Frames(128))).is_none(), "another input");
        let unpaired = AudioConfig { input_pairing: None, ..cfg };
        assert!(unpaired.calibration_for(device, &request(48_000, Period::Frames(128))).is_none(), "container-id pairing is its own key");
    }

    #[test]
    fn calibrations_survive_serde_and_old_files_have_none() {
        let cfg = AudioConfig { calibrations: vec![sample_calibration()], ..Default::default() };
        let text = serde_json::to_string(&cfg).unwrap();
        assert_eq!(serde_json::from_str::<AudioConfig>(&text).unwrap().calibrations, cfg.calibrations);
        assert!(serde_json::from_str::<AudioConfig>(r#"{"output":"X"}"#).unwrap().calibrations.is_empty());
    }

    #[test]
    fn config_round_trips_and_tolerates_missing_fields() {
        let c = AudioConfig {
            system: Some("WASAPI".into()),
            output: Some("Speakers".into()),
            input: None,
            input_pairing: Some("{0.0.1.00000000}.{abc}".into()),
            sample_rate: Some(48_000),
            buffer_size: Some(256),
            backend: Backend::NativeWasapi,
            calibrations: vec![sample_calibration()],
            share_device: true,
        };
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<AudioConfig>(&s).unwrap(), c);
        assert_eq!(serde_json::from_str::<AudioConfig>("{}").unwrap(), AudioConfig::default());
        assert_eq!(serde_json::from_str::<AudioConfig>("{}").unwrap().backend, Backend::Auto, "settings saved before the field exist stay on Auto");
        assert!(s.contains("\"native_wasapi\""));
        assert_eq!(serde_json::from_str::<AudioConfig>(r#"{"output":"X","unknown":1}"#).unwrap().output.as_deref(), Some("X"));
    }

    #[test]
    fn the_share_checkbox_defaults_to_exclusive_and_round_trips() {
        assert!(!AudioConfig::default().share_device, "exclusive is the default");
        assert!(
            !serde_json::from_str::<AudioConfig>(r#"{"output":"X"}"#).unwrap().share_device,
            "a config saved before the field exists is exclusive"
        );
        let shared = AudioConfig { share_device: true, ..Default::default() };
        let text = serde_json::to_string(&shared).unwrap();
        assert!(serde_json::from_str::<AudioConfig>(&text).unwrap().share_device);
    }

    #[test]
    fn a_calibration_is_keyed_by_the_requests_mode() {
        use transit_audio_backend::{Mode, Period};
        let shared = sample_calibration();
        let exclusive = LoopbackCalibration { mode: MODE_EXCLUSIVE.into(), round_trip_frames: 960, ..shared.clone() };
        let cfg = AudioConfig { input_pairing: shared.input.clone(), calibrations: vec![shared, exclusive], ..Default::default() };
        let device = "Speakers (Scarlett 2i2)";
        let mut req = request(48_000, Period::Frames(128));
        assert_eq!(cfg.calibration_for(device, &req).unwrap().round_trip_ns, 10_000_000, "shared request, shared calibration");
        req.mode = Mode::Exclusive;
        assert_eq!(cfg.calibration_for(device, &req).unwrap().round_trip_ns, 20_000_000, "exclusive request, exclusive calibration");
        let only_shared = AudioConfig { calibrations: vec![sample_calibration()], ..cfg };
        assert!(only_shared.calibration_for(device, &req).is_none(), "a shared calibration never applies to an exclusive open");
    }

    #[test]
    fn buffer_sizes_follow_the_device_range() {
        let r = cpal::SupportedBufferSize::Range { min: 64, max: 1024 };
        assert_eq!(buffer_size(Some(256), &r), cpal::BufferSize::Fixed(256));
        assert_eq!(buffer_size(Some(32), &r), cpal::BufferSize::Default);
        assert_eq!(buffer_size(Some(4096), &r), cpal::BufferSize::Default);
        assert_eq!(buffer_size(None, &r), cpal::BufferSize::Default);
        assert_eq!(buffer_size(Some(128), &cpal::SupportedBufferSize::Unknown), cpal::BufferSize::Fixed(128));
        assert_eq!(buffer_size(Some(1 << 20), &cpal::SupportedBufferSize::Unknown), cpal::BufferSize::Default);
        let d = DeviceInfo {
            name: "x".into(),
            is_default: true,
            max_channels: 2,
            sample_rates: vec![48_000],
            buffer_min: Some(64),
            buffer_max: Some(1024),
        };
        assert_eq!(d.buffer_sizes(), [64, 128, 256, 512, 1024]);
        let open = DeviceInfo { buffer_min: None, buffer_max: None, ..d };
        assert_eq!(open.buffer_sizes().len(), STANDARD_BUFFERS.len());
    }

    #[test]
    fn installed_asio_drivers_cpal_could_not_list_are_offered() {
        let open = DeviceInfo {
            name: "Open Driver".into(),
            is_default: false,
            max_channels: 2,
            sample_rates: vec![48_000],
            buffer_min: Some(64),
            buffer_max: Some(1024),
        };
        let mut list = vec![open.clone()];
        add_unlisted(&mut list, &["Open Driver".into(), "Other Driver".into()]);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], open, "a described device keeps what cpal reported");
        assert_eq!(list[1].name, "Other Driver");
        assert_eq!(list[1].sample_rates, STANDARD_RATES);
        assert_eq!(list[1].buffer_sizes().len(), STANDARD_BUFFERS.len());
    }

    #[test]
    #[ignore = "hardware enumeration: opt in only on a test audio machine"]
    fn unknown_systems_and_devices_fall_back_to_the_defaults() {
        let cfg = AudioConfig { system: Some("No Such System".into()), ..Default::default() };
        assert_eq!(host(&cfg).map(|h| h.id()), Some(cpal::default_host().id()));
        // No device with this name exists: the default (or none, on a machine without audio).
        let h = cpal::default_host();
        let fallback = crate::backend::cpal::device_on(&h, Some("No Such Device 1234"), true).as_ref().and_then(device_name);
        assert_eq!(fallback, h.default_output_device().as_ref().and_then(device_name));
    }

    #[test]
    #[ignore = "hardware enumeration loads ASIO drivers: opt in only on a test audio machine"]
    fn enumeration_does_not_fail() {
        let systems = audio_systems();
        // Every listed system has a name; on a machine without audio the lists are just empty.
        assert!(systems.iter().all(|s| !s.name.is_empty()));
        assert!(systems.iter().filter(|s| s.is_default).count() <= 1);
    }
}
