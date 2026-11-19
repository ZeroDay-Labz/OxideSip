use softphone_media::{MediaParams, MediaSession, ReservedSocket, SrtpKeys};
use std::net::SocketAddr;
use std::time::Duration;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let reserved = ReservedSocket::reserve().expect("reserve socket");
    let port = reserved.local_port();
    println!("reserved local port {port}");

    let remote: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let playback_target = std::env::var("SMOKE_PLAYBACK_TARGET").ok();
    // SMOKE_SRTP=1 loops the audio back encrypted (same key both ways,
    // since this session is its own peer).
    let srtp = std::env::var("SMOKE_SRTP").is_ok().then(|| SrtpKeys {
        local: "WVNfX19zZW1jdGwgKCkgewkyMjA7fQp9CnVubGVz".into(),
        remote: "WVNfX19zZW1jdGwgKCkgewkyMjA7fQp9CnVubGVz".into(),
    });
    let params = MediaParams {
        remote_addr: remote,
        payload_type: 0,
        telephone_event_pt: None,
        srtp,
    };
    let session = MediaSession::start(reserved, params, None, playback_target, "OxideSip".into())
        .await
        .expect("start media session");
    println!("media session started, sleeping 20s — check `pw-top` now for steady I/O with no starvation pattern");

    tokio::time::sleep(Duration::from_secs(20)).await;

    session.stop().await;
    println!("media session stopped");
}
