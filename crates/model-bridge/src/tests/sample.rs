//! Makes a value of a model type with every member set, so a conversion test touches every
//! member the mapping names.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

/// Structs and enums nest this deep before containers come back empty.
const MAX_DEPTH: u32 = 12;
/// Structs and variants one [`Seed::refill`] pays for; past it containers come back empty, so a
/// type that holds itself cannot grow a document without bound.
const BUDGET: u32 = 600;

/// Where the values come from, and what has been made so far.
pub(crate) struct Seed {
    state: u64,
    depth: u32,
    budget: u32,
    /// The next variant to make, per enum: each call takes the following one.
    picks: BTreeMap<&'static str, usize>,
    /// Every struct and every enum variant made, with its member count.
    pub(crate) made: BTreeMap<&'static str, usize>,
}

impl Seed {
    pub(crate) fn new(state: u64) -> Self {
        Self { state, depth: 0, budget: BUDGET, picks: BTreeMap::new(), made: BTreeMap::new() }
    }

    pub(crate) fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.state >> 33
    }

    pub(crate) fn enter(&mut self) {
        self.depth += 1;
    }

    pub(crate) fn leave(&mut self) {
        self.depth -= 1;
    }

    pub(crate) fn hit(&mut self, name: &'static str, members: usize) {
        self.budget = self.budget.saturating_sub(1);
        self.made.insert(name, members);
    }

    /// A fresh budget, for the next part of a document.
    pub(crate) fn refill(&mut self) {
        self.budget = BUDGET;
    }

    pub(crate) fn pick(&mut self, name: &'static str, count: usize) -> usize {
        let next = self.picks.entry(name).or_insert(0);
        let pick = *next % count;
        *next += 1;
        pick
    }

    /// How many elements a container gets here: none past the depth limit or the budget, and
    /// now and then none anyway, so empty containers are in the corpus too.
    fn len(&mut self) -> usize {
        if self.depth >= MAX_DEPTH || self.budget == 0 {
            return 0;
        }
        match self.next() % 6 {
            0 => 0,
            1 => 2,
            _ => 1,
        }
    }
}

pub(crate) trait Sample: Sized {
    fn sample(seed: &mut Seed) -> Self;
}

macro_rules! sample_ints {
    ($($ty:ty),*) => {$(
        impl Sample for $ty {
            fn sample(seed: &mut Seed) -> Self {
                // Zero one time in six: the value a skipped member reads back as.
                if seed.next().is_multiple_of(6) { 0 } else { (seed.next() % 120 + 1) as $ty }
            }
        }
    )*};
}

sample_ints!(u8, u16, u32, u64, usize, i8, i16, i32, i64);

impl Sample for bool {
    fn sample(seed: &mut Seed) -> Self {
        !seed.next().is_multiple_of(3)
    }
}

impl Sample for char {
    fn sample(seed: &mut Seed) -> Self {
        ['a', 'Z', '\u{e9}', '\u{4e2d}', '\u{2022}'][(seed.next() % 5) as usize]
    }
}

/// Finite numbers, in and out of the ranges a model clamps to.
const FLOATS: [f32; 12] = [0.5, 1.0, 12.25, -3.5, 72.0, 1.0e6, 0.0, 100.0, 0.01, 359.0, 0.0, -0.0];

impl Sample for f32 {
    fn sample(seed: &mut Seed) -> Self {
        FLOATS[(seed.next() % 12) as usize]
    }
}

impl Sample for f64 {
    fn sample(seed: &mut Seed) -> Self {
        f64::from(FLOATS[(seed.next() % 12) as usize]) + 0.125
    }
}

impl Sample for String {
    fn sample(seed: &mut Seed) -> Self {
        match seed.next() % 1000 {
            n if n < 80 => String::new(),
            n => format!("s{n}"),
        }
    }
}

impl Sample for Arc<str> {
    fn sample(seed: &mut Seed) -> Self {
        String::sample(seed).into()
    }
}

impl<T: Sample> Sample for Option<T> {
    fn sample(seed: &mut Seed) -> Self {
        (seed.len() > 0).then(|| T::sample(seed))
    }
}

