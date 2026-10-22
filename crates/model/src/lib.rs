//! The SoundCraft session document.
//!
//! A [`Session`] is pure data: tracks with playlists of clips, the mixer (inserts, sends, routing,
//! automation), tempo/meter, memory locations and groups. Audio sample data lives in
//! [`SourcePool`] behind `Arc`s, so cloning a session for undo is cheap.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod automation;
pub mod b64;
mod clip;
mod ids;
mod io_map;
mod markers;
mod midi_input;
mod mixer;
mod route_graph;
mod session;
mod source;
mod track;

pub use automation::{AutoParam, AutomationLane, AutomationMode, AutomationPoint};
pub use clip::{Clip, ClipContent, Fade, FadeShape};
pub use ids::{BusId, ClipId, GroupId, MarkerId, SourceId, TrackId};
pub use io_map::{HardwareLatency, IoDirection, IoError, IoMap, IoPath, MAX_DEVICE_CHANNELS, MAX_PATH_WIDTH, input_channel};
pub use markers::{Group, MarkerKind, MemoryLocation};
pub use midi_input::{MidiInput, MidiPort};
pub use mixer::{
    Bus, ChannelFormat, HARDWARE_PLUGIN, HardwareInsert, INSERT_SLOTS, Insert, InsertUid, KeyFrom, KeySource, Mixer, OutputPath, Route, SEND_SLOTS,
    SendSlot, Speaker, SurroundPan, Tap, fader_db_to_pos, fader_pos_to_db,
};
pub use route_graph::{
    EdgeKind, Layering, RouteChange, RouteCycle, RouteEdge, RouteGraph, RouteNode, RouteScratch, describe_cycle, hardware_problem, new_cycle,
    would_create_cycle, would_create_cycle_all,
};
pub use session::{
    BitDepthSetting, EditMode, EditState, FEET_FRAMES_START, MAX_COMMENT_CHARS, MAX_NAME_CHARS, SESSION_EXTENSION, Session, SessionError, Tool,
    TrackView, ZoomState, ZoomToggle, truncate_chars,
};
pub use source::{Source, SourceAudio, SourcePool, VideoSource};
pub use track::{Frozen, Playlist, TRACK_COLORS, Track, TrackHeight, TrackKind};

pub use soundcraft_time as time;
