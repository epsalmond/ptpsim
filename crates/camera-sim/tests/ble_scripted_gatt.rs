use camera_sim::{BleError, BleResponder};

const AUTH: &str = "00002000-3DD4-4255-8D62-6DC7B9BD5561";
const OTHER: &str = "00002001-3DD4-4255-8D62-6DC7B9BD5561";

fn connected(mut responder: BleResponder) -> BleResponder {
    responder.connect();
    responder.discover_services().unwrap();
    responder
}

#[test]
fn exact_writes_and_indications_follow_one_global_order() {
    let stage1 = [0x11; 17];
    let stage2 = [0x22; 17];
    let stage3 = [0x33; 17];
    let stage4 = [0x44; 17];
    let mut responder = connected(
        BleResponder::new([AUTH.to_string()])
            .expect_exact_write(AUTH, &stage1)
            .queue_ordered_indication(AUTH, &stage2)
            .expect_exact_write(AUTH, &stage3)
            .queue_ordered_indication(AUTH, &stage4),
    );

    responder.write(AUTH, &stage1).unwrap();
    assert_eq!(responder.take_notification(AUTH), Some(stage2.to_vec()));
    responder.write(AUTH, &stage3).unwrap();
    assert_eq!(responder.take_notification(AUTH), Some(stage4.to_vec()));
    assert_eq!(
        responder.written(AUTH),
        vec![stage1.as_slice(), stage3.as_slice()]
    );
}

#[test]
fn exact_write_rejects_bytes_and_indications_cannot_be_taken_out_of_order() {
    let stage1 = [0x11; 17];
    let stage2 = [0x22; 17];
    let mut responder = connected(
        BleResponder::new([AUTH.to_string(), OTHER.to_string()])
            .expect_exact_write(AUTH, &stage1)
            .queue_ordered_indication(AUTH, &stage2),
    );

    assert_eq!(responder.take_notification(AUTH), None);
    let mut malformed = stage1;
    malformed[16] ^= 1;
    assert!(matches!(
        responder.write(AUTH, &malformed),
        Err(BleError::UnexpectedWrite { .. })
    ));
    responder.write(AUTH, &stage1).unwrap();
    assert_eq!(responder.take_notification(OTHER), None);
    assert_eq!(responder.take_notification(AUTH), Some(stage2.to_vec()));
}

#[test]
fn scripted_indication_blocks_an_early_write() {
    let indication = [0x22; 17];
    let mut responder = connected(
        BleResponder::new([AUTH.to_string()]).queue_ordered_indication(AUTH, &indication),
    );

    assert!(matches!(
        responder.write(AUTH, &[0x33; 17]),
        Err(BleError::ScriptOutOfOrder { .. })
    ));
    assert_eq!(responder.take_notification(AUTH), Some(indication.to_vec()));
}

#[test]
fn rejected_fenced_write_is_transactional_and_can_be_retried() {
    let expected = [0x11; 17];
    let indication = [0x22; 17];
    let mut responder = connected(
        BleResponder::new([AUTH.to_string()])
            .queue_notification(AUTH, &[0x99; 17])
            .expect_exact_write(AUTH, &expected)
            .queue_ordered_indication(AUTH, &indication),
    );

    let mut malformed = expected;
    malformed[16] ^= 1;
    assert!(matches!(
        responder.write_with_notification_fence(AUTH, &malformed, AUTH),
        Err(BleError::UnexpectedWrite { .. })
    ));
    assert_eq!(
        responder.take_notification(AUTH),
        None,
        "the exact-write script must still block the stale notification"
    );

    responder
        .write_with_notification_fence(AUTH, &expected, AUTH)
        .unwrap();
    assert_eq!(
        responder.take_notification(AUTH),
        Some(indication.to_vec()),
        "the rejected attempt must not advance the fence or consume the script"
    );
}
