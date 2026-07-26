use crate::config::{SipAccountConfig, SipTransport};
use crate::error::{CoreError, Result};
use rsipstack::sip::{Transport, Uri};
use rsipstack::transaction::{Endpoint, EndpointBuilder};
use rsipstack::transport::tls::TlsConfig;
use rsipstack::transport::udp::UdpConnection;
use rsipstack::transport::{SipAddr, TcpListenerConnection, TransportLayer};
use std::convert::TryFrom;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio_util::sync::CancellationToken;

/// The registrar URI, with the transport forced via `;transport=<udp|tcp|tls>`
/// to match `config.transport` rather than relying on scheme/DNS guessing.
pub fn registrar_uri(config: &SipAccountConfig) -> Result<Uri> {
    let transport = match config.transport {
        SipTransport::Udp => "udp",
        SipTransport::Tcp => "tcp",
        SipTransport::Tls => "tls",
    };
    let uri_str = format!(
        "sip:{}:{};transport={}",
        config.sip_server_host, config.sip_server_port, transport
    );
    Uri::try_from(uri_str.as_str())
        .map_err(rsipstack::Error::from)
        .map_err(Into::into)
}

/// Build the transport layer for `config.transport`:
///
/// - UDP/TCP: bind a local listening socket up front and register it, so
///   `transport_layer.get_addrs()` (listens+connections) is non-empty
///   immediately — no warm-up connection needed before the first REGISTER.
/// - TLS: outbound-only, no local listener; see `warm_up_registrar_connection`.
pub async fn build_transport_layer(
    config: &SipAccountConfig,
    cancel_token: CancellationToken,
) -> Result<TransportLayer> {
    let transport_layer = TransportLayer::new(cancel_token.clone());
    let local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), config.local_port);

    match config.transport {
        SipTransport::Udp => {
            let udp =
                UdpConnection::create_connection(local_addr, None, Some(cancel_token)).await?;
            transport_layer.add_transport(udp.into());
        }
        SipTransport::Tcp => {
            let mut listen_addr = SipAddr::from(local_addr);
            listen_addr.r#type = Some(Transport::Tcp);
            let listener = TcpListenerConnection::new(listen_addr, None).await?;
            transport_layer.add_transport(listener.into());
        }
        SipTransport::Tls => {
            let ca_cert_path = config.ca_cert_path.as_ref().ok_or_else(|| {
                CoreError::Config("ca_cert_path is required for transport = \"tls\"".into())
            })?;
            let ca_certs = Some(std::fs::read(ca_cert_path)?);
            let client_cert = config
                .client_cert_path
                .as_ref()
                .map(std::fs::read)
                .transpose()?;
            let client_key = config
                .client_key_path
                .as_ref()
                .map(std::fs::read)
                .transpose()?;

            transport_layer.set_tls_config(TlsConfig {
                cert: None,
                key: None,
                client_cert,
                client_key,
                ca_certs,
                sni_hostname: None,
            });
        }
    }

    Ok(transport_layer)
}

pub fn build_endpoint(transport_layer: TransportLayer, cancel_token: CancellationToken) -> Endpoint {
    EndpointBuilder::new()
        .with_user_agent("OxideSip/0.1.2")
        .with_cancel_token(cancel_token)
        .with_transport_layer(transport_layer)
        .build()
}

/// Warm up an outbound TLS connection to `registrar` before the first
/// `Registration::register()` call. Only needed for the TLS transport.
///
/// `EndpointInner::get_via()` (called internally by `register()`) reads
/// `transport_layer.get_addrs()`, which is `listens + connections`. TLS has
/// no local listener (outbound-only), so `connections` is empty until a
/// `lookup()` call creates one — without this warm-up the very first
/// `register()` fails with `EndpointError("not sipaddrs")` before it ever
/// attempts a network connection. UDP/TCP don't need this: they bind a local
/// listener in `build_transport_layer`, so `listens` is already non-empty.
pub async fn warm_up_registrar_connection(
    transport_layer: &TransportLayer,
    registrar: &Uri,
) -> Result<()> {
    let target = SipAddr::try_from(registrar)?;
    transport_layer.lookup(&target, None).await?;
    Ok(())
}
