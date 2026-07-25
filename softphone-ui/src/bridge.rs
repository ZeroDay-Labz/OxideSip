use crate::app::Message;
use iced::futures::sink::SinkExt;
use iced::stream;
use iced::Subscription;
use softphone_core::config::SipAccountConfig;
use softphone_core::events::{CoreCommand, CoreEvent};
use softphone_core::SoftphoneCore;
use tokio_util::sync::CancellationToken;

/// `run_with` keys the subscription's identity off `accounts` (which is
/// `Hash` via `Vec<SipAccountConfig>`), so this stream is rebuilt only when
/// the account list itself changes (added/removed/edited), not on every
/// `update()`.
///
/// The builder must be a plain (non-capturing) `fn(&D) -> S` — passed as an
/// inline closure rather than a named function, because a named function
/// returning `impl Stream<..>` captures the `&Vec<SipAccountConfig>`
/// parameter's lifetime under edition-2024 RPIT rules, which doesn't coerce
/// to the `fn` pointer `run_with` expects. This mirrors how iced's own
/// `time::every` builds its subscriptions internally.
///
/// Cancels a running `SoftphoneCore` when dropped. `tokio::spawn` detaches
/// its task, so without this, tearing down the subscription's stream (e.g.
/// when the account list's `Hash` changes after a Settings save, which
/// makes `run_with` swap in a fresh stream) would leave the *old* cores
/// running forever in the background, still registered under stale
/// credentials. Holding one of these per account for the async block's
/// whole lifetime ties each core's lifetime to the stream's: when the block
/// is dropped, every `cancel.cancel()` fires and `SoftphoneCore::run`'s
/// existing shutdown path tears the old registrations down before any new
/// ones spin up.
struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Runs one `SoftphoneCore` per configured account, all multiplexed into a
/// single iced `Subscription` — each account gets its own registration,
/// dialog layer, and command/event channel pair (see `SoftphoneCore::run`'s
/// doc comment: nothing about it is global/shared, so N concurrent
/// instances need no core-side changes at all). Every message this stream
/// emits is tagged with the account's index into the `accounts` slice the
/// UI passed in, so `app.rs` can route it to that account's own line/call
/// state.
pub fn subscription(accounts: &[SipAccountConfig]) -> Subscription<Message> {
    Subscription::run_with(accounts.to_vec(), |accounts: &Vec<SipAccountConfig>| {
        let accounts = accounts.clone();
        stream::channel(64, async move |mut output| {
            // All accounts' events funnel through this one tagged channel,
            // so the single loop at the bottom is the only place that needs
            // to touch `output` — no `select!`-ing across N independent
            // `event_rx`s.
            let (tagged_tx, mut tagged_rx) = tokio::sync::mpsc::channel::<(usize, CoreEvent)>(128);
            let mut guards = Vec::with_capacity(accounts.len());

            for (index, config) in accounts.into_iter().enumerate() {
                let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<CoreEvent>(64);
                let (command_tx, command_rx) = tokio::sync::mpsc::channel::<CoreCommand>(32);

                let _ = output.send(Message::CoreConnected(index, command_tx)).await;

                let cancel = CancellationToken::new();
                guards.push(CancelOnDrop(cancel.clone()));
                tokio::spawn(SoftphoneCore::run(config, event_tx, command_rx, cancel));

                let forward_tx = tagged_tx.clone();
                tokio::spawn(async move {
                    while let Some(event) = event_rx.recv().await {
                        if forward_tx.send((index, event)).await.is_err() {
                            break;
                        }
                    }
                });
            }
            // Drop our own clone so `tagged_rx` can naturally end once every
            // per-account forwarder above has exited (it never does in
            // practice before the stream itself is torn down, but this
            // keeps the channel's lifetime honest either way).
            drop(tagged_tx);

            while let Some((index, event)) = tagged_rx.recv().await {
                if output.send(Message::Core(index, event)).await.is_err() {
                    break;
                }
            }
        })
    })
}
