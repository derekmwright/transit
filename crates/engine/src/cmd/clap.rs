//! Third-party CLAP plugins: discovery (`engine.clap_plugins`) and id lookup shared by the mixer
//! commands. Hosting itself lives in `soundcraft-clap-host`.

use super::*;
use crate::cmd;
use soundcraft_dsp::PluginInfo;

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        query "engine.clap_plugins",
        "List CLAP Plugins",
        [],
        None,
        "{rescan?: bool} → [{id: \"clap:…\", plugin_id, name, vendor, version, description, features, category, is_instrument, path}]",
        always,
        super::plugin_scan::clap
    )]
}

/// A built-in plugin's description, else a hosted CLAP (`clap:<id>`), VST3 (`vst3:<class id>`) or
/// Audio Units (`au:<type>:<subtype>:<manufacturer>`) plugin's.
///
/// A hosted type no instance has been made of yet is instantiated to read its parameter table,
/// on the plug-in host thread (ADR 0018), and waited for at most [`INFO_WAIT`]: a type that takes
/// longer is "loading" ([`loading`]), its read carries on there, and a later call finds it.
pub fn plugin_info(id: &str) -> Option<&'static PluginInfo> {
    // Tests never reach the real hosts (see `soundcraft_mix::create_plugin`): a lookup scans the
    // machine's plug-in folders, and some plug-ins never unload.
    #[cfg(test)]
    if soundcraft_mix::is_third_party(id) {
        return None;
    }
    soundcraft_dsp::plugin_info(id).or_else(|| hosted_info(id, INFO_WAIT).ok().flatten())
}

/// How long a command waits for a hosted type's description from the plug-in host thread.
pub const INFO_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// A hosted type's description is being read on the plug-in host thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loading;

type InfoRead = transit_plugin_host::Pending<Option<&'static PluginInfo>>;

/// Reads still running on the host thread after their command stopped waiting, by id.
fn info_reads() -> std::sync::MutexGuard<'static, Vec<(String, InfoRead)>> {
    static READS: std::sync::Mutex<Vec<(String, InfoRead)>> = std::sync::Mutex::new(Vec::new());
    READS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Whether `id`'s description is still being read on the plug-in host thread (a command that
/// needed it was refused with "loading" and can be tried again).
pub fn loading(id: &str) -> bool {
    info_reads().iter().any(|(read, _)| read == id)
}

/// A hosted type's description: from the hosts' caches, else read on the plug-in host thread
/// (which makes and destroys one instance there), waiting at most `wait`.
fn hosted_info(id: &str, wait: std::time::Duration) -> std::result::Result<Option<&'static PluginInfo>, Loading> {
    if let Some(info) = soundcraft_clap_host::cached_info(id).or_else(|| soundcraft_vst3_host::cached_info(id)) {
        return Ok(Some(info));
    }
    let running = {
        let mut reads = info_reads();
        reads.iter().position(|(read, _)| read == id).map(|i| reads.swap_remove(i).1)
    };
    let read = running.unwrap_or_else(|| {
        let id = id.to_owned();
        transit_plugin_host::shared().submit(move |_| {
            soundcraft_clap_host::plugin_info(&id).or_else(|| soundcraft_vst3_host::plugin_info(&id)).or_else(|| soundcraft_au_host::plugin_info(&id))
        })
    });
    let deadline = std::time::Instant::now() + wait;
    loop {
        match read.poll() {
            transit_plugin_host::Status::Ready(info) => return Ok(info),
            transit_plugin_host::Status::Failed => return Ok(None),
            // Its thread is not responding: the next call asks the thread that took over.
            transit_plugin_host::Status::Stuck => return Err(Loading),
            transit_plugin_host::Status::Waiting { .. } if std::time::Instant::now() >= deadline => {
                info_reads().push((id.to_owned(), read));
                return Err(Loading);
            }
            transit_plugin_host::Status::Waiting { .. } => std::thread::sleep(std::time::Duration::from_millis(2)),
        }
    }
}

/// [`plugin_info`] for a command: "unknown plugin", or that its description is still loading.
pub fn require_info(cmd: &str, id: &str) -> Result<&'static PluginInfo> {
    plugin_info(id).ok_or_else(|| {
        if loading(id) { bad(cmd, format!("`{id}` is still loading its parameter table; try again in a moment")) } else { bad(cmd, "unknown plugin") }
    })
}

