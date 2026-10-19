//! Finite axiomatic regression model, extended from the cutoff review's ordering_model.rs.
//! This models the atomic protocol, not native scheduling or the full rtrb implementation.
//! All locations are atomic; initialization precedes all threads. The worker reads the ring
//! publication, and control reads the worker's witness. We enumerate every remaining read-from
//! choice and stamp/RMW modification order for this one-offer, one-cutoff execution.
//!
//! Rules: acyclic happens-before (HB), no read from an HB-future write, all four coherence
//! constraints, and an RMW reading its immediate predecessor in modification order (MO).
//! A relaxed witness does NOT create HB or impose MO on the separate stamp atomic.
//! This small model needs no dependencies and is not a general-purpose memory-model checker.
//!
//! Rust ordering: https://doc.rust-lang.org/std/sync/atomic/enum.Ordering.html
//! Coherence: https://eel.is/c++draft/intro.races#11 through #14.
//! RMW predecessor rule: https://eel.is/c++draft/atomics.order#11.

#[derive(Clone, Copy)]
enum Op {
    Write { location: usize, modification: usize, release: bool },
    Read { location: usize, from: usize, acquire: bool },
    Rmw { location: usize, from: usize, modification: usize }, // AcqRel, identity update
}

impl Op {
    fn location(self) -> usize {
        match self {
            Self::Write { location, .. } | Self::Read { location, .. } | Self::Rmw { location, .. } => location,
        }
    }

    fn read(self) -> Option<(usize, bool)> {
        match self {
            Self::Read { from, acquire, .. } => Some((from, acquire)),
            Self::Rmw { from, .. } => Some((from, true)),
            Self::Write { .. } => None,
        }
    }

