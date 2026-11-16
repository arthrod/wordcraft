use super::*;
use crate::mode::Mode;

fn started(mode: Mode, can_edit: bool) -> (Handover, Vec<Action>) {
    Handover::start(mode, can_edit)
}

/// Started, and the viewer has opened its document.
fn opened(mode: Mode, can_edit: bool) -> (Handover, Vec<Action>) {
    let (mut h, actions) = Handover::start(mode, can_edit);
    assert_eq!(h.on(Event::DocumentOpened), Vec::new());
    (h, actions)
}

#[test]
fn viewer_first_starts_with_the_viewer_and_offers_edit() {
    let (h, actions) = opened(Mode::ViewerFirst, true);
    assert_eq!(actions, vec![Action::StartViewer]);
    assert_eq!(h.phase(), Phase::Viewing);
    assert!(h.edit_offered());
}

#[test]
fn the_happy_path_from_viewer_to_editor() {
    let (mut h, _) = opened(Mode::ViewerFirst, true);
    assert_eq!(h.on(Event::EditRequested), vec![Action::ShowLoading, Action::LoadEditor]);
    assert_eq!(h.phase(), Phase::LoadingEditor);
    assert!(!h.edit_offered(), "one request is enough");
    assert_eq!(h.on(Event::EditorReady), vec![Action::HandOver]);
    assert_eq!(h.phase(), Phase::HandingOver);
    assert_eq!(h.on(Event::DocumentAccepted), vec![Action::AwaitPaint]);
    assert_eq!(h.phase(), Phase::HandingOver, "the viewer stays until the editor has painted");
    assert_eq!(h.on(Event::EditorPainted), vec![Action::HideLoading, Action::RemoveViewer]);
    assert_eq!(h.phase(), Phase::Editing);
}

#[test]
fn a_second_edit_request_loads_nothing_twice() {
    let (mut h, _) = opened(Mode::ViewerFirst, true);
    h.on(Event::EditRequested);
    for _ in 0..3 {
        assert_eq!(h.on(Event::EditRequested), vec![]);
    }
    h.on(Event::EditorReady);
    assert_eq!(h.on(Event::EditRequested), vec![]);
    h.on(Event::DocumentAccepted);
    h.on(Event::EditorPainted);
    assert_eq!(h.on(Event::EditRequested), vec![]);
    assert_eq!(h.phase(), Phase::Editing);
}

#[test]
fn view_only_never_loads_the_editor() {
    let (mut h, actions) = started(Mode::ViewOnly, true);
    assert_eq!(actions, vec![Action::StartViewer]);
    assert!(!h.edit_offered());
    for event in [Event::EditRequested, Event::EditorReady, Event::DocumentAccepted, Event::EditorPainted] {
        assert_eq!(h.on(event), vec![], "{event:?}");
        assert_eq!(h.phase(), Phase::Viewing);
    }
}

#[test]
fn without_webgl_the_viewer_stays_and_says_why_and_nothing_is_fetched() {
    let (mut h, _) = opened(Mode::ViewerFirst, false);
    assert!(h.edit_offered(), "the control is there; using it explains");
    assert_eq!(h.on(Event::EditRequested), vec![Action::ShowNotice(Reason::NoWebGl)]);
    assert_eq!(h.phase(), Phase::ViewerOnly(Reason::NoWebGl));
    assert!(!h.edit_offered());
    assert_eq!(h.on(Event::EditRequested), vec![], "asked once, answered once");
}

#[test]
fn a_load_failure_keeps_the_viewer_with_a_notice() {
    for (failure, reason) in [(Event::ScriptFailed, Reason::LoadFailed), (Event::StartFailed, Reason::StartFailed)] {
        let (mut h, _) = opened(Mode::ViewerFirst, true);
        h.on(Event::EditRequested);
        assert_eq!(h.on(failure), vec![Action::HideLoading, Action::ShowNotice(reason)]);
        assert_eq!(h.phase(), Phase::ViewerOnly(reason));
        // Final: a late "ready" from a half-loaded editor must not remove the viewer.
        for late in [Event::EditorReady, Event::DocumentAccepted, Event::EditorPainted, Event::EditRequested, Event::ScriptFailed] {
            assert_eq!(h.on(late), vec![], "{late:?}");
            assert_eq!(h.phase(), Phase::ViewerOnly(reason));
        }
    }
}

