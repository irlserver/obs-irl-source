//! Round-trip tests for the reimplemented `calldata` inline helpers.
//!
//! These are integration tests rather than `#[cfg(test)]` modules on purpose:
//! only test *targets* get `cargo::rustc-link-arg-tests=-lobs` from build.rs,
//! and these are the tests that call into libobs. They need no `obs_startup()`
//! — `calldata_set_data`/`_get_data`/`_get_string` and `bfree` only touch the
//! calldata's own bmem allocation.

use obs::CallData;

#[test]
fn int_roundtrip() {
    let mut cd = CallData::new();
    cd.set_i64(c"count", -42);
    assert_eq!(cd.get_i64(c"count"), Some(-42));
    assert_eq!(cd.get_i64(c"missing"), None);
}

#[test]
fn float_bool_and_ptr_roundtrip() {
    let mut cd = CallData::new();
    cd.set_f64(c"speed", 1.05);
    cd.set_bool(c"adaptive", true);
    let target = 0x1234_5678_usize as *mut core::ffi::c_void;
    cd.set_ptr(c"ph", target);

    assert_eq!(cd.get_f64(c"speed"), Some(1.05));
    assert_eq!(cd.get_bool(c"adaptive"), Some(true));
    assert_eq!(cd.get_ptr(c"ph"), Some(target));
}

#[test]
fn string_roundtrip_and_overwrite() {
    let mut cd = CallData::new();
    cd.set_str(c"name", c"IRL Source");
    assert_eq!(cd.get_str(c"name"), Some("IRL Source"));

    cd.set_str(c"name", c"Other");
    assert_eq!(cd.get_str(c"name"), Some("Other"));
    assert_eq!(cd.get_str(c"nope"), None);
}

#[test]
fn typed_getters_reject_a_different_width() {
    let mut cd = CallData::new();
    cd.set_bool(c"flag", true);
    // A bool is one byte; asking for eight must fail rather than read past it.
    assert_eq!(cd.get_i64(c"flag"), None);
}

#[test]
fn many_entries_survive_stack_growth() {
    let mut cd = CallData::new();
    for i in 0..64i64 {
        let name = std::ffi::CString::new(format!("field_{i}")).unwrap();
        cd.set_i64(&name, i * 3);
    }
    for i in 0..64i64 {
        let name = std::ffi::CString::new(format!("field_{i}")).unwrap();
        assert_eq!(cd.get_i64(&name), Some(i * 3));
    }
}

#[test]
fn a_fresh_calldata_is_empty() {
    let cd = CallData::new();
    assert_eq!(cd.get_i64(c"anything"), None);
    assert_eq!(cd.get_str(c"anything"), None);
    assert_eq!(cd.get_bool(c"anything"), None);
    assert_eq!(cd.get_f64(c"anything"), None);
    assert_eq!(cd.get_ptr(c"anything"), None);
}
