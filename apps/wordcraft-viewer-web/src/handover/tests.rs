use super::*;
use crate::mode::Mode;

fn started(mode: Mode, can_edit: bool) -> (Handover, Vec<Action>) {
    Handover::start(mode, can_edit)
}

#[test]
fn viewer_first_starts_with_the_viewer_and_offers_edit() {
    let (h, actions) = started(Mode::ViewerFirst, true);
    assert_eq!(actions, vec![Action::StartViewer]);
    assert_eq!(h.phase(), Phase::Viewing);
    assert!(h.edit_offered());
}

#[test]
fn the_happy_path_from_viewer_to_editor() {
    let (mut h, _) = started(Mode::ViewerFirst, true);
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
    let (mut h, _) = started(Mode::ViewerFirst, true);
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
    let (mut h, _) = started(Mode::ViewerFirst, false);
    assert!(h.edit_offered(), "the control is there; using it explains");
    assert_eq!(h.on(Event::EditRequested), vec![Action::ShowNotice(Reason::NoWebGl)]);
    assert_eq!(h.phase(), Phase::ViewerOnly(Reason::NoWebGl));
    assert!(!h.edit_offered());
    assert_eq!(h.on(Event::EditRequested), vec![], "asked once, answered once");
}

#[test]
fn a_load_failure_keeps_the_viewer_with_a_notice() {
    for (failure, reason) in [(Event::ScriptFailed, Reason::LoadFailed), (Event::StartFailed, Reason::StartFailed)] {
        let (mut h, _) = started(Mode::ViewerFirst, true);
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
    let (mut h, _) = started(Mode::ViewerFirst, true);
    h.on(Event::EditRequested);
    h.on(Event::EditorReady);
    assert_eq!(h.on(Event::HandOverFailed), vec![Action::HideLoading, Action::ShowNotice(Reason::HandOverFailed)]);
    assert_eq!(h.phase(), Phase::ViewerOnly(Reason::HandOverFailed));
    // And a start failure reported after the editor announced itself still lands.
    let (mut h, _) = started(Mode::ViewerFirst, true);
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
    let (mut h, _) = started(Mode::ViewerFirst, true);
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
    ];
    for mode in [Mode::ViewOnly, Mode::ViewerFirst, Mode::EditorFirst] {
        for can_edit in [true, false] {
            // Every sequence of four events.
            for code in 0..events.len().pow(4) {
                let (mut h, first) = started(mode, can_edit);
                let mut actions = first;
                let mut n = code;
                for _ in 0..4 {
                    actions.extend(h.on(events[n % events.len()]));
                    n /= events.len();
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
                    assert!(actions.iter().all(|a| !matches!(a, Action::ShowNotice(_))), "{mode:?} {code}: never a notice and a hand-over both");
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
    let (mut h, _) = started(Mode::ViewerFirst, true);
    h.on(Event::EditRequested);
    assert_eq!(h.on(Event::EditorTimedOut), vec![Action::HideLoading, Action::ShowNotice(Reason::TimedOut)]);
    assert_eq!(h.phase(), Phase::ViewerOnly(Reason::TimedOut));
    // An editor that turns up afterwards, or another deadline, changes nothing.
    for late in [Event::EditorReady, Event::EditorTimedOut, Event::StartFailed, Event::EditRequested] {
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
    let (mut h, _) = started(Mode::ViewerFirst, true);
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
    assert!(!editor_wait_over(0.0, 3_000));
    assert!(!editor_wait_over(2_999.0, 3_000));
    assert!(editor_wait_over(3_000.0, 3_000));
    assert!(!editor_wait_over(-1e9, 3_000), "a clock that jumps back does not end the wait");
    assert!(!editor_wait_over(f64::NAN, 3_000));
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