#[test]
fn a_failed_hand_over_keeps_the_viewer() {
    let (mut h, _) = opened(Mode::ViewerFirst, true);
    h.on(Event::EditRequested);
    h.on(Event::EditorReady);
    assert_eq!(h.on(Event::HandOverFailed), vec![Action::HideLoading, Action::ShowNotice(Reason::HandOverFailed)]);
    assert_eq!(h.phase(), Phase::ViewerOnly(Reason::HandOverFailed));
    // And a start failure reported after the editor announced itself still lands.
    let (mut h, _) = opened(Mode::ViewerFirst, true);
    h.on(Event::EditRequested);
    h.on(Event::EditorReady);
    assert_eq!(h.on(Event::StartFailed), vec![Action::HideLoading, Action::ShowNotice(Reason::StartFailed)]);
}

#[test]
fn editor_first_loads_the_editor_at_once_and_no_viewer() {
    let (mut h, actions) = started(Mode::EditorFirst, true);
    assert_eq!(actions, vec![Action::ShowLoading, Action::LoadEditor]);
    assert_eq!(h.phase(), Phase::LoadingEditor);
    assert_eq!(h.on(Event::EditorReady), vec![Action::HandOver]);
    assert_eq!(h.on(Event::DocumentAccepted), vec![Action::AwaitPaint]);
    assert_eq!(h.on(Event::EditorPainted), vec![Action::HideLoading, Action::RemoveViewer]);
    assert_eq!(h.phase(), Phase::Editing);
}

#[test]
fn editor_first_falls_back_to_the_viewer_when_the_editor_cannot_start() {
    // No WebGL: the viewer is mounted where the failure text used to be.
    let (h, actions) = started(Mode::EditorFirst, false);
    assert_eq!(actions, vec![Action::StartViewer, Action::ShowNotice(Reason::NoWebGl)]);
    assert_eq!(h.phase(), Phase::ViewerOnly(Reason::NoWebGl));
    // The files are missing: the viewer starts then, once.
    let (mut h, _) = started(Mode::EditorFirst, true);
    assert_eq!(h.on(Event::ScriptFailed), vec![Action::HideLoading, Action::StartViewer, Action::ShowNotice(Reason::LoadFailed)]);
    assert_eq!(h.on(Event::StartFailed), vec![]);
}

#[test]
fn events_out_of_order_are_ignored() {
    let (mut h, _) = opened(Mode::ViewerFirst, true);
    for stray in [Event::EditorReady, Event::DocumentAccepted, Event::EditorPainted, Event::ScriptFailed, Event::StartFailed, Event::HandOverFailed] {
        assert_eq!(h.on(stray), vec![], "{stray:?} before any edit request");
        assert_eq!(h.phase(), Phase::Viewing);
    }
    h.on(Event::EditRequested);
    for stray in [Event::DocumentAccepted, Event::EditorPainted, Event::HandOverFailed] {
        assert_eq!(h.on(stray), vec![], "{stray:?} before the editor is ready");
        assert_eq!(h.phase(), Phase::LoadingEditor);
    }
    h.on(Event::EditorReady);
    assert_eq!(h.on(Event::EditorPainted), vec![], "painted before the document was accepted");
    assert_eq!(h.on(Event::EditorReady), vec![], "ready twice hands over once");
    assert_eq!(h.phase(), Phase::HandingOver);
}

