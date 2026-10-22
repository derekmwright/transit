//! More Options toggles, the Setup menu (hardware, playback engine, I/O, timecode, click,
//! shortcuts, MIDI setup) and File › Import › Session Data.

use super::more_util::{set_text, store_value, text, toggle_flag, values_with_prefix};
use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_model::{BusId, ChannelFormat, OutputPath, Route, Session, TrackId, TrackKind};
use soundcraft_time::Timecode;

macro_rules! option {
    ($id:literal, $label:literal, $flag:literal) => {
        cmd!(noundo $id, $label, ["Options"], None, "{value?: bool} (toggles when omitted)", always, |e, p| toggle_flag(e, p, $flag))
    };
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        option!("options.destructive_punch", "DestructivePunch", "options.destructive_punch"),
        option!("options.transport_online", "Transport Online", "options.transport_online"),
        option!("options.dynamic_transport", "Dynamic Transport", "options.dynamic_transport"),
        option!(
            "options.marker_target_follows_selection",
            "Marker Target Follows First Selected Track",
            "options.marker_target_follows_first_selected_track"
        ),
        option!("options.midi_thru", "MIDI Thru", "options.midi_thru"),
        option!("options.auto_spot_clips", "Auto-Spot Clips", "options.auto_spot_clips"),
        option!("options.keyboard_lock", "Edit/Tool Mode Keyboard Lock", "options.edit_tool_mode_keyboard_lock"),
        option!("options.calibration_mode", "Calibration Mode", "options.calibration_mode"),
        option!("options.midi_live_mode", "MIDI Live Mode", "options.midi_live_mode"),
        option!("options.low_latency_monitoring", "Low Latency Monitoring", "options.low_latency_monitoring"),
        cmd!(
            "options.prepare_dpe_tracks",
            "Prepare DPE Tracks",
            ["Options"],
            None,
            "{tracks?} — consolidates each record-armed (or given) audio track into one continuous clip for destructive punch",
            always,
            prepare_dpe
        ),
        cmd!(noundo "setup.hardware", "Hardware...", ["Setup"], None, "{device?, buffer_size?: 16..8192} — audio interface settings (sample rate is the session's)", always, |e, p| engine_settings(e, p)),
        cmd!(noundo "setup.playback_engine", "Playback Engine...", ["Setup"], None, "{device?, buffer_size?: 16..8192, delay_compensation?: bool}", always, |e, p| {
            if let Some(v) = p.get("delay_compensation").and_then(Value::as_bool) {
                e.session_mut().edit.delay_compensation = v;
            }
            engine_settings(e, p)
        }),
        cmd!(noundo "setup.disk_allocation", "Disk Allocation...", ["Setup"], None, "{path?, track?} — the record folder for the session, or for one track", always, disk_allocation),
        cmd!(noundo "setup.peripherals", "Peripherals...", ["Setup"], None, "{settings?: {name: number|bool|string}} — control surface / sync device settings", always, peripherals),
        cmd!(
            "setup.io",
            "I/O...",
            ["Setup"],
            None,
            "{action?: list|create_bus|rename_bus|delete_bus|create_output|rename_output|delete_output, name?, new_name?, format?: stereo, first_channel?: 0}",
            always,
            io_setup
        ),
        cmd!(
            noundo "setup.io_map",
            "I/O Paths...",
            ["Setup"],
            None,
            "{action?: list|set|add_path|rename_path|remove_path|set_channels|set_main, name?, new_name?, direction?: input|output, channels?: [0-based device channels], map?: IoMap} — this machine's device paths (per machine: no undo step, session untouched)",
            always,
            io_map_setup
        ),
        cmd!(
            noundo "io.measure_hardware_latency",
            "Measure Hardware Insert Latency",
            [],
            None,
            "{send: output path, ret: input path} -> {round_trip_frames, spread_frames, ms}: plays a click on the send path and times its return on the live audio device (the transport must be stopped); the result is kept per machine",
            always,
            measure_hardware_latency
        ),
        cmd!(
            noundo "io.clear_hardware_latency",
            "Clear Hardware Insert Latency",
            [],
            None,
            "{send: output path, ret: input path} -> {cleared}: forget the measured round trip of the pair",
            always,
            clear_hardware_latency
        ),
        cmd!(
            "setup.current_timecode",
            "Current Timecode Position...",
            ["Setup"],
            None,
            "{timecode: 'HH:MM:SS:FF'} — sets the session start so the insertion point reads this timecode",
            always,
            current_timecode
        ),
        cmd!(noundo "setup.current_feet_frames", "Current Feet+Frames Position...", ["Setup"], None, "{feet_frames: 'F+FF'} — the feet+frames reading at the insertion point", always, current_feet_frames),
        cmd!(noundo "setup.external_timecode_offset", "External Timecode Offset...", ["Setup"], None, "{frames?: ±, samples?: ±} — offset applied to incoming timecode", always, |e, p| offset_setting(e, p, "sync.external_offset")),
        cmd!(noundo "setup.video_sync_offset", "Video Sync Offset...", ["Setup"], None, "{frames?: ±, samples?: ±} — offset between audio and video playback", always, |e, p| offset_setting(e, p, "video.sync_offset")),
        cmd!(noundo "setup.click_countoff", "Click/Countoff...", ["Setup"], None, "{volume_db?: -60..12, accent_note?: 0..127, normal_note?: 0..127, accent_velocity?, normal_velocity?, countoff?: bool, countoff_bars?: 1..16, only_during_record?: bool}", always, click_countoff),
        cmd!(query "setup.keyboard_shortcuts", "Keyboard Shortcuts...", ["Setup"], None, "{filter?} — every command shortcut", always, |_, p| {
            let f = str_param(p, "filter").map(str::to_ascii_lowercase);
            let rows: Vec<Value> = command_specs()
                .iter()
                .filter_map(|c| c.shortcut.map(|sc| (c, sc)))
                .filter(|(c, _)| f.as_ref().is_none_or(|f| c.id.contains(f.as_str()) || c.label.to_ascii_lowercase().contains(f.as_str())))
                .map(|(c, sc)| json!({"id": c.id, "label": c.label, "menu": c.menu.join(" > "), "shortcut": sc}))
                .collect();
            Ok(json!({"shortcuts": rows}))
        }),
        cmd!(noundo "setup.midi_studio", "MIDI Studio...", ["Setup", "MIDI"], None, "{devices?: [names]} — the MIDI devices in the studio", always, |e, p| {
            let s = e.session_mut();
            if let Some(a) = p.get("devices").and_then(Value::as_array) {
                let names: Vec<&str> = a.iter().filter_map(Value::as_str).take(64).collect();
                set_text(&mut s.edit, "midi.studio.devices", Some(&names.join("|")));
            }
            let devices: Vec<String> = text(&s.edit, "midi.studio.devices").map(|t| t.split('|').map(str::to_string).collect()).unwrap_or_default();
            Ok(json!({"devices": devices}))
        }),
        cmd!(noundo "setup.midi_beat_clock", "MIDI Beat Clock...", ["Setup", "MIDI"], None, "{enabled?: bool, offset?: samples, destination?: name}", always, |e, p| {
            let s = e.session_mut();
            if let Some(v) = p.get("enabled").and_then(Value::as_bool) {
                s.edit.set_flag("midi.beat_clock", v);
            }
            store_value(&mut s.edit, p, "offset", "midi.beat_clock.offset", -1e7, 1e7);
            if let Some(d) = str_param(p, "destination") {
                set_text(&mut s.edit, "midi.beat_clock.destination", Some(d));
            }
            Ok(json!({"enabled": s.edit.flag("midi.beat_clock"), "offset": s.edit.value("midi.beat_clock.offset", 0.0), "destination": text(&s.edit, "midi.beat_clock.destination")}))
        }),
        cmd!(noundo "setup.midi_input_filter", "MIDI Input Filter...", ["Setup", "MIDI"], None, "{filter?: [notes|pitch_bend|mono_aftertouch|poly_aftertouch|program|cc|sysex|realtime], allow?: [...]} — unavailable: event-kind filtering is not supported", midi_filter_available, midi_filter),
        cmd!(noundo "setup.midi_input_devices", "MIDI Input Devices...", ["Setup", "MIDI"], None, "{enable?: [names], disable?: [names]} — unavailable: use track.midi_input", midi_inputs_available, midi_inputs),
        cmd!(noundo "setup.transcription_settings", "Transcription Settings...", ["Setup"], None, "{enabled?: bool, language?: 'en'}", always, |e, p| {
            let s = e.session_mut();
            if let Some(v) = p.get("enabled").and_then(Value::as_bool) {
                s.edit.set_flag("transcription.enabled", v);
            }
            if let Some(l) = str_param(p, "language") {
                set_text(&mut s.edit, "transcription.language", Some(l));
            }
            Ok(json!({"enabled": s.edit.flag("transcription.enabled"), "language": text(&s.edit, "transcription.language").unwrap_or_else(|| "en".into())}))
        }),
        cmd!(
            "file.import_session_data",
            "Session Data...",
            ["File", "Import"],
            Some("Alt+Shift+I"),
            "{path, tracks?: [names]} — imports tracks (clips, playlists, mixer, automation, audio) from another session",
            always,
            import_session_data
        ),
    ]
}

