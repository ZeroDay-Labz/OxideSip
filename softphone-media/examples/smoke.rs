use softphone_media::{MediaSession, ReservedSocket};
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
    let session =
        MediaSession::start(reserved, remote, 0, None, None, playback_target, "OxideSip".into())
            .await
            .expect("start media session");
    println!("media session started, sleeping 20s — check `pw-top` now for steady I/O with no starvation pattern");

    tokio::time::sleep(Duration::from_secs(20)).await;

    session.stop().await;
    println!("media session stopped");
}
