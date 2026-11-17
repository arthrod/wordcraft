//! What the JSON bridge did to numbers, kept so documents convert as they did.
//!
//! JSON has no NaN and no infinity: `serde_json` writes them as `null`. Read back, `null` is
//! "absent" for an optional number and an error for any other. The typed mapping reproduces
//! both: the first here, member by member; the second in [`crate::Bridge`]'s `Floats`.

use craft_convert::Floats;

use crate::Bridge;

/// An optional number, or a container of them, with the values that are not finite dropped.
pub(crate) trait Optional {
    type Out;
    fn opt(self) -> Self::Out;
}

/// `value` with numbers that are not finite turned into `None`.
pub(crate) fn opt<T: Optional>(value: T) -> T::Out {
    value.opt()
}

impl Optional for Option<f32> {
    type Out = Option<f32>;
    fn opt(self) -> Option<f32> {
        self.filter(|v| v.is_finite())
    }
}

impl Optional for Option<f64> {
    type Out = Option<f64>;
    fn opt(self) -> Option<f64> {
        self.filter(|v| v.is_finite())
    }
}

impl Optional for &Option<f32> {
    type Out = Option<f32>;
    fn opt(self) -> Option<f32> {
        (*self).opt()
    }
}

impl Optional for &Option<f64> {
    type Out = Option<f64>;
    fn opt(self) -> Option<f64> {
        (*self).opt()
    }
}

/// A chart series' values.
impl Optional for Vec<Option<f64>> {
    type Out = Vec<Option<f64>>;
    fn opt(self) -> Vec<Option<f64>> {
        self.into_iter().map(Optional::opt).collect()
    }
}

impl Optional for &Vec<Option<f64>> {
    type Out = Vec<Option<f64>>;
    fn opt(self) -> Vec<Option<f64>> {
        self.iter().map(Optional::opt).collect()
    }
}

/// A cell's own margins: each side on its own.
impl Optional for Option<[Option<f32>; 4]> {
    type Out = Option<[Option<f32>; 4]>;
    fn opt(self) -> Option<[Option<f32>; 4]> {
        self.map(|sides| sides.map(Optional::opt))
    }
}

impl Optional for &Option<[Option<f32>; 4]> {
    type Out = Option<[Option<f32>; 4]>;
    fn opt(self) -> Option<[Option<f32>; 4]> {
        (*self).opt()
    }
}

/// A number, by value or by reference.
pub(crate) trait Number {
    fn get(self) -> f32;
}

impl Number for f32 {
    fn get(self) -> f32 {
        self
    }
}

impl Number for &f32 {
    fn get(self) -> f32 {
        *self
    }
}

/// A number JSON leaves out when it is zero: `-0.0` counts as zero there, and reads back as
/// `0.0`. So does a solid fill's transparency, through the bare-colour form of an opaque fill.
pub(crate) fn zero(value: impl Number) -> f32 {
    let value = Bridge::f32(value.get());
    if value == 0.0 { 0.0 } else { value }
}
