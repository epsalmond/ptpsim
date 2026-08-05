//! Per-connection trait fields and `platforms:` token validation (§11.29).
//!
//! `session.ownership` and `events.delivery` load as typed data so a consumer
//! selects behavior from the trait, never from the connection id. The
//! `platforms:` token set is closed: a typo is a load error instead of a
//! connection silently hidden on every platform.

use camera_config::{CameraManifest, EventDelivery, SessionOwnership};

fn manifest(connections: &str) -> String {
    format!(
        r#"
schema: camera-config/v1
camera: {{ manufacturer: TESTCO, model: Test, firmware: "1" }}
connections:
{connections}
"#
    )
}

#[test]
fn session_and_events_traits_load_as_typed_fields() {
    let m = CameraManifest::from_yaml(&manifest(
        r#"  usbTether:
    kind: usb
    establishment: usb-claim-session
    session: { ownership: daemonAttached }
    events: { delivery: bestEffort }
  plain:
    kind: wifi"#,
    ))
    .expect("connection trait fields load");

    let usb = &m.connections["usbTether"];
    assert_eq!(
        usb.session.as_ref().map(|s| s.ownership),
        Some(SessionOwnership::DaemonAttached),
    );
    assert_eq!(
        usb.events.as_ref().map(|e| e.delivery),
        Some(EventDelivery::BestEffort),
    );

    // Undeclared trait fields stay `None`; the consumer falls back.
    let plain = &m.connections["plain"];
    assert!(plain.session.is_none());
    assert!(plain.events.is_none());
}

#[test]
fn trait_fields_round_trip_through_yaml() {
    let m = CameraManifest::from_yaml(&manifest(
        r#"  usbTether:
    kind: usb
    session: { ownership: initiatorOwned }
    events: { delivery: none }"#,
    ))
    .expect("traits load");
    let usb = &m.connections["usbTether"];
    assert_eq!(
        usb.session.as_ref().map(|s| s.ownership),
        Some(SessionOwnership::InitiatorOwned),
    );
    assert_eq!(
        usb.events.as_ref().map(|e| e.delivery),
        Some(EventDelivery::None),
    );

    let yaml = m.to_yaml().expect("manifest serializes");
    assert!(yaml.contains("initiatorOwned"), "got: {yaml}");
    assert!(yaml.contains("delivery: none"), "got: {yaml}");
}

#[test]
fn trait_fields_are_closed_vocabularies() {
    let error = CameraManifest::from_yaml(&manifest(
        "  usbTether:\n    session: { ownership: shared }",
    ))
    .expect_err("an unknown ownership token fails the load");
    assert!(
        error.to_string().contains("unknown variant `shared`"),
        "got: {error}",
    );

    let error = CameraManifest::from_yaml(&manifest(
        "  usbTether:\n    events: { delivery: sometimes }",
    ))
    .expect_err("an unknown delivery token fails the load");
    assert!(
        error.to_string().contains("unknown variant `sometimes`"),
        "got: {error}",
    );
}

#[test]
fn platform_tokens_accept_the_full_vocabulary() {
    CameraManifest::from_yaml(&manifest(
        "  usb:\n    platforms: [ios, macos, android, linux]",
    ))
    .expect("the full platform vocabulary loads");

    // No `platforms:` key means unrestricted, as before.
    CameraManifest::from_yaml(&manifest("  app:\n    kind: wifi"))
        .expect("a connection without platforms loads");
}

#[test]
fn unknown_platform_token_fails_load_naming_token_and_connection() {
    let error =
        CameraManifest::from_yaml(&manifest("  usbTether:\n    platforms: [macos, windos]"))
            .expect_err("an unknown platform token is a load error");
    let message = error.to_string();
    assert!(
        message.contains("windos"),
        "names the token, got: {message}"
    );
    assert!(
        message.contains("usbTether"),
        "names the connection, got: {message}",
    );
}

#[test]
fn non_sequence_platforms_fails_load() {
    let error = CameraManifest::from_yaml(&manifest("  usbTether:\n    platforms: macos"))
        .expect_err("a scalar platforms value is a load error");
    assert!(
        error.to_string().contains("usbTether"),
        "names the connection, got: {error}",
    );
}