fn prepare_dpe(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = "options.prepare_dpe_tracks";
    let s = e.session();
    let tracks: Vec<TrackId> = if p.get("tracks").is_some() || p.get("track").is_some() {
        tracks_param(e, id, p)?
    } else {
        s.tracks.iter().filter(|t| t.mixer.record_arm).map(|t| t.id).collect()
    };
    let tracks: Vec<TrackId> = tracks.into_iter().filter(|t| s.track(*t).is_some_and(|x| x.kind == TrackKind::Audio)).collect();
    if tracks.is_empty() {
        return Err(bad(id, "record-enable (or pass) the audio tracks to prepare"));
    }
    let end = s.content_end();
    if end <= 0 {
        return Err(bad(id, "there is no audio to prepare"));
    }
    let r = soundcraft_time::Range::new(0, end);
    let mut n = 0;
    for t in tracks {
        // Each track keeps its own length (its material plus a second), as before.
        let tr = crate::io::bounded_to_track(e.session(), t, r);
        if crate::io::consolidate(e, id, t, tr)? {
            n += 1;
        }
    }
    e.session_mut().edit.set_flag("options.dpe_prepared", true);
    Ok(json!({"prepared": n}))
}

fn engine_settings(e: &mut Engine, p: &Value) -> Result<Value> {
    let s = e.session_mut();
    if let Some(b) = p.get("buffer_size").and_then(Value::as_u64) {
        let b = b.clamp(16, 8192).next_power_of_two().min(8192);
        s.edit.values.insert("engine.buffer_size".into(), b as f64);
    }
    if let Some(d) = str_param(p, "device") {
        set_text(&mut s.edit, "engine.device", Some(d));
    }
    Ok(json!({
        "device": text(&s.edit, "engine.device"),
        "buffer_size": s.edit.value("engine.buffer_size", 1024.0),
        "sample_rate": s.sample_rate.hz(),
        "bit_depth": s.bit_depth,
        "delay_compensation": s.edit.delay_compensation,
    }))
}

