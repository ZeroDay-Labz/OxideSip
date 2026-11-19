//! SRTP (RFC 3711) for the one suite SDES negotiation in `softphone-core`
//! offers: `AES_CM_128_HMAC_SHA1_80` — AES-128 in counter mode over the RTP
//! payload, and a 10-byte HMAC-SHA1 tag over the whole packet.
//!
//! One [`SrtpContext`] per direction per call: each side encrypts with its
//! own key (the one in its own `a=crypto` line) and decrypts with the peer's.
//! Only RTP is handled; this app sends no RTCP.

use crate::error::MediaError;
use aes::Aes128;
use aes::cipher::{KeyIvInit, StreamCipher};
use base64::Engine;
use hmac::{Hmac, Mac};
use sha1::Sha1;

type AesCtr = ctr::Ctr128BE<Aes128>;
type HmacSha1 = Hmac<Sha1>;

const MASTER_KEY_LEN: usize = 16;
const MASTER_SALT_LEN: usize = 14;
const AUTH_KEY_LEN: usize = 20;
/// Length of the authentication tag appended to every packet.
pub const AUTH_TAG_LEN: usize = 10;
/// How far behind the newest packet an old one may arrive and still be
/// accepted (RFC 3711 §3.3.2 requires at least 64).
const REPLAY_WINDOW: u64 = 64;

/// RFC 3711 §4.3.1 key derivation labels.
const LABEL_ENCRYPTION: u8 = 0x00;
const LABEL_AUTH: u8 = 0x01;
const LABEL_SALT: u8 = 0x02;

/// The per-stream receive state of RFC 3711 §3.3.1: where in the 48-bit
/// packet index space the stream is, and which recent indexes were seen.
#[derive(Clone, Copy, Default)]
struct StreamState {
    ssrc: u32,
    /// Rollover counter: how many times the 16-bit sequence number wrapped.
    roc: u32,
    /// Highest sequence number authenticated so far (`s_l`).
    highest_seq: u16,
    /// Bit `n` set = packet index `highest - n` was already received.
    replay_mask: u64,
    started: bool,
}

pub struct SrtpContext {
    session_key: [u8; MASTER_KEY_LEN],
    session_salt: [u8; MASTER_SALT_LEN],
    auth_key: [u8; AUTH_KEY_LEN],
    stream: StreamState,
}

impl SrtpContext {
    /// `key` is the base64 `inline:` value of an `a=crypto` line: a 16-byte
    /// master key followed by a 14-byte master salt.
    pub fn new(key: &str) -> Result<Self, MediaError> {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(key)
            .map_err(|_| MediaError::Srtp("crypto key is not valid base64"))?;
        if decoded.len() != MASTER_KEY_LEN + MASTER_SALT_LEN {
            return Err(MediaError::Srtp("crypto key is not 30 bytes"));
        }
        let (master_key, master_salt) = decoded.split_at(MASTER_KEY_LEN);
        Ok(Self::from_master(
            master_key.try_into().unwrap(),
            master_salt.try_into().unwrap(),
        ))
    }

    fn from_master(master_key: &[u8; MASTER_KEY_LEN], master_salt: &[u8; MASTER_SALT_LEN]) -> Self {
        let mut session_key = [0u8; MASTER_KEY_LEN];
        let mut session_salt = [0u8; MASTER_SALT_LEN];
        let mut auth_key = [0u8; AUTH_KEY_LEN];
        derive(master_key, master_salt, LABEL_ENCRYPTION, &mut session_key);
        derive(master_key, master_salt, LABEL_AUTH, &mut auth_key);
        derive(master_key, master_salt, LABEL_SALT, &mut session_salt);
        Self {
            session_key,
            session_salt,
            auth_key,
            stream: StreamState::default(),
        }
    }

