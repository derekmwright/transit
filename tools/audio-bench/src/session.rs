//! Opening the streams through the backend trait only, and running one measurement.

use crate::probe::{ClickTemplate, Shared, capture_callback, render_callback};
use std::sync::Arc;
use transit_audio_backend as ab;

/// What a run asks of the backend.
#[derive(Debug, Clone)]
pub struct Request {
    pub device: Option<String>,
    /// Input device, when it differs from `device`.
    pub in_device: Option<String>,
    pub rate: u32,
    pub period: Option<u32>,
    pub loopback: bool,
    /// Shared, or exclusive (`--mode exclusive`): every open asks for it.
    pub mode: ab::Mode,
    /// The capture stream's own mode (`--in-mode`), when it differs from `mode`.
    pub in_mode: Option<ab::Mode>,
    /// 1-based channels of the cable.
    pub out_ch: u16,
    pub in_ch: u16,
}

/// What the backend opened.
#[derive(Debug, Clone)]
pub struct Opened {
    pub device: String,
    pub render_rate: u32,
    pub capture_rate: Option<u32>,
    /// What the render stream opened with (format, period, latency).
    pub render: ab::Achieved,
    /// What the capture stream opened with, on a loopback run.
    pub capture: Option<ab::Achieved>,
    /// The device-loop counters of each stream at the end of the run, where the backend keeps them.
    pub render_stats: Option<ab::StreamStats>,
    pub capture_stats: Option<ab::StreamStats>,
}

fn open_first<T>(attempts: &[ab::OpenRequest], mut open: impl FnMut(&ab::OpenRequest) -> Result<T, ab::OpenError>) -> Result<T, String> {
    let mut last = "the plan has no attempts".to_string();
    for attempt in attempts {
        match open(attempt) {
            Ok(opened) => return Ok(opened),
            Err(error) => last = error.to_string(),
        }
    }
    Err(last)
}

/// `plan` with every attempt in `mode`.
fn in_mode(mut plan: ab::Plan, mode: ab::Mode) -> ab::Plan {
    for attempt in &mut plan.attempts {
        attempt.mode = mode;
    }
    plan
}

/// Open render (and capture with `loopback`), start them, call `drive` for the run, stop them.
/// `drive` returns when the measurement time is over.
pub fn run(
    backend: &dyn ab::Backend,
    request: &Request,
    shared: &Arc<Shared>,
    drive: &mut dyn FnMut() -> Result<(), String>,
) -> Result<Opened, String> {
    if request.out_ch == 0 || request.in_ch == 0 {
        return Err("--out-ch and --in-ch count from 1".into());
    }
    let (out_index, in_index) = (usize::from(request.out_ch) - 1, usize::from(request.in_ch) - 1);
    let render_plan = backend
        .plan_render(&ab::OutputWish {
            device: request.device.clone(),
            rates: vec![request.rate],
            channels: request.out_ch.max(2),
            period: request.period,
        })
        .map_err(|e| format!("output: {e}"))?;
    let render_plan = in_mode(render_plan, request.mode);
    let capture_plan = request
        .loopback
        .then(|| {
            backend
                .plan_capture(&ab::InputWish {
                    device: request.in_device.clone().or_else(|| request.device.clone()),
                    rate: Some(request.rate),
                    period: request.period,
                })
                .map_err(|e| format!("input: {e}"))
                .map(|plan| in_mode(plan, request.in_mode.unwrap_or(request.mode)))
        })
        .transpose()?;
    let error_callback = |shared: &Arc<Shared>, render: bool| -> ab::ErrorCallback {
        let shared = Arc::clone(shared);
        Box::new(move |_| if render { shared.render.error() } else { shared.capture.error() })
    };
    let open_capture = |plan: &ab::Plan| -> Result<(Box<dyn ab::Stream>, ab::Achieved), String> {
        let (stream, achieved) = open_first(&plan.attempts, |attempt| {
            backend.open_capture(&plan.endpoint, attempt, capture_callback(Arc::clone(shared), in_index), error_callback(shared, false))
        })
        .map_err(|e| format!("open input: {e}"))?;
        if usize::from(achieved.format.channels) <= in_index {
            return Err(format!("the input opened with {} channels; --in-ch {} does not exist", achieved.format.channels, request.in_ch));
        }
        shared.capture_click.set(ClickTemplate::new(achieved.format.rate)?).map_err(|_| "capture template already set".to_string())?;
        Ok((stream, achieved))
    };
    // A duplex backend (ASIO) needs the input built first, on the output's own device. The native WASAPI
    // backend pairs an input with the output only when none is named: `--in-device` opens that input itself.
    let named_input = request.in_device.is_some() && backend.system() == "WASAPI";
    let duplex_first = capture_plan.is_some() && !named_input && backend.capture_with_render(&render_plan.endpoint);
    let mut capture = None;
    if let (true, Some(plan)) = (duplex_first, &capture_plan) {
        capture = Some(open_capture(&ab::Plan { endpoint: render_plan.endpoint.clone(), attempts: plan.attempts.clone() })?);
    }
    let (render, achieved) = open_first(&render_plan.attempts, |attempt| {
        backend.open_render(
            &render_plan.endpoint,
            attempt,
            render_callback(Arc::clone(shared), out_index, request.loopback),
            error_callback(shared, true),
        )
    })
    .map_err(|e| format!("open output: {e}"))?;
    if usize::from(achieved.format.channels) <= out_index {
        return Err(format!("the output opened with {} channels; --out-ch {} does not exist", achieved.format.channels, request.out_ch));
    }
    if request.loopback {
        shared.render_click.set(ClickTemplate::new(achieved.format.rate)?).map_err(|_| "render template already set".to_string())?;
    }
    if let (None, Some(plan)) = (&capture, &capture_plan) {
        capture = Some(open_capture(plan)?);
    }
    let started = (|| {
        if let Some((stream, _)) = &capture {
            stream.start().map_err(|e| format!("start input: {e}"))?;
        }
        render.start().map_err(|e| format!("start output: {e}"))?;
        drive()
    })();
    // Before the stops, so the figures are the run's alone.
    let render_stats = render.stats();
    let capture_stats = capture.as_ref().and_then(|(stream, _)| stream.stats());
    let _ = render.stop();
    if let Some((stream, _)) = &capture {
        let _ = stream.stop();
    }
    started?;
    Ok(Opened {
        device: render_plan.endpoint.name.clone(),
        render_rate: achieved.format.rate,
        capture_rate: capture.as_ref().map(|(_, a)| a.format.rate),
        render: achieved,
        capture: capture.as_ref().map(|(_, a)| *a),
        render_stats,
        capture_stats,
    })
}
