use super::*;

fn key(page: u32, scale: f32) -> PageKey {
    PageKey::new(page, scale)
}

const MB: u64 = 1 << 20;

#[test]
fn a_key_is_the_page_and_the_exact_scale() {
    assert_eq!(key(3, 1.5), key(3, 1.5));
    assert_ne!(key(3, 1.5), key(4, 1.5));
    assert_ne!(key(3, 1.5), key(3, 1.5000001));
    assert_eq!(key(3, 1.3333334).scale(), 1.3333334);
    assert_eq!(key(7, 2.0).page, 7);
}

#[test]
fn a_visible_page_is_asked_for_once() {
    let mut cache = PageCache::new(64 * MB);
    assert_eq!(cache.plan(&[key(0, 1.0), key(1, 1.0)]), vec![key(0, 1.0), key(1, 1.0)]);
    // Still in flight: scrolling a little does not ask again.
    assert_eq!(cache.plan(&[key(0, 1.0), key(1, 1.0)]), vec![]);
    assert_eq!(cache.arrived(key(0, 1.0), 4 * MB, &[0, 1]), Arrival::Paint { evict: vec![] });
    assert_eq!(cache.plan(&[key(0, 1.0), key(1, 1.0)]), vec![], "painted and in flight");
    assert_eq!(cache.painted(0), Some(key(0, 1.0)));
    assert_eq!(cache.painted(1), None);
}

#[test]
fn no_more_than_the_in_flight_limit_is_asked_at_once() {
    let mut cache = PageCache::new(64 * MB);
    let wanted: Vec<PageKey> = (0..6).map(|p| key(p, 1.0)).collect();
    let first = cache.plan(&wanted);
    assert_eq!(first.len(), MAX_IN_FLIGHT);
    assert_eq!(first, wanted[..MAX_IN_FLIGHT]);
    assert_eq!(cache.plan(&wanted), vec![], "the rest wait for a reply");
    assert!(matches!(cache.arrived(wanted[0], MB, &[0, 1, 2, 3, 4, 5]), Arrival::Paint { .. }));
    assert_eq!(cache.plan(&wanted), vec![wanted[MAX_IN_FLIGHT]], "one reply frees one slot, in the order asked");
}

#[test]
fn a_new_scale_waits_for_the_page_in_flight_then_replaces_it() {
    let mut cache = PageCache::new(64 * MB);
    assert_eq!(cache.plan(&[key(0, 1.0)]), vec![key(0, 1.0)]);
    // The window was resized before the page arrived: the worker renders the old request
    // anyway, so it is awaited and painted, not asked for twice.
    assert_eq!(cache.plan(&[key(0, 1.25)]), vec![]);
    assert_eq!(cache.arrived(key(0, 1.0), 4 * MB, &[0]), Arrival::Paint { evict: vec![] });
    assert_eq!(cache.painted(0), Some(key(0, 1.0)));
    assert_eq!(cache.plan(&[key(0, 1.25)]), vec![key(0, 1.25)]);
    assert_eq!(cache.arrived(key(0, 1.25), 5 * MB, &[0]), Arrival::Paint { evict: vec![] });
    assert_eq!(cache.painted(0), Some(key(0, 1.25)));
    // A page painted at an old scale stays up until the new one arrives, then is replaced.
    assert_eq!(cache.plan(&[key(0, 2.0)]), vec![key(0, 2.0)]);
    assert_eq!(cache.painted(0), Some(key(0, 1.25)));
    assert_eq!(cache.arrived(key(0, 2.0), 9 * MB, &[0]), Arrival::Paint { evict: vec![] });
    assert_eq!(cache.used_bytes(), 9 * MB, "the old pixels are not counted twice");
}

#[test]
fn a_reply_nobody_asked_for_is_stale() {
    let mut cache = PageCache::new(64 * MB);
    assert_eq!(cache.arrived(key(5, 1.0), MB, &[]), Arrival::Stale);
    assert_eq!(cache.used_bytes(), 0);
}

#[test]
fn pages_out_of_view_are_dropped_least_recently_shown_first() {
    let mut cache = PageCache::new(10 * MB);
    for page in 0..3 {
        cache.plan(&[key(page, 1.0)]);
        assert_eq!(
            cache.arrived(key(page, 1.0), 4 * MB, &[page]),
            if page < 2 { Arrival::Paint { evict: vec![] } } else { Arrival::Paint { evict: vec![0] } }
        );
    }
    assert_eq!(cache.used_bytes(), 8 * MB);
    // Page 1 is looked at again, so page 2 is now the older of the two out of view.
    assert_eq!(cache.plan(&[key(1, 1.0)]), vec![]);
    cache.plan(&[key(3, 1.0)]);
    assert_eq!(cache.arrived(key(3, 1.0), 4 * MB, &[3]), Arrival::Paint { evict: vec![2] });
    assert_eq!(cache.painted(1), Some(key(1, 1.0)));
}