fn disk_allocation(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = "setup.disk_allocation";
    let t = track_param(e, id, p, "track")?;
    let s = e.session_mut();
    if let Some(path) = str_param(p, "path") {
        let key = t.map_or_else(|| "disk.root".to_string(), |t| format!("disk.track.{}", t.0));
        set_text(&mut s.edit, &key, Some(path));
    }
    let root = text(&s.edit, "disk.root").unwrap_or_else(|| "Audio Files".into());
    let rows: Vec<Value> = s
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Audio)
        .map(|t| json!({"track": t.id, "name": t.name, "path": text(&s.edit, &format!("disk.track.{}", t.id.0)).unwrap_or_else(|| root.clone())}))
        .collect();
    Ok(json!({"root": root, "tracks": rows}))
}

fn peripherals(e: &mut Engine, p: &Value) -> Result<Value> {
    let s = e.session_mut();
    if let Some(m) = p.get("settings").and_then(Value::as_object) {
        for (k, v) in m.iter().take(256) {
            let key: String = k.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')).take(64).collect();
            if key.is_empty() {
                continue;
            }
            let key = format!("peripherals.{key}");
            match v {
                Value::Bool(b) => s.edit.set_flag(&key, *b),
                Value::Number(n) => {
                    if let Some(x) = n.as_f64().filter(|x| x.is_finite()) {
                        s.edit.values.insert(key, x);
                    }
                }
                Value::String(t) => set_text(&mut s.edit, &key, Some(t)),
                _ => {}
            }
        }
    }
    let flags: Vec<&String> = s.edit.flags.iter().filter(|f| f.starts_with("peripherals.")).collect();
    Ok(json!({"values": values_with_prefix(&s.edit, "peripherals."), "flags": flags}))
}

fn io_state(s: &Session) -> Value {
    json!({"busses": s.busses, "outputs": s.outputs})
}

fn io_map_state(e: &Engine) -> Value {
    let io = e.io_map();
    let overlaps: Vec<Value> = io.overlaps().into_iter().map(|(a, b, c)| json!({"a": a, "b": b, "channel": c})).collect();
    let unresolved = e.unresolved_routes();
    json!({"io": io, "overlaps": overlaps, "unresolved": unresolved})
}