#[test]
fn every_event_in_every_phase_leaves_a_valid_machine() {
    let events = [
        Event::EditRequested,
        Event::EditorReady,
        Event::DocumentAccepted,
        Event::EditorPainted,
        Event::ScriptFailed,
        Event::StartFailed,
        Event::HandOverFailed,
        Event::EditorTimedOut,
        Event::DocumentOpened,
    ];
    const STEPS: u32 = 5;
    for mode in [Mode::ViewOnly, Mode::ViewerFirst, Mode::EditorFirst] {
        for can_edit in [true, false] {
            // Every sequence of five events.
            for code in 0..events.len().pow(STEPS) {
                let (mut h, first) = started(mode, can_edit);
                // Whether the viewer, once started, has a document open: the editor is handed
                // the viewer's document only then.
                let mut viewer = first.contains(&Action::StartViewer);
                let mut open = false;
                let mut actions = first;
                let mut n = code;
                for _ in 0..STEPS {
                    let event = events[n % events.len()];
                    n /= events.len();
                    match event {
                        Event::DocumentOpened => open = true,
                        Event::PasswordAsked => open = false,
                        _ => {}
                    }
                    let step = h.on(event);
                    if step.contains(&Action::StartViewer) {
                        (viewer, open) = (true, false);
                    }
                    if step.contains(&Action::HandOver) {
                        assert!(!viewer || open, "{mode:?} can_edit={can_edit} {code}: handed over while the viewer had no open document");
                    }
                    actions.extend(step);
                }
                let count = |a: Action| actions.iter().filter(|x| **x == a).count();
                assert!(count(Action::LoadEditor) <= 1, "{mode:?} {code}: the editor is loaded at most once");
                assert!(count(Action::StartViewer) <= 1, "{mode:?} {code}: the viewer is started at most once");
                assert!(count(Action::RemoveViewer) <= 1);
                if mode == Mode::ViewOnly || !can_edit {
                    assert_eq!(count(Action::LoadEditor), 0, "{mode:?} can_edit={can_edit} {code}");
                }
                if count(Action::RemoveViewer) == 1 {
                    assert_eq!(h.phase(), Phase::Editing);
                }
                if matches!(h.phase(), Phase::ViewerOnly(_)) {
                    assert_eq!(count(Action::StartViewer), 1, "{mode:?} {code}: a page left with the viewer has one");
                }
            }
        }
    }
}

#[test]
fn an_editor_that_never_starts_times_out_into_the_viewer() {
    let (mut h, _) = opened(Mode::ViewerFirst, true);
    h.on(Event::EditRequested);
    assert_eq!(h.on(Event::EditorTimedOut), vec![Action::HideLoading, Action::ShowNotice(Reason::TimedOut)]);
    assert_eq!(h.phase(), Phase::ViewerOnly(Reason::TimedOut));
    // Another deadline, a late failure or an edit action changes nothing. (An editor that does
    // turn up afterwards takes over: `an_editor_that_arrives_after_the_wait_takes_over`.)
    for late in [Event::EditorTimedOut, Event::StartFailed, Event::EditRequested] {
        assert_eq!(h.on(late), Vec::new(), "{late:?}");
    }
    assert!(!h.edit_offered());
}

#[test]
fn editor_first_times_out_into_a_started_viewer() {
    let (mut h, _) = started(Mode::EditorFirst, true);
    assert_eq!(h.on(Event::EditorTimedOut), vec![Action::HideLoading, Action::StartViewer, Action::ShowNotice(Reason::TimedOut)]);
}

#[test]
fn the_deadline_only_applies_while_the_editor_loads() {
    let (mut h, _) = opened(Mode::ViewerFirst, true);
    assert_eq!(h.on(Event::EditorTimedOut), Vec::new(), "nothing is loading yet");
    assert_eq!(h.phase(), Phase::Viewing);
    h.on(Event::EditRequested);
    h.on(Event::EditorReady);
    assert_eq!(h.on(Event::EditorTimedOut), Vec::new(), "the editor is up: the hand-over has its own failures");
    assert_eq!(h.phase(), Phase::HandingOver);
}

#[test]
fn the_wait_is_bounded_whatever_the_host_passes() {
    assert_eq!(editor_wait_ms(None), EDITOR_WAIT_MS);
    for junk in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(editor_wait_ms(Some(junk)), EDITOR_WAIT_MS, "{junk}");
    }
    assert_eq!(editor_wait_ms(Some(-5.0)), EDITOR_WAIT_MS_MIN);
    assert_eq!(editor_wait_ms(Some(0.0)), EDITOR_WAIT_MS_MIN);
    assert_eq!(editor_wait_ms(Some(2_500.4)), 2_500);
    assert_eq!(editor_wait_ms(Some(1e300)), EDITOR_WAIT_MS_MAX);
}

