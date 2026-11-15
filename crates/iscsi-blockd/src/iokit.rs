use crate::abi::{self, Request};
use anyhow::{Context, Result, ensure};
use std::{
    ffi::{c_char, c_void},
    ops::Range,
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};
use tokio::time::Instant;
use tracing::{error, warn};

const QUEUE_HEADER_BYTES: usize = 12;
const ENTRY_HEADER_BYTES: usize = 4;
const MIN_ARENA_BYTES: u64 = 4 * 1024 * 1024;
const IO_MAP_ANYWHERE: u32 = 1;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> u32;
    fn IOServiceOpen(service: u32, task: u32, kind: u32, connect: *mut u32) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IOServiceClose(connect: u32) -> i32;
    fn IOConnectCallScalarMethod(
        connect: u32,
        selector: u32,
        input: *const u64,
        input_count: u32,
        output: *mut u64,
        output_count: *mut u32,
    ) -> i32;
    fn IOConnectMapMemory64(
        connect: u32,
        memory_type: u32,
        task: u32,
        address: *mut u64,
        size: *mut u64,
        options: u32,
    ) -> i32;
    fn IOConnectUnmapMemory64(connect: u32, memory_type: u32, task: u32, address: u64) -> i32;
}

// The requested libc API is deprecated in favor of an additional crate.
#[allow(deprecated)]
fn task_port() -> u32 {
    unsafe { libc::mach_task_self() }
}

fn check_return(operation: &str, code: i32) -> Result<()> {
    ensure!(code == 0, "{operation}: IOKit error {:#010x}", code as u32);
    Ok(())
}

pub struct UserClient {
    port: u32,
    detached: AtomicBool,
}