/// `setup.io_map`: the machine's I/O paths. Not a session edit (the table is per machine, kept by
/// the app with its preferences), so there is no undo step and the document stays clean.
fn io_map_setup(e: &mut Engine, p: &Value) -> Result<Value> {
    use soundcraft_model::{IoDirection, IoMap, IoPath};
    let id = "setup.io_map";
    let action = str_param(p, "action").unwrap_or("list");
    let refuse = |error: soundcraft_model::IoError| bad(id, error.to_string());
    let name = || str_param(p, "name").map(str::trim).filter(|n| !n.is_empty()).map(str::to_string).ok_or_else(|| bad(id, "`name` required"));
    let channels = || -> Result<Vec<u16>> {
        let list = p.get("channels").and_then(Value::as_array).ok_or_else(|| bad(id, "`channels` required (0-based device channels)"))?;
        list.iter().map(|c| c.as_u64().and_then(|c| u16::try_from(c).ok()).ok_or_else(|| bad(id, "channels are small whole numbers"))).collect()
    };
    let mut io = e.io_map().clone();
    match action {
        "list" => return Ok(io_map_state(e)),
        "set" => {
            let map: IoMap = serde_json::from_value(p.get("map").cloned().ok_or_else(|| bad(id, "`map` required"))?)
                .map_err(|error| bad(id, format!("`map` is not an I/O map: {error}")))?;
            // Counts are bounded like a device's: a path beyond them is refused by `add_path`.
            let cap = soundcraft_model::MAX_DEVICE_CHANNELS as u16;
            let mut checked = IoMap { paths: Vec::new(), inputs: map.inputs.min(cap), outputs: map.outputs.min(cap), ..map.clone() };
            for path in map.paths {
                checked.add_path(path).map_err(refuse)?;
            }
            checked.set_main(&map.main).map_err(refuse)?;
            io = checked;
        }
        "add_path" => {
            let direction = match str_param(p, "direction") {
                Some("input") => IoDirection::Input,
                Some("output") | None => IoDirection::Output,
                Some(other) => return Err(bad(id, format!("direction is input or output, not `{other}`"))),
            };
            let channels = channels()?;
            let format = soundcraft_model::ChannelFormat::for_channels(channels.len());
            io.add_path(IoPath { name: name()?, direction, channels, format }).map_err(refuse)?;
        }
        "rename_path" => {
            let to = str_param(p, "new_name").ok_or_else(|| bad(id, "`new_name` required"))?;
            io.rename_path(&name()?, to).map_err(refuse)?;
        }
        "remove_path" => io.remove_path(&name()?).map_err(refuse)?,
        "set_channels" => io.set_channels(&name()?, channels()?).map_err(refuse)?,
        "set_main" => io.set_main(&name()?).map_err(refuse)?,
        other => return Err(bad(id, format!("unknown action `{other}`"))),
    }
    e.set_io_map(io);
    Ok(io_map_state(e))
}

/// The pair named by `send` and `ret` (`ret` is also accepted as `return`).
fn hardware_pair(p: &Value, id: &str) -> Result<(String, String)> {
    let name = |keys: &[&str]| keys.iter().find_map(|k| str_param(p, k)).map(str::trim).filter(|n| !n.is_empty()).map(str::to_string);
    let send = name(&["send"]).ok_or_else(|| bad(id, "`send` (an output path) required"))?;
    let ret = name(&["ret", "return"]).ok_or_else(|| bad(id, "`ret` (an input path) required"))?;
    Ok((send, ret))
}

/// `io.measure_hardware_latency`: the engine checks the request; the click and the clock are the
/// app's (the live audio device), which runs this command itself (`StudioApp::run`). Without one
/// there is nothing to measure through.
fn measure_hardware_latency(e: &mut Engine, p: &Value) -> Result<Value> {
    let (send, ret) = hardware_pair(p, "io.measure_hardware_latency")?;
    e.hardware_measurement(&send, &ret)?;
    Err(bad("io.measure_hardware_latency", "measuring needs the live audio device: run it from the app"))
}

/// `io.clear_hardware_latency`: forget a pair's measurements (every rate).
fn clear_hardware_latency(e: &mut Engine, p: &Value) -> Result<Value> {
    let (send, ret) = hardware_pair(p, "io.clear_hardware_latency")?;
    let mut io = e.io_map().clone();
    let cleared = io.clear_hardware_latency(&send, &ret);
    if cleared {
        e.set_io_map(io);
    }
    Ok(json!({"cleared": cleared}))
}