#[test]
fn a_hosted_page_belongs_to_the_editor() {
    let (mut h, actions) = Handover::hosted();
    assert_eq!(actions, vec![Action::ShowLoading, Action::LoadEditor], "fetched at once, with no WebGL probe");
    assert_eq!(h.phase(), Phase::LoadingEditor);
    assert!(!h.edit_offered());
    assert_eq!(h.on(Event::EditorReady), vec![Action::HandOver]);
    assert_eq!(h.on(Event::DocumentAccepted), vec![Action::AwaitPaint]);
    assert_eq!(h.on(Event::EditorPainted), vec![Action::HideLoading, Action::RemoveViewer]);
    assert_eq!(h.phase(), Phase::Editing);
}

#[test]
fn a_hosted_page_that_fails_never_starts_the_viewer() {
    for (event, reason) in
        [(Event::ScriptFailed, Reason::LoadFailed), (Event::StartFailed, Reason::StartFailed), (Event::EditorTimedOut, Reason::TimedOut)]
    {
        let (mut h, _) = Handover::hosted();
        assert_eq!(h.on(event), vec![Action::HideLoading, Action::ShowNotice(reason)], "{event:?}");
        assert_eq!(h.phase(), Phase::ViewerOnly(reason));
        assert_eq!(h.on(Event::EditRequested), Vec::new());
    }
    let (mut h, _) = Handover::hosted();
    h.on(Event::EditorReady);
    assert_eq!(h.on(Event::HandOverFailed), vec![Action::HideLoading, Action::ShowNotice(Reason::HandOverFailed)]);
}

#[test]
fn no_edit_action_while_the_password_is_asked_for() {
    let (mut h, _) = opened(Mode::ViewerFirst, true);
    assert_eq!(h.on(Event::PasswordAsked), Vec::new());
    assert!(!h.edit_offered(), "the Edit control is withdrawn");
    assert_eq!(h.on(Event::EditRequested), Vec::new(), "the Edit control, a double click, Enter/F2 and edit() all land here");
    assert_eq!(h.phase(), Phase::Viewing);
    // A wrong password asks again: still locked.
    assert_eq!(h.on(Event::PasswordAsked), Vec::new());
    assert!(!h.edit_offered());
    assert_eq!(h.on(Event::DocumentOpened), Vec::new());
    assert!(h.edit_offered());
    assert_eq!(h.on(Event::EditRequested), vec![Action::ShowLoading, Action::LoadEditor]);
}

#[test]
fn a_document_opening_unlocks_whatever_was_asked_before() {
    let (mut h, _) = opened(Mode::ViewerFirst, true);
    h.on(Event::PasswordAsked);
    h.on(Event::DocumentOpened);
    h.on(Event::DocumentOpened);
    assert!(h.edit_offered());
    // view-only never offers it, locked or not.
    let (mut v, _) = started(Mode::ViewOnly, true);
    v.on(Event::DocumentOpened);
    assert!(!v.edit_offered());
}

#[test]
fn an_editor_that_arrives_after_the_wait_takes_over() {
    let (mut h, _) = opened(Mode::ViewerFirst, true);
    h.on(Event::EditRequested);
    h.on(Event::EditorTimedOut);
    assert_eq!(h.on(Event::EditorReady), vec![Action::HideNotice, Action::ShowLoading, Action::HandOver]);
    assert_eq!(h.phase(), Phase::HandingOver);
    assert_eq!(h.on(Event::DocumentAccepted), vec![Action::AwaitPaint]);
    assert_eq!(h.on(Event::EditorPainted), vec![Action::HideLoading, Action::RemoveViewer]);
    assert_eq!(h.phase(), Phase::Editing);
}

