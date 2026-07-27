use softphone_core::sdp;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

#[test]
fn sdes_srtp_offer_round_trips() {
    let local_addr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50));
    let offer = sdp::generate_offer(40000, true, false, &[0], false);
    let sdp_text = sdp::build_sdp(local_addr, &offer).to_string();

    let parsed = sdp::parse_offer(&sdp_text).expect("parse should succeed");

    assert_eq!(parsed.port, offer.port);
    assert_eq!(parsed.payload_types, offer.payload_types);
    assert_eq!(parsed.crypto_key, offer.crypto_key);
    assert!(parsed.crypto_key.is_some());
    assert_eq!(parsed.remote_addr, Some(SocketAddr::new(local_addr, 40000)));
}

#[test]
fn plain_rtp_offer_round_trips_without_crypto() {
    let local_addr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50));
    let offer = sdp::generate_offer(40002, false, false, &[0], false);
    let sdp_text = sdp::build_sdp(local_addr, &offer).to_string();

    assert!(!sdp_text.contains("a=crypto"));
    assert!(sdp_text.contains("RTP/AVP"));

    let parsed = sdp::parse_offer(&sdp_text).expect("parse should succeed");

    assert_eq!(parsed.port, offer.port);
    assert_eq!(parsed.payload_types, offer.payload_types);
    assert!(parsed.crypto_key.is_none());
}

#[test]
fn hold_offer_includes_sendonly_attribute() {
    let local_addr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50));
    let offer = sdp::generate_offer(40004, false, true, &[0], false);
    let sdp_text = sdp::build_sdp(local_addr, &offer).to_string();

    assert!(sdp_text.contains("a=sendonly"));

    let resumed = sdp::generate_offer(40004, false, false, &[0], false);
    let resumed_text = sdp::build_sdp(local_addr, &resumed).to_string();
    assert!(!resumed_text.contains("a=sendonly"));
}

#[test]
fn alaw_offer_advertises_pcma() {
    let local_addr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50));
    let offer = sdp::generate_offer(40006, false, false, &[8], false);
    let sdp_text = sdp::build_sdp(local_addr, &offer).to_string();

    assert!(sdp_text.contains("PCMA"));
    assert!(!sdp_text.contains("PCMU"));

    let parsed = sdp::parse_offer(&sdp_text).expect("parse should succeed");
    assert_eq!(parsed.payload_types, vec![8]);
}
