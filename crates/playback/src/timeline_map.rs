//! Bounded history of consumed output frames and session phase. The render-ahead work
//! ordinal is deliberately absent. The caller supplies the phase at the converter's
//! consumed output, including its named SRC delay; mix PDC must not be subtracted again.

use crate::device_clock::{ClockKey, Direction};
use soundcraft_time::MAX_POSITION;

pub const SEGMENTS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    Invalid,
    Arithmetic,
    WrongGeneration,
    MissingHistory,
    TooManyBoundaries,
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    numerator: u64,
    denominator: u64,
}

impl Rate {
    pub fn new(numerator: u64, denominator: u64) -> Result<Self, MapError> {
        if numerator == 0 || denominator == 0 {
            return Err(MapError::Invalid);
        }
        let common = gcd(numerator, denominator);
        Ok(Self { numerator: numerator / common, denominator: denominator / common })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub sample: i64,
    pub fraction: u64,
    pub denominator: u64,
}

impl Position {
    fn from_parts(numerator: i128, denominator: u64) -> Result<Self, MapError> {
        if denominator == 0 {
            return Err(MapError::Invalid);
        }
        let sample = i64::try_from(numerator.div_euclid(i128::from(denominator))).map_err(|_| MapError::Arithmetic)?;
        if !(-MAX_POSITION..=MAX_POSITION).contains(&sample) {
            return Err(MapError::Arithmetic);
        }
        Ok(Self { sample, fraction: numerator.rem_euclid(i128::from(denominator)) as u64, denominator })
    }
    fn numerator(self) -> Result<i128, MapError> {
        if self.denominator == 0 || self.fraction >= self.denominator {
            return Err(MapError::Invalid);
        }
        Ok(i128::from(self.sample) * i128::from(self.denominator) + i128::from(self.fraction))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub key: ClockKey,
    pub transport: u64,
    pub first_output: u64,
    pub end_output: u64,
    pub session: Position,
    pub rate: Rate,
    pub playing: bool,
    pub loop_iteration: u64,
}

impl Segment {
    fn end_position(&self) -> Result<Position, MapError> {
        let delta = if self.playing {
            i128::from(self.end_output - self.first_output).checked_mul(i128::from(self.rate.numerator)).ok_or(MapError::Arithmetic)?
        } else {
            0
        };
        Position::from_parts(self.session.numerator()?.checked_add(delta).ok_or(MapError::Arithmetic)?, self.rate.denominator)
    }
    pub fn session_at(&self, output: u64) -> Result<Position, MapError> {
        if output < self.first_output || output >= self.end_output {
            return Err(MapError::MissingHistory);
        }
        let position = self.session.numerator()?;
        if self.rate.denominator != self.session.denominator {
            return Err(MapError::Invalid);
        }
        let delta = if self.playing {
            i128::from(output - self.first_output).checked_mul(i128::from(self.rate.numerator)).ok_or(MapError::Arithmetic)?
        } else {
            0
        };
        Position::from_parts(position.checked_add(delta).ok_or(MapError::Arithmetic)?, self.rate.denominator)
    }
}

pub struct Timeline {
    key: ClockKey,
    transport: u64,
    next_output: u64,
    position: i128,
    rate: Rate,
    playing: bool,
    loop_range: Option<(i64, i64)>,
    loop_active: bool,
    iteration: u64,
}

impl Timeline {
    pub fn new(key: ClockKey, transport: u64, output: u64, sample: i64, rate: Rate) -> Result<Self, MapError> {
        if key.direction != Direction::Output || !(-MAX_POSITION..=MAX_POSITION).contains(&sample) {
            return Err(MapError::Invalid);
        }
        Ok(Self {
            key,
            transport,
            next_output: output,
            position: i128::from(sample) * i128::from(rate.denominator),
            rate,
            playing: false,
            loop_range: None,
            loop_active: false,
            iteration: 0,
        })
    }
    /// A seek/start/stop/speed edit is one transaction at the next consumed output frame.
    /// A strictly newer transport identity prevents old events from acquiring a new meaning.
    /// Loop policy: seeking at/past the loop end continues linearly; it does not
    /// jump back into the loop. Seeking before its start plays through into it.
    /// A new seek starts iteration zero; only crossing an engaged loop end adds
    /// iterations. A later seek into/before the range engages looping again.
    pub fn change(&mut self, transport: u64, seek: Option<i64>, rate: Rate, playing: bool, loop_range: Option<(i64, i64)>) -> Result<(), MapError> {
        if transport <= self.transport {
            return Err(MapError::WrongGeneration);
        }
        if let Some((start, end)) = loop_range
            && (start >= end || start < -MAX_POSITION || end > MAX_POSITION)
        {
            return Err(MapError::Invalid);
        }
        if let Some(sample) = seek
            && !(-MAX_POSITION..=MAX_POSITION).contains(&sample)
        {
            return Err(MapError::Invalid);
        }
        let common = self
            .rate
            .denominator
            .checked_div(gcd(self.rate.denominator, rate.denominator))
            .and_then(|n| n.checked_mul(rate.denominator))
            .ok_or(MapError::Arithmetic)?;
        let numerator = rate.numerator.checked_mul(common / rate.denominator).ok_or(MapError::Arithmetic)?;
        let position = if let Some(sample) = seek {
            i128::from(sample) * i128::from(common)
        } else {
            self.position.checked_mul(i128::from(common / self.rate.denominator)).ok_or(MapError::Arithmetic)?
        };
        Position::from_parts(position, common)?;
        self.transport = transport;
        self.position = position;
        self.rate = Rate { numerator, denominator: common };
        self.playing = playing;
        self.loop_range = loop_range;
        self.loop_active = loop_range.is_some_and(|(_, end)| position < i128::from(end) * i128::from(common));
        if seek.is_some() {
            self.iteration = 0;
        }
        Ok(())
    }
    pub fn next_output(&self) -> u64 {
        self.next_output
    }
    pub fn position(&self) -> Result<Position, MapError> {
        Position::from_parts(self.position, self.rate.denominator)
    }

