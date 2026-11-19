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
        // The sockets and the SIP endpoint outlive `cancel` by a moment (see
        // the end of this function): the un-REGISTER sent on shutdown still
        // needs them. The guard stops them on every way out of here.
        let transport_cancel = CancellationToken::new();
        let _stop_transport = transport_cancel.clone().drop_guard();
        let (transport_layer, udp_socket) = transport::build_transport_layer(&config, transport_cancel.clone()).await?;
        let endpoint = transport::build_endpoint(transport_layer.clone(), transport_cancel.clone());

        let registrar = transport::registrar_uri(&config)?;
        if config.transport == SipTransport::Tls {
            transport::warm_up_registrar_connection(&transport_layer, &registrar).await?;
        }

        let local_media_addr = local_media_addr(&transport_layer)?;

        let endpoint_inner_for_registration = endpoint.inner.clone();
        let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));
        let (state_tx, state_rx) = dialog_layer.new_dialog_state_channel();
        let incoming = endpoint.incoming_transactions()?;
        // Starts as our local address; the registration loop replaces the
        // host and port with the ones the registrar actually saw us come
        // from (see `registration.rs`), so that behind NAT the Contact in
        // our INVITEs and 200 OKs is one the PBX can reach.
        let contact = dialog_layer.build_local_contact(Some(config.username.clone()), None)?;
        let (contact_tx, contact_rx) = tokio::sync::watch::channel(contact);
        let dialog_config = config.clone();

        // Where NAT keepalives go: only meaningful for UDP, which has no
        // connection for the NAT to track otherwise.
        let keepalive = match udp_socket {
            Some(socket) => tokio::net::lookup_host((config.sip_server_host.as_str(), config.sip_server_port))
                .await
                .ok()
                .and_then(|mut addrs| addrs.next())
                .map(|addr| (socket, addr)),
            None => None,
        };

        let serve_task = tokio::spawn(async move {
            endpoint.serve().await;
        });

        let mut registration_task = tokio::spawn(registration::run_registration_loop(
            endpoint_inner_for_registration,
            config,
            event_tx.clone(),
            contact_tx,
            keepalive,
            cancel.child_token(),
        ));

        let incoming_task = tokio::spawn(dialog::incoming_request_loop(
            dialog_layer.clone(),
            incoming,
            state_tx.clone(),
            contact_rx.clone(),
        ));

        let dialog_state_task = tokio::spawn(dialog::dialog_state_loop(
            dialog_layer,
            state_rx,
            state_tx,
            event_tx,
            command_rx,
            local_media_addr,
            contact_rx,
            dialog_config,
        ));

        cancel.cancelled().await;
        // The registration loop un-REGISTERs on cancel; give that a moment
        // (it needs `serve_task` still running to get its reply) so the PBX
        // stops routing calls to a phone that is gone.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), &mut registration_task).await;
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
