//! The audio benchmark harness (ADR 0016 slice b). It drives any [`Backend`] through the trait
//! only and measures by host arrival time at callback entry: achieved period, arrival jitter,
//! xruns (gaps in the arrival times), callback time and, with an output-to-input cable, the
//! loopback round trip. Results are a Markdown table (`docs/benchmarks/`).
//!
//! The callbacks (`probe`) never allocate or lock; `analysis` and `report` run after the streams
//! stop. `fake` is the device-free mode used by tests and CI.

pub mod analysis;
pub mod cli;
pub mod fake;
pub mod probe;
pub mod report;
pub mod session;

use transit_audio_backend as ab;

/// Which backend a run measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The cpal adapter (WASAPI shared or ASIO, chosen with `--system`).
    Cpal,
    /// The native WASAPI backend (shared mode, `IAudioClient3`); needs the `native-wasapi` feature.
    Wasapi,
    /// The scripted [`ab::fake::FakeBackend`] in virtual time.
    Fake,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Cpal => "cpal",
            Kind::Wasapi => "wasapi",
            Kind::Fake => "fake",
        }
    }
}

/// Everything a run needs.
#[derive(Debug, Clone)]
pub struct Config {
    pub kind: Kind,
    pub system: Option<String>,
    pub request: session::Request,
    pub seconds: f64,
    pub fake: fake::Script,
    pub out: Option<std::path::PathBuf>,
}

/// The period the fake mode pumps when none is requested.
pub const FAKE_DEFAULT_PERIOD: u32 = 64;

/// Run one measurement and return its table row.
pub fn run(config: &Config) -> Result<report::Row, String> {
    let shared = probe::Shared::new(config.request.rate, config.seconds);
    let (system, opened) = match config.kind {
        Kind::Fake => {
            let period = config.request.period.unwrap_or(FAKE_DEFAULT_PERIOD);
            let fake_backend = fake::backend(&config.request, period);
            let script = fake::Script { seconds: config.seconds, ..config.fake.clone() };
            let handle = fake_backend.clone();
            let opened = session::run(&fake_backend, &config.request, &shared, &mut || fake::drive(&handle, &config.request, period, &script))?;
            (ab::Backend::system(&fake_backend).to_string(), opened)
        }
        Kind::Wasapi => {
            let backend = wasapi_backend()?;
            let seconds = config.seconds;
            let opened = session::run(backend.as_ref(), &config.request, &shared, &mut || {
                std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
                Ok(())
            })?;
            (backend.system().to_string(), opened)
        }
        Kind::Cpal => {
            let backend = cpal_backend(config.system.as_deref())?;
            let seconds = config.seconds;
            let opened = session::run(backend.as_ref(), &config.request, &shared, &mut || {
                std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
                Ok(())
            })?;
            (backend.system().to_string(), opened)
        }
    };
    let measurements = analysis::measure(&shared, opened.render_rate, opened.capture_rate);
    Ok(report::Row {
        backend: config.kind.name().to_string(),
        system,
        device: opened.device,
        requested: config.request.period,
        measurements,
        machine: report::machine(),
        date: report::today(),
    })
}

/// The cpal backend of the audio system named (else the default); an unavailable name is an error
/// rather than the silent fallback the Player takes.
pub fn cpal_backend(system: Option<&str>) -> Result<Box<dyn ab::Backend>, String> {
    use ab::BackendFactory;
    let backend = soundcraft_playback::backend::cpal::CpalFactory.backend(system).ok_or("no audio system is available")?;
    match system {
        Some(want) if !backend.system().eq_ignore_ascii_case(want) => {
            Err(format!("audio system {want} is not available (got {}); ASIO needs a build with --features asio", backend.system()))
        }
        _ => Ok(backend),
    }
}

/// The native WASAPI backend (`--backend wasapi`).
#[cfg(all(windows, feature = "native-wasapi"))]
pub fn wasapi_backend() -> Result<Box<dyn ab::Backend>, String> {
    use ab::BackendFactory;
    transit_audio_wasapi::WasapiFactory::default().backend(None).ok_or_else(|| "the native WASAPI backend is not available".to_string())
}

/// Without the feature (or off Windows) there is no native backend to measure.
#[cfg(not(all(windows, feature = "native-wasapi")))]
pub fn wasapi_backend() -> Result<Box<dyn ab::Backend>, String> {
    Err("--backend wasapi needs Windows and a build with --features native-wasapi".into())
}

/// The device names of a cpal system, for `--list-devices` (enumerates; opens no stream).
pub fn list_devices(system: Option<&str>) -> Result<String, String> {
    let backend = cpal_backend(system)?;
    let mut text = format!("system: {}\n", backend.system());
    for (label, direction) in [("outputs", ab::Direction::Output), ("inputs", ab::Direction::Input)] {
        text.push_str(&format!("{label}:\n"));
        for endpoint in backend.endpoints(direction).map_err(|e| e.to_string())? {
            text.push_str(&format!("  {} ({} ch)\n", endpoint.name, endpoint.inputs.max(endpoint.outputs)));
        }
    }
    Ok(text)
}
