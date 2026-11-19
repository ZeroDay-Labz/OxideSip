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

/// The one SDES-SRTP crypto suite (RFC 4568 §6.2) this app implements.
pub const SRTP_SUITE: &str = "AES_CM_128_HMAC_SHA1_80";
/// Length of that suite's master key (16) + master salt (14).
const SRTP_KEY_AND_SALT_LEN: usize = 30;

/// A fresh random SRTP master key + salt, base64-encoded for `a=crypto`.
pub fn new_crypto_key() -> String {
    let mut key_and_salt = [0u8; SRTP_KEY_AND_SALT_LEN];
    rand::fill(&mut key_and_salt);
    base64::engine::general_purpose::STANDARD.encode(key_and_salt)
}

/// Which way audio flows, from the point of view of whoever wrote the SDP
/// (RFC 3264 §5.1). `SendRecv` is the default when no attribute is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MediaDirection {
    #[default]
    SendRecv,
    /// "I will send but not listen": how a party puts the other on hold.
    SendOnly,
    RecvOnly,
    Inactive,
}

impl MediaDirection {
    /// The direction to answer an offer of `self` with: the mirror image.
    pub fn answer(self) -> Self {
        match self {
            MediaDirection::SendRecv => MediaDirection::SendRecv,
            MediaDirection::SendOnly => MediaDirection::RecvOnly,
            MediaDirection::RecvOnly => MediaDirection::SendOnly,
            MediaDirection::Inactive => MediaDirection::Inactive,
        }
    }
}

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
    /// The `a=crypto` tag that goes with `crypto_key` (RFC 4568 §5.1). An
    /// answer must echo the tag of the offered line it accepted.
    pub crypto_tag: u32,
    /// The peer's RTP socket address, parsed from their SDP connection line.
    /// `None` when building our own offer (no remote to speak of yet).
    pub remote_addr: Option<SocketAddr>,
    /// `SendOnly` is how we put a call on hold; `SendRecv` is the normal case
    /// (attribute omitted on the wire). For a parsed SDP, what the peer
    /// declared — see `puts_us_on_hold`.
    pub direction: MediaDirection,
}

impl MediaOffer {
    /// Whether this (peer's) SDP stops them listening to us: `sendonly` or
    /// `inactive`, or the older `c=0.0.0.0` convention (RFC 2543).
    pub fn puts_us_on_hold(&self) -> bool {
        matches!(self.direction, MediaDirection::SendOnly | MediaDirection::Inactive)
            || self.remote_addr.is_some_and(|a| a.ip().is_unspecified())
    }
}

