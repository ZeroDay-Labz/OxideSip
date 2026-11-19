use crate::config::SipAccountConfig;
use crate::error::Result;
use crate::events::CoreEvent;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::registration::Registration;
use rsipstack::sip::{StatusCode, Uri};
use rsipstack::transaction::endpoint::EndpointInnerRef;
use rsipstack::transport::SipAddr;
use rsipstack::transport::udp::UdpConnection;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Initial/floor retry delay for transient failures — also the delay used
/// for the first couple of 401/407s before the auth-failure breaker trips.
const INITIAL_BACKOFF: Duration = Duration::from_secs(30);
/// Ceiling for the exponential backoff applied to transient (non-auth)
/// failures, so a prolonged network/server outage settles into a slow,
/// steady poll instead of hammering the registrar indefinitely at 30s.
const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// How many consecutive 401/407 responses (i.e. `Registration::register()`
/// already tried rsipstack's own one-shot digest challenge-response and
/// still came back unauthorized) are tolerated before giving up entirely.
/// This — not the per-request digest logic, which rsipstack already
/// handles correctly — is what was tripping FreePBX's Fail2ban: without a
/// cap, this loop retried a failing auth forever at a steady cadence,
/// reading indistinguishably from a brute-force scanner.
const MAX_CONSECUTIVE_AUTH_FAILURES: u32 = 3;

/// Longest wait between two REGISTERs however long the registrar granted:
/// it is also how the UI learns the server has gone away.
const MAX_REFRESH: Duration = Duration::from_secs(120);
const MIN_REFRESH: Duration = Duration::from_secs(20);
/// How often an empty CRLF packet is sent to the registrar over UDP so a NAT
/// router keeps the mapping incoming calls arrive through (RFC 5626 §3.5.1
/// style; routers commonly drop idle UDP mappings after 30s).
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(25);
/// How long to wait for the un-REGISTER on shutdown before giving up.
const UNREGISTER_TIMEOUT: Duration = Duration::from_millis(1500);

/// How long the registrar actually registered us for, read from its 200 OK:
/// the `expires` parameter of our Contact (recognised by `our_address`, the
/// host:port it knows us by — the response lists every device registered
/// on the account), else the shortest one listed, else the `Expires` header.
fn granted_expires(response: &str, our_address: Option<&str>) -> Option<u32> {
    let (mut ours, mut shortest, mut header) = (None, None::<u32>, None);
    for line in response.lines() {
        if line.trim().is_empty() {
            break; // end of headers
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "contact" | "m" => {
                for contact in value.split(',') {
                    let lower = contact.to_ascii_lowercase();
                    let Some(expires) = lower.split("expires=").nth(1).and_then(|rest| {
                        rest.chars()
                            .take_while(char::is_ascii_digit)
                            .collect::<String>()
                            .parse::<u32>()
                            .ok()
                    }) else {
                        continue;
                    };
                    if our_address.is_some_and(|addr| contact.contains(addr)) {
                        ours = Some(expires);
                    }
                    shortest = Some(shortest.map_or(expires, |s| s.min(expires)));
                }
            }
            "expires" => header = value.trim().parse().ok(),
            _ => {}
        }
    }
    ours.or(shortest).or(header)
}

fn grow_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_BACKOFF)
}

