use softphone_core::config;
use softphone_core::events::{CoreCommand, CoreEvent};
use softphone_core::SoftphoneCore;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "oxidesip.toml".to_string());
    let cfg = if PathBuf::from(&config_path).exists() {
        config::load_config(Path::new(&config_path)).expect("failed to load config file")
    } else {
        config::load_config_from_env().expect("failed to load config from OXIDESIP_* env vars")
    };

    let (event_tx, mut event_rx) = mpsc::channel(32);
    let (command_tx, command_rx) = mpsc::channel(32);
    let cancel = CancellationToken::new();

    let run_cancel = cancel.clone();
    let core_task = tokio::spawn(async move {
        if let Err(e) = SoftphoneCore::run(cfg, event_tx, command_rx, run_cancel).await {
            tracing::error!(error = %e, "softphone core exited with error");
        }
    });

    let ctrl_c_cancel = cancel.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        println!("shutting down...");
        ctrl_c_cancel.cancel();
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut current_call: Option<String> = None;

    println!("register_probe running. 'a' answers, 'r' rejects the most recent incoming call.");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            event = event_rx.recv() => {
                let Some(event) = event else { break };
                if let CoreEvent::IncomingCall { id, remote, .. } = &event {
                    println!("incoming call {id} from {remote} -- answer with 'a', reject with 'r'");
                    current_call = Some(id.clone());
                }
                println!("event: {event:?}");
            }
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { continue };
                let Some(id) = current_call.clone() else { continue };
                match line.trim() {
                    // register_probe is a signaling-only manual test tool with no real
                    // audio pipeline, so it doesn't reserve a real RTP port -- the real
                    // UI reserves one via softphone-media before sending AnswerCall.
                    "a" => { command_tx.send(CoreCommand::AnswerCall { id, local_rtp_port: 40000 }).await.ok(); }
                    "r" => { command_tx.send(CoreCommand::RejectCall(id)).await.ok(); }
                    _ => {}
                }
            }
        }
    }

    cancel.cancel();
    core_task.await.ok();
}
