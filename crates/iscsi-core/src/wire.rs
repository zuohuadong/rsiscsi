//! Bounded RFC 7143 framing, login text and CHAP helpers.
use crate::Error;
use base64::{Engine, engine::general_purpose::STANDARD};
use md5::{Digest, Md5};
use tokio::io::{AsyncRead, AsyncReadExt};

pub const BHS_LEN: usize = 48;
pub const MAX_DATA_LEN: usize = 0x00ff_ffff;
pub const RESERVED_TAG: u32 = u32::MAX;
pub const NOP_OUT: u8 = 0x00;
pub const SCSI_COMMAND: u8 = 0x01;
pub const LOGIN_REQUEST: u8 = 0x03;
pub const TEXT_REQUEST: u8 = 0x04;
pub const DATA_OUT: u8 = 0x05;
pub const LOGOUT_REQUEST: u8 = 0x06;
pub const NOP_IN: u8 = 0x20;
pub const SCSI_RESPONSE: u8 = 0x21;
pub const LOGIN_RESPONSE: u8 = 0x23;
pub const TEXT_RESPONSE: u8 = 0x24;
pub const DATA_IN: u8 = 0x25;
pub const LOGOUT_RESPONSE: u8 = 0x26;
pub const R2T: u8 = 0x31;
pub const REJECT: u8 = 0x3f;

/// Exact 48-byte Basic Header Segment. Multi-byte fields are network endian.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bhs(pub [u8; BHS_LEN]);