    fn write(self) -> Option<(usize, bool)> {
        match self {
            Self::Write { modification, release, .. } => Some((modification, release)),
            Self::Rmw { modification, .. } => Some((modification, true)),
            Self::Read { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Cutoff {
    Load,
    Rmw { stamp_precedes_cutoff: bool },
}

fn allowed(handoff: bool, cutoff_reads_stamp: bool, stamp_before_publish: bool, cutoff: Cutoff) -> bool {
    use Op::{Read, Rmw, Write};
    let from = if cutoff_reads_stamp { 3 } else { 0 };
    let (stamp_modification, cutoff_op) = match cutoff {
        Cutoff::Load => (1, Read { location: 0, from, acquire: true }),
        Cutoff::Rmw { stamp_precedes_cutoff: true } => (1, Rmw { location: 0, from, modification: 2 }),
        Cutoff::Rmw { stamp_precedes_cutoff: false } => (2, Rmw { location: 0, from, modification: 1 }),
    };
    // 0..2 initialize offered, tail, seen. P: stamp(3), publish(4).
    // W: acquire ring(5), witness(6). C: witness(7), cutoff(8).
    // The witness is relaxed unless `handoff` adds a release/acquire synchronization edge.
    let ops = [
        Write { location: 0, modification: 0, release: false },
        Write { location: 1, modification: 0, release: false },
        Write { location: 2, modification: 0, release: false },
        Write { location: 0, modification: stamp_modification, release: true },
        Write { location: 1, modification: 1, release: true },
        Read { location: 1, from: 4, acquire: true },
        Write { location: 2, modification: 1, release: handoff },
        Read { location: 2, from: 6, acquire: handoff },
        cutoff_op,
    ];
    let mut hb = [[false; 9]; 9];
    for row in hb.iter_mut().take(3) {
        for edge in row.iter_mut().skip(3) {
            *edge = true;
        }
    }
    if stamp_before_publish {
        hb[3][4] = true;
    } else {
        hb[4][3] = true;
    }
    hb[5][6] = true;
    hb[7][8] = true;
    for (read, op) in ops.iter().enumerate() {
        if let Some((from, acquire)) = op.read() {
            let source = ops[from];
            let Some((source_modification, release)) = source.write() else { return false };
            if source.location() != op.location() || from == read {
                return false;
            }
            if let Rmw { modification, .. } = *op {
                // Ranks are contiguous per location. Even an identity RMW is a modification:
                // it cannot skip a preceding stamp and read the initialization instead.
                if source_modification + 1 != modification {
                    return false;
                }
            }
            if acquire && release {
                hb[from][read] = true;
            }
        }
    }
    for via in 0..ops.len() {
        for before in 0..ops.len() {
            for after in 0..ops.len() {
                hb[before][after] |= hb[before][via] && hb[via][after];
            }
        }
    }
    let read_modification = |op: Op| op.read().and_then(|(from, _)| ops[from].write().map(|(modification, _)| modification));
    for (a, op_a) in ops.iter().enumerate() {
        if hb[a][a] || op_a.read().is_some_and(|(from, _)| hb[a][from]) {
            return false;
        }
        for (b, op_b) in ops.iter().enumerate() {
            if !hb[a][b] || op_a.location() != op_b.location() {
                continue;
            }
            // Check read and write halves independently: an RMW participates in both.
            if let (Some((ma, _)), Some((mb, _))) = (op_a.write(), op_b.write())
                && ma >= mb
            {
                return false; // write-write coherence
            }
            if let (Some(ma), Some((mb, _))) = (read_modification(*op_a), op_b.write())
                && ma >= mb
            {
                return false; // read-write coherence
            }
            if let (Some(ma), Some(mb)) = (read_modification(*op_a), read_modification(*op_b))
                && ma > mb
            {
                return false; // read-read coherence
            }
            if let (Some((ma, _)), Some(mb)) = (op_a.write(), read_modification(*op_b))
                && ma > mb
            {
                return false; // write-read coherence
            }
        }
    }
    true
}

#[test]
fn independent_control_can_read_zero_after_worker_observed_publication() {
    assert!(allowed(false, false, true, Cutoff::Load));
}

#[test]
fn worker_handoff_forbids_the_stale_load() {
    assert!(!allowed(true, false, true, Cutoff::Load));
    assert!(allowed(true, true, true, Cutoff::Load));
}

#[test]
fn delaying_the_stamp_until_after_publication_breaks_even_the_handoff() {
    assert!(allowed(true, false, false, Cutoff::Load));
}

#[test]
fn rmw_cannot_skip_a_preceding_stamp_without_a_worker_handoff() {
    let cutoff = Cutoff::Rmw { stamp_precedes_cutoff: true };
    assert!(!allowed(false, false, true, cutoff));
    assert!(allowed(false, true, true, cutoff));
}

#[test]
fn racing_stamp_after_rmw_is_outside_the_cutoff_guarantee() {
    // Seeing the relaxed witness is not enough to order the stamp before the RMW.
    let cutoff = Cutoff::Rmw { stamp_precedes_cutoff: false };
    assert!(allowed(false, false, true, cutoff));
    // A release/acquire worker handoff DOES order the stamp before the RMW, ruling this out.
    assert!(!allowed(true, false, true, cutoff));
}

#[test]
fn exhaust_all_read_from_and_modification_orders() {
    let mut load_cases = 0;
    let mut allowed_loads = 0;
    let mut rmw_cases = 0;
    let mut allowed_rmws = 0;
    for handoff in [false, true] {
        for stamp_before_publish in [false, true] {
            for cutoff_reads_stamp in [false, true] {
                let load = allowed(handoff, cutoff_reads_stamp, stamp_before_publish, Cutoff::Load);
                assert_eq!(load, cutoff_reads_stamp || !handoff || !stamp_before_publish);
                load_cases += 1;
                allowed_loads += usize::from(load);
                for stamp_precedes_cutoff in [false, true] {
                    let cutoff = Cutoff::Rmw { stamp_precedes_cutoff };
                    let rmw = allowed(handoff, cutoff_reads_stamp, stamp_before_publish, cutoff);
                    assert_eq!(
                        rmw,
                        cutoff_reads_stamp == stamp_precedes_cutoff && (stamp_precedes_cutoff || !handoff || !stamp_before_publish),
                        "handoff={handoff}, stamp_before_publish={stamp_before_publish}, cutoff_reads_stamp={cutoff_reads_stamp}, {cutoff:?}"
                    );
                    if rmw && stamp_precedes_cutoff {
                        assert!(cutoff_reads_stamp, "seq=0 escaped a cutoff ordered after its stamp");
                    }
                    rmw_cases += 1;
                    allowed_rmws += usize::from(rmw);
                }
            }
        }
    }
    assert_eq!((load_cases, allowed_loads), (8, 7));
    assert_eq!((rmw_cases, allowed_rmws), (16, 7));
}
