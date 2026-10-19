//! Scheduling observations, distinct from requested policy and measured deadline headroom.
//!
//! CPAL owns Windows device-worker promotion before the first data callback and releases the
//! registration on the same worker at exit. The pinned fork exposes its actual helper result.
//! No soundcraft-rt promotion is called on that worker. Render-pool workers have a separate
//! policy; their Pro Audio benchmark results do not describe this MMCSS Audio registration.

/// Last observed scheduling state of one device worker lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkerPriority {
    /// There is no device worker (for example the silent transport clock).
    NoWorker,
    /// This backend exposes no observed result. This is neither success nor failure.
    /// Includes ASIO's driver-owned callback and the current non-Windows backends.
    Unobserved,
    /// The WASAPI worker has not yet completed its startup registration attempt.
    Pending,
    /// Successful MMCSS Audio registration on this worker, before data callbacks.
    /// Does not imply Pro Audio priority, glitch-free playback or a hardware deadline margin.
    MmcssAudio,
    /// The worker tried registration and the helper reported failure; playback may continue.
    PromotionFailed,
    /// The worker explicitly released its successful registration at exit.
    Released,
    /// Explicit release was attempted but the helper reported failure.
    ReleaseFailed,
}

pub(crate) fn observe(stream: Option<&cpal::Stream>) -> WorkerPriority {
    let Some(stream) = stream else { return WorkerPriority::NoWorker };
    #[cfg(target_os = "windows")]
    {
        use cpal::platform::{StreamInner, WasapiThreadPriorityStatus as Status};
        match stream.as_inner() {
            StreamInner::Wasapi(stream) => match stream.thread_priority_observation().status() {
                Status::Unobserved => WorkerPriority::Unobserved,
                Status::Pending => WorkerPriority::Pending,
                Status::Promoted => WorkerPriority::MmcssAudio,
                Status::PromotionFailed => WorkerPriority::PromotionFailed,
                Status::Released => WorkerPriority::Released,
                Status::ReleaseFailed => WorkerPriority::ReleaseFailed,
                // The fork marks its API non-exhaustive. Unknown future states are not success.
                _ => WorkerPriority::Unobserved,
            },
            #[cfg(feature = "asio")]
            StreamInner::Asio(_) => WorkerPriority::Unobserved,
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = stream;
        WorkerPriority::Unobserved
    }
}