impl Bhs {
    pub fn new(opcode: u8, flags: u8) -> Self {
        let mut bytes = [0; BHS_LEN];
        bytes[0] = opcode;
        bytes[1] = flags;
        Self(bytes)
    }
    pub fn opcode(&self) -> u8 {
        self.0[0] & 0x3f
    }
    pub fn data_len(&self) -> usize {
        (usize::from(self.0[5]) << 16) | (usize::from(self.0[6]) << 8) | usize::from(self.0[7])
    }
    pub fn set_data_len(&mut self, len: usize) -> Result<(), Error> {
        if len > MAX_DATA_LEN {
            return Err(Error::Protocol("DataSegmentLength exceeds 24 bits".into()));
        }
        let bytes = (len as u32).to_be_bytes();
        self.0[5..8].copy_from_slice(&bytes[1..4]);
        Ok(())
    }
    pub fn u32(&self, offset: usize) -> u32 {
        u32::from_be_bytes([
            self.0[offset],
            self.0[offset + 1],
            self.0[offset + 2],
            self.0[offset + 3],
        ])
    }
    pub fn put_u32(&mut self, offset: usize, value: u32) {
        self.0[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }
    pub fn itt(&self) -> u32 {
        self.u32(16)
    }
}

#[derive(Debug)]
pub struct Pdu {
    pub header: Bhs,
    pub data: Vec<u8>,
}

/// Append one complete PDU to a coalescing buffer.
pub fn encode(
    header: &Bhs,
    ahs: &[u8],
    data: &[u8],
    header_digest: bool,
    data_digest: bool,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    if !ahs.len().is_multiple_of(4) || ahs.len() > 1020 {
        return Err(Error::Protocol("invalid AHS length".into()));
    }
    let mut header = header.clone();
    header.set_data_len(data.len())?;
    header.0[4] = (ahs.len() / 4) as u8;
    let start = out.len();
    out.extend_from_slice(&header.0);
    out.extend_from_slice(ahs);
    if header_digest {
        out.extend_from_slice(&crc32c::crc32c(&out[start..]).to_le_bytes());
    }
    let start = out.len();
    out.extend_from_slice(data);
    out.resize(out.len() + padding(data.len()), 0);
    if data_digest && !data.is_empty() {
        out.extend_from_slice(&crc32c::crc32c(&out[start..]).to_le_bytes());
    }
    Ok(())
}

pub(crate) fn padding(len: usize) -> usize {
    (4 - len % 4) % 4
}

/// Read a bounded frame header, including AHS and header digest.
pub async fn read_header<R: AsyncRead + Unpin>(
    reader: &mut R,
    header_digest: bool,
    max_data_len: usize,
) -> Result<Bhs, Error> {
    read_header_tail(reader, Bhs([0; BHS_LEN]), 0, header_digest, max_data_len).await
}

pub(crate) async fn read_header_tail<R: AsyncRead + Unpin>(
    reader: &mut R,
    mut header: Bhs,
    received: usize,
    header_digest: bool,
    max_data_len: usize,
) -> Result<Bhs, Error> {
    reader.read_exact(&mut header.0[received..]).await?;
    if header.data_len() > max_data_len {
        return Err(Error::Protocol(
            "peer exceeded MaxRecvDataSegmentLength".into(),
        ));
    }
    let mut ahs = [0u8; 1020];
    let len = usize::from(header.0[4]) * 4;
    reader.read_exact(&mut ahs[..len]).await?;
    if header_digest {
        let actual = reader.read_u32_le().await?;
        let expected = crc32c::crc32c_append(crc32c::crc32c(&header.0), &ahs[..len]);
        if actual != expected {
            return Err(Error::Digest("header"));
        }
    }
    // None of the implemented target-to-initiator opcodes define AHS.
    if len != 0 {
        return Err(Error::Protocol("unsupported target AHS".into()));
    }
    Ok(header)
}

/// Receive directly into an owned command buffer; padding is never exposed.
pub async fn read_data<R: AsyncRead + Unpin>(
    reader: &mut R,
    data: &mut [u8],
    data_digest: bool,
) -> Result<(), Error> {
    reader.read_exact(data).await?;
    let mut pad = [0; 3];
    let pad_len = padding(data.len());
    reader.read_exact(&mut pad[..pad_len]).await?;
    if data_digest && !data.is_empty() {
        let actual = reader.read_u32_le().await?;
        let expected = crc32c::crc32c_append(crc32c::crc32c(data), &pad[..pad_len]);
        if expected != actual {
            return Err(Error::Digest("data"));
        }
    }
    Ok(())
}

pub async fn read_pdu<R: AsyncRead + Unpin>(
    reader: &mut R,
    header_digest: bool,
    data_digest: bool,
    limit: usize,
) -> Result<Pdu, Error> {
    let header = read_header(reader, header_digest, limit).await?;
    let mut data = vec![0; header.data_len()];
    read_data(reader, &mut data, data_digest).await?;
    Ok(Pdu { header, data })
}

/// Parse NUL-separated text. Split only the first '=', preserving values.
/// Quotes enclosing a whole value are accepted without splitting embedded '='.
pub fn parse_text(bytes: &[u8]) -> Result<Vec<(String, String)>, Error> {
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let text = std::str::from_utf8(s)
                .map_err(|_| Error::Protocol("non-UTF8 login text".into()))?;
            let (key, value) = text
                .split_once('=')
                .ok_or_else(|| Error::Protocol("text key missing '='".into()))?;
            if key.is_empty() {
                return Err(Error::Protocol("empty text key".into()));
            }
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value);
            Ok((key.to_ascii_lowercase(), value.to_owned()))
        })
        .collect()
}

pub(crate) fn text(pairs: &[(&str, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (key, value) in pairs {
        out.extend_from_slice(key.as_bytes());
        out.push(b'=');
        out.extend_from_slice(value.as_bytes());
        out.push(0);
    }
    out
}

/// RFC binary values: 0x is hexadecimal, 0b is base64 (not binary digits).
pub fn decode_binary(value: &str) -> Result<Vec<u8>, Error> {
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        if hex.len() % 2 != 0 {
            return Err(Error::Login("odd CHAP hex length".into()));
        }
        hex.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|s| {
                let s =
                    std::str::from_utf8(s).map_err(|_| Error::Login("invalid CHAP hex".into()))?;
                u8::from_str_radix(s, 16).map_err(|_| Error::Login("invalid CHAP hex".into()))
            })
            .collect()
    } else if let Some(encoded) = value
        .strip_prefix("0b")
        .or_else(|| value.strip_prefix("0B"))
    {
        STANDARD
            .decode(encoded)
            .map_err(|_| Error::Login("invalid CHAP base64".into()))
    } else {
        Err(Error::Login("CHAP challenge needs 0x or 0b prefix".into()))
    }
}