impl UserClient {
    pub async fn connect() -> Result<Arc<Self>> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            // GetMatchingService consumes this dictionary on every attempt.
            let matching = unsafe { IOServiceMatching(c"ISCSIVirtualDisk".as_ptr()) };
            ensure!(!matching.is_null(), "IOServiceMatching failed");
            let service = unsafe { IOServiceGetMatchingService(0, matching) };
            if service != 0 {
                return Self::open(service);
            }
            ensure!(
                Instant::now() < deadline,
                "ISCSIVirtualDisk 未出现；请先运行 sudo ./scripts/load-kext.sh，再重试"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    fn open(service: u32) -> Result<Arc<Self>> {
        let mut port = 0;
        let code = unsafe { IOServiceOpen(service, task_port(), 0, &mut port) };
        if let Err(failure) = check_return("IOObjectRelease", unsafe { IOObjectRelease(service) }) {
            warn!(%failure, "无法释放 service 引用");
        }
        check_return("IOServiceOpen", code)?;
        ensure!(port != 0, "IOServiceOpen returned a null connection");
        Ok(Arc::new(Self {
            port,
            detached: AtomicBool::new(false),
        }))
    }

    fn scalar(&self, selector: u32, input: &[u64], output: &mut [u64]) -> Result<()> {
        let mut output_count = u32::try_from(output.len())?;
        let code = unsafe {
            IOConnectCallScalarMethod(
                self.port,
                selector,
                if input.is_empty() {
                    ptr::null()
                } else {
                    input.as_ptr()
                },
                u32::try_from(input.len())?,
                if output.is_empty() {
                    ptr::null_mut()
                } else {
                    output.as_mut_ptr()
                },
                &mut output_count,
            )
        };
        check_return(&format!("IOConnectCallScalarMethod({selector})"), code)?;
        ensure!(
            output_count as usize == output.len(),
            "unexpected scalar output count"
        );
        Ok(())
    }

    pub fn config(&self, mapped_bytes: usize) -> Result<Config> {
        let mut scalars = [0; 4];
        self.scalar(abi::METHOD_GET_CONFIG, &[], &mut scalars)?;
        Config::parse(scalars, mapped_bytes)
    }

    pub fn set_geometry(&self, block_size: u32, blocks: u64) -> Result<()> {
        self.scalar(
            abi::METHOD_SET_GEOMETRY,
            &[u64::from(block_size), blocks],
            &mut [],
        )
    }

    pub fn set_online(&self) -> Result<()> {
        self.scalar(abi::METHOD_SET_ONLINE, &[1], &mut [])
    }

    pub fn set_offline(&self) -> Result<()> {
        let offline = self.scalar(abi::METHOD_SET_ONLINE, &[0], &mut []);
        if let Err(failure) = &offline
            && self.is_detached()
        {
            warn!(%failure, "kext 已断开，继续关闭用户客户端");
            return Ok(());
        }
        offline
    }

    pub fn complete(&self, request: &Request, status: u64, bytes: u64) -> Result<()> {
        let completion = self.scalar(
            abi::METHOD_COMPLETE_IO,
            &[
                u64::from(request.token),
                u64::from(request.chunk),
                status,
                bytes,
            ],
            &mut [],
        );
        if let Err(failure) = &completion {
            // A failed completion leaves ownership uncertain; never dequeue again.
            if !self.detached.swap(true, Ordering::AcqRel) {
                error!(%failure, "COMPLETE_IO 失败，kext 可能已断开，正在关闭");
            }
        }
        completion
    }

    pub fn is_detached(&self) -> bool {
        self.detached.load(Ordering::Acquire)
    }
}

impl Drop for UserClient {
    fn drop(&mut self) {
        if let Err(failure) = check_return("IOServiceClose", unsafe { IOServiceClose(self.port) }) {
            warn!(%failure, "关闭用户客户端失败");
        }
    }
}

pub struct Config {
    pub arena_bytes: usize,
    pub max_request_bytes: u32,
}

impl Config {
    fn parse(scalars: [u64; 4], mapped_bytes: usize) -> Result<Self> {
        let [version, arena_bytes, page_granule, max_request_bytes] = scalars;
        ensure!(
            version == abi::ABI_VERSION,
            "unsupported ABI version {version}, expected 1"
        );
        ensure!(
            arena_bytes >= MIN_ARENA_BYTES && arena_bytes <= mapped_bytes as u64,
            "arena must contain at least 4 MiB and fit the mapped memory"
        );
        ensure!(
            page_granule == abi::PAGE_GRANULE as u64 && arena_bytes.is_multiple_of(page_granule),
            "invalid ABI page granule {page_granule}"
        );
        ensure!(
            max_request_bytes > 0 && max_request_bytes <= abi::MAX_REQUEST_BYTES as u64,
            "invalid maximum request size {max_request_bytes}"
        );
        Ok(Self {
            arena_bytes: usize::try_from(arena_bytes)?,
            max_request_bytes: u32::try_from(max_request_bytes)?,
        })
    }
}

pub struct Mapping {
    client: Arc<UserClient>,
    memory_type: u32,
    address: u64,
    pub len: usize,
}

impl Mapping {
    pub fn new(client: Arc<UserClient>, memory_type: u32) -> Result<Self> {
        let mut address = 0;
        let mut size = 0;
        check_return("IOConnectMapMemory64", unsafe {
            IOConnectMapMemory64(
                client.port,
                memory_type,
                task_port(),
                &mut address,
                &mut size,
                IO_MAP_ANYWHERE,
            )
        })?;
        let mapping = Self {
            client,
            memory_type,
            address,
            len: usize::try_from(size).unwrap_or(0),
        };
        ensure!(
            address != 0
                && mapping.len > 0
                && mapping.len <= isize::MAX as usize
                && address.checked_add(size).is_some(),
            "invalid shared memory mapping"
        );
        Ok(mapping)
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        if let Err(failure) = check_return("IOConnectUnmapMemory64", unsafe {
            IOConnectUnmapMemory64(
                self.client.port,
                self.memory_type,
                task_port(),
                self.address,
            )
        }) {
            warn!(%failure, memory_type = self.memory_type, "解除共享内存映射失败");
        }
    }
}

pub struct Arena {
    ptr: *mut u8,
    len: usize,
    _mapping: Mapping,
    active: Mutex<Vec<Range<usize>>>,
}

// Mapping keeps the allocation/client alive; Region grants disjoint mutable
// ranges, and shutdown joins all tasks before allowing the kernel to reuse it.
unsafe impl Send for Arena {}
unsafe impl Sync for Arena {}

impl Arena {
    pub fn new(mapping: Mapping, len: usize) -> Result<Arc<Self>> {
        ensure!(len > 0 && len <= mapping.len, "arena exceeds its mapping");
        Ok(Arc::new(Self {
            ptr: mapping.address as *mut u8,
            len,
            _mapping: mapping,
            active: Mutex::new(Vec::new()),
        }))
    }

