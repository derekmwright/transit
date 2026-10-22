//! Host time: the process origin every stamp is relative to, and the stamp itself.

/// Quality of a timestamp, worst first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Quality {
    Invalid,
    Arrival,
    BackendEstimate,
    Measured,
}

/// Backend times are integer nanoseconds in this stream's explicitly identified domain.
/// `host_callback_ns` is in a common process origin. An arrival observation can have an
/// unknown dispatch bias; the residual jitter of a fit does not bound that bias. The backend
/// times are signed offsets from the stream's first callback: negative values are legal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub host_callback_ns: u64,
    pub backend_callback_ns: i64,
    pub backend_frame_ns: i64,
    pub quality: Quality,
    pub offset_uncertainty_ns: Option<u64>,
}

impl Stamp {
    /// A stamp that claims nothing (a block with no frames, or a clock that could not be read).
    pub const INVALID: Stamp =
        Stamp { host_callback_ns: 0, backend_callback_ns: 0, backend_frame_ns: 0, quality: Quality::Invalid, offset_uncertainty_ns: None };
}

/// Initialized on control. Reading the cached origin performs no lazy callback initialization.
#[derive(Clone, Copy)]
pub struct HostOrigin {
    #[cfg(not(target_arch = "wasm32"))]
    instant: std::time::Instant,
}

impl std::fmt::Debug for HostOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostOrigin")
    }
}

impl Default for HostOrigin {
    fn default() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
            Self { instant: *ORIGIN.get_or_init(std::time::Instant::now) }
        }
        #[cfg(target_arch = "wasm32")]
        {
            Self {}
        }
    }
}

impl HostOrigin {
    /// The exact origin used by native audio observations. Pass this to other
    /// device adapters rather than comparing nanoseconds from separate origins.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn instant(self) -> std::time::Instant {
        self.instant
    }

    pub fn now_ns(self) -> Option<u64> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            u64::try_from(self.instant.elapsed().as_nanos()).ok()
        }
        #[cfg(target_arch = "wasm32")]
        {
            None
        }
    }
}
