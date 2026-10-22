//! Scheduling observations, distinct from requested policy and measured deadline headroom.
//!
//! [`WorkerPriority`] is the backend trait's report of a device worker's last observed state.
//! The cpal adapter (`backend/cpal.rs`) maps the fork's observation to it.

pub use transit_audio_backend::WorkerPriority;