    pub fn reserve(self: &Arc<Self>, offset: u64, length: u32) -> Result<Region> {
        let range = checked_range(offset, length, self.len)?;
        let mut active = self
            .active
            .lock()
            .map_err(|_| anyhow::anyhow!("arena lock poisoned"))?;
        ensure!(
            !active.iter().any(|other| ranges_overlap(&range, other)),
            "request overlaps an active arena range"
        );
        active.push(range.clone());
        Ok(Region {
            arena: self.clone(),
            range,
        })
    }
}

fn checked_range(offset: u64, length: u32, arena_len: usize) -> Result<Range<usize>> {
    // Zero, overflow, and one-past-end are rejected before any slice exists.
    let start = usize::try_from(offset).context("arena offset overflow")?;
    let end = start
        .checked_add(length as usize)
        .context("arena range overflow")?;
    ensure!(
        length != 0 && end <= arena_len,
        "request exceeds arena bounds"
    );
    Ok(start..end)
}

fn ranges_overlap(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

pub struct Region {
    arena: Arc<Arena>,
    range: Range<usize>,
}

impl Region {
    pub fn bytes(&mut self) -> Result<&mut [u8]> {
        ensure!(
            self.range.start < self.range.end && self.range.end <= self.arena.len,
            "invalid arena region"
        );
        // This lease is exclusive until Drop, including across await points.
        Ok(unsafe {
            std::slice::from_raw_parts_mut(self.arena.ptr.add(self.range.start), self.range.len())
        })
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        match self.arena.active.lock() {
            Ok(mut active) => active.retain(|range| *range != self.range),
            Err(_) => error!("arena lock poisoned while releasing request"),
        }
    }
}

pub struct RequestQueue {
    memory: QueueMemory,
    _mapping: Mapping,
}

impl RequestQueue {
    pub fn new(mapping: Mapping) -> Result<Self> {
        let memory = unsafe { QueueMemory::new(mapping.address as *mut u8, mapping.len) }?;
        Ok(Self {
            memory,
            _mapping: mapping,
        })
    }

    pub fn dequeue(&mut self) -> Result<Dequeue> {
        self.memory.dequeue()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Dequeue {
    Empty,
    Skipped(u32),
    Request(Request),
}

struct QueueMemory {
    ptr: *mut u8,
    queue_size: usize,
}

impl QueueMemory {
    /// The caller must keep an aligned, live mapping and be its only consumer.
    unsafe fn new(ptr: *mut u8, len: usize) -> Result<Self> {
        ensure!(
            !ptr.is_null() && (ptr as usize).is_multiple_of(4),
            "unaligned queue mapping"
        );
        ensure!(len >= QUEUE_HEADER_BYTES, "truncated queue header");
        let queue_size = unsafe { ptr.cast::<u32>().read_volatile() } as usize;
        ensure!(
            queue_size >= ENTRY_HEADER_BYTES
                && queue_size.is_multiple_of(4)
                && queue_size <= len - QUEUE_HEADER_BYTES,
            "invalid queue size {queue_size}"
        );
        Ok(Self { ptr, queue_size })
    }

    fn cursor(&self, offset: usize) -> &AtomicU32 {
        // Only the aligned head/tail fields are accessed atomically.
        unsafe { &*self.ptr.add(offset).cast::<AtomicU32>() }
    }

    fn payload_size(&self, offset: usize) -> Option<usize> {
        if offset.checked_add(ENTRY_HEADER_BYTES)? > self.queue_size {
            return None;
        }
        Some(unsafe {
            self.ptr
                .add(QUEUE_HEADER_BYTES + offset)
                .cast::<u32>()
                .read_volatile()
        } as usize)
    }

    fn entry(&self, head: usize, tail: usize) -> Result<(usize, usize)> {
        let size = self.payload_size(head);
        if let Some(size) = size
            && head + ENTRY_HEADER_BYTES + size <= self.queue_size
        {
            return Ok((head, size));
        }
        ensure!(tail < head, "queue entry exceeds published memory");
        let size = self
            .payload_size(0)
            .context("missing wrapped entry header")?;
        ensure!(
            ENTRY_HEADER_BYTES + size <= tail,
            "invalid wrapped queue entry"
        );
        Ok((0, size))
    }

    fn dequeue(&mut self) -> Result<Dequeue> {
        let head = self.cursor(4).load(Ordering::Acquire) as usize;
        let tail = self.cursor(8).load(Ordering::Acquire) as usize;
        ensure!(
            head <= self.queue_size
                && tail <= self.queue_size
                && head.is_multiple_of(4)
                && tail.is_multiple_of(4),
            "invalid queue cursors: head={head}, tail={tail}"
        );
        if head == tail {
            return Ok(Dequeue::Empty);
        }
        let (offset, size) = self.entry(head, tail)?;
        let end = (offset + ENTRY_HEADER_BYTES + size + 3) & !3;
        ensure!(offset >= tail || end <= tail, "queue entry exceeds tail");
        let request = if size == size_of::<Request>() {
            let request = unsafe {
                self.ptr
                    .add(QUEUE_HEADER_BYTES + offset + ENTRY_HEADER_BYTES)
                    .cast::<Request>()
                    .read_unaligned()
            };
            Dequeue::Request(request)
        } else {
            Dequeue::Skipped(size as u32)
        };
        // IOKit may publish tail == queue_size. Preserve equality when draining
        // that endpoint; premature wrapping would replay entries from zero.
        let next_head = if end >= self.queue_size && tail != self.queue_size {
            0
        } else {
            end as u32
        };
        self.cursor(4).store(next_head, Ordering::Release);
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue_storage(size: usize) -> Vec<u32> {
        let mut storage = vec![0; (QUEUE_HEADER_BYTES + size) / 4];
        storage[0] = size as u32;
        storage
    }

    fn queue(storage: &mut [u32]) -> QueueMemory {
        unsafe { QueueMemory::new(storage.as_mut_ptr().cast(), size_of_val(storage)).unwrap() }
    }

    fn put_request(storage: &mut [u32], offset: usize, token: u32) -> Request {
        let request = Request {
            token,
            length: 512,
            block_size: 512,
            ..Request::default()
        };
        storage[3 + offset / 4] = size_of::<Request>() as u32;
        unsafe {
            storage
                .as_mut_ptr()
                .cast::<u8>()
                .add(QUEUE_HEADER_BYTES + offset + ENTRY_HEADER_BYTES)
                .cast::<Request>()
                .write_unaligned(request);
        }
        request
    }

    #[test]
    fn empty_and_multiple_requests() {
        let mut storage = queue_storage(128);
        assert_eq!(queue(&mut storage).dequeue().unwrap(), Dequeue::Empty);
        let first = put_request(&mut storage, 0, 7);
        let second = put_request(&mut storage, 44, 8);
        storage[2] = 88;
        let mut queue = queue(&mut storage);
        assert_eq!(queue.dequeue().unwrap(), Dequeue::Request(first));
        assert_eq!(queue.dequeue().unwrap(), Dequeue::Request(second));
        assert_eq!(queue.dequeue().unwrap(), Dequeue::Empty);
        assert_eq!(storage[1], 88);
    }

    #[test]
    fn skips_mismatched_payload_and_keeps_alignment() {
        let mut storage = queue_storage(128);
        storage[3] = 1;
        let request = put_request(&mut storage, 8, 9);
        storage[2] = 52;
        let mut queue = queue(&mut storage);
        assert_eq!(queue.dequeue().unwrap(), Dequeue::Skipped(1));
        assert_eq!(queue.dequeue().unwrap(), Dequeue::Request(request));
        assert_eq!(queue.dequeue().unwrap(), Dequeue::Empty);
    }

    #[test]
    fn wraps_when_payload_does_not_fit() {
        let mut storage = queue_storage(128);
        storage[1] = 108;
        storage[2] = 44;
        storage[3 + 108 / 4] = 40;
        let request = put_request(&mut storage, 0, 10);
        assert_eq!(
            queue(&mut storage).dequeue().unwrap(),
            Dequeue::Request(request)
        );
        assert_eq!(storage[1], 44);
    }

    #[test]
    fn wraps_when_header_does_not_fit() {
        let mut storage = queue_storage(128);
        storage[1] = 128;
        storage[2] = 44;
        let request = put_request(&mut storage, 0, 11);
        assert_eq!(
            queue(&mut storage).dequeue().unwrap(),
            Dequeue::Request(request)
        );
    }

    #[test]
    fn advances_head_to_zero_at_end() {
        let mut storage = queue_storage(128);
        storage[1] = 84;
        storage[2] = 44;
        let request = put_request(&mut storage, 84, 12);
        assert_eq!(
            queue(&mut storage).dequeue().unwrap(),
            Dequeue::Request(request)
        );
        assert_eq!(storage[1], 0);
    }

    #[test]
    fn exact_end_tail_drains_once_and_accepts_next_wrap() {
        let mut storage = queue_storage(88);
        let first = put_request(&mut storage, 0, 13);
        let last = put_request(&mut storage, 44, 14);
        storage[2] = 88;
        let mut consumer = queue(&mut storage);
        assert_eq!(consumer.dequeue().unwrap(), Dequeue::Request(first));
        assert_eq!(consumer.dequeue().unwrap(), Dequeue::Request(last));
        assert_eq!(consumer.dequeue().unwrap(), Dequeue::Empty);
        assert_eq!(storage[1], 88);

        let next = put_request(&mut storage, 0, 15);
        storage[2] = 44;
        assert_eq!(consumer.dequeue().unwrap(), Dequeue::Request(next));
        assert_eq!(consumer.dequeue().unwrap(), Dequeue::Empty);
        assert_eq!(storage[1], 44);
    }

    #[test]
    fn rejects_corrupt_queue_without_reading_payload() {
        let mut storage = queue_storage(128);
        storage[2] = 4;
        storage[3] = u32::MAX;
        assert!(queue(&mut storage).dequeue().is_err());
        storage[1] = 132;
        assert!(queue(&mut storage).dequeue().is_err());
        storage[1] = 1;
        assert!(queue(&mut storage).dequeue().is_err());
        storage[0] = 4096;
        assert!(
            unsafe {
                QueueMemory::new(storage.as_mut_ptr().cast(), size_of_val(storage.as_slice()))
            }
            .is_err()
        );
    }

    #[test]
    fn arena_range_boundaries_and_aliases() {
        assert_eq!(checked_range(0, 512, 1024).unwrap(), 0..512);
        assert_eq!(checked_range(512, 512, 1024).unwrap(), 512..1024);
        for (offset, length) in [(0, 0), (1024, 1), (513, 512), (u64::MAX, 512)] {
            assert!(checked_range(offset, length, 1024).is_err());
        }
        assert!(ranges_overlap(&(0..512), &(511..1024)));
        assert!(ranges_overlap(&(0..1024), &(128..512)));
        assert!(!ranges_overlap(&(0..512), &(512..1024)));
    }

    #[test]
    fn config_rejects_incompatible_mapping() {
        let valid = [
            1,
            MIN_ARENA_BYTES,
            abi::PAGE_GRANULE as u64,
            abi::MAX_REQUEST_BYTES as u64,
        ];
        assert!(Config::parse(valid, MIN_ARENA_BYTES as usize).is_ok());
        for (index, replacement) in [(0, 2), (1, MIN_ARENA_BYTES - 1), (2, 0), (3, 0)] {
            let mut invalid = valid;
            invalid[index] = replacement;
            assert!(Config::parse(invalid, MIN_ARENA_BYTES as usize).is_err());
        }
        assert!(Config::parse(valid, MIN_ARENA_BYTES as usize - 1).is_err());
    }
}
