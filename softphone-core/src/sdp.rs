use crate::error::{CoreError, Result};
use base64::Engine;
use sdp_rs::lines::attribute::{Attribute, Rtpmap};
use sdp_rs::lines::common::{Addrtype, Nettype};
use sdp_rs::lines::media::{MediaType, ProtoType};
use sdp_rs::lines::{Active, Connection, Media, Origin, SessionName, Version};
use sdp_rs::{MediaDescription, SessionDescription, Time};
use std::net::{IpAddr, SocketAddr};
use vec1::vec1;

/// Negotiated audio media parameters. `port` is a placeholder until
/// softphone-media reserves a real local port (see `dialog.rs`'s deferred
/// answer-SDP construction); no RTP/SRTP socket is opened by this crate.
#[derive(Debug, Clone)]
pub struct MediaOffer {
    pub port: u16,
    /// One or more RTP/AVP static payload types, in priority order. An
    /// *offer* we build lists every codec we're willing to use (see
    /// `SipAccountConfig::preferred_codecs`); an SDP *answer* — ours or the
    /// remote's — always settles on exactly one, so this ends up a
    /// single-element list once negotiation is done. See
    /// `select_payload_type` for picking one from a remote's offered list.
    pub payload_types: Vec<u8>,
    /// base64-encoded 30-byte AES_CM_128_HMAC_SHA1_80 master key + salt.
    /// `None` when negotiating plain RTP (no SDES-SRTP, RFC 4568).
    pub crypto_key: Option<String>,
    /// The peer's RTP socket address, parsed from their SDP connection line.
    /// `None` when building our own offer (no remote to speak of yet).
    pub remote_addr: Option<SocketAddr>,
    /// `true` marks this offer as a hold re-INVITE (`a=sendonly`); `false` is
    /// the normal `sendrecv` case (attribute omitted, matching prior wire
    /// behavior exactly).
    pub hold: bool,
}

pub fn generate_offer(port: u16, srtp: bool, hold: bool, payload_types: &[u8]) -> MediaOffer {
    let crypto_key = srtp.then(|| {
        let mut key_and_salt = [0u8; 30];
        rand::fill(&mut key_and_salt);
        base64::engine::general_purpose::STANDARD.encode(key_and_salt)
    });
    MediaOffer {
        port,
        payload_types: payload_types.to_vec(),
        crypto_key,
        remote_addr: None,
        hold,
    }
}

/// Picks which codec to actually use when `offered` (what the remote listed
/// in their SDP) and `preference` (our own codec priority order, see
/// `SipAccountConfig::preferred_codecs`) don't necessarily agree on order:
/// walks *our* preference order and returns the first entry that's also in
/// `offered`, so a call always uses the best codec both sides can agree on
/// by our own quality ranking rather than whichever the remote listed
/// first. Falls back to the remote's first offered type if none of our
/// preferred codecs were offered at all — better to attempt the call with
/// an unranked codec than to refuse it outright.
pub fn select_payload_type(offered: &[u8], preference: &[u8]) -> Option<u8> {
    preference
        .iter()
        .find(|pt| offered.contains(pt))
        .copied()
        .or_else(|| offered.first().copied())
}

/// RTP/AVP static payload type -> `rtpmap` encoding name (RFC 3551). Only
/// the two codecs this app actually implements are meaningful here; any
/// other value falls back to PCMU's name since `payload_type` is always one
/// we generated ourselves via `generate_offer` (never an arbitrary/unknown
/// ID from a remote offer, which `parse_offer` just carries through as-is).
fn encoding_name(payload_type: u8) -> &'static str {
    match payload_type {
        8 => "PCMA",
        _ => "PCMU",
    }
}

