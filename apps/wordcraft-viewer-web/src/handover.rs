//! The page's one state machine: viewer, loading the editor, editor, or the viewer left in
//! place because the editor cannot start. Kept free of the DOM: events come in, the actions
//! for the page come out, and the host tests walk every path.

use crate::mode::Mode;

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
}

impl Handover {
    /// The machine for a page in `mode`, and what the page does first.
    pub fn start(mode: Mode, can_edit: bool) -> (Handover, Vec<Action>) {
        let mut machine = Handover { mode, can_edit, phase: Phase::Viewing, viewer_started: false, accepted: false };
        let actions = match (mode, can_edit) {
            (Mode::EditorFirst, true) => {
                machine.phase = Phase::LoadingEditor;
                vec![Action::ShowLoading, Action::LoadEditor]
            }
            // Where the editor used to print that it failed to start, the viewer is mounted.
            (Mode::EditorFirst, false) => {
                machine.viewer_started = true;
                machine.phase = Phase::ViewerOnly(Reason::NoWebGl);
                vec![Action::StartViewer, Action::ShowNotice(Reason::NoWebGl)]
            }
            (Mode::ViewOnly | Mode::ViewerFirst, _) => {
                machine.viewer_started = true;
                vec![Action::StartViewer]
            }
        };
        (machine, actions)
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Whether the page offers an edit action now.
    pub fn edit_offered(&self) -> bool {
        self.mode != Mode::ViewOnly && self.phase == Phase::Viewing
    }

    /// Take `event`; the actions it calls for, in order. An event that makes no sense in the
    /// current phase (late, repeated, out of order) changes nothing.
    pub fn on(&mut self, event: Event) -> Vec<Action> {
        match (self.phase, event) {
            (Phase::Viewing, Event::EditRequested) if self.mode == Mode::ViewOnly => Vec::new(),
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
            (Phase::HandingOver, Event::DocumentAccepted) if !self.accepted => {
                self.accepted = true;
                vec![Action::AwaitPaint]
            }
            (Phase::HandingOver, Event::EditorPainted) if self.accepted => {
                self.phase = Phase::Editing;
                vec![Action::HideLoading, Action::RemoveViewer]
            }
            (Phase::LoadingEditor, Event::ScriptFailed) => self.fail(Reason::LoadFailed),
            (Phase::LoadingEditor | Phase::HandingOver, Event::StartFailed) => self.fail(Reason::StartFailed),
            (Phase::HandingOver, Event::HandOverFailed) if !self.accepted => self.fail(Reason::HandOverFailed),
            _ => Vec::new(),
        }
    }

    /// The editor did not take over: keep (or start) the viewer and say why.
    fn fail(&mut self, reason: Reason) -> Vec<Action> {
        self.phase = Phase::ViewerOnly(reason);
        let mut actions = vec![Action::HideLoading];
        if !self.viewer_started {
            self.viewer_started = true;
            actions.push(Action::StartViewer);
        }
        actions.push(Action::ShowNotice(reason));
        actions
    }
}

#[cfg(test)]
mod tests;