#[test]
fn visible_pages_are_kept_even_over_the_budget() {
    let mut cache = PageCache::new(5 * MB);
    cache.plan(&[key(0, 1.0), key(1, 1.0)]);
    assert_eq!(cache.arrived(key(0, 1.0), 4 * MB, &[0, 1]), Arrival::Paint { evict: vec![] });
    assert_eq!(cache.arrived(key(1, 1.0), 4 * MB, &[0, 1]), Arrival::Paint { evict: vec![] });
    assert_eq!(cache.used_bytes(), 8 * MB, "what is on screen is never dropped to meet the budget");
}

#[test]
fn a_page_that_failed_is_not_asked_for_in_a_loop() {
    let mut cache = PageCache::new(64 * MB);
    assert_eq!(cache.plan(&[key(0, 1.0)]), vec![key(0, 1.0)]);
    cache.failed(key(0, 1.0));
    assert_eq!(cache.plan(&[key(0, 1.0)]), vec![], "the same request would fail the same way");
    assert_eq!(cache.plan(&[key(0, 0.5)]), vec![key(0, 0.5)], "another scale may fit");
    // A failure nobody is waiting for changes nothing.
    cache.failed(key(9, 1.0));
    assert_eq!(cache.plan(&[key(9, 1.0)]), vec![key(9, 1.0)]);
}

#[test]
fn clearing_forgets_everything_and_names_what_was_painted() {
    let mut cache = PageCache::new(64 * MB);
    cache.plan(&[key(0, 1.0), key(1, 1.0)]);
    cache.arrived(key(1, 1.0), MB, &[0, 1]);
    cache.failed(key(0, 1.0));
    assert_eq!(cache.clear(), vec![1]);
    assert_eq!(cache.used_bytes(), 0);
    // The replies of the old document are stale; the new one is asked for from scratch.
    assert_eq!(cache.plan(&[key(0, 1.0)]), vec![key(0, 1.0)]);
}

#[test]
fn hostile_sizes_do_not_overflow() {
    let mut cache = PageCache::new(u64::MAX);
    cache.plan(&[key(0, 1.0), key(1, 1.0)]);
    cache.arrived(key(0, 1.0), u64::MAX, &[0]);
    cache.arrived(key(1, 1.0), u64::MAX, &[1]);
    assert_eq!(cache.used_bytes(), u64::MAX);
}

#[test]
fn a_resize_storm_never_asks_more_than_the_in_flight_limit() {
    let mut cache = PageCache::new(64 * MB);
    let mut asked = cache.plan(&[key(0, 1.0), key(1, 1.0)]).len();
    // Every resize lands on a new scale before the worker answers: each request asked is one
    // the serial worker renders, answered or not, so none may pile up behind the two.
    for step in 1..100u8 {
        let scale = 1.0 + f32::from(step) / 100.0;
        asked += cache.plan(&[key(0, scale), key(1, scale)]).len();
    }
    assert_eq!(asked, MAX_IN_FLIGHT);
    // The old scale's answer is painted (stretched until replaced) and frees its slot.
    assert_eq!(cache.arrived(key(0, 1.0), MB, &[0, 1]), Arrival::Paint { evict: vec![] });
    assert_eq!(cache.plan(&[key(0, 1.99), key(1, 1.99)]), vec![key(0, 1.99)]);
}

#[test]
fn a_page_whose_canvas_went_is_asked_for_again() {
    let mut cache = PageCache::new(64 * MB);
    assert_eq!(cache.plan(&[key(0, 1.0)]), vec![key(0, 1.0)]);
    // Scrolled away before the answer: its canvas went, so nothing would hold the pixels.
    cache.forget(0);
    assert_eq!(cache.arrived(key(0, 1.0), 4 * MB, &[]), Arrival::Stale);
    assert_eq!(cache.painted(0), None);
    assert_eq!(cache.used_bytes(), 0);
    // Scrolled back: asked for again, not taken for painted.
    assert_eq!(cache.plan(&[key(0, 1.0)]), vec![key(0, 1.0)]);
}