fn io_setup(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = "setup.io";
    let action = str_param(p, "action").unwrap_or("list");
    if action == "list" {
        return Ok(io_state(e.session()));
    }
    let name = str_param(p, "name").map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| bad(id, "`name` required"))?.to_string();
    let new_name = str_param(p, "new_name").map(str::trim).filter(|n| !n.is_empty()).map(str::to_string);
    let format = match str_param(p, "format") {
        Some(f) => ChannelFormat::from_id(f).ok_or_else(|| bad(id, format!("unknown format `{f}`")))?,
        None => ChannelFormat::Stereo,
    };
    let paths = e.io_map().clone();
    let s = e.session_mut();
    match action {
        "create_bus" => {
            if s.bus_by_name(&name).is_some() {
                return Err(bad(id, format!("a bus named `{name}` exists")));
            }
            // Buses and I/O paths share one namespace, so a route's name means one thing.
            if paths.path(&name).is_some() {
                return Err(bad(id, format!("`{name}` is an I/O path of this machine; a bus needs another name")));
            }
            s.add_bus(&name, format);
        }
        "rename_bus" => {
            let to = new_name.ok_or_else(|| bad(id, "`new_name` required"))?;
            if s.bus_by_name(&to).is_some() {
                return Err(bad(id, format!("a bus named `{to}` exists")));
            }
            if paths.path(&to).is_some() {
                return Err(bad(id, format!("`{to}` is an I/O path of this machine; a bus needs another name")));
            }
            let b = s.busses.iter_mut().find(|b| b.name.eq_ignore_ascii_case(&name)).ok_or_else(|| bad(id, format!("no bus `{name}`")))?;
            b.name = to;
        }
        "delete_bus" => {
            let bid: BusId = s.bus_by_name(&name).map(|b| b.id).ok_or_else(|| bad(id, format!("no bus `{name}`")))?;
            s.busses.retain(|b| b.id != bid);
            for t in &mut s.tracks {
                if t.mixer.input == Route::Bus(bid) {
                    t.mixer.input = Route::None;
                }
                if t.mixer.output == Route::Bus(bid) {
                    t.mixer.output = Route::None;
                }
                for slot in &mut t.mixer.sends {
                    if slot.as_ref().is_some_and(|x| x.target == Route::Bus(bid)) {
                        *slot = None;
                    }
                }
            }
        }
        "create_output" => {
            if s.outputs.iter().any(|o| o.name.eq_ignore_ascii_case(&name)) {
                return Err(bad(id, format!("an output named `{name}` exists")));
            }
            let first = i64_or(p, "first_channel", 0).clamp(0, 1023) as u16;
            s.outputs.push(OutputPath { name, first_channel: first, format });
        }
        "rename_output" => {
            let to = new_name.ok_or_else(|| bad(id, "`new_name` required"))?;
            let o = s.outputs.iter_mut().find(|o| o.name.eq_ignore_ascii_case(&name)).ok_or_else(|| bad(id, format!("no output `{name}`")))?;
            let old = std::mem::replace(&mut o.name, to.clone());
            for t in &mut s.tracks {
                if t.mixer.output == Route::Hardware(old.clone()) {
                    t.mixer.output = Route::Hardware(to.clone());
                }
            }
        }
        "delete_output" => {
            let before = s.outputs.len();
            s.outputs.retain(|o| !o.name.eq_ignore_ascii_case(&name));
            if s.outputs.len() == before {
                return Err(bad(id, format!("no output `{name}`")));
            }
        }
        other => return Err(bad(id, format!("unknown action `{other}`"))),
    }
    Ok(io_state(s))
}

fn current_timecode(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = "setup.current_timecode";
    let tc = str_param(p, "timecode").ok_or_else(|| bad(id, "`timecode` required (HH:MM:SS:FF)"))?;
    let tc = Timecode::parse(tc).map_err(|err| bad(id, err.to_string()))?;
    let s = e.session_mut();
    let at = s.edit.selection.start.max(0);
    let abs = tc.to_samples(s.frame_rate, s.sample_rate);
    s.timecode_start = abs - at;
    Ok(json!({"timecode_start": s.timecode_start, "at": at}))
}

fn current_feet_frames(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = "setup.current_feet_frames";
    let text_v = str_param(p, "feet_frames").ok_or_else(|| bad(id, "`feet_frames` required (F+FF)"))?;
    let (ft, fr) = text_v.split_once('+').ok_or_else(|| bad(id, "use F+FF, e.g. 12+08"))?;
    let ft: i64 = ft.trim().parse().map_err(|_| bad(id, "bad feet"))?;
    let fr: i64 = fr.trim().parse().map_err(|_| bad(id, "bad frames"))?;
    if !(0..=10_000_000).contains(&ft) || !(0..16).contains(&fr) {
        return Err(bad(id, "feet must be 0.. and frames 0..15"));
    }
    let s = e.session_mut();
    let at = s.edit.selection.start.max(0);
    let frames_at = s.frame_rate.frames_at(at, s.sample_rate);
    let start = ft * 16 + fr - frames_at;
    s.edit.values.insert(soundcraft_model::FEET_FRAMES_START.into(), start as f64);
    Ok(json!({"feet_frames_start": start}))
}

fn offset_setting(e: &mut Engine, p: &Value, key: &str) -> Result<Value> {
    let s = e.session_mut();
    let sr = s.sample_rate;
    let per_frame = s.frame_rate.samples_per_frame(sr);
    if let Some(f) = p.get("frames").and_then(Value::as_f64).filter(|v| v.is_finite()) {
        s.edit.values.insert(key.to_string(), (f.clamp(-10_000.0, 10_000.0) * per_frame).round());
    } else if let Some(n) = p.get("samples").and_then(Value::as_f64).filter(|v| v.is_finite()) {
        let lim = sr.as_f64() * 3600.0;
        s.edit.values.insert(key.to_string(), n.clamp(-lim, lim).round());
    }
    let samples = s.edit.value(key, 0.0);
    Ok(json!({"samples": samples, "frames": samples / per_frame.max(1e-9)}))
}

