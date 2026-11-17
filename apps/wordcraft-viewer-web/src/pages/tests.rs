use super::*;
use wordcraft_worker_proto::ViewPageSize;

const LETTER: ViewPageSize = ViewPageSize { width: 612.0, height: 792.0 };
const A4_LANDSCAPE: ViewPageSize = ViewPageSize { width: 841.9, height: 595.3 };

fn letters(n: usize) -> Vec<ViewPageSize> {
    vec![LETTER; n]
}

#[test]
fn a_wide_window_shows_pages_at_their_natural_size_centred() {
    let sheet = lay_out(&letters(3), 1400.0);
    assert_eq!(sheet.zoom, CSS_PX_PER_POINT);
    assert_eq!(sheet.pages.len(), 3);
    let first = sheet.pages[0];
    assert_eq!((first.width, first.height), (816.0, 1056.0), "Letter at 96 px an inch");
    assert_eq!(first.left, (1400.0 - 816.0) / 2.0);
    assert_eq!(first.top, PAGE_GAP);
    assert_eq!(sheet.pages[1].top, PAGE_GAP + 1056.0 + PAGE_GAP);
    assert_eq!(sheet.height, 3.0 * (1056.0 + PAGE_GAP) + PAGE_GAP);
    assert_eq!(sheet.width, 1400.0);
}

#[test]
fn a_narrow_window_fits_the_widest_page() {
    let sheet = lay_out(&[LETTER, A4_LANDSCAPE], 400.0);
    let widest = sheet.pages[1];
    assert!((widest.width - (400.0 - 2.0 * SIDE_MARGIN)).abs() < 1e-6, "{}", widest.width);
    assert!((widest.left - SIDE_MARGIN).abs() < 1e-6);
    assert!(sheet.zoom < CSS_PX_PER_POINT);
    // One zoom for the whole document: the narrower page is smaller and centred.
    assert!((sheet.pages[0].width - 612.0 * sheet.zoom).abs() < 1e-6);
    assert!(sheet.pages[0].left > SIDE_MARGIN);
}

#[test]
fn hostile_sizes_and_widths_never_panic_or_produce_nonsense() {
    let junk = [
        ViewPageSize { width: f32::NAN, height: 10.0 },
        ViewPageSize { width: -4.0, height: f32::INFINITY },
        ViewPageSize { width: 0.0, height: 0.0 },
        ViewPageSize { width: 1e30, height: 1e30 },
    ];
    for width in [f64::NAN, -1.0, 0.0, 1e300, f64::INFINITY, 320.0] {
        let sheet = lay_out(&junk, width);
        assert_eq!(sheet.pages.len(), junk.len());
        assert!(sheet.zoom.is_finite() && sheet.zoom >= MIN_ZOOM, "{width}: zoom {}", sheet.zoom);
        assert!(sheet.height.is_finite() && sheet.width.is_finite());
        let mut last_bottom = 0.0;
        for page in &sheet.pages {
            assert!(page.top.is_finite() && page.left.is_finite() && page.width > 0.0 && page.height > 0.0, "{width}: {page:?}");
            assert!(page.top >= last_bottom, "pages never overlap");
            last_bottom = page.top + page.height;
        }
    }
    let empty = lay_out(&[], 800.0);
    assert!(empty.pages.is_empty());
    assert_eq!(visible(&empty, 0.0, 600.0, 0.0), 0..0);
}

#[test]
fn only_the_pages_in_the_window_are_visible() {
    let sheet = lay_out(&letters(10), 1000.0);
    let page_step = 1056.0 + PAGE_GAP;
    // At the top of a 700 px window: page 0 only.
    assert_eq!(visible(&sheet, 0.0, 700.0, 0.0), 0..1);
    // Scrolled so the window straddles pages 2 and 3.
    assert_eq!(visible(&sheet, 2.0 * page_step + 800.0, 700.0, 0.0), 2..4);
    // A window taller than a page shows three.
    assert_eq!(visible(&sheet, page_step - 100.0, 2200.0, 0.0), 0..3);
    // The end of the document.
    assert_eq!(visible(&sheet, sheet.height - 700.0, 700.0, 0.0), 9..10);
    // Scrolled past the end, or before the start: nothing, never an index out of range.
    assert_eq!(visible(&sheet, sheet.height + 5000.0, 700.0, 0.0), 10..10);
    assert_eq!(visible(&sheet, -5000.0, 700.0, 0.0), 0..0);
    // The gap between two pages touches neither.
    let gap_top = PAGE_GAP + 1056.0;
    assert_eq!(visible(&sheet, gap_top + 1.0, PAGE_GAP - 2.0, 0.0).len(), 0);
}

