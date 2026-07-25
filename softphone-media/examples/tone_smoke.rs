use softphone_media::DtmfTonePlayer;
use std::time::Duration;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let player = DtmfTonePlayer::start(None).expect("start tone player");
    println!("tone player started");
    for digit in ['1', '2', '3'] {
        println!("pressing {digit} at t={:?}", std::time::Instant::now());
        player.play(digit);
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
}