fn click_countoff(e: &mut Engine, p: &Value) -> Result<Value> {
    let s = e.session_mut();
    store_value(&mut s.edit, p, "volume_db", "click.volume_db", -60.0, 12.0);
    store_value(&mut s.edit, p, "accent_note", "click.accent_note", 0.0, 127.0);
    store_value(&mut s.edit, p, "normal_note", "click.normal_note", 0.0, 127.0);
    store_value(&mut s.edit, p, "accent_velocity", "click.accent_velocity", 1.0, 127.0);
    store_value(&mut s.edit, p, "normal_velocity", "click.normal_velocity", 1.0, 127.0);
    if let Some(v) = p.get("countoff").and_then(Value::as_bool) {
        s.edit.countoff = v;
    }
    if let Some(b) = p.get("countoff_bars").and_then(Value::as_u64) {
        s.edit.countoff_bars = b.clamp(1, 16) as u32;
    }
    if let Some(v) = p.get("only_during_record").and_then(Value::as_bool) {
        s.edit.set_flag("click.only_during_record", v);
    }
    Ok(json!({
        "volume_db": s.edit.value("click.volume_db", -6.0),
        "accent_note": s.edit.value("click.accent_note", 84.0),
        "normal_note": s.edit.value("click.normal_note", 72.0),
        "accent_velocity": s.edit.value("click.accent_velocity", 127.0),
        "normal_velocity": s.edit.value("click.normal_velocity", 100.0),
        "countoff": s.edit.countoff,
        "countoff_bars": s.edit.countoff_bars,
        "only_during_record": s.edit.flag("click.only_during_record"),
    }))
}

fn midi_filter_available(_: &Engine) -> std::result::Result<(), String> {
    Err("MIDI event-kind filtering is not supported".into())
}

fn midi_inputs_available(_: &Engine) -> std::result::Result<(), String> {
    Err("use track.midi_input to choose a track's MIDI port and channel".into())
}

fn midi_filter(_: &mut Engine, _: &Value) -> Result<Value> {
    Err(EngineError::Disabled("setup.midi_input_filter".into(), "MIDI event-kind filtering is not supported".into()))
}

fn midi_inputs(_: &mut Engine, _: &Value) -> Result<Value> {
    Err(EngineError::Disabled("setup.midi_input_devices".into(), "use track.midi_input to choose a track's MIDI port and channel".into()))
}

