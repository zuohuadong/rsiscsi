use crate::Error;
use serde::Serialize;

/// Parsed standard INQUIRY and supported identification VPD pages.
#[derive(Debug, Clone, Serialize)]
pub struct InquiryData {
    pub vendor: String,
    pub product: String,
    pub revision: String,
    pub serial: Option<String>,
    /// Raw device identification descriptors from VPD 0x83.
    pub vpd83: Vec<u8>,
}

/// Number of logical blocks and bytes per logical block.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Capacity {
    pub blocks: u64,
    pub block_size: u32,
}

impl Capacity {
    pub fn bytes(&self) -> u128 {
        u128::from(self.blocks) * u128::from(self.block_size)
    }
}

/// Caching mode page (0x08) flags that drive write-cache policy.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct CacheMode {
    /// WCE: the target holds writes in a volatile cache.
    pub write_cache: bool,
    /// RCD: the target asks the initiator not to cache reads.
    pub read_cache_disabled: bool,
}

pub(crate) fn be32(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| Error::Protocol("truncated SCSI data".into()))?;
    Ok(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

pub(crate) fn be64(bytes: &[u8], offset: usize) -> Result<u64, Error> {
    Ok((u64::from(be32(bytes, offset)?) << 32) | u64::from(be32(bytes, offset + 4)?))
}

pub(crate) fn lun_bytes(lun: u64) -> Result<[u8; 8], Error> {
    // Peripheral and flat-space addressing for LUNs <= 16383. Preserve the
    // complete REPORT LUNS encoding for extended addresses.
    if lun <= 255 {
        Ok([0, lun as u8, 0, 0, 0, 0, 0, 0])
    } else if lun <= 16383 {
        Ok([0x40 | ((lun >> 8) as u8), lun as u8, 0, 0, 0, 0, 0, 0])
    } else if lun >> 56 >= 0xc0 {
        Ok(lun.to_be_bytes())
    } else {
        Err(Error::Config(
            "unsupported LUN addressing (use REPORT LUNS encoding)".into(),
        ))
    }
}

pub(crate) fn parse_luns(data: &[u8]) -> Result<Vec<u64>, Error> {
    let size = be32(data, 0)? as usize;
    if !size.is_multiple_of(8) || size + 8 > data.len() {
        return Err(Error::Protocol("truncated REPORT LUNS".into()));
    }
    let mut luns = Vec::new();
    for entry in data[8..8 + size].as_chunks::<8>().0 {
        let lun = match entry[0] >> 6 {
            0 if entry[0] == 0 && entry[2..].iter().all(|b| *b == 0) => u64::from(entry[1]),
            1 if entry[2..].iter().all(|b| *b == 0) => {
                (u64::from(entry[0] & 0x3f) << 8) | u64::from(entry[1])
            }
            _ => be64(entry, 0)?,
        };
        luns.push(lun);
    }
    Ok(luns)
}

pub(crate) fn inquiry(data: &[u8]) -> Result<InquiryData, Error> {
    if data.len() < 36 {
        return Err(Error::Protocol("short standard INQUIRY".into()));
    }
    let text = |range| String::from_utf8_lossy(&data[range]).trim().to_owned();
    Ok(InquiryData {
        vendor: text(8..16),
        product: text(16..32),
        revision: text(32..36),
        serial: None,
        vpd83: Vec::new(),
    })
}

pub(crate) fn vpd(data: &[u8], page: u8) -> Result<&[u8], Error> {
    if data.len() < 4 || data[1] != page {
        return Err(Error::Protocol("invalid VPD page".into()));
    }
    let len = usize::from(u16::from_be_bytes([data[2], data[3]]));
    data.get(4..4 + len)
        .ok_or_else(|| Error::Protocol("truncated VPD page".into()))
}

/// Parse the caching mode page returned by MODE SENSE(10).
///
/// The response starts with an 8-byte mode parameter header whose last two
/// bytes give the block descriptor length; the page follows it.
pub(crate) fn cache_mode(data: &[u8]) -> Result<CacheMode, Error> {
    if data.len() < 8 {
        return Err(Error::Protocol("short MODE SENSE response".into()));
    }
    let descriptor = usize::from(u16::from_be_bytes([data[6], data[7]]));
    let page = data
        .get(8 + descriptor..)
        .ok_or_else(|| Error::Protocol("truncated MODE SENSE response".into()))?;
    if page.first().is_none_or(|code| code & 0x3f != 0x08) {
        return Err(Error::Protocol("unexpected mode page".into()));
    }
    let flags = *page
        .get(2)
        .ok_or_else(|| Error::Protocol("truncated caching mode page".into()))?;
    Ok(CacheMode {
        write_cache: flags & 0x04 != 0,
        read_cache_disabled: flags & 0x01 != 0,
    })
}

pub(crate) fn rw_cdb(op: u8, lba: u64, blocks: u32) -> [u8; 16] {
    let mut cdb = [0; 16];
    cdb[0] = op;
    cdb[2..10].copy_from_slice(&lba.to_be_bytes());
    cdb[10..14].copy_from_slice(&blocks.to_be_bytes());
    cdb
}

pub(crate) fn unit_attention(error: &Error) -> bool {
    if let Error::Scsi { status: 2, sense } = error {
        match sense.first().map(|b| b & 0x7f) {
            Some(0x70 | 0x71) => sense.get(2).is_some_and(|s| s & 0xf == 6),
            Some(0x72 | 0x73) => sense.get(1).is_some_and(|s| s & 0xf == 6),
            _ => false,
        }
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lun_and_cdb_encoding() {
        assert_eq!(lun_bytes(1).unwrap(), [0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(lun_bytes(256).unwrap(), [0x41, 0, 0, 0, 0, 0, 0, 0]);
        let cdb = rw_cdb(0x88, 0x0102030405060708, 256);
        assert_eq!(&cdb[2..10], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&cdb[10..14], &[0, 0, 1, 0]);
        let mut list = vec![0, 0, 0, 8, 0, 0, 0, 0];
        list.extend_from_slice(&lun_bytes(1).unwrap());
        assert_eq!(parse_luns(&list).unwrap(), vec![1]);
    }

    #[test]
    fn caches_mode_page_flags() {
        // 8-byte header with no block descriptor, then page 0x08.
        let mut data = vec![0, 0, 0, 0, 0, 0, 0, 0, 0x08, 0x12, 0x05, 0, 0];
        let mode = cache_mode(&data).unwrap();
        assert!(mode.write_cache && mode.read_cache_disabled);
        data[10] = 0x00;
        let mode = cache_mode(&data).unwrap();
        assert!(!mode.write_cache && !mode.read_cache_disabled);
        // A block descriptor shifts the page.
        let mut shifted = vec![0, 0, 0, 0, 0, 0, 0, 8];
        shifted.extend_from_slice(&[0; 8]);
        shifted.extend_from_slice(&[0x08, 0x12, 0x04]);
        assert!(cache_mode(&shifted).unwrap().write_cache);
        assert!(cache_mode(&[0; 4]).is_err());
    }
}