    fn wrap(&self, position: &mut i128, iteration: &mut u64) -> Result<(), MapError> {
        if self.playing
            && self.loop_active
            && let Some((start, stop)) = self.loop_range
        {
            let loop_end = i128::from(stop) * i128::from(self.rate.denominator);
            let loop_length = i128::from(stop - start) * i128::from(self.rate.denominator);
            if *position >= loop_end {
                let wraps = (*position - loop_end) / loop_length + 1;
                *iteration = iteration.checked_add(u64::try_from(wraps).map_err(|_| MapError::Arithmetic)?).ok_or(MapError::Arithmetic)?;
                *position -= wraps * loop_length;
            }
        }
        Ok(())
    }

    /// Emit half-open intervals, splitting wraps even within a callback. At most SEGMENTS
    /// boundaries are serviced; an extreme request is refused atomically, never looped over
    /// an unbounded number of wraps. Copy staging keeps failure from publishing half a block.
    pub fn advance(&mut self, frames: u32, mut emit: impl FnMut(Segment)) -> Result<(), MapError> {
        let end = self.next_output.checked_add(u64::from(frames)).ok_or(MapError::Arithmetic)?;
        let (mut output, mut position, mut iteration) = (self.next_output, self.position, self.iteration);
        let mut staged = [None; SEGMENTS];
        let mut count = 0;
        while output < end {
            let mut length = end - output;
            self.wrap(&mut position, &mut iteration)?;
            if self.playing
                && self.loop_active
                && let Some((_, stop)) = self.loop_range
            {
                let loop_end = i128::from(stop) * i128::from(self.rate.denominator);
                let distance = loop_end.checked_sub(position).ok_or(MapError::Arithmetic)?;
                let step = i128::from(self.rate.numerator);
                let until_wrap = (distance / step) + i128::from(distance % step != 0);
                length = length.min(u64::try_from(until_wrap).map_err(|_| MapError::Arithmetic)?);
            }
            if length == 0 {
                return Err(MapError::Invalid);
            }
            let segment = Segment {
                key: self.key,
                transport: self.transport,
                first_output: output,
                end_output: output + length,
                session: Position::from_parts(position, self.rate.denominator)?,
                rate: self.rate,
                playing: self.playing,
                loop_iteration: iteration,
            };
            let Some(slot) = staged.get_mut(count) else {
                return Err(MapError::TooManyBoundaries);
            };
            *slot = Some(segment);
            count += 1;
            if self.playing {
                position = position
                    .checked_add(i128::from(length).checked_mul(i128::from(self.rate.numerator)).ok_or(MapError::Arithmetic)?)
                    .ok_or(MapError::Arithmetic)?;
            }
            output += length;
        }
        self.wrap(&mut position, &mut iteration)?;
        Position::from_parts(position, self.rate.denominator)?;
        self.position = position;
        self.next_output = end;
        self.iteration = iteration;
        for segment in staged.into_iter().flatten() {
            emit(segment);
        }
        Ok(())
    }
}

pub struct History {
    segments: [Option<Segment>; SEGMENTS],
    len: usize,
    pub overwritten: u64,
}

impl Default for History {
    fn default() -> Self {
        Self { segments: [None; SEGMENTS], len: 0, overwritten: 0 }
    }
}

impl History {
    pub fn push(&mut self, segment: Segment) -> Result<(), MapError> {
        if segment.key.direction != Direction::Output || segment.end_output <= segment.first_output {
            return Err(MapError::Invalid);
        }
        segment.session_at(segment.first_output)?;
        segment.session_at(segment.end_output - 1)?;
        if let Some(previous) = self.segments.get(self.len.saturating_sub(1)).copied().flatten() {
            if previous.key != segment.key || segment.transport < previous.transport {
                return Err(MapError::WrongGeneration);
            }
            if previous.end_output != segment.first_output {
                return Err(MapError::Invalid);
            }
            if previous.transport == segment.transport
                && previous.rate == segment.rate
                && previous.playing == segment.playing
                && previous.loop_iteration == segment.loop_iteration
                && previous.end_position()? == segment.session
            {
                if let Some(Some(last)) = self.segments.get_mut(self.len.saturating_sub(1)) {
                    last.end_output = segment.end_output;
                }
                return Ok(());
            }
        }
        if self.len == SEGMENTS {
            self.segments.rotate_left(1);
            self.len -= 1;
            self.overwritten = self.overwritten.saturating_add(1);
        }
        if let Some(slot) = self.segments.get_mut(self.len) {
            *slot = Some(segment);
        }
        self.len += 1;
        Ok(())
    }
    pub fn locate(&self, key: ClockKey, transport: u64, output: u64) -> Result<(Position, u64), MapError> {
        let segment = self.segments.iter().flatten().find(|s| s.first_output <= output && output < s.end_output).ok_or(MapError::MissingHistory)?;
        if segment.key != key || segment.transport != transport {
            return Err(MapError::WrongGeneration);
        }
        Ok((segment.session_at(output)?, segment.loop_iteration))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementError {
    Clock(crate::device_clock::Unavailable),
    Timeline(MapError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub original_host_ns: u64,
    pub output_frame: u64,
    pub session: Position,
    pub loop_iteration: u64,
    pub clock_revision: u64,
}

/// Shared recording/event placement path. The input adapter first maps capture ordinal
/// through its own clock to host time. MIDI ingress supplies a converted host timestamp.
/// Neither uses writer wake time, UI cursor, callback number, or render-ahead position.
pub fn place_host_event(
    host_ns: u64,
    expected_output: ClockKey,
    transport: u64,
    clock: &crate::device_clock::ClockMap,
    history: &History,
    now: u64,
) -> Result<Placement, PlacementError> {
    if expected_output.direction != Direction::Output {
        return Err(PlacementError::Clock(crate::device_clock::Unavailable::WrongClock));
    }
    let output_frame = clock.host_to_frame(expected_output, host_ns, now).map_err(PlacementError::Clock)?;
    let (session, loop_iteration) = history.locate(expected_output, transport, output_frame).map_err(PlacementError::Timeline)?;
    Ok(Placement { original_host_ns: host_ns, output_frame, session, loop_iteration, clock_revision: clock.revision })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Ready { offset: u32, late_frames: u64 },
    Future,
}

/// Lookahead is an explicit frame budget, independent of the device callback partition.
pub fn admit(output_frame: u64, lookahead: u32, first: u64, frames: u32) -> Result<Admission, MapError> {
    let desired = output_frame.checked_add(u64::from(lookahead)).ok_or(MapError::Arithmetic)?;
    let end = first.checked_add(u64::from(frames)).ok_or(MapError::Arithmetic)?;
    if desired >= end || frames == 0 {
        return Ok(Admission::Future);
    }
    Ok(Admission::Ready {
        offset: u32::try_from(desired.saturating_sub(first)).map_err(|_| MapError::Arithmetic)?,
        late_frames: first.saturating_sub(desired),
    })
}