#[test]
fn overscan_reaches_the_neighbours() {
    let sheet = lay_out(&letters(10), 1000.0);
    let page_step = 1056.0 + PAGE_GAP;
    assert_eq!(visible(&sheet, 3.0 * page_step + PAGE_GAP + 10.0, 500.0, 0.0), 3..4);
    assert_eq!(visible(&sheet, 3.0 * page_step + PAGE_GAP + 10.0, 500.0, 800.0), 2..5);
}

#[test]
fn hostile_scroll_positions_are_ranges_inside_the_document() {
    let sheet = lay_out(&letters(4), 1000.0);
    for (top, height, overscan) in [
        (f64::NAN, 600.0, 0.0),
        (0.0, f64::NAN, 0.0),
        (0.0, 600.0, f64::NAN),
        (f64::INFINITY, 600.0, 0.0),
        (f64::NEG_INFINITY, f64::INFINITY, 0.0),
        (0.0, -600.0, -50.0),
        (1e300, 1e300, 1e300),
    ] {
        let range = visible(&sheet, top, height, overscan);
        assert!(range.start <= range.end && range.end <= 4, "{top} {height} {overscan}: {range:?}");
    }
}

#[test]
fn the_render_scale_is_zoom_times_device_pixel_ratio() {
    assert_eq!(render_scale(CSS_PX_PER_POINT, 1.0, &LETTER), Some((96.0f64 / 72.0) as f32));
    assert_eq!(render_scale(CSS_PX_PER_POINT, 2.0, &LETTER), Some((2.0 * 96.0f64 / 72.0) as f32));
    assert_eq!(render_scale(0.5, 1.5, &LETTER), Some(0.75));
}

#[test]
fn the_render_scale_stays_inside_the_protocol_and_the_pixel_budget() {
    use wordcraft_worker_proto::MAX_VIEW_SCALE;
    // A huge poster at a high zoom on a dense display: lowered until it fits the budget.
    let poster = ViewPageSize { width: 4000.0, height: 6000.0 };
    let scale = render_scale(CSS_PX_PER_POINT, 3.0, &poster).unwrap();
    let pixels = (f64::from(poster.width * scale).ceil() * f64::from(poster.height * scale).ceil()) as u64;
    assert!(pixels <= PAGE_PIXEL_BUDGET, "{pixels}");
    assert!(pixels > PAGE_PIXEL_BUDGET * 9 / 10, "and no lower than it has to be: {pixels}");
    // A tiny label never asks for more than the protocol's largest scale.
    let label = ViewPageSize { width: 20.0, height: 10.0 };
    assert_eq!(render_scale(40.0, 4.0, &label), Some(MAX_VIEW_SCALE));
    // Junk in, nothing out; and a junk ratio is read as 1.
    for junk in [ViewPageSize { width: f32::NAN, height: 1.0 }, ViewPageSize { width: 0.0, height: 5.0 }, ViewPageSize { width: 5.0, height: -1.0 }] {
        assert_eq!(render_scale(1.0, 1.0, &junk), None);
    }
    assert_eq!(render_scale(f64::NAN, 1.0, &LETTER), None);
    assert_eq!(render_scale(0.0, 1.0, &LETTER), None);
    for ratio in [f64::NAN, 0.0, -2.0, f64::INFINITY] {
        assert_eq!(render_scale(1.0, ratio, &LETTER), Some(1.0), "{ratio}");
    }
    assert_eq!(render_scale(1.0, 64.0, &LETTER), render_scale(1.0, MAX_DEVICE_PIXEL_RATIO, &LETTER), "a ratio past the cap is the cap");
}

