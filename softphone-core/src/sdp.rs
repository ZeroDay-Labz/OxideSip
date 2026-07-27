use crate::error::{CoreError, Result};
use base64::Engine;
use sdp_rs::lines::attribute::{Attribute, Rtpmap};
use sdp_rs::lines::common::{Addrtype, Nettype};
use sdp_rs::lines::media::{MediaType, ProtoType};
use sdp_rs::lines::{Active, Connection, Media, Origin, SessionName, Version};
use sdp_rs::{MediaDescription, SessionDescription, Time};
use std::net::{IpAddr, SocketAddr};
use vec1::vec1;

/// RFC 4733 §2.2 dynamic payload type for RTP `telephone-event`. Fixed
/// rather than negotiated from the dynamic range — 101 is the de facto
/// standard virtually every SIP stack/PBX uses, and this app doesn't use any
/// other dynamic payload type today, so collision risk is negligible.
pub const TELEPHONE_EVENT_PT: u8 = 101;

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
    /// Never includes `telephone_event_pt` — that's tracked separately so it
    /// can't accidentally be selected as an audio codec.
    pub payload_types: Vec<u8>,
    /// The RFC 4733 `telephone-event` payload type, if this offer/answer
    /// advertises RTP-based DTMF alongside the audio codec(s) above.
    pub telephone_event_pt: Option<u8>,
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

pub fn generate_offer(
    port: u16,
    srtp: bool,
    hold: bool,
    payload_types: &[u8],
    telephone_event: bool,
) -> MediaOffer {
    let crypto_key = srtp.then(|| {
        let mut key_and_salt = [0u8; 30];
        rand::fill(&mut key_and_salt);
        base64::engine::general_purpose::STANDARD.encode(key_and_salt)
    });
    MediaOffer {
        port,
        payload_types: payload_types.to_vec(),
        telephone_event_pt: telephone_event.then_some(TELEPHONE_EVENT_PT),
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
    if let Some(pt) = offer.telephone_event_pt {
        attributes.push(Attribute::Rtpmap(Rtpmap {
            payload_type: pt as u32,
            encoding_name: "telephone-event".into(),
            clock_rate: 8000,
            encoding_params: None,
        }));
        // Digits 0-15: 0-9, *, #, and the A-D "letter" events (RFC 4733 §3.2).
        attributes.push(Attribute::Other("fmtp".into(), Some(format!("{pt} 0-15"))));
    }
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
                    .chain(offer.telephone_event_pt.iter())
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

    // The peer's `a=rtpmap` for `telephone-event`, if any — pulled out
    // before the audio-codec list below so RFC 4733 DTMF is never mistaken
    // for (and never accidentally decoded as) an audio codec.
    let telephone_event_pt = media.attributes.iter().find_map(|attr| match attr {
        Attribute::Rtpmap(r) if r.encoding_name.eq_ignore_ascii_case("telephone-event") => {
            Some(r.payload_type as u8)
        }
        _ => None,
    });

    // Every payload type the peer listed, in the order *they* prioritized
    // them — `select_payload_type` is what reconciles this against our own
    // preference order when we're the one answering. Excludes
    // `telephone_event_pt`, which isn't an audio codec candidate.
    let payload_types: Vec<u8> = media
        .media
        .fmt
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .filter(|pt| Some(*pt) != telephone_event_pt)
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
        telephone_event_pt,
        crypto_key,
        remote_addr,
        hold: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn build_and_parse_round_trip_without_telephone_event() {
        let offer = generate_offer(10000, false, false, &[0, 8], false);
        let sdp = build_sdp(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), &offer);
        let parsed = parse_offer(&sdp.to_string()).unwrap();
        assert_eq!(parsed.payload_types, vec![0, 8]);
        assert_eq!(parsed.telephone_event_pt, None);
    }

    #[test]
    fn build_and_parse_round_trip_with_telephone_event() {
        let offer = generate_offer(10000, false, false, &[0, 8], true);
        assert_eq!(offer.telephone_event_pt, Some(TELEPHONE_EVENT_PT));

        let sdp = build_sdp(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), &offer);
        let sdp_text = sdp.to_string();
        assert!(sdp_text.contains("a=rtpmap:101 telephone-event/8000"));
        assert!(sdp_text.contains("a=fmtp:101 0-15"));

        let parsed = parse_offer(&sdp_text).unwrap();
        assert_eq!(parsed.payload_types, vec![0, 8]);
        assert_eq!(parsed.telephone_event_pt, Some(TELEPHONE_EVENT_PT));
    }

    #[test]
    fn parse_offer_handles_telephone_event_listed_first() {
        let sdp_text = "v=0\r\n\
            o=- 1 1 IN IP4 127.0.0.1\r\n\
            s=OxideSip\r\n\
            c=IN IP4 127.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 101 0 8\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=fmtp:101 0-15\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n";
        let parsed = parse_offer(sdp_text).unwrap();
        assert_eq!(parsed.payload_types, vec![0, 8]);
        assert_eq!(parsed.telephone_event_pt, Some(101));
    }

    #[test]
    fn parse_offer_with_only_telephone_event_errors_as_empty_codec_list() {
        let sdp_text = "v=0\r\n\
            o=- 1 1 IN IP4 127.0.0.1\r\n\
            s=OxideSip\r\n\
            c=IN IP4 127.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 101\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=fmtp:101 0-15\r\n";
        let err = parse_offer(sdp_text).unwrap_err();
        assert!(matches!(err, CoreError::Sdp(_)));
    }
}
