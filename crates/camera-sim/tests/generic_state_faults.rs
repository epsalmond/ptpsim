//! Generic state faults for #357 (ACT_SHORT) and #375 (aux descriptor exhaustion).
//! Both use the generic StateOverlay surface from #164; engine stays generic.

use camera_config::CameraManifest;
use camera_media_store::MediaStore;
use camera_sim::{Engine, StateOverlay};
use tempfile;

fn minimal_manifest() -> CameraManifest {
    CameraManifest::from_yaml(
        r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TEST }
properties:
  "0x5001": { name: prop01, type: u16, access: readWrite }
connections:
  app:
    kind: ptpip-app
    bindings: { command: 55740, event: 55741, liveView: 55742 }
"#,
    )
    .expect("minimal manifest loads")
}

fn engine() -> Engine {
    let manifest = minimal_manifest();
    let dir = tempfile::tempdir().expect("temp dir");
    let store = MediaStore::open(dir.path()).expect("temp store");
    // Leak dir handle to keep it alive for test duration; OS cleans up.
    std::mem::forget(dir);
    Engine::new(manifest, store)
}

#[test]
fn aux_one_orphan_leaks_one_slot_without_wedging() {
    let mut eng = engine();
    eng.apply_state_overlay(&StateOverlay {
        aux_descriptor_budget: Some(3),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(eng.aux_available(), Some(3));
    eng.aux_orphan_leak().unwrap();
    assert_eq!(eng.aux_available(), Some(2));
    // Listener still usable: managed open succeeds while capacity remains
    eng.aux_managed_open().unwrap();
    assert_eq!(eng.aux_available(), Some(1));
}

#[test]
fn aux_repeated_orphans_reach_exhaustion() {
    let mut eng = engine();
    eng.apply_state_overlay(&StateOverlay {
        aux_descriptor_budget: Some(2),
        ..Default::default()
    })
    .unwrap();
    eng.aux_orphan_leak().unwrap();
    eng.aux_orphan_leak().unwrap();
    assert_eq!(eng.aux_available(), Some(0));
    let err = eng.aux_orphan_leak().expect_err("budget exhausted");
    assert_eq!(err, "EMFILE");
    let err2 = eng.aux_managed_open().expect_err("exhausted");
    assert_eq!(err2, "EMFILE");
}

#[test]
fn aux_exhaustion_does_not_change_ap_refusal_state() {
    let mut eng = engine();
    eng.apply_state_overlay(&StateOverlay {
        aux_descriptor_budget: Some(2),
        ap_act_short_latch: Some(true),
        ..Default::default()
    })
    .unwrap();
    // Exhaust aux
    eng.aux_orphan_leak().unwrap();
    eng.aux_orphan_leak().unwrap();
    assert_eq!(eng.aux_available(), Some(0));
    // AP latch still true, refusal still active
    assert!(eng.ap_refusal_active());
    // AP refusal path still reports 0080/2, consumed on next check
    let refusal = eng.ap_should_refuse();
    assert_eq!(refusal, Some((0x0080, 2)));
    // After consumption, latch false but held false, so no longer active
    assert!(!eng.ap_refusal_active());
    // Aux still exhausted, proving independence
    assert_eq!(eng.aux_available(), Some(0));
}

#[test]
fn aux_managed_close_releases_capacity_orphan_cannot() {
    let mut eng = engine();
    eng.apply_state_overlay(&StateOverlay {
        aux_descriptor_budget: Some(3),
        ..Default::default()
    })
    .unwrap();
    eng.aux_orphan_leak().unwrap(); // orphan 1
    eng.aux_managed_open().unwrap(); // managed 1
    assert_eq!(eng.aux_available(), Some(1));
    // Managed close releases one slot
    eng.aux_managed_close().unwrap();
    assert_eq!(eng.aux_available(), Some(2));
    // No direct close for orphan: managed_used is 0 now, further close fails
    let err = eng.aux_managed_close().expect_err("no managed to close");
    assert!(err.contains("no managed"));
    // Still one orphan leaked, so available stays 2 (not 3)
    assert_eq!(eng.aux_available(), Some(2));
}

#[test]
fn aux_reset_clears_all_leaked_capacity() {
    let mut eng = engine();
    eng.apply_state_overlay(&StateOverlay {
        aux_descriptor_budget: Some(2),
        ..Default::default()
    })
    .unwrap();
    eng.aux_orphan_leak().unwrap();
    eng.aux_orphan_leak().unwrap();
    assert_eq!(eng.aux_available(), Some(0));
    eng.apply_state_overlay(&StateOverlay {
        aux_reset: Some(true),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(eng.aux_available(), Some(2));
    // Fresh connection now succeeds
    eng.aux_managed_open().unwrap();
    assert_eq!(eng.aux_available(), Some(1));
}

#[test]
fn aux_both_channels_share_budget() {
    let mut eng = engine();
    eng.apply_state_overlay(&StateOverlay {
        aux_descriptor_budget: Some(2),
        ..Default::default()
    })
    .unwrap();
    // Simulate event and liveView each orphans one
    eng.aux_orphan_leak().unwrap();
    eng.aux_orphan_leak().unwrap();
    assert_eq!(eng.aux_available(), Some(0));
    // Both channels now exhausted together
    assert!(eng.aux_managed_open().is_err());
}

#[test]
fn act_short_one_shot_consumed_next_launch_succeeds() {
    let mut eng = engine();
    eng.apply_state_overlay(&StateOverlay {
        ap_act_short_latch: Some(true),
        ..Default::default()
    })
    .unwrap();
    assert!(eng.ap_refusal_active());
    // First launch is refused with 0080 detail 2, latch consumed
    let refusal = eng.ap_should_refuse();
    assert_eq!(refusal, Some((0x0080, 2)));
    // Next fresh launch succeeds (no refusal)
    assert!(!eng.ap_refusal_active());
    assert_eq!(eng.ap_should_refuse(), None);
}

#[test]
fn act_short_held_term_keeps_refusing_until_cleared() {
    let mut eng = engine();
    eng.apply_state_overlay(&StateOverlay {
        ap_held_term: Some(true),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(eng.ap_should_refuse(), Some((0x0080, 2)));
    assert_eq!(eng.ap_should_refuse(), Some((0x0080, 2)));
    assert_eq!(eng.ap_should_refuse(), Some((0x0080, 2)));
    // Clear held term
    eng.apply_state_overlay(&StateOverlay {
        ap_held_term: Some(false),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(eng.ap_should_refuse(), None);
}

#[test]
fn act_short_refusal_is_0080_serialization() {
    let mut eng = engine();
    eng.apply_state_overlay(&StateOverlay {
        ap_act_short_latch: Some(true),
        ..Default::default()
    })
    .unwrap();
    let (raw, detail) = eng.ap_should_refuse().unwrap();
    assert_eq!(format!("{raw:04x}"), "0080");
    assert_eq!(detail, 2);
    // Normal success would be 0180, never 0380
    assert_ne!(format!("{raw:04x}"), "0380");
}

#[test]
fn ap_not_exposed_while_refusal_active() {
    let mut eng = engine();
    eng.apply_state_overlay(&StateOverlay {
        ap_act_short_latch: Some(true),
        ..Default::default()
    })
    .unwrap();
    assert!(
        eng.ap_refusal_active(),
        "AP should be unavailable while refusal latch active"
    );
    // After refusal consumed, AP becomes available
    eng.ap_should_refuse();
    assert!(
        !eng.ap_refusal_active(),
        "AP becomes available after latch consumed"
    );
}

#[test]
fn normal_launch_never_emits_0380() {
    let mut eng = engine();
    // No latch, no held term
    assert_eq!(eng.ap_should_refuse(), None);
    // Ensure 0380 is not a normal path value
    let refusal = eng.ap_should_refuse();
    assert_ne!(refusal.map(|(r, _)| r), Some(0x0380));
}

#[test]
fn faults_remain_independent() {
    let mut eng = engine();
    eng.apply_state_overlay(&StateOverlay {
        aux_descriptor_budget: Some(2),
        ap_act_short_latch: Some(true),
        ap_held_term: Some(false),
        ..Default::default()
    })
    .unwrap();
    // Exhaust aux
    eng.aux_orphan_leak().unwrap();
    eng.aux_orphan_leak().unwrap();
    assert_eq!(eng.aux_available(), Some(0));
    // AP latch still independent
    assert!(eng.ap_refusal_active());
    eng.ap_should_refuse(); // consumes latch
    assert!(!eng.ap_refusal_active());
    assert_eq!(eng.aux_available(), Some(0));
    // Reset aux does not affect AP held term
    eng.apply_state_overlay(&StateOverlay {
        ap_held_term: Some(true),
        ..Default::default()
    })
    .unwrap();
    eng.aux_reset_state();
    assert_eq!(eng.aux_available(), Some(2));
    assert!(eng.ap_refusal_active());
}
