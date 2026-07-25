pub mod config;
pub mod dialog;
pub mod error;
pub mod events;
pub mod registration;
pub mod sdp;
pub mod transport;

use config::{SipAccountConfig, SipTransport};
use error::{CoreError, Result};
use events::{CoreCommand, CoreEvent};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::sip::Host;
use rsipstack::transport::TransportLayer;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct SoftphoneCore;

impl SoftphoneCore {
    /// Runs the headless signaling core until `cancel` fires: TLS transport
    /// warm-up, the REGISTER retry loop, and inbound INVITE/dialog handling.
    /// Communicates with a UI (Phase 3) purely through `event_tx`/`command_rx`.
    pub async fn run(
        config: SipAccountConfig,
        event_tx: mpsc::Sender<CoreEvent>,
        command_rx: mpsc::Receiver<CoreCommand>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let transport_layer =
            transport::build_transport_layer(&config, cancel.child_token()).await?;
        let endpoint = transport::build_endpoint(transport_layer.clone(), cancel.child_token());

        let registrar = transport::registrar_uri(&config)?;
        if config.transport == SipTransport::Tls {
            transport::warm_up_registrar_connection(&transport_layer, &registrar).await?;
        }

        let local_media_addr = local_media_addr(&transport_layer)?;

        let endpoint_inner_for_registration = endpoint.inner.clone();
        let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));
        let (state_tx, state_rx) = dialog_layer.new_dialog_state_channel();
        let incoming = endpoint.incoming_transactions()?;
        let contact = dialog_layer.build_local_contact(Some(config.username.clone()), None)?;
        let dialog_config = config.clone();

        let serve_task = tokio::spawn(async move {
            endpoint.serve().await;
        });

        let registration_task = tokio::spawn(registration::run_registration_loop(
            endpoint_inner_for_registration,
            config,
            event_tx.clone(),
            cancel.child_token(),
        ));

        let incoming_task = tokio::spawn(dialog::incoming_request_loop(
            dialog_layer.clone(),
            incoming,
            state_tx.clone(),
            contact.clone(),
        ));

        let dialog_state_task = tokio::spawn(dialog::dialog_state_loop(
            dialog_layer,
            state_rx,
            state_tx,
            event_tx,
            command_rx,
            local_media_addr,
            contact,
            dialog_config,
        ));

        cancel.cancelled().await;
        serve_task.abort();
        registration_task.abort();
        incoming_task.abort();
        dialog_state_task.abort();

        Ok(())
    }
}

/// The local IP address advertised in SDP for media (Phase 2 will bind RTP
/// there). Prefer the address the TLS registrar connection actually used;
/// fall back to the first non-loopback interface if that's unavailable.
fn local_media_addr(transport_layer: &TransportLayer) -> Result<IpAddr> {
    if let Some(addr) = transport_layer.get_addrs().first()
        && let Host::IpAddr(ip) = addr.addr.host
    {
        return Ok(ip);
    }
    if_addrs::get_if_addrs()?
        .into_iter()
        .find(|iface| !iface.is_loopback())
        .map(|iface| iface.ip())
        .ok_or_else(|| CoreError::Config("no non-loopback network interface found".into()))
}