/// What adding a plug-in needs to know of it: its id and whether it is an instrument, and its full
/// description when `params` is set (a command that names parameters). A hosted plug-in's
/// summary is its scan result; the full description ([`plugin_info`]) creates an instance, which
/// blocks the thread running the command for as long as the plug-in takes to load, and adding the
/// plug-in to a session has no use for it: the playback runtime creates the instance itself, one
/// frame later, with the UI saying so.
pub fn plugin_head(id: &str, params: bool) -> Option<(String, bool, Option<&'static PluginInfo>)> {
    if let Some(info) = soundcraft_dsp::plugin_info(id) {
        return Some((info.id.to_string(), info.is_instrument, Some(info)));
    }
    #[cfg(test)]
    if soundcraft_mix::is_third_party(id) {
        return None;
    }
    if !params {
        let scanned = if id.starts_with(soundcraft_clap_host::ID_PREFIX) {
            soundcraft_clap_host::cached().and_then(|all| all.into_iter().find(|d| d.id == id).map(|d| (d.id, d.is_instrument)))
        } else if id.starts_with(soundcraft_vst3_host::ID_PREFIX) {
            soundcraft_vst3_host::cached().and_then(|all| all.into_iter().find(|d| d.id == id).map(|d| (d.id, d.is_instrument)))
        } else {
            None
        };
        if let Some((id, is_instrument)) = scanned {
            return Some((id, is_instrument, None));
        }
    }
    // Adding a plug-in (with parameter values, or one the scan does not list) needs its
    // description: read on the plug-in host thread like `plugin_info`, but waited for up to the
    // host's request timeout, as the command did when it instantiated the plug-in itself.
    soundcraft_dsp::plugin_info(id)
        .or_else(|| hosted_info(id, transit_plugin_host::shared().timeout()).ok().flatten())
        .map(|info| (info.id.to_string(), info.is_instrument, Some(info)))
}

#[cfg(test)]
mod tests {
    use crate::Engine;
    use serde_json::json;

    #[test]
    fn a_plugins_head_names_it_without_the_hosts() {
        let (id, instrument, info) = super::plugin_head("eq_7band", false).unwrap();
        assert_eq!((id.as_str(), instrument), ("eq_7band", false));
        assert!(info.is_some(), "a built-in always has its description");
        assert!(super::plugin_head("clap:no.such.plugin", false).is_none());
        assert!(super::plugin_head("vst3:00000000000000000000000000000000", true).is_none());
    }

    /// Issue #56 (i): a hosted type's description is read on the plug-in host thread, and a
    /// command waits for it at most `INFO_WAIT`. With the host thread busy (a plug-in's long call),
    /// the lookup says "loading" in time and keeps the read; once the thread is free the next
    /// lookup gets its answer (here: an id no host knows).
    #[test]
    fn a_hosted_description_is_read_on_the_host_thread_with_a_bounded_wait() {
        if transit_plugin_host::INLINE_BY_DEFAULT {
            return;
        }
        let id = "vst3:not-a-class-id-issue-56";
        let host = transit_plugin_host::shared();
        let stall = host.submit(|_| std::thread::sleep(std::time::Duration::from_millis(1200)));
        while !matches!(stall.poll(), transit_plugin_host::Status::Waiting { running: true }) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let t = std::time::Instant::now();
        assert_eq!(super::hosted_info(id, super::INFO_WAIT), Err(super::Loading));
        assert!(t.elapsed() < super::INFO_WAIT + std::time::Duration::from_millis(300), "waited {:?}", t.elapsed());
        assert!(super::loading(id), "the read carries on on the host thread");
        while matches!(stall.poll(), transit_plugin_host::Status::Waiting { .. }) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(super::hosted_info(id, super::INFO_WAIT), Ok(None), "the read finished: no host knows the id");
        assert!(!super::loading(id));
    }

    #[test]
    fn clap_plugins_query_returns_a_list() {
        let mut e = Engine::default();
        let v = e.execute("engine.clap_plugins", &json!({})).unwrap();
        assert!(v.is_array());
    }

    #[test]
    fn unknown_clap_insert_is_an_error() {
        let mut e = Engine::default();
        e.execute("track.new", &json!({})).unwrap();
        let t = e.session().tracks[0].id.0;
        let r = e.execute("mix.insert", &json!({"track": t, "plugin": "clap:no.such.plugin"}));
        assert!(r.is_err());
        assert!(super::plugin_info("eq_7band").is_some());
        assert!(super::plugin_info("clap:no.such.plugin").is_none());
    }
}