    /// Turns a plain RTP packet into an SRTP one in place: encrypts the
    /// payload and appends the authentication tag. Sequence numbers must be
    /// passed in sending order (it is how the rollover counter is tracked).
    pub fn protect(&mut self, packet: &mut Vec<u8>) -> Result<(), MediaError> {
        let header_len = rtp_header_len(packet)?;
        let seq = u16::from_be_bytes([packet[2], packet[3]]);
        let ssrc = u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]);

        let stream = &mut self.stream;
        if !stream.started || stream.ssrc != ssrc {
            *stream = StreamState {
                ssrc,
                started: true,
                ..StreamState::default()
            };
        } else if seq < stream.highest_seq {
            stream.roc = stream.roc.wrapping_add(1);
        }
        stream.highest_seq = seq;
        let roc = stream.roc;

        self.keystream(ssrc, roc, seq, &mut packet[header_len..]);
        let tag = self.tag(packet, roc);
        packet.extend_from_slice(&tag);
        Ok(())
    }

    /// Authenticates and decrypts an SRTP packet in place. Returns the
    /// length of the plain RTP packet now at the start of `packet`. Fails,
    /// leaving nothing usable, for anything not sent by the holder of the
    /// key: a wrong tag, or a packet already received (replay).
    pub fn unprotect(&mut self, packet: &mut [u8]) -> Result<usize, MediaError> {
        let header_len = rtp_header_len(packet)?;
        if packet.len() < header_len + AUTH_TAG_LEN {
            return Err(MediaError::RtpTooShort);
        }
        let seq = u16::from_be_bytes([packet[2], packet[3]]);
        let ssrc = u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]);

        // A different SSRC is a new stream (RFC 3711 §3.2.3 keeps state per
        // SSRC) — e.g. the PBX switching from early media to the answered
        // call. It starts from a clean state, and only replaces the current
        // one below, once a packet from it has authenticated.
        let mut stream = if self.stream.started && self.stream.ssrc == ssrc {
            self.stream
        } else {
            StreamState {
                ssrc,
                highest_seq: seq,
                ..StreamState::default()
            }
        };

        // RFC 3711 §3.3.1 / appendix A: guess which rollover period `seq`
        // belongs to from how far it is from the highest one seen.
        let roc = if !stream.started {
            0
        } else if stream.highest_seq < 0x8000 {
            if seq > stream.highest_seq && seq - stream.highest_seq > 0x8000 {
                stream.roc.wrapping_sub(1)
            } else {
                stream.roc
            }
        } else if stream.highest_seq - 0x8000 > seq {
            stream.roc.wrapping_add(1)
        } else {
            stream.roc
        };
        let index = (u64::from(roc) << 16) | u64::from(seq);
        let highest = (u64::from(stream.roc) << 16) | u64::from(stream.highest_seq);

        if stream.started && index <= highest {
            let behind = highest - index;
            if behind >= REPLAY_WINDOW || stream.replay_mask & (1 << behind) != 0 {
                return Err(MediaError::Srtp("replayed or too-old packet"));
            }
        }

        let (authenticated, tag) = packet.split_at(packet.len() - AUTH_TAG_LEN);
        let mut mac = HmacSha1::new_from_slice(&self.auth_key).expect("hmac accepts any key length");
        mac.update(authenticated);
        mac.update(&roc.to_be_bytes());
        // Constant-time comparison.
        mac.verify_truncated_left(tag)
            .map_err(|_| MediaError::Srtp("authentication failed"))?;

        if !stream.started || index > highest {
            let ahead = if stream.started { index - highest } else { 0 };
            stream.replay_mask = if ahead >= REPLAY_WINDOW {
                1
            } else {
                (stream.replay_mask << ahead) | 1
            };
            stream.roc = roc;
            stream.highest_seq = seq;
            stream.started = true;
        } else {
            stream.replay_mask |= 1 << (highest - index);
        }
        self.stream = stream;

        let plain_len = packet.len() - AUTH_TAG_LEN;
        self.keystream(ssrc, roc, seq, &mut packet[header_len..plain_len]);
        Ok(plain_len)
    }

    /// XORs the AES-CM keystream for packet (`ssrc`, `roc`, `seq`) over
    /// `payload` — encryption and decryption are the same operation.
    fn keystream(&self, ssrc: u32, roc: u32, seq: u16, payload: &mut [u8]) {
        // RFC 3711 §4.1.1: IV = (salt * 2^16) XOR (SSRC * 2^64) XOR (index * 2^16)
        let mut iv = [0u8; 16];
        iv[..MASTER_SALT_LEN].copy_from_slice(&self.session_salt);
        for (b, s) in iv[4..8].iter_mut().zip(ssrc.to_be_bytes()) {
            *b ^= s;
        }
        for (b, r) in iv[8..12].iter_mut().zip(roc.to_be_bytes()) {
            *b ^= r;
        }
        for (b, s) in iv[12..14].iter_mut().zip(seq.to_be_bytes()) {
            *b ^= s;
        }
        AesCtr::new(&self.session_key.into(), &iv.into()).apply_keystream(payload);
    }

    /// The tag for `data` (header + encrypted payload) under rollover
    /// counter `roc` (RFC 3711 §4.2: the ROC is authenticated but not sent).
    fn tag(&self, data: &[u8], roc: u32) -> [u8; AUTH_TAG_LEN] {
        let mut mac = HmacSha1::new_from_slice(&self.auth_key).expect("hmac accepts any key length");
        mac.update(data);
        mac.update(&roc.to_be_bytes());
        let full = mac.finalize().into_bytes();
        full[..AUTH_TAG_LEN].try_into().unwrap()
    }
}

