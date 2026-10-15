//! Desktop notification for an incoming call, with Answer / Decline actions.
//!
//! Complements the always-on-top window: KWin's focus-stealing prevention
//! can keep a brand-new toplevel from surfacing over a full-screen app or on
//! another virtual desktop, while the notification server is the
//! desktop-sanctioned attention channel (and KDE Connect mirrors it to the
//! phone). The window is still the primary UI; this is the safety net.

use crate::branding;
use iced::futures::{SinkExt, Stream};
use notify_rust::{Hint, Notification, Timeout, Urgency};
use softphone_core::events::CallId;
use tokio::sync::mpsc;

/// What the notification server told us about one notification.
#[derive(Debug, Clone)]
pub enum Event {
    /// The notification is on screen under this server-assigned id.
    Shown(u32),
    /// The user pressed an action button (`"answer"` / `"decline"`), or the
    /// notification was closed (`"__closed"`).
    Action(String),
    Failed(String),
}

/// Shows the notification and streams its lifecycle events. Runs on a plain
/// thread: `notify-rust`'s action wait is blocking and must own the handle.
pub fn incoming_call(call_id: CallId, summary: String, body: String) -> impl Stream<Item = (CallId, Event)> {
    iced::stream::channel(
        8,
        move |mut out: iced::futures::channel::mpsc::Sender<(CallId, Event)>| async move {
            let (tx, mut rx) = mpsc::channel::<Event>(4);
            std::thread::Builder::new()
                .name(format!("notify-{call_id}"))
                .spawn(move || {
                    let shown = Notification::new()
                        .appname(branding::APP_NAME)
                        .summary(&summary)
                        .body(&body)
                        .icon(branding::APP_ID)
                        .urgency(Urgency::Critical)
                        .timeout(Timeout::Never)
                        .hint(Hint::Resident(true))
                        .hint(Hint::Category("im.received".into()))
                        .hint(Hint::DesktopEntry(branding::APP_ID.into()))
                        .action("answer", "Answer")
                        .action("decline", "Decline")
                        .show();
                    match shown {
                        Ok(handle) => {
                            let _ = tx.blocking_send(Event::Shown(handle.id()));
                            handle.wait_for_action(|action| {
                                let _ = tx.blocking_send(Event::Action(action.to_string()));
                            });
                        }
                        Err(e) => {
                            let _ = tx.blocking_send(Event::Failed(e.to_string()));
                        }
                    }
                })
                .ok();
            while let Some(event) = rx.recv().await {
                if out.send((call_id.clone(), event)).await.is_err() {
                    break;
                }
            }
        },
    )
}

/// Takes a notification off the screen. `notify-rust` only lets the owning
/// handle close a notification, and that handle is parked in
/// `wait_for_action` on its own thread — so instead *replace* it (same id)
/// with an empty, immediately-expiring one, which every notification server
/// treats as a close and which also unblocks the waiting thread.
pub fn dismiss(id: u32) {
    std::thread::spawn(move || {
        let _ = Notification::new()
            .appname(branding::APP_NAME)
            .id(id)
            .summary("Call ended")
            .timeout(Timeout::Milliseconds(1))
            .hint(Hint::Transient(true))
            .show();
    });
}
