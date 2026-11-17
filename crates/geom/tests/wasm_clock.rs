//! Registered WASM clock callbacks keep their clock domains apart. A separate test binary,
//! because registration is process-global and permanent.

#[test]
fn registered_callbacks_feed_their_own_accessors() {
    wordcraft_geom::register_wasm_clock(|| 42.5, || 1_700_000_000_000.0, |_| 3600);
    // The stored atomic values must not override the registered callbacks.
    wordcraft_geom::set_wasm_clock(7.0, 1_600_000_000_000.0);
    assert_eq!(wordcraft_geom::wasm_now_ms(), 42.5);
    assert_eq!(wordcraft_geom::wasm_epoch_ms(), 1_700_000_000_000.0);
    assert_eq!(wordcraft_geom::wasm_tz_offset_secs(0), 3600);
}
