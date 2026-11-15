// Keep this module in sync with kext/ISCSIProtocol.h, including unused constants.
#![allow(dead_code)]

pub const ABI_VERSION: u64 = 1;
pub const PENDING_IO: usize = 32;
pub const CHUNKS_PER_IO: usize = 32;
pub const CHUNK_BYTES: usize = 1024 * 1024;
pub const MAX_REQUEST_BYTES: usize = CHUNKS_PER_IO * CHUNK_BYTES;
pub const QUEUE_ENTRIES: usize = 1024;
pub const PAGE_GRANULE: usize = 16384;
pub const DEFAULT_ARENA_BYTES: u64 = 64 * 1024 * 1024;

pub const METHOD_SET_GEOMETRY: u32 = 0;
pub const METHOD_COMPLETE_IO: u32 = 1;
pub const METHOD_SET_ONLINE: u32 = 2;
pub const METHOD_GET_CONFIG: u32 = 3;
pub const METHOD_COUNT: u32 = 4;

pub const MEMORY_ARENA: u32 = 0;
pub const MEMORY_REQUESTS: u32 = 1;
pub const TRANSFER_READ: u32 = 0;
pub const TRANSFER_WRITE: u32 = 1;
pub const STATUS_SUCCESS: u64 = 0;
pub const STATUS_FAILURE: u64 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Request {
    pub token: u32,
    pub chunk: u32,
    pub length: u32,
    pub block_size: u32,
    pub lba: u64,
    pub arena_offset: u64,
    pub direction: u32,
    pub reserved: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn request_matches_kernel_abi() {
        assert_eq!(size_of::<Request>(), 40);
        assert_eq!(offset_of!(Request, token), 0);
        assert_eq!(offset_of!(Request, chunk), 4);
        assert_eq!(offset_of!(Request, length), 8);
        assert_eq!(offset_of!(Request, block_size), 12);
        assert_eq!(offset_of!(Request, lba), 16);
        assert_eq!(offset_of!(Request, arena_offset), 24);
        assert_eq!(offset_of!(Request, direction), 32);
        assert_eq!(offset_of!(Request, reserved), 36);
    }
}
