#![no_main]

use libfuzzer_sys::fuzz_target;

use protocol_primitives::pcss;

fuzz_target!(|data: &[u8]| {
    for protocol in ["PTP/IP", "FUJIFILM"] {
        if let Some(discovery) = pcss::parse_discovery(data, protocol) {
            let peer = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
            let matched =
                camera_sim_service::inbound::pcss_host_matches_peer(&discovery.host, peer);
            assert_eq!(
                matched,
                discovery.host.parse::<std::net::IpAddr>().ok() == Some(peer)
            );
        }
        let _ = pcss::parse_notify(data, protocol);
    }
    let _ = pcss::parse_pcss_init(data);
    let _ = pcss::parse_pcss_init_ack(data);
});