/// `crypto_key` is our SRTP key for this call (see `new_crypto_key`), or
/// `None` for plain RTP. It is passed in rather than generated here because
/// one call keeps one key: hold/resume re-INVITEs must re-offer the same one.
pub fn generate_offer(
    port: u16,
    crypto_key: Option<String>,
    hold: bool,
    payload_types: &[u8],
    telephone_event: bool,
) -> MediaOffer {
    MediaOffer {
        port,
        payload_types: payload_types.to_vec(),
        telephone_event_pt: telephone_event.then_some(TELEPHONE_EVENT_PT),
        crypto_key,
        crypto_tag: 1,
        remote_addr: None,
        direction: if hold {
            MediaDirection::SendOnly
        } else {
            MediaDirection::SendRecv
        },
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
            Some(format!("{} {SRTP_SUITE} inline:{crypto_key}", offer.crypto_tag)),
        ));
    }
    match offer.direction {
        MediaDirection::SendRecv => {}
        MediaDirection::SendOnly => attributes.push(Attribute::Sendonly),
        MediaDirection::RecvOnly => attributes.push(Attribute::Recvonly),
        // Not `Attribute::Inactive`: sdp-rs 0.2.1 only recognises that
        // variant under a misspelt name, so spell the attribute out.
        MediaDirection::Inactive => attributes.push(Attribute::Other("inactive".into(), None)),
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
/// plain RTP. An `RTP/SAVP` media line without a crypto line we can use *is*
/// one: the peer wants encryption and we can't key it.
pub fn parse_offer(sdp_text: &str) -> Result<MediaOffer> {
    let sdp =
        SessionDescription::try_from(sdp_text).map_err(|e| CoreError::Sdp(format!("failed to parse SDP: {e}")))?;

    let media = sdp
        .media_descriptions
        .iter()
        .find(|m| m.media.media == MediaType::Audio)
        .ok_or_else(|| CoreError::Sdp("no audio media line".into()))?;

    // The peer's `a=rtpmap` for `telephone-event`, if any — pulled out
    // before the audio-codec list below so RFC 4733 DTMF is never mistaken
    // for (and never accidentally decoded as) an audio codec.
    let telephone_event_pt = media.attributes.iter().find_map(|attr| match attr {
        Attribute::Rtpmap(r) if r.encoding_name.eq_ignore_ascii_case("telephone-event") => Some(r.payload_type as u8),
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

    let crypto = media.attributes.iter().find_map(|attr| match attr {
        Attribute::Other(key, Some(value)) if key == "crypto" => parse_crypto(value),
        _ => None,
    });
    if media.media.proto == ProtoType::RtpSavp && crypto.is_none() {
        return Err(CoreError::Sdp(format!(
            "RTP/SAVP without a usable {SRTP_SUITE} crypto line"
        )));
    }
    let (crypto_tag, crypto_key) = match crypto {
        Some((tag, key)) => (tag, Some(key)),
        None => (1, None),
    };

    // Media-level direction wins over session-level (RFC 4566 §6).
    let direction_of = |attrs: &[Attribute]| {
        attrs.iter().find_map(|attr| match attr {
            Attribute::Sendonly => Some(MediaDirection::SendOnly),
            Attribute::Recvonly => Some(MediaDirection::RecvOnly),
            Attribute::Sendrecv => Some(MediaDirection::SendRecv),
            Attribute::Inactive => Some(MediaDirection::Inactive),
            Attribute::Other(key, None) if key == "inactive" => Some(MediaDirection::Inactive),
            _ => None,
        })
    };
    let direction = direction_of(&media.attributes)
        .or_else(|| direction_of(&sdp.attributes))
        .unwrap_or_default();

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
        crypto_tag,
        remote_addr,
        direction,
    })
}

/// Parses one `a=crypto` value (`<tag> <suite> inline:<key>[|lifetime][|MKI]
/// [session params]`, RFC 4568 §9.1) into `(tag, base64 key)`. Returns `None`
/// for any suite other than `SRTP_SUITE`, for a key that isn't 30 bytes, and
/// for a line carrying an MKI (`|<id>:<len>`), which would change the packet
/// format in a way we don't implement — so a peer listing several lines gets
/// the first one we can actually honour.
fn parse_crypto(value: &str) -> Option<(u32, String)> {
    let mut parts = value.split_whitespace();
    let tag: u32 = parts.next()?.parse().ok()?;
    if parts.next()? != SRTP_SUITE {
        return None;
    }
    let mut key_params = parts.next()?.strip_prefix("inline:")?.split('|');
    let key = key_params.next()?;
    if key_params.any(|p| p.contains(':')) {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD.decode(key).ok()?;
    (decoded.len() == SRTP_KEY_AND_SALT_LEN).then(|| (tag, key.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn build_and_parse_round_trip_without_telephone_event() {
        let offer = generate_offer(10000, None, false, &[0, 8], false);
        let sdp = build_sdp(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), &offer);
        let parsed = parse_offer(&sdp.to_string()).unwrap();
        assert_eq!(parsed.payload_types, vec![0, 8]);
        assert_eq!(parsed.telephone_event_pt, None);
    }

    #[test]
    fn build_and_parse_round_trip_with_telephone_event() {
        let offer = generate_offer(10000, None, false, &[0, 8], true);
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

    const KEY: &str = "WVNfX19zZW1jdGwgKCkgewkyMjA7fQp9CnVubGVz";

    fn offer_with(media_line: &str, attrs: &str) -> String {
        format!("v=0\r\no=- 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n{media_line}\r\n{attrs}")
    }

    #[test]
    fn crypto_line_with_lifetime_and_extra_suites_is_parsed() {
        let sdp = offer_with(
            "m=audio 4000 RTP/SAVP 0",
            &format!(
                "a=crypto:1 AES_256_CM_HMAC_SHA1_80 inline:{KEY}\r\n\
                 a=crypto:2 AES_CM_128_HMAC_SHA1_80 inline:{KEY}|2^31 UNENCRYPTED_SRTCP\r\n\
                 a=crypto:3 AES_CM_128_HMAC_SHA1_32 inline:{KEY}\r\n"
            ),
        );
        let parsed = parse_offer(&sdp).unwrap();
        assert_eq!(parsed.crypto_key.as_deref(), Some(KEY));
        assert_eq!(parsed.crypto_tag, 2);
    }

    #[test]
    fn savp_without_a_usable_crypto_line_is_rejected() {
        for attrs in [
            String::new(),
            format!("a=crypto:1 AES_CM_128_HMAC_SHA1_32 inline:{KEY}\r\n"),
            format!("a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{KEY}|2^31|1:4\r\n"),
            "a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:dG9vIHNob3J0\r\n".to_string(),
        ] {
            let sdp = offer_with("m=audio 4000 RTP/SAVP 0", &attrs);
            assert!(
                matches!(parse_offer(&sdp), Err(CoreError::Sdp(_))),
                "accepted: {attrs:?}"
            );
        }
    }

    #[test]
    fn answer_echoes_the_offered_crypto_tag() {
        let mut answer = generate_offer(10000, Some(new_crypto_key()), false, &[0], false);
        answer.crypto_tag = 2;
        let text = build_sdp("192.168.1.50".parse().unwrap(), &answer).to_string();
        assert!(text.contains("RTP/SAVP"));
        assert!(text.contains("a=crypto:2 AES_CM_128_HMAC_SHA1_80 inline:"));
    }

    #[test]
    fn our_preference_order_picks_the_codec() {
        assert_eq!(select_payload_type(&[0, 8], &[8, 0]), Some(8));
        assert_eq!(select_payload_type(&[9, 8], &[0, 8]), Some(8));
        assert_eq!(select_payload_type(&[9], &[0, 8]), Some(9));
        assert_eq!(select_payload_type(&[], &[0, 8]), None);
    }

    #[test]
    fn direction_round_trips_and_detects_hold() {
        for (direction, held) in [
            (MediaDirection::SendRecv, false),
            (MediaDirection::SendOnly, true),
            (MediaDirection::RecvOnly, false),
            (MediaDirection::Inactive, true),
        ] {
            let mut offer = generate_offer(10000, None, false, &[0], false);
            offer.direction = direction;
            let text = build_sdp("192.168.1.50".parse().unwrap(), &offer).to_string();
            let parsed = parse_offer(&text).unwrap();
            assert_eq!(parsed.direction, direction, "{text}");
            assert_eq!(parsed.puts_us_on_hold(), held);
        }
        assert_eq!(MediaDirection::SendOnly.answer(), MediaDirection::RecvOnly);
        assert_eq!(MediaDirection::Inactive.answer(), MediaDirection::Inactive);
    }

    #[test]
    fn session_level_direction_and_zero_address_mean_hold() {
        let session_level = "v=0\r\no=- 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\na=sendonly\r\nm=audio 4000 RTP/AVP 0\r\n";
        assert!(parse_offer(session_level).unwrap().puts_us_on_hold());
        let zero = "v=0\r\no=- 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\n";
        assert!(parse_offer(zero).unwrap().puts_us_on_hold());
    }
}
