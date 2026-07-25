use crate::error::MediaError;

/// RFC 3550 §5.1 fixed 12-byte RTP header. We never send a CSRC list or
/// extension header, but `decode` still skips past a peer's CSRC list (per
/// the CC field) so we don't misparse the payload of a packet that has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    pub marker: bool,
    pub payload_type: u8,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

impl RtpHeader {
    pub fn encode(&self) -> [u8; 12] {
        let mut buf = [0u8; 12];
        buf[0] = 0x80; // V=2, P=0, X=0, CC=0
        buf[1] = ((self.marker as u8) << 7) | (self.payload_type & 0x7F);
        buf[2..4].copy_from_slice(&self.sequence_number.to_be_bytes());
        buf[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        buf[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
        buf
    }

    pub fn decode(data: &[u8]) -> Result<(RtpHeader, &[u8]), MediaError> {
        if data.len() < 12 {
            return Err(MediaError::RtpTooShort);
        }
        let version = data[0] >> 6;
        if version != 2 {
            return Err(MediaError::RtpBadVersion(version));
        }
        let cc = (data[0] & 0x0F) as usize;
        let header_len = 12 + cc * 4;
        if data.len() < header_len {
            return Err(MediaError::RtpTooShort);
        }
        let header = RtpHeader {
            marker: data[1] & 0x80 != 0,
            payload_type: data[1] & 0x7F,
            sequence_number: u16::from_be_bytes([data[2], data[3]]),
            timestamp: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
            ssrc: u32::from_be_bytes([data[8], data[9], data[10], data[11]]),
        };
        Ok((header, &data[header_len..]))
    }

    pub fn build_packet(&self, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + payload.len());
        out.extend_from_slice(&self.encode());
        out.extend_from_slice(payload);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_header() -> RtpHeader {
        RtpHeader {
            marker: true,
            payload_type: 0,
            sequence_number: 0x1234,
            timestamp: 0xDEAD_BEEF,
            ssrc: 0xCAFE_BABE,
        }
    }

    #[test]
    fn round_trips_through_build_packet() {
        let header = sample_header();
        let payload = [1u8, 2, 3, 4, 5];
        let packet = header.build_packet(&payload);

        let (decoded, rest) = RtpHeader::decode(&packet).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(rest, &payload);
    }

    #[test]
    fn payload_type_masks_to_7_bits() {
        let header = RtpHeader {
            payload_type: 0xFF,
            ..sample_header()
        };
        let (decoded, _) = RtpHeader::decode(&header.build_packet(&[])).unwrap();
        assert_eq!(decoded.payload_type, 0x7F);
    }

    #[test]
    fn decode_skips_csrc_list() {
        let mut packet = sample_header().encode().to_vec();
        packet[0] |= 0x02; // CC = 2
        packet.extend_from_slice(&[0u8; 8]); // two CSRC entries
        packet.extend_from_slice(&[9, 9, 9]); // payload

        let (_, rest) = RtpHeader::decode(&packet).unwrap();
        assert_eq!(rest, &[9, 9, 9]);
    }

    #[test]
    fn rejects_short_buffer() {
        assert!(matches!(
            RtpHeader::decode(&[0u8; 11]),
            Err(MediaError::RtpTooShort)
        ));
    }

    #[test]
    fn rejects_bad_version() {
        let mut packet = sample_header().encode().to_vec();
        packet[0] = 0x00; // version 0
        assert!(matches!(
            RtpHeader::decode(&packet),
            Err(MediaError::RtpBadVersion(0))
        ));
    }
}
