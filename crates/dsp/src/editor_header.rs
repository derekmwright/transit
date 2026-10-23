//! The header strip of a hosted plug-in's editor window (issue #68) and the plug-in state jobs
//! behind it: plain data the UI hands to an editor ([`EditorHeader`]), what the user did in the
//! strip ([`HeaderAction`]), and a state read or load running on the plug-in's own thread
//! ([`StateJob`], polled once per frame so the UI never waits for plug-in code).

/// What the header strip of an editor window shows. The UI builds it each frame; an editor
/// passes it on to its window only when it changed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EditorHeader {
    /// The window title: "Track · Slot · Plug-in".
    pub title: String,
    /// The current preset's name (empty: none).
    pub preset: String,
    /// The user presets of this plug-in type, in dropdown order.
    pub user: Vec<String>,
    /// The factory presets the format exposes (read-only); empty when it exposes none.
    pub factory: Vec<String>,
    /// The insert is bypassed.
    pub bypassed: bool,
    /// The slot can be bypassed (inserts; not instruments).
    pub can_bypass: bool,
    /// A state of the same plug-in type was copied and can be pasted.
    pub can_paste: bool,
    /// The current preset is a user preset that can be deleted.
    pub can_delete: bool,
    /// Every control is disabled, with the reason shown in the strip (the plug-in is not
    /// responding, or a preset operation is under way).
    pub disabled: Option<String>,
}

/// What the user did in the header strip, reported by [`crate::PluginEditor::take_header_actions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderAction {
    /// A user preset, by its index in [`EditorHeader::user`].
    User(usize),
    /// A factory preset, by its index in [`EditorHeader::factory`].
    Factory(usize),
    Previous,
    Next,
    Save,
    SaveAs,
    Load,
    Bypass,
    Reset,
    Copy,
    Paste,
    Delete,
    ShowInChain,
}

/// What [`StateJob::poll`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobPoll<T> {
    /// Not finished yet: poll again next frame.
    Waiting,
    /// Finished (once; later polls report [`JobPoll::Failed`]).
    Ready(T),
    /// It cannot finish: the reason, for the status line.
    Failed(String),
}

type Poller<T> = Box<dyn FnMut() -> JobPoll<T> + Send>;

/// A plug-in state operation (a read for a preset save, a load) that may run on another thread,
/// polled once per frame. Readers that answer at once make ready jobs.
pub struct StateJob<T> {
    inner: Inner<T>,
}

enum Inner<T> {
    Done(Option<Result<T, String>>),
    Remote(Poller<T>),
}

impl<T> StateJob<T> {
    /// A job that has finished with `value`.
    #[must_use]
    pub fn ready(value: T) -> StateJob<T> {
        StateJob { inner: Inner::Done(Some(Ok(value))) }
    }

    /// A job that cannot run, with the reason.
    #[must_use]
    pub fn failed(why: impl Into<String>) -> StateJob<T> {
        StateJob { inner: Inner::Done(Some(Err(why.into()))) }
    }

    /// A job finished elsewhere: `poll` is asked once per [`StateJob::poll`] until it stops
    /// waiting.
    #[must_use]
    pub fn remote(poll: impl FnMut() -> JobPoll<T> + Send + 'static) -> StateJob<T> {
        StateJob { inner: Inner::Remote(Box::new(poll)) }
    }

    /// Non-blocking.
    pub fn poll(&mut self) -> JobPoll<T> {
        match &mut self.inner {
            Inner::Done(slot) => match slot.take() {
                Some(Ok(v)) => JobPoll::Ready(v),
                Some(Err(e)) => JobPoll::Failed(e),
                None => JobPoll::Failed("already taken".into()),
            },
            Inner::Remote(f) => {
                let r = f();
                if !matches!(r, JobPoll::Waiting) {
                    self.inner = Inner::Done(None);
                }
                r
            }
        }
    }
}

impl<T> std::fmt::Debug for StateJob<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.inner {
            Inner::Done(Some(_)) => "StateJob(done)",
            Inner::Done(None) => "StateJob(taken)",
            Inner::Remote(_) => "StateJob(remote)",
        })
    }
}

/// The parameter values a state load left the plug-in with: `(param id, value)` in SoundCraft
/// units (ids and ranges of [`crate::PluginInfo::params`]).
pub type LoadedValues = Vec<(String, f32)>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ready_job_answers_once() {
        let mut j = StateJob::ready(5);
        assert_eq!(j.poll(), JobPoll::Ready(5));
        assert!(matches!(j.poll(), JobPoll::Failed(_)));
        let mut f: StateJob<u8> = StateJob::failed("busy");
        assert_eq!(f.poll(), JobPoll::Failed("busy".into()));
    }

    #[test]
    fn a_remote_job_is_polled_until_it_stops_waiting() {
        let mut n = 0;
        let mut j = StateJob::remote(move || {
            n += 1;
            if n < 3 { JobPoll::Waiting } else { JobPoll::Ready(n) }
        });
        assert_eq!(j.poll(), JobPoll::Waiting);
        assert_eq!(j.poll(), JobPoll::Waiting);
        assert_eq!(j.poll(), JobPoll::Ready(3));
        assert!(matches!(j.poll(), JobPoll::Failed(_)), "the poller is not asked again");
    }
}