pub async fn run_registration_loop(
    endpoint_inner: EndpointInnerRef,
    config: SipAccountConfig,
    event_tx: mpsc::Sender<CoreEvent>,
    contact_tx: watch::Sender<Uri>,
    keepalive: Option<(UdpConnection, SocketAddr)>,
    cancel: CancellationToken,
) -> Result<()> {
    let registrar = crate::transport::registrar_uri(&config)?;

    let credential = Credential {
        username: config.username.clone(),
        password: config.password.clone(),
        realm: None,
    };
    let mut registration = Registration::new(endpoint_inner, Some(credential));

    let mut consecutive_auth_failures: u32 = 0;
    let mut backoff = INITIAL_BACKOFF;
    let mut registered = false;

    loop {
        let sent_at = Instant::now();
        let retry_after = match registration
            .register(registrar.clone(), Some(config.register_expires))
            .await
        {
            Ok(resp) if resp.status_code == StatusCode::OK => {
                let public_address = registration.discovered_public_address();
                // `registration.expires()` can't be used here: it only
                // reports a value when a Contact was set explicitly, and
                // otherwise a fixed 50s whatever the registrar said.
                let expires = granted_expires(
                    &resp.to_string(),
                    public_address.as_ref().map(|a| a.to_string()).as_deref(),
                )
                .unwrap_or(config.register_expires);
                let rtt_ms = sent_at.elapsed().as_millis() as u32;
                info!(expires, rtt_ms, "registered");
                event_tx.send(CoreEvent::Registered { expires, rtt_ms }).await.ok();
                consecutive_auth_failures = 0;
                backoff = INITIAL_BACKOFF;
                registered = true;
                // Advertise the address the registrar saw us come from
                // (Via received/rport) in call Contacts too.
                if let Some(address) = public_address {
                    contact_tx.send_if_modified(|contact| {
                        if contact.host_with_port == address {
                            return false;
                        }
                        info!(%address, "using the address the registrar sees for call contacts");
                        contact.host_with_port = address;
                        true
                    });
                }
                Duration::from_secs(u64::from(expires) * 3 / 4).clamp(MIN_REFRESH, MAX_REFRESH)
            }
            // FreePBX's (and most PBXes') way of saying "stop trying" —
            // typically an explicitly disabled/banned extension. Halting
            // outright, rather than retrying, is the whole point: a stuck
            // 403 retried every 30s indefinitely is exactly the pattern
            // Fail2ban flags as an attack.
            Ok(resp) if resp.status_code == StatusCode::Forbidden => {
                warn!(status = %resp.status_code, "registration forbidden, halting retries");
                event_tx
                    .send(CoreEvent::RegistrationHalted {
                        reason: "403 Forbidden — check credentials/account status".to_string(),
                    })
                    .await
                    .ok();
                return Ok(());
            }
            // A 401/407 reaching this point means `Registration::register()`
            // already attempted rsipstack's built-in digest challenge-
            // response once internally and still came back unauthorized —
            // i.e. this loop iteration represents one full failed auth
            // attempt, not a missing-Authorization-header retry.
            Ok(resp)
                if matches!(
                    resp.status_code,
                    StatusCode::Unauthorized | StatusCode::ProxyAuthenticationRequired
                ) =>
            {
                consecutive_auth_failures += 1;
                warn!(
                    status = %resp.status_code,
                    consecutive_auth_failures,
                    "registration unauthorized"
                );
                if consecutive_auth_failures >= MAX_CONSECUTIVE_AUTH_FAILURES {
                    event_tx
                        .send(CoreEvent::RegistrationHalted {
                            reason: format!("repeated {} — check username/password", resp.status_code),
                        })
                        .await
                        .ok();
                    return Ok(());
                }
                event_tx
                    .send(CoreEvent::RegistrationFailed {
                        reason: resp.status_code.to_string(),
                    })
                    .await
                    .ok();
                let after = backoff;
                backoff = grow_backoff(backoff);
                after
            }
            Ok(resp) => {
                warn!(status = %resp.status_code, "registration rejected");
                event_tx
                    .send(CoreEvent::RegistrationFailed {
                        reason: resp.status_code.to_string(),
                    })
                    .await
                    .ok();
                let after = backoff;
                backoff = grow_backoff(backoff);
                after
            }
            Err(e) => {
                warn!(error = %e, "registration error");
                event_tx
                    .send(CoreEvent::RegistrationFailed { reason: e.to_string() })
                    .await
                    .ok();
                let after = backoff;
                backoff = grow_backoff(backoff);
                after
            }
        };

        let next_register = tokio::time::sleep(retry_after);
        tokio::pin!(next_register);
        let mut keepalive_tick = tokio::time::interval(KEEPALIVE_INTERVAL);
        keepalive_tick.tick().await; // the first tick is immediate; we just sent a REGISTER
        loop {
            tokio::select! {
                _ = &mut next_register => break,
                _ = keepalive_tick.tick(), if keepalive.is_some() => {
                    if let Some((socket, registrar_addr)) = &keepalive {
                        let _ = socket.send_raw(b"\r\n\r\n", &SipAddr::from(*registrar_addr)).await;
                    }
                }
                _ = cancel.cancelled() => {
                    if registered {
                        // Expires: 0 removes this device's binding only.
                        match tokio::time::timeout(UNREGISTER_TIMEOUT, registration.register(registrar.clone(), Some(0))).await {
                            Ok(Ok(resp)) => info!(status = %resp.status_code, "unregistered"),
                            Ok(Err(e)) => warn!(error = %e, "unregister failed"),
                            Err(_) => warn!("unregister timed out"),
                        }
                    }
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::granted_expires;

    const OK: &str = "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 10.0.0.5:5060;rport=41000;received=203.0.113.9\r\n";

    #[test]
    fn our_own_contact_decides_even_when_others_expire_sooner() {
        let resp = format!(
            "{OK}Contact: <sip:6968@198.51.100.7:5060>;expires=12\r\nContact: <sip:6968@203.0.113.9:41000>;expires=600\r\nExpires: 3600\r\n\r\n"
        );
        assert_eq!(granted_expires(&resp, Some("203.0.113.9:41000")), Some(600));
    }

    #[test]
    fn several_contacts_on_one_line_and_upper_case_parameter() {
        let resp =
            format!("{OK}m: <sip:a@198.51.100.7>;Expires=90, <sip:a@203.0.113.9:41000>;q=0.5;EXPIRES=45\r\n\r\n");
        assert_eq!(granted_expires(&resp, Some("203.0.113.9:41000")), Some(45));
        assert_eq!(granted_expires(&resp, None), Some(45));
    }

    #[test]
    fn falls_back_to_the_expires_header_then_to_nothing() {
        let resp = format!("{OK}Contact: <sip:a@203.0.113.9:41000>\r\nExpires: 300\r\n\r\n");
        assert_eq!(granted_expires(&resp, Some("203.0.113.9:41000")), Some(300));
        assert_eq!(granted_expires(OK, None), None);
    }

    #[test]
    fn the_body_is_not_read_as_headers() {
        let resp = format!("{OK}\r\nExpires: 5\r\n");
        assert_eq!(granted_expires(&resp, None), None);
    }
}
