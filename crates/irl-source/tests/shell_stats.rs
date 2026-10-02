//! The stats writer against the field table.
//!
//! `calldata_*` is pure libobs bookkeeping over its own allocator: it needs
//! no `obs_startup`, so a calldata can be filled and read back in a test
//! binary. Nothing else in libobs is called.

use std::ffi::CString;

use irl_core::stats::{FIELDS, StatKind, StatValue, StatsSnapshot, proc_declaration};
use obs::CallData;
use obs_irl_source::source::write_stats;

/// A snapshot where every field carries its own value, so a writer that
/// reaches for the wrong member shows up as a mismatch rather than a zero.
fn distinct_snapshot() -> StatsSnapshot {
    StatsSnapshot {
        buffer_fill_ms: 101,
        current_speed: 1.05,
        total_audio_frames: 102,
        total_video_frames: 103,
        pts_max_gap_ms: 104,
        audio_underruns: 105,
        audio_output_restarts: 106,
        video_delay_ms: 107,
        av_skew_ms: 108,
        audio_hold_ms: 109,
        reconnecting: true,
    }
}

#[test]
fn every_declared_field_is_written_with_its_declared_type() {
    let snap = distinct_snapshot();
    let mut cd = CallData::new();
    write_stats(&mut cd, &snap);

    for ((name, kind), expected) in FIELDS.iter().zip(snap.values()) {
        let key = CString::new(*name).unwrap();
        match (kind, expected) {
            (StatKind::Int, StatValue::Int(v)) => {
                assert_eq!(cd.get_i64(&key), Some(v), "{name}");
                // A reader asking for the wrong type gets nothing back, which
                // is what keeps the declaration honest.
                assert_eq!(cd.get_bool(&key), None, "{name} read back as bool");
            }
            (StatKind::Float, StatValue::Float(v)) => {
                assert_eq!(cd.get_f64(&key), Some(v), "{name}");
            }
            (StatKind::Bool, StatValue::Bool(v)) => {
                assert_eq!(cd.get_bool(&key), Some(v), "{name}");
                assert_eq!(cd.get_i64(&key), None, "{name} read back as int");
            }
            (kind, value) => panic!("{name}: declared {kind:?} but written as {value:?}"),
        }
    }
}

#[test]
fn a_default_snapshot_writes_zeroes_not_absences() {
    let mut cd = CallData::new();
    write_stats(&mut cd, &StatsSnapshot::default());

    for (name, kind) in FIELDS {
        let key = CString::new(*name).unwrap();
        match kind {
            StatKind::Int => assert_eq!(cd.get_i64(&key), Some(0), "{name}"),
            StatKind::Float => assert_eq!(cd.get_f64(&key), Some(0.0), "{name}"),
            StatKind::Bool => assert_eq!(cd.get_bool(&key), Some(false), "{name}"),
        }
    }
}

/// The proc declaration and the writer walk the same table, so a client that
/// reads a declared name always finds it.
#[test]
fn the_declaration_names_exactly_what_is_written() {
    let decl = proc_declaration();
    assert!(decl.starts_with("void get_stats("));
    for (name, kind) in FIELDS {
        assert!(
            decl.contains(&format!("out {} {name}", kind.as_str())),
            "{name} is not in the declaration"
        );
    }
    // A dropped stat is gone from every surface, not just the table.
    assert!(!decl.contains("reconnect_count"));
}
