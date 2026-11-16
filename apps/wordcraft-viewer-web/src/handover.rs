//! The page's one state machine: viewer, loading the editor, editor, or the viewer left in
//! place because the editor cannot start. Kept free of the DOM: events come in, the actions
//! for the page come out, and the host tests walk every path.

use crate::mode::Mode;

/// How long the page waits, from the edit action, for the editor to publish its page API
/// before it gives up and says so. The editor is the largest download of the app (about 5 MB
/// over Brotli), so the default leaves a slow connection time to finish; a host can set its own
/// (`editorWaitMs`), within [`EDITOR_WAIT_MS_MIN`]..=[`EDITOR_WAIT_MS_MAX`].
pub const EDITOR_WAIT_MS: u32 = 120_000;
pub const EDITOR_WAIT_MS_MIN: u32 = 1_000;
pub const EDITOR_WAIT_MS_MAX: u32 = 600_000;

/// The wait to use for a host's `editorWaitMs` (a JavaScript number, or a `data-` attribute
/// read as one): the default when absent or not a finite number, otherwise clamped.
pub fn editor_wait_ms(given: Option<f64>) -> u32 {
    match given {
        Some(ms) if ms.is_finite() => {
            // Clamped as a float first, so the cast never saturates or wraps.
            ms.round().clamp(f64::from(EDITOR_WAIT_MS_MIN), f64::from(EDITOR_WAIT_MS_MAX)) as u32
        }
        _ => EDITOR_WAIT_MS,
    }
}

/// Where the page is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The viewer shows the document. The editor has not been asked for.
    Viewing,
    /// The editor's files are being fetched and started. The viewer, if there is one, stays
    /// visible with a small "loading the editor" indicator.
    LoadingEditor,
    /// The editor is up and is being given the document; the viewer stays until the editor has
    /// painted it.
    HandingOver,
    /// The editor owns the page. The viewer is gone.
    Editing,
    /// The editor could not take over. The viewer stays, with a notice. Final.
    ViewerOnly(Reason),
}

/// Why the editor did not take over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The browser has no WebGL, which the editor draws with. Nothing was fetched.
    NoWebGl,
    /// The editor's files could not be fetched.
    LoadFailed,
    /// The editor was fetched and failed to start.
    StartFailed,
    /// The editor started and did not accept the document.
    HandOverFailed,
    /// The editor did not publish its page API within the wait ([`editor_wait_ms`]).
    TimedOut,
}

/// What happened on the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// An edit action: the Edit control, a double click on a page, or Enter or F2 with the
    /// viewer focused.
    EditRequested,
    /// The editor's page API is on the page.
    EditorReady,
    /// The editor took the document (or there was none to give it).
    DocumentAccepted,
    /// The editor has had the frames to paint it.
    EditorPainted,
    /// The editor's script element reported an error (a 404, a network failure, a refused
    /// integrity check).
    ScriptFailed,
    /// The editor reported that it failed to start.
    StartFailed,
    /// The editor refused the document.
    HandOverFailed,
    /// The wait for the editor's page API ran out.
    EditorTimedOut,
    /// The viewer asks for the open document's password. Until a document opens, no edit
    /// action is taken: the editor would be handed a document it cannot read.
    PasswordAsked,
    /// The viewer opened a document (the password, if one was asked, was right).
    DocumentOpened,
}

/// What the page does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Start the viewer worker and show the document.
    StartViewer,
    /// Show the "loading the editor" indicator.
    ShowLoading,
    /// Add the editor's canvas and its script element.
    LoadEditor,
    /// Give the editor the document's bytes.
    HandOver,
    /// Wait for the editor to paint, then report [`Event::EditorPainted`].
    AwaitPaint,
    /// Remove the indicator.
    HideLoading,
    /// Remove the viewer and stop its worker.
    RemoveViewer,
    /// Tell the reader the editor is unavailable, and why.
    ShowNotice(Reason),
    /// Take the notice down: the editor arrived after all.
    HideNotice,
}

/// The machine. [`Handover::start`] gives the first actions; [`Handover::on`] the rest.
#[derive(Debug)]
pub struct Handover {
    mode: Mode,
    /// Whether this browser can run the editor at all (it has WebGL).
    can_edit: bool,
    phase: Phase,
    viewer_started: bool,
    /// In [`Phase::HandingOver`]: the editor has the document and only painting is awaited.
    accepted: bool,
    /// The editor runs for a host bridge ([`Handover::hosted`]): the viewer is never started.
    hosted: bool,
    /// The viewer has no open document to hand over: it is still fetching or opening one, or
    /// asks for its password. No edit action is taken until one opens ([`Event::DocumentOpened`]):
    /// the editor would be handed nothing, or bytes it cannot read.
    locked: bool,
    /// The editor arrived late ([`Reason::TimedOut`]) while the viewer had no open document: it
    /// takes over once one opens.
    editor_waiting: bool,
}

