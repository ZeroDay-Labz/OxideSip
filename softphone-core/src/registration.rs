use crate::config::SipAccountConfig;
use crate::error::Result;
use crate::events::CoreEvent;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::registration::Registration;
use rsipstack::sip::StatusCode;
use rsipstack::transaction::endpoint::EndpointInnerRef;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
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

fn grow_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_BACKOFF)
}

pub async fn run_registration_loop(
    endpoint_inner: EndpointInnerRef,
    config: SipAccountConfig,
    event_tx: mpsc::Sender<CoreEvent>,
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

    loop {
        let sent_at = Instant::now();
        let retry_after = match registration
            .register(registrar.clone(), Some(config.register_expires))
            .await
        {
            Ok(resp) if resp.status_code == StatusCode::OK => {
                let expires = registration.expires();
                let rtt_ms = sent_at.elapsed().as_millis() as u32;
                info!(expires, rtt_ms, "registered");
                event_tx
                    .send(CoreEvent::Registered { expires, rtt_ms })
                    .await
                    .ok();
                consecutive_auth_failures = 0;
                backoff = INITIAL_BACKOFF;
                Duration::from_secs((expires.max(4) as u64 * 3) / 4)
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
                            reason: format!(
                                "repeated {} — check username/password",
                                resp.status_code
                            ),
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
                    .send(CoreEvent::RegistrationFailed {
                        reason: e.to_string(),
                    })
                    .await
                    .ok();
                let after = backoff;
                backoff = grow_backoff(backoff);
                after
            }
        };

        tokio::select! {
            _ = tokio::time::sleep(retry_after) => {}
            _ = cancel.cancelled() => return Ok(()),
        }
    }
}