/// MD5(CHAP_I || secret || binary CHAP_C), per RFC 1994.
pub fn chap_response(identifier: u8, secret: &[u8], challenge: &[u8]) -> String {
    let mut md5 = Md5::new();
    md5.update([identifier]);
    md5.update(secret);
    md5.update(challenge);
    format!("0x{:x}", md5.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn all_opcodes_round_trip_digests_and_padding() {
        for op in [
            NOP_OUT,
            SCSI_COMMAND,
            LOGIN_REQUEST,
            TEXT_REQUEST,
            DATA_OUT,
            LOGOUT_REQUEST,
            NOP_IN,
            SCSI_RESPONSE,
            LOGIN_RESPONSE,
            TEXT_RESPONSE,
            DATA_IN,
            LOGOUT_RESPONSE,
            R2T,
        ] {
            for len in [0, 1, 3, 4, 5, 8192] {
                for digests in [false, true] {
                    let mut h = Bhs::new(op, 0x80);
                    h.put_u32(16, 0x12345678);
                    h.set_data_len(len).unwrap();
                    let data = vec![0x42; len];
                    let mut encoded = Vec::new();
                    encode(&h, &[], &data, digests, digests, &mut encoded).unwrap();
                    let result = read_pdu(&mut encoded.as_slice(), digests, digests, MAX_DATA_LEN)
                        .await
                        .unwrap();
                    assert_eq!(h, result.header);
                    assert_eq!(data, result.data);
                }
            }
        }
    }

    #[test]
    fn data_length_boundaries() {
        for len in [0, 255, 256, 65535, 65536, MAX_DATA_LEN] {
            let mut h = Bhs::new(DATA_IN, 0);
            h.set_data_len(len).unwrap();
            assert_eq!(h.data_len(), len);
            assert_eq!(h.0[4], 0);
        }
        assert!(Bhs::new(DATA_IN, 0).set_data_len(MAX_DATA_LEN + 1).is_err());
        assert_eq!(crc32c::crc32c(b"123456789"), 0xe3069283);
        // RFC 7143 Appendix A.4, digest wire order.
        assert_eq!(
            crc32c::crc32c(&[0; 32]).to_le_bytes(),
            [0xaa, 0x36, 0x91, 0x8a]
        );
    }

    #[tokio::test]
    async fn corrupt_digests_and_oversize_are_rejected() {
        let mut frame = Vec::new();
        encode(&Bhs::new(DATA_IN, 0), &[], b"abc", true, true, &mut frame).unwrap();
        let mut corrupted = frame.clone();
        corrupted[8] ^= 1;
        assert!(matches!(
            read_pdu(&mut corrupted.as_slice(), true, true, 100).await,
            Err(Error::Digest("header"))
        ));
        frame[52] ^= 1;
        assert!(matches!(
            read_pdu(&mut frame.as_slice(), true, true, 100).await,
            Err(Error::Digest("data"))
        ));
        assert!(
            read_pdu(&mut frame.as_slice(), true, true, 2)
                .await
                .is_err()
        );
    }

    #[test]
    fn text_and_chap_vectors() {
        let parsed = parse_text(b"HeAdErDiGeSt=None\0CHAP_C=\"0x12=ab\"\0Alias=a=b=c\0").unwrap();
        assert_eq!(parsed[0], ("headerdigest".into(), "None".into()));
        assert_eq!(parsed[1].1, "0x12=ab");
        assert_eq!(parsed[2].1, "a=b=c");
        assert!(parse_text(b"broken\0").is_err());
        // Constructed RFC 1994 layout: 0x01 || "secret" || 0x01020304.
        assert_eq!(
            chap_response(1, b"secret", &[1, 2, 3, 4]),
            "0xafd81c66f2d5f7fc733e1b2d88bb2630"
        );
        assert_eq!(decode_binary("0x01020304").unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(decode_binary("0bAQIDBA==").unwrap(), vec![1, 2, 3, 4]);
    }
}