/// RFC 3711 §4.3.1 with a key derivation rate of 0: the keystream of
/// AES-CM under the master key, with IV `(master_salt XOR label) * 2^16`.
fn derive(master_key: &[u8; MASTER_KEY_LEN], master_salt: &[u8; MASTER_SALT_LEN], label: u8, out: &mut [u8]) {
    let mut iv = [0u8; 16];
    iv[..MASTER_SALT_LEN].copy_from_slice(master_salt);
    iv[7] ^= label;
    out.fill(0);
    AesCtr::new(master_key.into(), &iv.into()).apply_keystream(out);
}

/// Length of the RTP header including CSRC list and header extension — the
/// part SRTP authenticates but leaves in the clear.
fn rtp_header_len(packet: &[u8]) -> Result<usize, MediaError> {
    if packet.len() < 12 {
        return Err(MediaError::RtpTooShort);
    }
    let mut len = 12 + 4 * usize::from(packet[0] & 0x0F);
    if packet[0] & 0x10 != 0 {
        if packet.len() < len + 4 {
            return Err(MediaError::RtpTooShort);
        }
        len += 4 + 4 * usize::from(u16::from_be_bytes([packet[len + 2], packet[len + 3]]));
    }
    if packet.len() < len {
        return Err(MediaError::RtpTooShort);
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::RtpHeader;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// RFC 3711 appendix B.3.
    const MASTER_KEY: &str = "E1F97A0D3E018BE0D64FA32C06DE4139";
    const MASTER_SALT: &str = "0EC675AD498AFEEBB6960B3AABE6";

    fn rfc_context() -> SrtpContext {
        SrtpContext::from_master(
            hex(MASTER_KEY).as_slice().try_into().unwrap(),
            hex(MASTER_SALT).as_slice().try_into().unwrap(),
        )
    }

    fn context_pair() -> (SrtpContext, SrtpContext) {
        (rfc_context(), rfc_context())
    }

    fn packet(seq: u16, payload: &[u8]) -> Vec<u8> {
        RtpHeader {
            marker: false,
            payload_type: 0,
            sequence_number: seq,
            timestamp: u32::from(seq) * 160,
            ssrc: 0xCAFE_BABE,
        }
        .build_packet(payload)
    }

    #[test]
    fn aes_cm_keystream_matches_rfc_3711_b2() {
        let mut out = [0u8; 32];
        AesCtr::new(
            hex("2B7E151628AED2A6ABF7158809CF4F3C").as_slice().into(),
            hex("F0F1F2F3F4F5F6F7F8F9FAFBFCFD0000").as_slice().into(),
        )
        .apply_keystream(&mut out);
        assert_eq!(
            out.to_vec(),
            hex("E03EAD0935C95E80E166B16DD92B4EB4D23513162B02D0F72A43A2FE4A5F97AB")
        );
    }

    #[test]
    fn key_derivation_matches_rfc_3711_b3() {
        let ctx = rfc_context();
        assert_eq!(ctx.session_key.to_vec(), hex("C61E7A93744F39EE10734AFE3FF7A087"));
        assert_eq!(ctx.session_salt.to_vec(), hex("30CBBC08863D8C85D49DB34A9AE1"));
        assert_eq!(ctx.auth_key.to_vec(), hex("CEBE321F6FF7716B6FD4AB49AF256A156D38BAA4"));
    }

    #[test]
    fn round_trips_and_hides_the_payload() {
        let (mut tx, mut rx) = context_pair();
        let payload = [0x55u8; 160];
        let mut p = packet(1000, &payload);
        tx.protect(&mut p).unwrap();
        assert_eq!(p.len(), 12 + 160 + AUTH_TAG_LEN);
        assert_eq!(&p[..12], &packet(1000, &payload)[..12], "header stays in the clear");
        assert_ne!(&p[12..172], &payload[..], "payload must be encrypted");

        let len = rx.unprotect(&mut p).unwrap();
        assert_eq!(&p[..len], &packet(1000, &payload)[..]);
    }

    #[test]
    fn tampered_or_wrong_key_packets_are_rejected() {
        let (mut tx, mut rx) = context_pair();
        let mut good = packet(1, &[1, 2, 3, 4]);
        tx.protect(&mut good).unwrap();

        for flip in [0, 5, 13, good.len() - 1] {
            let mut bad = good.clone();
            bad[flip] ^= 0x01;
            assert!(
                rx.unprotect(&mut bad).is_err(),
                "accepted a packet with byte {flip} flipped"
            );
        }
        let mut other = SrtpContext::new("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap();
        assert!(other.unprotect(&mut good.clone()).is_err());
        // A plain RTP packet is not accepted either.
        assert!(rx.unprotect(&mut packet(2, &[0u8; 160])).is_err());
        // None of that disturbed the real stream.
        assert!(rx.unprotect(&mut good).is_ok());
    }

    #[test]
    fn replays_are_rejected_but_reordering_is_not() {
        let (mut tx, mut rx) = context_pair();
        let mut packets: Vec<Vec<u8>> = (10..14u16)
            .map(|seq| {
                let mut p = packet(seq, &[seq as u8; 8]);
                tx.protect(&mut p).unwrap();
                p
            })
            .collect();
        for i in [0, 2, 1, 3] {
            assert!(rx.unprotect(&mut packets[i].clone()).is_ok(), "packet {i}");
        }
        for p in &mut packets {
            assert!(rx.unprotect(&mut p.clone()).is_err(), "replay accepted");
        }
    }

    #[test]
    fn sequence_number_wrap_keeps_decrypting() {
        let (mut tx, mut rx) = context_pair();
        // 65530..=65535 then 0..=5, with the two around the wrap swapped.
        let seqs: Vec<u16> = (65530..=65535u16).chain(0..=5).collect();
        let mut packets: Vec<(u16, Vec<u8>)> = seqs
            .iter()
            .map(|&seq| {
                let mut p = packet(seq, &seq.to_be_bytes());
                tx.protect(&mut p).unwrap();
                (seq, p)
            })
            .collect();
        packets.swap(5, 6);
        for (seq, p) in &mut packets {
            let len = rx.unprotect(p).unwrap_or_else(|e| panic!("seq {seq}: {e}"));
            assert_eq!(&p[12..len], &seq.to_be_bytes());
        }
        assert_eq!((tx.stream.roc, rx.stream.roc), (1, 1));
    }

    #[test]
    fn a_new_ssrc_starts_a_new_stream() {
        let (mut tx, mut rx) = context_pair();
        let mut first = packet(40_000, &[1; 4]);
        tx.protect(&mut first).unwrap();
        rx.unprotect(&mut first).unwrap();

        // Same key, new SSRC, sequence numbers starting over.
        let mut second = RtpHeader {
            marker: true,
            payload_type: 0,
            sequence_number: 7,
            timestamp: 0,
            ssrc: 0x1234_5678,
        }
        .build_packet(&[2; 4]);
        tx.protect(&mut second).unwrap();
        let len = rx.unprotect(&mut second).unwrap();
        assert_eq!(&second[12..len], &[2; 4]);
    }

    #[test]
    fn bad_keys_are_refused() {
        assert!(SrtpContext::new("not base64!").is_err());
        assert!(SrtpContext::new("dG9vIHNob3J0").is_err());
    }
}
