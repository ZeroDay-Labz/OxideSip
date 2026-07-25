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
                Duration::from_secs((expires.max(4) as u64 * 3) / 4)
            }
            Ok(resp) => {
                warn!(status = %resp.status_code, "registration rejected");
                event_tx
                    .send(CoreEvent::RegistrationFailed {
                        reason: resp.status_code.to_string(),
                    })
                    .await
                    .ok();
                Duration::from_secs(30)
            }
            Err(e) => {
                warn!(error = %e, "registration error");
                event_tx
                    .send(CoreEvent::RegistrationFailed {
                        reason: e.to_string(),
                    })
                    .await
                    .ok();
                Duration::from_secs(30)
            }
        };

        tokio::select! {
            _ = tokio::time::sleep(retry_after) => {}
            _ = cancel.cancelled() => return Ok(()),
        }
    }
}