fn import_session_data(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = "file.import_session_data";
    let path = str_param(p, "path").filter(|x| !x.trim().is_empty()).ok_or_else(|| bad(id, "`path` required"))?.to_string();
    let names: Option<Vec<String>> =
        p.get("tracks").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect());
    let imported = crate::io::import_session_data(e, &path, names)?;
    Ok(json!({"tracks": imported.tracks, "sources": imported.sources, "missing": imported.missing}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_flags_toggle() {
        let mut e = Engine::default();
        assert_eq!(e.execute("options.midi_thru", &json!({})).unwrap()["value"], true);
        assert!(e.session().edit.flag("options.midi_thru"));
        e.execute("options.midi_thru", &json!({"value": false})).unwrap();
        assert!(!e.session().edit.flag("options.midi_thru"));
    }

    #[test]
    fn io_bus_lifecycle() {
        let mut e = crate::demo::demo_engine();
        e.execute("setup.io", &json!({"action": "create_bus", "name": "Drums"})).unwrap();
        e.execute("setup.io", &json!({"action": "rename_bus", "name": "Verb", "new_name": "Hall"})).unwrap();
        assert!(e.session().bus_by_name("Hall").is_some());
        e.execute("setup.io", &json!({"action": "delete_bus", "name": "Hall"})).unwrap();
        assert!(
            e.session().tracks.iter().all(|t| t
                .mixer
                .sends
                .iter()
                .flatten()
                .all(|s| matches!(s.target, Route::None | Route::Main | Route::Hardware(_))))
        );
        assert!(e.session().tracks.iter().all(|t| {
            !matches!(t.mixer.input, Route::Bus(_))
                || e.session()
                    .bus(match t.mixer.input {
                        Route::Bus(b) => b,
                        _ => BusId(0),
                    })
                    .is_some()
        }));
        e.execute("setup.io", &json!({"action": "create_output", "name": "Out 3-4", "first_channel": 2})).unwrap();
        assert_eq!(e.session().outputs.len(), 2);
        assert!(e.execute("setup.io", &json!({"action": "nope", "name": "x"})).is_err());
        assert!(e.can_undo());
    }

    #[test]
    fn io_map_commands_edit_the_machine_table_without_an_undo_step_or_a_dirty_session() {
        let mut e = Engine::default();
        let saved = e.session().clone();
        let (undo, revision) = (e.can_undo(), e.revision);
        assert_eq!(e.execute("setup.io_map", &json!({})).unwrap()["io"]["main"], "Out 1-2", "headless engines hold the stereo default");
        e.set_io_map(soundcraft_model::IoMap::for_device("ASIO", "TASCAM", 12, 10));
        let r = e.execute("setup.io_map", &json!({"action": "add_path", "name": "Headphones", "channels": [2]})).unwrap();
        assert_eq!(r["overlaps"][0]["b"], "Headphones");
        e.execute("setup.io_map", &json!({"action": "rename_path", "name": "Headphones", "new_name": "Cue"})).unwrap();
        e.execute("setup.io_map", &json!({"action": "set_channels", "name": "Cue", "channels": [8, 9]})).unwrap();
        e.execute("setup.io_map", &json!({"action": "set_main", "name": "Out 3-4"})).unwrap();
        assert_eq!(e.io_map().main_channels(), [2, 3]);
        e.execute("setup.io_map", &json!({"action": "remove_path", "name": "Cue"})).unwrap();
        assert!(e.io_map().path("Cue").is_none());
        assert_eq!((e.can_undo(), e.revision), (undo, revision), "no undo entry, no document change");
        assert!(!e.is_dirty());
        assert_eq!(e.session(), &saved);
        assert!(e.io_revision() >= 6, "the host sees every change");
    }

    #[test]
    fn io_map_set_bounds_the_channel_counts_like_a_device() {
        let mut e = Engine::default();
        let mut map = soundcraft_model::IoMap::for_device("ASIO", "x", 2, 2);
        map.inputs = u16::MAX;
        map.outputs = 40;
        let r = e.execute("setup.io_map", &json!({"action": "set", "map": map})).unwrap();
        assert_eq!((r["io"]["inputs"].clone(), r["io"]["outputs"].clone()), (json!(32), json!(32)));
        assert_eq!((e.io_map().inputs, e.io_map().outputs), (32, 32));
        // A path on a channel past the cap is still refused.
        map.paths.push(soundcraft_model::IoPath {
            name: "Far".into(),
            direction: soundcraft_model::IoDirection::Output,
            channels: vec![33],
            format: soundcraft_model::ChannelFormat::Mono,
        });
        assert!(matches!(e.execute("setup.io_map", &json!({"action": "set", "map": map})), Err(crate::EngineError::BadParams(..))));
    }

    #[test]
    fn io_map_commands_refuse_bad_paths_and_leave_the_table_alone() {
        let mut e = Engine::default();
        e.set_io_map(soundcraft_model::IoMap::for_device("ASIO", "x", 2, 4));
        let before = (e.io_map().clone(), e.io_revision());
        for params in [
            json!({"action": "add_path", "name": "Out 3-4", "channels": [2, 3]}),
            json!({"action": "add_path", "name": "Far", "channels": [4]}),
            json!({"action": "add_path", "name": "Wide", "channels": (0..17).collect::<Vec<u16>>()}),
            json!({"action": "add_path", "name": "Twice", "channels": [1, 1]}),
            json!({"action": "add_path", "channels": [1]}),
            json!({"action": "remove_path", "name": "Out 1-2"}),
            json!({"action": "set_main", "name": "In 1"}),
            json!({"action": "nope"}),
        ] {
            assert!(matches!(e.execute("setup.io_map", &params), Err(crate::EngineError::BadParams(..))), "{params}");
        }
        assert_eq!((e.io_map().clone(), e.io_revision()), before);
    }

    #[test]
    fn a_bus_cannot_take_a_path_name_and_the_list_names_unresolved_routes() {
        let mut e = Engine::default();
        e.set_io_map(soundcraft_model::IoMap::for_device("ASIO", "x", 2, 4));
        assert!(matches!(e.execute("setup.io", &json!({"action": "create_bus", "name": "out 3-4"})), Err(crate::EngineError::BadParams(..))));
        assert!(matches!(e.execute("setup.io", &json!({"action": "create_bus", "name": "In 1"})), Err(crate::EngineError::BadParams(..))));
        e.execute("setup.io", &json!({"action": "create_bus", "name": "Drums"})).unwrap();
        assert!(matches!(
            e.execute("setup.io", &json!({"action": "rename_bus", "name": "Drums", "new_name": "Out 1-2"})),
            Err(crate::EngineError::BadParams(..))
        ));
        let t = e.execute("track.new", &json!({"name": "Cue"})).unwrap()["tracks"][0].as_u64().unwrap();
        e.session_mut().track_mut(soundcraft_model::TrackId(t)).unwrap().mixer.output = Route::Hardware("Headphones".into());
        assert_eq!(e.execute("setup.io_map", &json!({})).unwrap()["unresolved"], json!(["Headphones"]));
        // Renaming a path leaves the session's routes alone: the old name is then unresolved.
        e.session_mut().track_mut(soundcraft_model::TrackId(t)).unwrap().mixer.output = Route::Hardware("Out 3-4".into());
        assert_eq!(e.execute("setup.io_map", &json!({})).unwrap()["unresolved"], json!([]));
        e.execute("setup.io_map", &json!({"action": "rename_path", "name": "Out 3-4", "new_name": "Wedge"})).unwrap();
        assert_eq!(e.session().track(soundcraft_model::TrackId(t)).unwrap().mixer.output, Route::Hardware("Out 3-4".into()));
        assert_eq!(e.execute("setup.io_map", &json!({})).unwrap()["unresolved"], json!(["Out 3-4"]));
    }

    #[test]
    fn io_map_set_replaces_the_table_and_validates_it() {
        let mut e = Engine::default();
        let mut map = soundcraft_model::IoMap::for_device("ASIO", "TASCAM", 12, 10);
        map.rename_path("Out 3-4", "Cue").unwrap();
        e.execute("setup.io_map", &json!({"action": "set", "map": map})).unwrap();
        assert_eq!(e.io_map(), &map);
        map.paths[0].channels = vec![40];
        assert!(e.execute("setup.io_map", &json!({"action": "set", "map": map})).is_err());
    }

    #[test]
    fn current_timecode_sets_start() {
        let mut e = Engine::default();
        e.execute("edit.select", &json!({"start": 48_000})).unwrap();
        e.execute("setup.current_timecode", &json!({"timecode": "01:00:00:00"})).unwrap();
        let s = e.session();
        let shown = s.format_position(48_000, soundcraft_time::TimeFormat::Timecode);
        assert_eq!(shown, "01:00:00:00");
    }

    #[test]
    fn click_countoff_settings() {
        let mut e = Engine::default();
        let r = e.execute("setup.click_countoff", &json!({"volume_db": -12.0, "countoff_bars": 4, "countoff": true})).unwrap();
        assert_eq!(r["volume_db"], -12.0);
        assert_eq!(e.session().edit.countoff_bars, 4);
        assert!(e.session().edit.countoff);
    }

    #[test]
    fn shortcuts_listed() {
        let mut e = Engine::default();
        let r = e.execute("setup.keyboard_shortcuts", &json!({})).unwrap();
        assert!(r["shortcuts"].as_array().unwrap().iter().any(|x| x["id"] == "edit.undo"));
    }

    #[test]
    fn engine_and_midi_settings_issue_10() {
        let mut e = Engine::default();
        let r = e.execute("setup.playback_engine", &json!({"buffer_size": 300, "device": "Interface"})).unwrap();
        assert_eq!(r["buffer_size"], 512.0);
        assert_eq!(r["device"], "Interface");
        let before = e.session_arc();
        for (id, params) in [("setup.midi_input_filter", json!({"filter": ["sysex"]})), ("setup.midi_input_devices", json!({"enable": ["Keys"]}))] {
            assert!(!find_command(id).unwrap().info(&e).enabled);
            assert!(matches!(e.execute(id, &params), Err(EngineError::Disabled(..))));
            assert_eq!(e.session(), before.as_ref());
        }
        let r = e.execute("setup.video_sync_offset", &json!({"frames": 2})).unwrap();
        assert_eq!(r["samples"], 3200.0);
    }

    #[test]
    fn import_session_data_brings_tracks_and_audio() {
        let dir = std::env::temp_dir().join(format!("sc-import-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("donor.transit").to_string_lossy().into_owned();
        let mut donor = crate::demo::demo_engine();
        crate::io::save_session(&mut donor, &path).unwrap();
        let mut e = Engine::default();
        let r = e.execute("file.import_session_data", &json!({"path": path, "tracks": ["Kick", "Keys"]})).unwrap();
        assert_eq!(r["tracks"].as_array().unwrap().len(), 2);
        assert_eq!(r["missing"], json!([]));
        let kick = e.session().track_by_name("Kick").unwrap();
        let src = kick.clips()[0].source().unwrap();
        assert!(e.session().pool.get(src).is_some());
        assert_eq!(e.session().track_by_name("Keys").unwrap().clips().len(), 1);
        assert!(e.execute("file.import_session_data", &json!({"path": path, "tracks": ["Nope"]})).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_session_data_brings_a_samplers_sample() {
        let dir = std::env::temp_dir().join(format!("sc-import-sampler-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("hit.wav");
        let buf = soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.5; 4_800]] };
        std::fs::write(&wav, soundcraft_audio_io::encode(&buf, &soundcraft_audio_io::EncodeOptions::default()).unwrap()).unwrap();
        let mut donor = Engine::default();
        donor.execute("track.new", &json!({"count": 1, "name": "Pads", "kind": "instrument"})).unwrap();
        donor.execute("mix.sampler_load", &json!({"track": "Pads", "path": wav.to_string_lossy()})).unwrap();
        let path = dir.join("donor.transit").to_string_lossy().into_owned();
        crate::io::save_session(&mut donor, &path).unwrap();
        let mut e = crate::demo::demo_engine();
        e.execute("file.import_session_data", &json!({"path": path, "tracks": ["Pads"]})).unwrap();
        let sample = e.session().track_by_name("Pads").and_then(|t| t.instrument.as_ref()).and_then(|i| i.sample).unwrap();
        let src = e.session().source(sample).unwrap();
        assert_eq!(src.name, "hit");
        assert!(e.session().pool.get(sample).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