impl<T: Sample> Sample for Vec<T> {
    fn sample(seed: &mut Seed) -> Self {
        (0..seed.len()).map(|_| T::sample(seed)).collect()
    }
}

impl<T: Sample> Sample for Box<T> {
    fn sample(seed: &mut Seed) -> Self {
        Box::new(T::sample(seed))
    }
}

impl<T: Sample> Sample for Arc<T> {
    fn sample(seed: &mut Seed) -> Self {
        Arc::new(T::sample(seed))
    }
}

impl<K: Sample + Ord, V: Sample> Sample for BTreeMap<K, V> {
    fn sample(seed: &mut Seed) -> Self {
        (0..seed.len()).map(|_| (K::sample(seed), V::sample(seed))).collect()
    }
}

impl<K: Sample + Eq + std::hash::Hash, V: Sample> Sample for HashMap<K, V> {
    fn sample(seed: &mut Seed) -> Self {
        (0..seed.len()).map(|_| (K::sample(seed), V::sample(seed))).collect()
    }
}

impl<T: Sample + Ord> Sample for BTreeSet<T> {
    fn sample(seed: &mut Seed) -> Self {
        (0..seed.len()).map(|_| T::sample(seed)).collect()
    }
}

impl<T: Sample, const N: usize> Sample for [T; N] {
    fn sample(seed: &mut Seed) -> Self {
        std::array::from_fn(|_| T::sample(seed))
    }
}

impl<A: Sample, B: Sample> Sample for (A, B) {
    fn sample(seed: &mut Seed) -> Self {
        (A::sample(seed), B::sample(seed))
    }
}

impl<A: Sample, B: Sample, C: Sample> Sample for (A, B, C) {
    fn sample(seed: &mut Seed) -> Self {
        (A::sample(seed), B::sample(seed), C::sample(seed))
    }
}

/// `Sample` for structs: a literal that names every field.
macro_rules! sample_structs {
    ($($ty:ty { $($field:ident),* $(,)? })*) => {
        /// Every struct sampled, with its field count.
        pub(crate) const STRUCTS: &[(&str, usize)] = &[$((stringify!($ty), 0 $(+ { let _ = stringify!($field); 1 })*),)*];
        $(
        impl Sample for $ty {
            fn sample(seed: &mut Seed) -> Self {
                seed.hit(stringify!($ty), 0 $(+ { let _ = stringify!($field); 1 })*);
                seed.enter();
                let value = Self { $($field: Sample::sample(seed),)* };
                seed.leave();
                value
            }
        }
        )*
    };
}

/// `Sample` for enums: each call makes the next variant, with every member of it set.
macro_rules! sample_enums {
    ($($ty:ty { $($variant:ident $(($($item:ident),*))? $({ $($field:ident),* })?),* $(,)? })*) => {
        /// Every enum variant sampled, with its field count.
        pub(crate) const VARIANTS: &[(&str, usize)] = &[$($((
            concat!(stringify!($ty), "::", stringify!($variant)),
            0 $($(+ { let _ = stringify!($item); 1 })*)? $($(+ { let _ = stringify!($field); 1 })*)?,
        ),)*)*];
        $(
        impl Sample for $ty {
            fn sample(seed: &mut Seed) -> Self {
                let makers: &[fn(&mut Seed) -> Self] = &[$(
                    |seed| {
                        seed.hit(
                            concat!(stringify!($ty), "::", stringify!($variant)),
                            0 $($(+ { let _ = stringify!($item); 1 })*)? $($(+ { let _ = stringify!($field); 1 })*)?,
                        );
                        Self::$variant $(($({ let _ = stringify!($item); Sample::sample(seed) }),*))? $({ $($field: Sample::sample(seed)),* })?
                    },
                )*];
                seed.enter();
                let value = makers[seed.pick(stringify!($ty), makers.len())](seed);
                seed.leave();
                value
            }
        }
        )*
    };
}

pub(crate) use {sample_enums, sample_structs};
