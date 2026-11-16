//! What one conversion keeps while it runs.
//!
//! `Convert::convert` takes nothing but the value, so the two things a whole-document conversion
//! has to remember live here, per thread: whether a number that is not finite went by, and which
//! graphics have been converted already.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

use craft_convert::Convert;
use craft_wordmodel as wm;
use wordcraft_doc as doc;

use crate::{Bridge, Error};

thread_local! {
    static NOT_FINITE: Cell<bool> = const { Cell::new(false) };
    /// The shared model's graphics already converted, by the address of the source. `None`
    /// outside a document conversion: an address means nothing once its document is gone.
    static GRAPHICS: RefCell<Option<HashMap<usize, Arc<doc::graphic::Graphic>>>> = const { RefCell::new(None) };
}

/// Record that a number that is not finite was converted.
pub(crate) fn not_finite() {
    let _ = NOT_FINITE.try_with(|seen| seen.set(true));
}

/// Run a conversion and refuse its result if a number that is not finite went through
/// [`Bridge`]'s `Floats` meanwhile.
pub(crate) fn checked<T>(convert: impl FnOnce() -> T) -> Result<T, Error> {
    let before = NOT_FINITE.try_with(|seen| seen.replace(false)).unwrap_or(false);
    let value = convert();
    let seen = NOT_FINITE.try_with(|seen| seen.replace(before)).unwrap_or(false);
    if seen { Err(Error::NotFinite) } else { Ok(value) }
}

/// Keeps one converted copy of each graphic while it lives.
pub(crate) struct SharedGraphics;

impl SharedGraphics {
    pub(crate) fn start() -> Self {
        set(Some(HashMap::new()));
        SharedGraphics
    }
}

impl Drop for SharedGraphics {
    fn drop(&mut self) {
        set(None);
    }
}

fn set(value: Option<HashMap<usize, Arc<doc::graphic::Graphic>>>) {
    let _ = GRAPHICS.try_with(|graphics| {
        if let Ok(mut graphics) = graphics.try_borrow_mut() {
            *graphics = value;
        }
    });
}

/// A graphic of the shared model as WordCraft's. Inside a document conversion, a graphic that
/// several objects point to is converted once and they all point to the one result, as they did
/// in the source.
pub(crate) fn graphic(source: Arc<wm::graphic::Graphic>) -> Arc<doc::graphic::Graphic> {
    // Every source graphic is alive until its own turn here, so no two share an address.
    let key = Arc::as_ptr(&source) as usize;
    let known = GRAPHICS.try_with(|graphics| graphics.try_borrow().ok().and_then(|graphics| graphics.as_ref()?.get(&key).cloned())).ok().flatten();
    if let Some(known) = known {
        return known;
    }
    let converted: Arc<doc::graphic::Graphic> = Convert::<_, Bridge>::convert(source);
    let _ = GRAPHICS.try_with(|graphics| {
        if let Ok(mut graphics) = graphics.try_borrow_mut()
            && let Some(graphics) = graphics.as_mut()
        {
            graphics.insert(key, Arc::clone(&converted));
        }
    });
    converted
}
