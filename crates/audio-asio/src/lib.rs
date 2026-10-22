//! The native ASIO backend (ADR 0016 amendment, draft "native ASIO"), slice (a), first part: the
//! hand-written `IASIO` bindings ([`abi`]), the calls through them (`com`) and the dedicated STA
//! host thread that makes every driver call (`host`). The `Backend` implementation follows.
//! 64-bit Windows only: on every other target the crate is empty.
#![cfg(all(windows, target_pointer_width = "64"))]
// No audio callback exists in this slice; control-thread code may allocate and format.
#![allow(clippy::disallowed_macros, clippy::disallowed_methods)]
// The backend that uses the host thread lands in the next commit.
#![allow(dead_code)]

pub mod abi;
mod com;
pub mod driver;
mod host;

use std::time::Duration;

/// How long a caller waits for the host thread before a request fails (a blocking driver).
pub const HOST_TIMEOUT: Duration = Duration::from_secs(10);
