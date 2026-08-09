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
fn best_effort_event_await_without_then_poll_fails_load() {
    let error = CameraManifest::from_yaml(&manifest(
        r#"  usbTether:
    kind: usb
    events: { delivery: bestEffort }
    entries:
      - to: shooting
        steps:
          - awaitUntil:
              source: { event: { code: "0xc005" } }
              until: { prop: "0xd209", eq: 1 }
              timeoutMs: 30000"#,
    ))
    .expect_err("a bestEffort event-source await without thenPoll is a load error");
    let message = error.to_string();
    assert!(
        message.contains("usbTether"),
        "names the connection, got: {message}",
    );
    assert!(
        message.contains("entries[0].steps[0]"),
        "names the step path, got: {message}",
    );
}

#[test]
fn none_event_channel_forbids_event_source_awaits() {
    // `thenPoll` is declared, so only the `none` rule can reject this.
    let error = CameraManifest::from_yaml(&manifest(
        r#"  usbTether:
    kind: usb
    events: { delivery: none }
    actions:
      autofocusLock:
        mode: ""
        initiator:
          steps:
            - awaitUntil:
                source: { event: { code: "0xc005", thenPoll: "0xd209" } }
                until: { prop: "0xd209", eq: 1 }
                timeoutMs: 30000"#,
    ))
    .expect_err("a none event channel forbids event-source awaits outright");
    let message = error.to_string();
    assert!(
        message.contains("usbTether"),
        "names the connection, got: {message}",
    );
    assert!(
        message.contains("actions.AutofocusLock.steps[0]"),
        "names the step path, got: {message}",
    );
}

#[test]
fn best_effort_event_await_with_then_poll_loads() {
    let m = CameraManifest::from_yaml(&manifest(
        r#"  usbTether:
    kind: usb
    events: { delivery: bestEffort }
    entries:
      - to: shooting
        steps:
          - awaitUntil:
              source: { event: { code: "0xc005", thenPoll: "0xd209" } }
              until: { prop: "0xd209", eq: 1 }
              timeoutMs: 30000"#,
    ))
    .expect("a bestEffort event-source await with thenPoll loads");
    assert_eq!(
        m.connections["usbTether"]
            .events
            .as_ref()
            .map(|e| e.delivery),
        Some(EventDelivery::BestEffort),
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

#[test]
fn usb_discovery_is_typed_and_pid_is_optional() {
    let manifest = CameraManifest::from_yaml(&manifest(
        r#"  usbPassthrough:
    kind: usb-passthrough
    platforms: [ios, macos]
    discovery:
      mechanism: usb
      announces: attachment
      platforms: [ios, macos]
      vid: 0x04cb"#,
    ))
    .expect("vendor-level USB discovery loads");
    let discovery = manifest.connections["usbPassthrough"]
        .discovery
        .as_ref()
        .expect("typed discovery");
    assert_eq!(discovery.mechanism, "usb");
    assert_eq!(discovery.vid, Some(0x04cb));
    assert_eq!(discovery.pid, None);
    assert_eq!(discovery.platforms, ["ios", "macos"]);
}

#[test]
fn usb_discovery_requires_vendor_id() {
    let error = CameraManifest::from_yaml(&manifest(
        "  usb:\n    kind: usb\n    discovery: { mechanism: usb, platforms: [linux] }",
    ))
    .expect_err("USB discovery without a vendor ID fails load");
    assert!(
        error.to_string().contains("connections.usb.discovery.vid"),
        "got: {error}",
    );
}

#[test]
fn usb_discovery_requires_an_automatic_recognition_platform() {
    let error = CameraManifest::from_yaml(&manifest(
        "  usb:\n    kind: usb\n    discovery: { mechanism: usb, vid: 0x04cb }",
    ))
    .expect_err("USB discovery without a platform fails load");
    assert!(
        error
            .to_string()
            .contains("connections.usb.discovery.platforms"),
        "got: {error}",
    );
}

#[test]
fn discovery_mechanism_rejects_noncanonical_whitespace() {
    let error = CameraManifest::from_yaml(&manifest(
        "  usb:\n    kind: usb\n    discovery: { mechanism: ' usb ', platforms: [linux], vid: 0x04cb }",
    ))
    .expect_err("whitespace cannot bypass USB discovery validation");
    assert!(
        error
            .to_string()
            .contains("connections.usb.discovery.mechanism must be a lowercase kebab-case token"),
        "got: {error}",
    );
}

#[test]
fn discovery_platforms_must_be_available_on_the_connection() {
    let error = CameraManifest::from_yaml(&manifest(
        r#"  usb:
    kind: usb
    platforms: [android, linux]
    discovery: { mechanism: usb, platforms: [macos], vid: 0x04cb }"#,
    ))
    .expect_err("automatic recognition cannot name an unavailable platform");
    let message = error.to_string();
    assert!(message.contains("connections.usb.discovery.platforms"));
    assert!(message.contains("macos"));
}