#[test]
fn a_long_narrow_page_is_painted_no_taller_than_the_core_draws() {
    use wordcraft_worker_proto::MAX_VIEW_SIDE;
    // 100 by 10,000 points at zoom 1 on a 2x display: 200 by 20,000 pixels is only 4 million,
    // inside the pixel budget, but a side past what the core draws, so the worker refuses it
    // and the page would stay blank. The scale is lowered until the long side fits.
    let side = |points: f32, scale: f32| (points * scale).ceil();
    for (page, zoom, ratio) in [
        (ViewPageSize { width: 100.0, height: 10_000.0 }, 1.0, 2.0),
        (ViewPageSize { width: 10_000.0, height: 100.0 }, 1.0, 2.0),
        (ViewPageSize { width: 30.0, height: 99_999.0 }, CSS_PX_PER_POINT, 4.0),
    ] {
        let scale = render_scale(zoom, ratio, &page).unwrap();
        let (w, h) = (side(page.width, scale), side(page.height, scale));
        assert!(w <= MAX_VIEW_SIDE as f32 && h <= MAX_VIEW_SIDE as f32, "{page:?}: {w}x{h} at {scale}");
        assert!(w.max(h) > MAX_VIEW_SIDE as f32 * 0.99, "{page:?}: and no smaller than it has to be: {w}x{h}");
    }
    // A page that fits keeps its scale.
    assert_eq!(render_scale(1.0, 2.0, &ViewPageSize { width: 100.0, height: 7_999.0 }), Some(2.0));
}

#[test]
fn a_window_taller_than_the_document_mounts_a_bounded_run_of_pages() {
    // The most pages the protocol allows, each at the smallest zoom, in a window as tall as a
    // host page can make it: no more than the cap is mounted or asked for.
    let sheet = lay_out(&letters(wordcraft_worker_proto::MAX_VIEW_PAGES), 1.0);
    for (top, height) in [(0.0, 1e9), (1e6, 1e9), (f64::NAN, f64::INFINITY), (-1e9, 1e12)] {
        let w = wanted(&sheet, top, height);
        let order: Vec<usize> = w.order().collect();
        assert!(order.len() <= MAX_MOUNTED_PAGES, "{top} {height}: {} pages", order.len());
        assert!(w.near.len() <= MAX_MOUNTED_PAGES);
        assert!(order.iter().all(|i| w.contains(*i) && *i < sheet.pages.len()));
        assert_eq!(order.len(), w.near.len(), "each page once");
    }
}

#[test]
fn the_wanted_pages_are_the_window_first_then_around_it() {
    let sheet = lay_out(&letters(40), 1400.0);
    let (top, height) = (5.0 * (1056.0 + PAGE_GAP), 900.0);
    let w = wanted(&sheet, top, height);
    let seen = visible(&sheet, top, height, 0.0);
    let near = visible(&sheet, top, height, height / 2.0);
    let order: Vec<usize> = w.order().collect();
    // Under the cap, it is every page in or near the window, the window's first.
    let mut expected: Vec<usize> = seen.clone().collect();
    expected.extend(near.clone().filter(|i| !seen.contains(i)));
    assert_eq!(order, expected);
    for i in 0..40 {
        assert_eq!(w.contains(i), near.contains(&i), "{i}");
    }
}

#[test]
fn a_capped_run_keeps_the_window_and_shares_the_rest_above_and_below() {
    // Tiny pages: hundreds in the window and around it.
    let tiny = vec![ViewPageSize { width: 612.0, height: 1.0 }; 2000];
    let sheet = lay_out(&tiny, 1400.0);
    let row = sheet.pages[1].top - sheet.pages[0].top;
    let (top, height) = (1000.0 * row, 100.0 * row);
    let w = wanted(&sheet, top, height);
    let seen = visible(&sheet, top, height, 0.0);
    assert_eq!(w.seen, seen, "the window itself fits the cap");
    assert_eq!(w.near.len(), MAX_MOUNTED_PAGES);
    let (above, below) = (seen.start - w.near.start, w.near.end - seen.end);
    assert!(above.abs_diff(below) <= 1, "{above} above, {below} below");
}