impl Handover {
    /// The machine for a page in `mode`, and what the page does first.
    pub fn start(mode: Mode, can_edit: bool) -> (Handover, Vec<Action>) {
        let mut machine = Handover {
            mode,
            can_edit,
            phase: Phase::Viewing,
            viewer_started: false,
            accepted: false,
            hosted: false,
            locked: false,
            editor_waiting: false,
        };
        let actions = match (mode, can_edit) {
            (Mode::EditorFirst, true) => {
                machine.phase = Phase::LoadingEditor;
                vec![Action::ShowLoading, Action::LoadEditor]
            }
            // Where the editor used to print that it failed to start, the viewer is mounted.
            (Mode::EditorFirst, false) => {
                machine.viewer_started = true;
                machine.locked = true;
                machine.phase = Phase::ViewerOnly(Reason::NoWebGl);
                vec![Action::StartViewer, Action::ShowNotice(Reason::NoWebGl)]
            }
            (Mode::ViewOnly | Mode::ViewerFirst, _) => {
                machine.viewer_started = true;
                machine.locked = true;
                vec![Action::StartViewer]
            }
        };
        (machine, actions)
    }

    /// The machine for an editor-first page that a host drives through the editor's own bridge
    /// (`?host=`): the page is the editor's, as it was before the viewer existed. The editor is
    /// fetched without a WebGL probe, because the editor itself tells the host when it cannot
    /// start (`wordcraft:failed`), and a failure leaves the notice but never starts the viewer:
    /// the viewer would open the sample, and a hosted page opens only what the host sends.
    pub fn hosted() -> (Handover, Vec<Action>) {
        let machine = Handover {
            mode: Mode::EditorFirst,
            can_edit: true,
            phase: Phase::LoadingEditor,
            viewer_started: false,
            accepted: false,
            hosted: true,
            locked: false,
            editor_waiting: false,
        };
        (machine, vec![Action::ShowLoading, Action::LoadEditor])
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Whether the page offers an edit action now.
    pub fn edit_offered(&self) -> bool {
        self.mode != Mode::ViewOnly && self.phase == Phase::Viewing && !self.locked
    }

    /// Take `event`; the actions it calls for, in order. An event that makes no sense in the
    /// current phase (late, repeated, out of order) changes nothing.
    pub fn on(&mut self, event: Event) -> Vec<Action> {
        match (self.phase, event) {
            (_, Event::PasswordAsked) => {
                self.locked = true;
                Vec::new()
            }
            (_, Event::DocumentOpened) => {
                self.locked = false;
                if self.editor_waiting && self.phase == Phase::ViewerOnly(Reason::TimedOut) {
                    return self.take_over_late();
                }
                Vec::new()
            }
            (Phase::Viewing, Event::EditRequested) if self.mode == Mode::ViewOnly || self.locked => Vec::new(),
            (Phase::Viewing, Event::EditRequested) if !self.can_edit => {
                self.phase = Phase::ViewerOnly(Reason::NoWebGl);
                vec![Action::ShowNotice(Reason::NoWebGl)]
            }
            (Phase::Viewing, Event::EditRequested) => {
                self.phase = Phase::LoadingEditor;
                vec![Action::ShowLoading, Action::LoadEditor]
            }
            (Phase::LoadingEditor, Event::EditorReady) => {
                self.phase = Phase::HandingOver;
                self.accepted = false;
                vec![Action::HandOver]
            }
            // The wait ran out but the download did not fail: the editor still starts when it
            // arrives, and once its files are fetched nothing can stop it. So it takes over, as
            // it would have on time, instead of running unseen under the viewer: once the viewer
            // has a document to give it.
            (Phase::ViewerOnly(Reason::TimedOut), Event::EditorReady) if self.locked => {
                self.editor_waiting = true;
                Vec::new()
            }
            (Phase::ViewerOnly(Reason::TimedOut), Event::EditorReady) => self.take_over_late(),
            (Phase::HandingOver, Event::DocumentAccepted) if !self.accepted => {
                self.accepted = true;
                vec![Action::AwaitPaint]
            }
            (Phase::HandingOver, Event::EditorPainted) if self.accepted => {
                self.phase = Phase::Editing;
                vec![Action::HideLoading, Action::RemoveViewer]
            }
            (Phase::LoadingEditor, Event::ScriptFailed) => self.fail(Reason::LoadFailed),
            (Phase::LoadingEditor, Event::EditorTimedOut) => self.fail(Reason::TimedOut),
            (Phase::LoadingEditor | Phase::HandingOver, Event::StartFailed) => self.fail(Reason::StartFailed),
            (Phase::HandingOver, Event::HandOverFailed) if !self.accepted => self.fail(Reason::HandOverFailed),
            _ => Vec::new(),
        }
    }

    fn take_over_late(&mut self) -> Vec<Action> {
        self.phase = Phase::HandingOver;
        self.accepted = false;
        self.editor_waiting = false;
        vec![Action::HideNotice, Action::ShowLoading, Action::HandOver]
    }

    /// The editor did not take over: keep (or start) the viewer and say why.
    fn fail(&mut self, reason: Reason) -> Vec<Action> {
        self.phase = Phase::ViewerOnly(reason);
        let mut actions = vec![Action::HideLoading];
        if !self.viewer_started && !self.hosted {
            self.viewer_started = true;
            self.locked = true;
            actions.push(Action::StartViewer);
        }
        actions.push(Action::ShowNotice(reason));
        actions
    }
}

#[cfg(test)]
mod tests;