pub fn build_sdp(local_addr: IpAddr, offer: &MediaOffer) -> SessionDescription {
    let addrtype = match local_addr {
        IpAddr::V4(_) => Addrtype::Ip4,
        IpAddr::V6(_) => Addrtype::Ip6,
    };
    let session_id = rand::random::<u32>().to_string();

    let proto = if offer.crypto_key.is_some() {
        ProtoType::RtpSavp
    } else {
        ProtoType::RtpAvp
    };

    let mut attributes: Vec<Attribute> = offer
        .payload_types
        .iter()
        .map(|&pt| {
            Attribute::Rtpmap(Rtpmap {
                payload_type: pt as u32,
                encoding_name: encoding_name(pt).into(),
                clock_rate: 8000,
                encoding_params: None,
            })
        })
        .collect();
    if let Some(crypto_key) = &offer.crypto_key {
        attributes.push(Attribute::Other(
            "crypto".into(),
            Some(format!("1 AES_CM_128_HMAC_SHA1_80 inline:{crypto_key}")),
        ));
    }
    if offer.hold {
        attributes.push(Attribute::Sendonly);
    }

    SessionDescription {
        version: Version::V0,
        origin: Origin {
            username: "-".into(),
            sess_id: session_id.clone(),
            sess_version: session_id,
            nettype: Nettype::In,
            addrtype: addrtype.clone(),
            unicast_address: local_addr,
        },
        session_name: SessionName::new("OxideSip".into()),
        session_info: None,
        uri: None,
        emails: vec![],
        phones: vec![],
        connection: Some(Connection {
            nettype: Nettype::In,
            addrtype,
            connection_address: local_addr.into(),
        }),
        bandwidths: vec![],
        times: vec1![Time {
            active: Active { start: 0, stop: 0 },
            repeat: vec![],
            zone: None,
        }],
        key: None,
        attributes: vec![],
        media_descriptions: vec![MediaDescription {
            media: Media {
                media: MediaType::Audio,
                port: offer.port,
                num_of_ports: None,
                proto,
                fmt: offer
                    .payload_types
                    .iter()
                    .map(|pt| pt.to_string())
                    .collect::<Vec<_>>()
                    .join(" "),
            },
            info: None,
            connections: vec![],
            bandwidths: vec![],
            key: None,
            attributes,
        }],
    }
}

/// Parse the `m=audio`/`a=rtpmap`/`a=crypto` fields out of a peer's SDP body.
/// A missing `a=crypto` line is not an error — it just means the peer offered
/// plain RTP.
pub fn parse_offer(sdp_text: &str) -> Result<MediaOffer> {
    let sdp = SessionDescription::try_from(sdp_text)
        .map_err(|e| CoreError::Sdp(format!("failed to parse SDP: {e}")))?;

    let media = sdp
        .media_descriptions
        .iter()
        .find(|m| m.media.media == MediaType::Audio)
        .ok_or_else(|| CoreError::Sdp("no audio media line".into()))?;

    // Every payload type the peer listed, in the order *they* prioritized
    // them — `select_payload_type` is what reconciles this against our own
    // preference order when we're the one answering.
    let payload_types: Vec<u8> = media
        .media
        .fmt
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();
    if payload_types.is_empty() {
        return Err(CoreError::Sdp("empty or invalid fmt list".into()));
    }

    let crypto_key = media.attributes.iter().find_map(|attr| match attr {
        Attribute::Other(key, Some(value)) if key == "crypto" => value
            .split("inline:")
            .nth(1)
            .map(|s| s.split_whitespace().next().unwrap_or(s).to_string()),
        _ => None,
    });

    let ip = media
        .connections
        .first()
        .or(sdp.connection.as_ref())
        .map(|c| c.connection_address.base)
        .ok_or_else(|| CoreError::Sdp("no connection address (c=) in offer".into()))?;
    let remote_addr = Some(SocketAddr::new(ip, media.media.port));

    Ok(MediaOffer {
        port: media.media.port,
        payload_types,
        crypto_key,
        remote_addr,
        hold: false,
    })
}
