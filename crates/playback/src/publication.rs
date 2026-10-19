//! Bounded round-trip ownership for prepared audio publications.
//!
//! Each accepted publication spends one of N credits. Its envelope travels through the
//! forward ring, an audio-owned in-flight slot, and the return ring. Both rings have capacity
//! N. A credit returns only after control reclaims that envelope, including rejected plans.
//! Thus at most N envelopes are outstanding, and every admitted envelope has return space.
//! A stalled control thread stops publication; it cannot make the callback wait or drop work.
//!
//! The payload should own both the prepared change and its preallocated retirement storage.
//! The callback swaps old resources into that same envelope. Generation validation, resource
//! bounds, coalescing *before* preparation, and the adoption operation belong to the caller.
//! This primitive does not make an allocating or unbounded adoption operation realtime-safe.
//!
//! # Lifetime contract
//!
//! Keep both ports in control-owned state. The callback only borrows [`AudioPort`]; it must
//! never own the port's final destructor. Stop/join the stream, recover its state after the
//! last callback, then destroy both ports on the resource lifecycle thread. In particular,
//! `AudioPort` can retain an envelope after a panic. A ring's last endpoint also destroys
//! queued payloads and storage. Credits do not replace the shutdown ownership contract.
//!
//! `rtrb` is pinned because its single-element consumer returns Empty for an incompletely
//! published slot rather than waiting for a preempted producer. Neither port uses a blocking
//! receive, CAS retry loop, notification, or secondary overflow queue.

use rtrb::{Consumer, Producer, PushError, RingBuffer};
use std::num::NonZeroUsize;

/// The control-side publisher and reclaimer. Not clonable: one serialized publisher only.
pub struct ControlPort<T> {
    forward: Producer<T>,
    returned: Consumer<T>,
    credits: usize,
}

/// Audio-side state, borrowed by the callback and reclaimed by control after quiescence.
pub struct AudioPort<T> {
    forward: Consumer<T>,
    returned: Producer<T>,
    // Stored in the holder, never only on the callback stack: unwinding must not drop it.
    in_flight: Option<T>,
}

/// Result of one bounded audio-side service call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    /// No publication was available.
    Empty,
    /// One envelope was returned (possibly recovering an earlier interrupted adoption).
    Returned,
    /// A return could not be published. Its owner is retained and no new work is admitted.
    /// Unreachable under the credit invariant, but does not panic, leak, or free on failure.
    ReturnPending,
}

/// Allocate equal-capacity forward/return rings on the control thread.
///
/// The caller must impose its resource limit before constructing the queues.
#[must_use]
pub fn channel<T>(capacity: NonZeroUsize) -> (ControlPort<T>, AudioPort<T>) {
    let (send, receive) = RingBuffer::new(capacity.get());
    let (return_send, return_receive) = RingBuffer::new(capacity.get());
    (
        ControlPort { forward: send, returned: return_receive, credits: capacity.get() },
        AudioPort { forward: receive, returned: return_send, in_flight: None },
    )
}

impl<T> ControlPort<T> {
    /// Number of publications that may be accepted before another reclamation.
    #[must_use]
    pub fn credits(&self) -> usize {
        self.credits
    }

    /// Publish one prepared envelope, or return its ownership without changing any credit.
    /// Control may coalesce an unsent document request; ordered actions need backpressure.
    pub fn try_publish(&mut self, envelope: T) -> Result<(), T> {
        if self.credits == 0 {
            return Err(envelope);
        }
        match self.forward.push(envelope) {
            Ok(()) => {
                self.credits -= 1;
                Ok(())
            }
            Err(PushError::Full(envelope)) => Err(envelope),
        }
    }

    /// Reclaim at most one returned envelope on the lifecycle thread, then restore its credit.
    /// `reclaim` must finish releasing resources before it returns. If it panics, no credit is
    /// restored; shutdown must recover the remaining ports rather than resume publication.
    pub fn reclaim_one(&mut self, reclaim: impl FnOnce(T)) -> bool {
        let Ok(envelope) = self.returned.pop() else { return false };
        reclaim(envelope);
        self.credits += 1;
        true
    }
}

impl<T> AudioPort<T> {
    /// Admit at most one envelope. `apply` must be bounded and allocation/destruction-free,
    /// including destruction of its captures when it is not called (Empty or recovery).
    /// It validates the predecessor/device generation before moving resources. Rejection
    /// leaves the envelope intact for return; success moves all retired owners into it.
    ///
    /// If `apply` unwinds, the envelope stays in this port. The caller's fault handler must
    /// stop processing; the next call only returns that envelope, without applying it again.
    /// The port adds no allocation or free during unwind; the panic runtime and `apply` may.
    /// This protects its ownership, not the semantic consistency of a half-applied plan.
    pub fn service_one(&mut self, apply: impl FnOnce(&mut T)) -> Service {
        if self.in_flight.is_none() {
            let Ok(envelope) = self.forward.pop() else { return Service::Empty };
            self.in_flight = Some(envelope);
            if let Some(envelope) = self.in_flight.as_mut() {
                apply(envelope);
            }
        }
        let Some(envelope) = self.in_flight.take() else { return Service::Empty };
        match self.returned.push(envelope) {
            Ok(()) => Service::Returned,
            Err(PushError::Full(envelope)) => {
                self.in_flight = Some(envelope);
                Service::ReturnPending
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn an_impossible_full_return_retains_the_payload_without_readmitting() {
        let (mut control, mut audio) = channel(NonZeroUsize::new(1).unwrap());
        control.try_publish(7).unwrap();
        // Deliberately violate the credit invariant using private fields to exercise the
        // defensive path. No public API can insert this extra return.
        audio.returned.push(99).unwrap();
        assert_eq!(audio.service_one(|v| *v += 1), Service::ReturnPending);
        assert_eq!(audio.in_flight, Some(8));
        assert_eq!(audio.service_one(|_| panic!("must not reapply")), Service::ReturnPending);
        assert_eq!(control.returned.pop(), Ok(99));
        assert_eq!(audio.service_one(|_| panic!("must not reapply")), Service::Returned);
        assert!(control.reclaim_one(|v| assert_eq!(v, 8)));
        assert_eq!(control.credits(), 1);
    }
}
