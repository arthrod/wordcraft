//! Where pages sit in the scrolling sheet, which of them the window shows, and the scale each
//! is painted at. Plain arithmetic on numbers the browser reports, every one of them treated
//! as hostile: the DOM code around it only applies the answers.

use std::ops::Range;

use wordcraft_worker_proto::{MAX_VIEW_PIXELS, MAX_VIEW_SCALE, ViewPageSize};

/// CSS pixels in a point at 100% zoom (96 to the inch against 72).
pub const CSS_PX_PER_POINT: f64 = 96.0 / 72.0;
/// Space above the first page, between pages and below the last, in CSS pixels.
pub const PAGE_GAP: f64 = 16.0;
/// Least space left and right of the widest page, in CSS pixels.
pub const SIDE_MARGIN: f64 = 16.0;
/// The smallest zoom a narrow window shrinks the pages to.
pub const MIN_ZOOM: f64 = 0.1;
/// The largest device pixel ratio pages are painted for; a denser display gets this many
/// pixels and the browser scales them.
pub const MAX_DEVICE_PIXEL_RATIO: f64 = 4.0;
/// Most pixels asked for in one page: 16 million (64 MiB of RGBA), half of what the protocol
/// allows. A Letter page on a 3× display at 100% is 7.8 million.
pub const PAGE_PIXEL_BUDGET: u64 = 16_000_000;

const _: () = assert!(PAGE_PIXEL_BUDGET <= MAX_VIEW_PIXELS);

/// The size a page with an unusable size is drawn at: US Letter.
const FALLBACK_PAGE: (f64, f64) = (612.0, 792.0);
/// The largest side a page is laid out with, in points (about 35 m).
const MAX_PAGE_POINTS: f64 = 100_000.0;
/// The widest window the layout reckons with, in CSS pixels.
const MAX_CONTAINER_WIDTH: f64 = 10_000_000.0;
/// Scroll positions and window heights are clamped to this before any arithmetic.
const MAX_SCROLL: f64 = 1e15;

/// One page's box in the sheet, in CSS pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageBox {
    pub top: f64,
    pub left: f64,
    pub width: f64,
    pub height: f64,
}

/// Every page's box, and the size of the sheet that holds them.
#[derive(Clone, Debug, PartialEq)]
pub struct Sheet {
    /// CSS pixels per point: [`CSS_PX_PER_POINT`], or less when the window is narrower than
    /// the widest page.
    pub zoom: f64,
    pub width: f64,
    pub height: f64,
    pub pages: Vec<PageBox>,
}

/// A page size as usable numbers: a side that is not a positive finite number makes the page
/// Letter, and no side is past [`MAX_PAGE_POINTS`].
fn usable(size: &ViewPageSize) -> (f64, f64) {
    let ok = |v: f32| v.is_finite() && v > 0.0;
    if ok(size.width) && ok(size.height) {
        (f64::from(size.width).min(MAX_PAGE_POINTS), f64::from(size.height).min(MAX_PAGE_POINTS))
    } else {
        FALLBACK_PAGE
    }
}

/// Stack the pages top to bottom in a window `container_width` CSS pixels wide: at 100% when
/// the widest fits, shrunk to fit when it does not, each centred.
pub fn lay_out(sizes: &[ViewPageSize], container_width: f64) -> Sheet {
    let container = if container_width.is_finite() { container_width.clamp(1.0, MAX_CONTAINER_WIDTH) } else { 1.0 };
    let points: Vec<(f64, f64)> = sizes.iter().map(usable).collect();
    let widest = points.iter().map(|p| p.0).fold(0.0, f64::max);
    let fit = if widest > 0.0 { (container - 2.0 * SIDE_MARGIN) / widest } else { CSS_PX_PER_POINT };
    let zoom = fit.min(CSS_PX_PER_POINT).max(MIN_ZOOM);
    let width = container.max(widest * zoom + 2.0 * SIDE_MARGIN);
    let mut top = PAGE_GAP;
    let mut pages = Vec::with_capacity(points.len());
    for (w, h) in points {
        let (w, h) = (w * zoom, h * zoom);
        pages.push(PageBox { top, left: ((width - w) / 2.0).max(SIDE_MARGIN), width: w, height: h });
        top += h + PAGE_GAP;
    }
    Sheet { zoom, width, height: if pages.is_empty() { 0.0 } else { top }, pages }
}

/// The pages any part of which lies in the window: `viewport_height` CSS pixels of the sheet
/// starting at `scroll_top`, widened by `overscan` above and below. Always a range inside the
/// document, whatever the numbers.
pub fn visible(sheet: &Sheet, scroll_top: f64, viewport_height: f64, overscan: f64) -> Range<usize> {
    let finite = |v: f64, low: f64| if v.is_nan() { low.max(0.0) } else { v.clamp(low, MAX_SCROLL) };
    let top = finite(scroll_top, -MAX_SCROLL);
    let height = finite(viewport_height, 0.0);
    let overscan = finite(overscan, 0.0);
    let (low, high) = (top - overscan, top + height + overscan);
    // Pages are in order top to bottom, so both ends are found by bisection.
    let start = sheet.pages.partition_point(|p| p.top + p.height <= low);
    let end = sheet.pages.partition_point(|p| p.top < high);
    start..end.max(start)
}

/// The scale to paint a page at, in pixels per point: zoom times the device pixel ratio, so a
/// page pixel is a device pixel. Lowered to the protocol's largest scale and until the page
/// fits [`PAGE_PIXEL_BUDGET`]. `None` for a page or zoom that is not a positive finite number;
/// a ratio that is not one is read as 1.
pub fn render_scale(zoom: f64, device_pixel_ratio: f64, size: &ViewPageSize) -> Option<f32> {
    let ok = |v: f32| v.is_finite() && v > 0.0;
    if !(zoom.is_finite() && zoom > 0.0 && ok(size.width) && ok(size.height)) {
        return None;
    }
    let ratio = if device_pixel_ratio.is_finite() && device_pixel_ratio > 0.0 { device_pixel_ratio.min(MAX_DEVICE_PIXEL_RATIO) } else { 1.0 };
    let mut scale = ((zoom * ratio) as f32).min(MAX_VIEW_SCALE);
    // The raster the view-only core makes is `ceil(points * scale)` a side, in single
    // precision; the same arithmetic here, so the size checked is the size asked for.
    let pixels = |scale: f32| (f64::from((size.width * scale).ceil().max(1.0)) * f64::from((size.height * scale).ceil().max(1.0))) as u64;
    if pixels(scale) > PAGE_PIXEL_BUDGET {
        let area = f64::from(size.width) * f64::from(size.height);
        scale = scale.min((PAGE_PIXEL_BUDGET as f64 / area).sqrt() as f32);
        // Rounding each side up can still land a few pixels over: step down until it fits.
        for _ in 0..64 {
            if pixels(scale) <= PAGE_PIXEL_BUDGET {
                break;
            }
            scale *= 0.999;
        }
    }
    (scale.is_finite() && scale > 0.0 && pixels(scale) <= PAGE_PIXEL_BUDGET).then_some(scale)
}

#[cfg(test)]
mod tests;
