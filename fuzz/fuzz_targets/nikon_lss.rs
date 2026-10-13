#![no_main]

use libfuzzer_sys::fuzz_target;
use protocol_primitives::{
    NikonLssAuthenticationSelection, NikonLssClient, NikonLssServer, NikonLssSession,
};

fn session() -> NikonLssSession {
    let mut client = NikonLssClient::new([0x10; 8], [0x20; 8]);
    let selection = NikonLssAuthenticationSelection::new(3).unwrap();
    let mut server = NikonLssServer::new(selection, [0x30; 8], [0x40; 8]);
    let stage1 = client.stage1_record().unwrap();
    let stage2 = server.handle_stage1(&stage1).unwrap();
    let stage3 = client.handle_stage2(&stage2).unwrap();
    let (stage4, _) = server.finish_stage3(&stage3).unwrap();
    client.finish_stage4(&stage4).unwrap()
}

fn synthetic_configuration(session: &NikonLssSession, data: &[u8]) -> Vec<u8> {
    let mut ssid = [0u8; 32];
    let mut password = [0u8; 64];
    for (output, input) in ssid.iter_mut().take(31).zip(data.iter().copied()) {
        *output = b'a' + input % 26;
    }
    for (output, input) in password.iter_mut().take(63).zip(data.iter().copied().rev()) {
        *output = b'A' + input % 26;
    }
    let mut value = vec![0x03];
    value.extend_from_slice(&session.encrypt(&ssid).unwrap());
    value.extend_from_slice(&session.encrypt(&password).unwrap());
    value.push(data.first().copied().unwrap_or(0) % 4);
    value.extend_from_slice(&(data.len() as u32).to_le_bytes());
    value
}

fuzz_target!(|data: &[u8]| {
    let mut client = NikonLssClient::new([0x10; 8], [0x20; 8]);
    let _ = client.stage1_record();
    let _ = client.handle_stage2(data);

    let selection = NikonLssAuthenticationSelection::new(3).unwrap();
    let mut server = NikonLssServer::new(selection, [0x30; 8], [0x40; 8]);
    let _ = server.handle_stage1(data);

    let active = session();
    let _ = active.encrypt(data);
    let _ = active.decrypt(data);
    let _ = active.decode_connection_configuration(data);
    let encoded = synthetic_configuration(&active, data);
    let decoded = active.decode_connection_configuration(&encoded).unwrap();
    assert!(decoded.wifi.is_some());
    assert_eq!(decoded.spp_maximum_length, Some(data.len() as u32));
});
