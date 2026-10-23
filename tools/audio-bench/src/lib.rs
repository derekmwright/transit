//! The audio benchmark harness (ADR 0016 slice b). It drives any [`Backend`] through the trait
//! only and measures by host arrival time at callback entry: achieved period, arrival jitter,
//! xruns (steps in the arrival drift and render callbacks longer than a period), callback time and, with an output-to-input cable, the
//! loopback round trip. Results are a Markdown table (`docs/benchmarks/`).
//!
//! The callbacks (`probe`) never allocate or lock; `analysis` and `report` run after the streams
//! stop. `fake` is the device-free mode used by tests and CI.

pub mod analysis;
pub mod calibration;
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
    /// The native WASAPI backend (shared mode through `IAudioClient3`, or exclusive with
    /// `--mode exclusive`); needs the `native-wasapi` feature.
    Wasapi,
    /// The native ASIO backend (`transit-audio-asio`); needs the `native-asio` feature.
    Asio,
    /// The scripted [`ab::fake::FakeBackend`] in virtual time.
    Fake,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Cpal => "cpal",
            Kind::Wasapi => "wasapi",
            Kind::Asio => "asio",
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
    /// `ui.json` to merge the measured loopback calibration into (`--settings`).
    pub settings: Option<std::path::PathBuf>,
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
        Kind::Asio => {
            let backend = asio_backend()?;
            let request = resolve_asio_device(&config.request)?;
            let seconds = config.seconds;
            let opened = session::run(backend.as_ref(), &request, &shared, &mut || {
                std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
                Ok(())
            })?;
            (backend.system().to_string(), opened)
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
    let date = report::today();
    // Frame times of the two streams are comparable only when one backend stamps both (the native
    // one, in the shared host origin); a cpal run measures no calibration.
    let calibration = (config.kind != Kind::Cpal && opened.capture_rate == Some(opened.render_rate))
        .then_some(measurements.frame_trip)
        .flatten()
        .map(|trip| calibration::figures(&trip, opened.render_rate, &measurements.input_peaks));
    if let Some(path) = &config.settings {
        let figures = calibration.clone().ok_or("--settings needs a native loopback run (--backend wasapi --loopback) at one rate")??;
        let record = calibration::record(
            &figures,
            &opened.device,
            config.request.in_device.as_deref(),
            config.request.mode,
            config.request.period.unwrap_or(0),
            &date,
        );
        calibration::merge_settings(path, &record)?;
    }
    Ok(report::Row {
        backend: config.kind.name().to_string(),
        system,
        device: opened.device,
        requested: config.request.period,
        mode: (config.request.mode == ab::Mode::Exclusive).then(|| report::exclusive_note(&opened.render)),
        measurements,
        calibration,
        render_stats: opened.render_stats,
        capture_stats: opened.capture_stats,
        render_achieved: Some(opened.render),
        capture_achieved: opened.capture,
        machine: report::machine(),
        date,
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

/// The native ASIO backend (`--backend asio`): the process-wide one the app's factory hands out.
#[cfg(all(windows, target_pointer_width = "64", feature = "native-asio"))]
pub fn asio_backend() -> Result<Box<dyn ab::Backend>, String> {
    use ab::BackendFactory;
    transit_audio_asio::AsioFactory.backend(None).ok_or_else(|| "the native ASIO backend is not available".to_string())
}

/// `--device` of `--backend asio` as an installed driver's registry name: the exact name, else the
/// one that matches ignoring case or contains it ("model mixer"), said on stderr. An unknown or
/// ambiguous name is an error listing the installed drivers; the default driver never stands in.
#[cfg(all(windows, target_pointer_width = "64", feature = "native-asio"))]
pub fn resolve_asio_device(request: &session::Request) -> Result<session::Request, String> {
    let Some(wanted) = request.device.as_deref() else { return Ok(request.clone()) };
    let backend = transit_audio_asio::AsioBackend::shared().map_err(|e| format!("the native ASIO backend did not start: {e}"))?;
    let installed = backend.driver_names().map_err(|e| format!("the ASIO drivers could not be listed: {e}"))?;
    let resolved = transit_audio_asio::resolve_driver_name(&installed, wanted).map_err(|e| format!("--device: {e}"))?;
    if !resolved.exact {
        eprintln!("--device \"{wanted}\" is the ASIO driver \"{}\"", resolved.name);
    }
    Ok(session::Request { device: Some(resolved.name), ..request.clone() })
}

/// Without the native ASIO backend there is no driver to resolve (`asio_backend` says why).
#[cfg(not(all(windows, target_pointer_width = "64", feature = "native-asio")))]
pub fn resolve_asio_device(request: &session::Request) -> Result<session::Request, String> {
    Ok(request.clone())
}

/// Without the feature (or off 64-bit Windows) there is no native ASIO backend to measure.
#[cfg(not(all(windows, target_pointer_width = "64", feature = "native-asio")))]
pub fn asio_backend() -> Result<Box<dyn ab::Backend>, String> {
    Err("--backend asio needs 64-bit Windows and a build with --features native-asio".into())
}

/// The device names of a cpal system, for `--list-devices` (enumerates; opens no stream).
pub fn list_devices(system: Option<&str>) -> Result<String, String> {
    let backend = cpal_backend(system)?;
    let mut text = format!("system: {}\n", backend.system());
    #[cfg(all(windows, feature = "asio"))]
    let mut listed: Vec<String> = Vec::new();
    for (label, direction) in [("outputs", ab::Direction::Output), ("inputs", ab::Direction::Input)] {
        text.push_str(&format!("{label}:\n"));
        for endpoint in backend.endpoints(direction).map_err(|e| e.to_string())? {
            text.push_str(&format!("  {} ({} ch)\n", endpoint.name, endpoint.inputs.max(endpoint.outputs)));
            #[cfg(all(windows, feature = "asio"))]
            listed.push(endpoint.name.clone());
        }
    }
    #[cfg(all(windows, feature = "asio"))]
    if backend.system().eq_ignore_ascii_case("ASIO") {
        // Release cpal's loaded driver first: ASIO holds one driver at a time, and a still-loaded
        // one would make every probe below fail with "already exists".
        drop(backend);
        for line in unlisted_asio_drivers(&listed) {
            text.push_str(&line);
            text.push('\n');
        }
    }
    Ok(text)
}

/// One `driver "<name>": <error>` line per registered ASIO driver that cpal did not list. cpal's
/// ASIO host skips a driver whose load fails without saying why; this repeats the load and the
/// channel and sample-type queries, one driver at a time, and reports the first error. It opens no
/// stream, but loading a driver initializes it (`ASIOInit`).
#[cfg(all(windows, feature = "asio"))]
fn unlisted_asio_drivers(listed: &[String]) -> Vec<String> {
    let asio = asio_sys::Asio::new();
    asio.driver_names()
        .into_iter()
        .filter(|name| !listed.contains(name))
        .map(|name| {
            let why = match asio.load_driver(&name) {
                Err(e) => format!("load_driver failed: {e}"),
                Ok(driver) => match driver.channels() {
                    Err(e) => format!("loaded, but ASIOGetChannels failed: {e}"),
                    Ok(ch) => match (driver.input_data_type(), driver.output_data_type()) {
                        (Err(e), _) => format!("loaded ({} in, {} out), but the input sample type query failed: {e}", ch.ins, ch.outs),
                        (_, Err(e)) => format!("loaded ({} in, {} out), but the output sample type query failed: {e}", ch.ins, ch.outs),
                        (Ok(i), Ok(o)) => format!(
                            "loaded ({} in, {} out, input type {i:?}, output type {o:?}) but cpal skipped it (unsupported sample type, no channels, or sample rate query failed)",
                            ch.ins, ch.outs
                        ),
                    },
                },
            };
            format!("driver \"{name}\": {why}")
        })
        .collect()
}