#[test]
fn a_late_editor_that_then_fails_says_so_again() {
    let (mut h, _) = opened(Mode::ViewerFirst, true);
    h.on(Event::EditRequested);
    h.on(Event::EditorTimedOut);
    h.on(Event::EditorReady);
    assert_eq!(h.on(Event::HandOverFailed), vec![Action::HideLoading, Action::ShowNotice(Reason::HandOverFailed)]);
    // Its start failing after the wait is the same notice the wait gave: nothing to redo.
    let (mut late, _) = opened(Mode::ViewerFirst, true);
    late.on(Event::EditRequested);
    late.on(Event::EditorTimedOut);
    assert_eq!(late.on(Event::StartFailed), Vec::new());
    assert_eq!(late.phase(), Phase::ViewerOnly(Reason::TimedOut));
}

#[test]
fn only_a_timed_out_load_is_taken_over_late() {
    // The other failures have no editor that could still arrive.
    for (setup, reason) in [(Event::ScriptFailed, Reason::LoadFailed), (Event::StartFailed, Reason::StartFailed)] {
        let (mut h, _) = opened(Mode::ViewerFirst, true);
        h.on(Event::EditRequested);
        h.on(setup);
        assert_eq!(h.on(Event::EditorReady), Vec::new(), "{reason:?}");
        assert_eq!(h.phase(), Phase::ViewerOnly(reason));
    }
    let (mut h, _) = opened(Mode::ViewerFirst, false);
    h.on(Event::EditRequested);
    assert_eq!(h.on(Event::EditorReady), Vec::new(), "no WebGL: nothing was fetched");
    // A hosted page's late editor takes over too.
    let (mut hosted, _) = Handover::hosted();
    hosted.on(Event::EditorTimedOut);
    assert_eq!(hosted.on(Event::EditorReady), vec![Action::HideNotice, Action::ShowLoading, Action::HandOver]);
}

#[test]
fn no_edit_action_until_the_viewer_has_opened_its_document() {
    // `?doc=/slow-document` still downloading: an edit action then would hand the editor
    // nothing, and the reader would be left with a blank editor.
    let (mut h, _) = started(Mode::ViewerFirst, true);
    assert!(!h.edit_offered(), "no Edit control before the document is open");
    assert_eq!(h.on(Event::EditRequested), Vec::new());
    assert_eq!(h.phase(), Phase::Viewing);
    assert_eq!(h.on(Event::DocumentOpened), Vec::new());
    assert!(h.edit_offered());
    assert_eq!(h.on(Event::EditRequested), vec![Action::ShowLoading, Action::LoadEditor]);
}

#[test]
fn an_edit_action_before_the_password_is_asked_hands_nothing_over() {
    // Edit pressed before the worker answers that the document needs a password: the editor
    // must never be handed the protected bytes without it.
    let (mut h, _) = started(Mode::ViewerFirst, true);
    let mut actions = h.on(Event::EditRequested);
    actions.extend(h.on(Event::PasswordAsked));
    actions.extend(h.on(Event::EditorReady));
    assert!(!actions.contains(&Action::HandOver), "{actions:?}");
}

#[test]
fn a_late_editor_waits_for_the_document_the_viewer_is_opening() {
    // Editor-first, the wait ran out: the viewer is started and is still fetching the document
    // when the editor turns up. It takes over once the document is open, not before.
    let (mut h, _) = started(Mode::EditorFirst, true);
    assert_eq!(h.on(Event::EditorTimedOut), vec![Action::HideLoading, Action::StartViewer, Action::ShowNotice(Reason::TimedOut)]);
    assert_eq!(h.on(Event::EditorReady), Vec::new());
    assert_eq!(h.phase(), Phase::ViewerOnly(Reason::TimedOut));
    assert_eq!(h.on(Event::PasswordAsked), Vec::new(), "a protected document: still waiting");
    assert_eq!(h.on(Event::DocumentOpened), vec![Action::HideNotice, Action::ShowLoading, Action::HandOver]);
    assert_eq!(h.phase(), Phase::HandingOver);
    assert_eq!(h.on(Event::DocumentOpened), Vec::new(), "handed over once");
}
