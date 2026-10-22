//! Audio backend adapters (ADR 0016). The Player opens streams only through the
//! `transit_audio_backend` traits; cpal is the one adapter so far.

pub mod cpal;
