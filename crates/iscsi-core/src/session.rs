use crate::{
    BufferPool, CacheMode, Capacity, ChapCredentials, DiscoveredTarget, Error, InquiryData,
    NegotiatedParams, PooledBuffer, SessionOptions, SessionStats, StatsSnapshot, Target,
    TargetAddr,
    login::{self, Connection},
    scsi,
    stats::Stats,
    wire::{self, Bhs},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncWriteExt,
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    time::{interval, timeout},
};

/// A cloneable handle to one socket-owning connection task.
///
/// Dropping all handles closes the socket. `logout` closes the entire session,
/// including other clones; it must not race outstanding commands.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

struct Inner {
    tx: mpsc::Sender<Request>,
    permits: Arc<Semaphore>,
    params: NegotiatedParams,
    opts: SessionOptions,
    lun: u64,
    stats: Arc<Stats>,
    start: Instant,
    pool: BufferPool,
    block_size: AtomicU32,
    closed: Arc<AtomicBool>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    None,
    Read,
    Write,
    Logout,
}

struct Request {
    cdb: [u8; 16],
    lun: [u8; 8],
    direction: Direction,
    data: PooledBuffer,
    allow_short: bool,
    done: oneshot::Sender<Result<PooledBuffer, Error>>,
    _permit: OwnedSemaphorePermit,
}

struct Pending {
    tag: u32,
    request: Request,
    received: usize,
    sent: usize,
    datasn: u32,
    r2tsn: u32,
    start: Instant,
}

/// Authenticate and negotiate a normal session. No SCSI writes are issued.
pub async fn login(target: &Target, opts: &SessionOptions) -> Result<Session, Error> {
    scsi::lun_bytes(target.lun)?;
    let connection =
        login::connect(&target.addr, Some(&target.iqn), target.chap.as_ref(), opts).await?;
    let stats = Arc::new(Stats::default());
    let (tx, rx) = mpsc::channel(opts.queue_depth);
    let inner = Arc::new(Inner {
        tx,
        permits: Arc::new(Semaphore::new(opts.queue_depth)),
        params: connection.params.clone(),
        opts: opts.clone(),
        lun: target.lun,
        stats: stats.clone(),
        start: Instant::now(),
        pool: BufferPool::new(opts.queue_depth, 16 * 1024 * 1024),
        block_size: AtomicU32::new(0),
        closed: Arc::new(AtomicBool::new(false)),
    });
    // The actor intentionally holds no Arc<Inner>, so the final handle closes rx.
    tokio::spawn(run_connection(
        connection,
        rx,
        stats,
        opts.clone(),
        inner.closed.clone(),
    ));
    Ok(Session { inner })
}

/// SendTargets=All in a discovery session without authentication.
pub async fn discover(
    addr: &TargetAddr,
    opts: &SessionOptions,
) -> Result<Vec<DiscoveredTarget>, Error> {
    discover_with_chap(addr, opts, None).await
}

/// Discovery variant supporting CHAP without embedding secrets in options.
pub async fn discover_with_chap(
    addr: &TargetAddr,
    opts: &SessionOptions,
    chap: Option<&ChapCredentials>,
) -> Result<Vec<DiscoveredTarget>, Error> {
    let mut conn = login::connect(addr, None, chap, opts).await?;
    timeout(opts.io_timeout, async {
        let mut data = Vec::new();
        let mut transfer_tag = wire::RESERVED_TAG;
        for round in 0..1024 {
            let mut h = Bhs::new(wire::TEXT_REQUEST | 0x40, 0x80);
            h.put_u32(16, 2);
            h.put_u32(20, transfer_tag);
            h.put_u32(24, conn.cmdsn);
            h.put_u32(28, conn.expstatsn);
            let mut encoded = Vec::new();
            wire::encode(
                &h,
                &[],
                if round == 0 {
                    b"SendTargets=All\0"
                } else {
                    b""
                },
                conn.params.header_digest,
                conn.params.data_digest,
                &mut encoded,
            )?;
            conn.socket.write_all(&encoded).await?;
            let p = wire::read_pdu(
                &mut conn.socket,
                conn.params.header_digest,
                conn.params.data_digest,
                conn.params.max_recv_data_segment_length as usize,
            )
            .await?;
            if p.header.opcode() != wire::TEXT_RESPONSE || p.header.itt() != 2 {
                return Err(Error::Protocol("unexpected discovery response".into()));
            }
            conn.expstatsn = p.header.u32(24).wrapping_add(1);
            data.extend_from_slice(&p.data);
            if data.len() > 16 * 1024 * 1024 {
                return Err(Error::Protocol("discovery exceeds 16 MiB limit".into()));
            }
            if p.header.0[1] & 0x80 != 0 {
                let mut targets: Vec<DiscoveredTarget> = Vec::new();
                for (key, value) in wire::parse_text(&data)? {
                    match key.as_str() {
                        "targetname" => targets.push(DiscoveredTarget {
                            iqn: value,
                            addresses: Vec::new(),
                        }),
                        "targetaddress" => {
                            if let Some(target) = targets.last_mut() {
                                target.addresses.push(value);
                            }
                        }
                        _ => {}
                    }
                }
                logout_connection(&mut conn, 3).await?;
                return Ok(targets);
            }
            transfer_tag = p.header.u32(20);
            if transfer_tag == wire::RESERVED_TAG {
                return Err(Error::Protocol("missing discovery continuation tag".into()));
            }
        }
        Err(Error::Protocol("discovery continuation limit".into()))
    })
    .await
    .map_err(|_| Error::Timeout)?
}

async fn logout_connection(conn: &mut Connection, tag: u32) -> Result<(), Error> {
    let mut h = Bhs::new(wire::LOGOUT_REQUEST | 0x40, 0x80);
    h.put_u32(16, tag);
    h.put_u32(24, conn.cmdsn);
    h.put_u32(28, conn.expstatsn);
    let mut bytes = Vec::new();
    wire::encode(
        &h,
        &[],
        &[],
        conn.params.header_digest,
        conn.params.data_digest,
        &mut bytes,
    )?;
    conn.socket.write_all(&bytes).await?;
    let p = wire::read_pdu(
        &mut conn.socket,
        conn.params.header_digest,
        conn.params.data_digest,
        conn.params.max_recv_data_segment_length as usize,
    )
    .await?;
    if p.header.opcode() != wire::LOGOUT_RESPONSE || p.header.itt() != tag || p.header.0[2] != 0 {
        return Err(Error::Protocol("logout rejected".into()));
    }
    Ok(())
}

impl Session {
    /// Observe transport/protocol termination without issuing any SCSI command.
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    pub fn negotiated(&self) -> &NegotiatedParams {
        &self.inner.params
    }
    pub fn queue_depth(&self) -> usize {
        self.inner.opts.queue_depth
    }
    pub fn buffer_pool(&self) -> &BufferPool {
        &self.inner.pool
    }
    pub fn stats(&self) -> StatsSnapshot {
        self.inner.stats.snapshot(self.inner.start)
    }
    pub fn live_stats(&self) -> SessionStats {
        SessionStats::new(self.inner.stats.clone(), self.inner.start)
    }

    async fn execute(
        &self,
        cdb: [u8; 16],
        direction: Direction,
        data: PooledBuffer,
        allow_short: bool,
        lun: u64,
    ) -> Result<PooledBuffer, Error> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(Error::Protocol("session closed".into()));
        }
        if data.len() > u32::MAX as usize {
            return Err(Error::Config(
                "command exceeds 32-bit transfer length".into(),
            ));
        }
        let result = timeout(self.inner.opts.io_timeout, async {
            let permit = self
                .inner
                .permits
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| Error::Protocol("session closed".into()))?;
            let (done, result) = oneshot::channel();
            self.inner
                .tx
                .send(Request {
                    cdb,
                    lun: scsi::lun_bytes(lun)?,
                    direction,
                    data,
                    allow_short,
                    done,
                    _permit: permit,
                })
                .await
                .map_err(|_| Error::Protocol("connection task exited".into()))?;
            result
                .await
                .map_err(|_| Error::Protocol("connection task exited".into()))?
        })
        .await;
        match result {
            Ok(result) => result,
            Err(_) => {
                // ERL 0: a timed-out command poisons the connection. Never let
                // a queued write start later after its caller saw a timeout.
                self.inner.closed.store(true, Ordering::Release);
                self.inner.permits.close();
                Err(Error::Timeout)
            }
        }
    }

    async fn command(&self, cdb: [u8; 16], len: usize, lun: u64) -> Result<PooledBuffer, Error> {
        for attempt in 0..3 {
            let result = self
                .execute(
                    cdb,
                    if len == 0 {
                        Direction::None
                    } else {
                        Direction::Read
                    },
                    self.inner.pool.acquire(len),
                    true,
                    lun,
                )
                .await;
            match result {
                Err(ref e) if attempt < 2 && scsi::unit_attention(e) => {
                    // The socket owner already counted the rejected attempt;
                    // report it as a retry so a healthy session shows no errors.
                    self.inner.stats.errors.fetch_sub(1, Ordering::Relaxed);
                    self.inner.stats.retries.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                other => return other,
            }
        }
        Err(Error::Protocol("unit attention retry exhausted".into()))
    }

    pub async fn test_unit_ready(&self) -> Result<(), Error> {
        self.command([0; 16], 0, self.inner.lun).await?;
        Ok(())
    }

    pub async fn report_luns(&self) -> Result<Vec<u64>, Error> {
        let mut cdb = [0; 16];
        cdb[0] = 0xa0;
        cdb[6..10].copy_from_slice(&65536u32.to_be_bytes());
        let data = self.command(cdb, 65536, 0).await?;
        scsi::parse_luns(&data)
    }

    pub async fn inquiry(&self) -> Result<InquiryData, Error> {
        let mut cdb = [0; 16];
        cdb[0] = 0x12;
        cdb[4] = 255;
        let data = self.command(cdb, 255, self.inner.lun).await?;
        let mut info = scsi::inquiry(&data)?;
        cdb[1] = 1;
        let supported = self.command(cdb, 255, self.inner.lun).await?;
        let supported = scsi::vpd(&supported, 0)?;
        for page in [0x80, 0x83] {
            if supported.contains(&page) {
                cdb[2] = page;
                // INQUIRY uses a two-byte allocation length.
                cdb[3..5].copy_from_slice(&4096u16.to_be_bytes());
                let data = self.command(cdb, 4096, self.inner.lun).await?;
                let payload = scsi::vpd(&data, page)?;
                if page == 0x80 {
                    info.serial = Some(String::from_utf8_lossy(payload).trim().to_owned());
                } else {
                    info.vpd83 = payload.to_vec();
                }
            }
        }
        Ok(info)
    }

    /// Reads the caching mode page so callers can decide whether a write-back
    /// policy would mean anything. Read-only: MODE SENSE never changes state.
    pub async fn cache_mode(&self) -> Result<CacheMode, Error> {
        let mut cdb = [0; 16];
        cdb[0] = 0x5a; // MODE SENSE(10), current values
        cdb[2] = 0x08; // caching mode page
        cdb[7..9].copy_from_slice(&64u16.to_be_bytes());
        let data = self.command(cdb, 64, self.inner.lun).await?;
        scsi::cache_mode(&data)
    }

    pub async fn capacity(&self) -> Result<Capacity, Error> {
        let mut cdb = [0; 16];
        cdb[0] = 0x25;
        let data = self.command(cdb, 8, self.inner.lun).await?;
        let last = scsi::be32(&data, 0)?;
        let capacity = if last == u32::MAX {
            cdb = [0; 16];
            cdb[0] = 0x9e;
            cdb[1] = 0x10;
            cdb[10..14].copy_from_slice(&32u32.to_be_bytes());
            let data = self.command(cdb, 32, self.inner.lun).await?;
            Capacity {
                blocks: scsi::be64(&data, 0)?
                    .checked_add(1)
                    .ok_or_else(|| Error::Protocol("capacity overflow".into()))?,
                block_size: scsi::be32(&data, 8)?,
            }
        } else {
            Capacity {
                blocks: u64::from(last) + 1,
                block_size: scsi::be32(&data, 4)?,
            }
        };
        if capacity.block_size == 0 {
            return Err(Error::Protocol("zero block size".into()));
        }
        self.inner
            .block_size
            .store(capacity.block_size, Ordering::Release);
        Ok(capacity)
    }

    async fn block_size(&self) -> Result<u32, Error> {
        let size = self.inner.block_size.load(Ordering::Acquire);
        if size == 0 {
            Ok(self.capacity().await?.block_size)
        } else {
            Ok(size)
        }
    }

    pub async fn read(&self, lba: u64, blocks: u32) -> Result<Vec<u8>, Error> {
        Ok(self.read_pooled(lba, blocks).await?.to_vec())
    }

    /// Read directly from the socket into a reusable owned buffer.
    pub async fn read_pooled(&self, lba: u64, blocks: u32) -> Result<PooledBuffer, Error> {
        let size = transfer_len(blocks, self.block_size().await?)?;
        self.read_buffer(lba, blocks, self.inner.pool.acquire(size))
            .await
    }

    /// Supply an owned buffer from any BufferPool, avoiding a command data allocation.
    pub async fn read_buffer(
        &self,
        lba: u64,
        blocks: u32,
        data: PooledBuffer,
    ) -> Result<PooledBuffer, Error> {
        let size = transfer_len(blocks, self.block_size().await?)?;
        if data.len() != size {
            return Err(Error::Config(
                "buffer length does not match block count".into(),
            ));
        }
        lba.checked_add(u64::from(blocks))
            .ok_or_else(|| Error::Config("LBA overflow".into()))?;
        self.execute(
            scsi::rw_cdb(0x88, lba, blocks),
            Direction::Read,
            data,
            false,
            self.inner.lun,
        )
        .await
    }

    /// Borrowed-buffer convenience; one copy from the actor-owned pooled buffer.
    pub async fn read_into(&self, lba: u64, blocks: u32, buf: &mut [u8]) -> Result<(), Error> {
        if buf.len() != transfer_len(blocks, self.block_size().await?)? {
            return Err(Error::Config(
                "buffer length does not match block count".into(),
            ));
        }
        let data = self.read_pooled(lba, blocks).await?;
        buf.copy_from_slice(&data);
        Ok(())
    }

    /// Destructive: overwrite logical blocks on the remote LUN.
    pub async fn write(&self, lba: u64, data: &[u8]) -> Result<(), Error> {
        let mut buffer = self.inner.pool.acquire(data.len());
        buffer.copy_from_slice(data);
        self.write_buffer(lba, buffer).await.map(|_| ())
    }

    /// Write an owned buffer and return it after target completion for reuse.
    pub async fn write_buffer(&self, lba: u64, data: PooledBuffer) -> Result<PooledBuffer, Error> {
        let block_size = self.block_size().await? as usize;
        if data.is_empty()
            || !data.len().is_multiple_of(block_size)
            || data.len() > u32::MAX as usize
        {
            return Err(Error::Config(
                "write must contain whole logical blocks".into(),
            ));
        }
        let blocks = (data.len() / block_size) as u32;
        lba.checked_add(u64::from(blocks))
            .ok_or_else(|| Error::Config("LBA overflow".into()))?;
        self.execute(
            scsi::rw_cdb(0x8a, lba, blocks),
            Direction::Write,
            data,
            false,
            self.inner.lun,
        )
        .await
    }

    pub async fn flush(&self) -> Result<(), Error> {
        let mut cdb = [0; 16];
        cdb[0] = 0x91;
        self.command(cdb, 0, self.inner.lun).await?;
        Ok(())
    }

    pub async fn logout(self) -> Result<(), Error> {
        self.execute(
            [0; 16],
            Direction::Logout,
            self.inner.pool.acquire(0),
            false,
            self.inner.lun,
        )
        .await?;
        self.inner.closed.store(true, Ordering::Release);
        self.inner.permits.close();
        Ok(())
    }
}

fn transfer_len(blocks: u32, size: u32) -> Result<usize, Error> {
    let len = u64::from(blocks) * u64::from(size);
    if len == 0 || len > u64::from(u32::MAX) {
        return Err(Error::Config("transfer must be 1..=u32::MAX bytes".into()));
    }
    Ok(len as usize)
}

fn window_open(cmdsn: u32, maxcmdsn: u32) -> bool {
    maxcmdsn.wrapping_sub(cmdsn) < 0x8000_0000
}

async fn run_connection(
    mut conn: Connection,
    mut rx: mpsc::Receiver<Request>,
    stats: Arc<Stats>,
    opts: SessionOptions,
    closed: Arc<AtomicBool>,
) {
    let mut slots: Vec<Option<Pending>> = (0..opts.queue_depth).map(|_| None).collect();
    let mut free: Vec<usize> = (0..opts.queue_depth).rev().collect();
    let mut generations = vec![0u32; opts.queue_depth];
    let mut outgoing = Vec::with_capacity(1024 * 1024);
    let mut queued = None;
    let mut timer = interval(Duration::from_millis(100));
    let result: Result<(), Error> = async {
        loop {
            if closed.load(Ordering::Acquire) {
                return Err(Error::Timeout);
            }
            while !free.is_empty() {
                let request = match queued.take().or_else(|| rx.try_recv().ok()) {
                    Some(request) => request, None => break,
                };
                if request.done.is_closed() { continue; }
                if request.direction == Direction::Logout {
                    if slots.iter().any(Option::is_some) {
                        let _ = request.done.send(Err(Error::Config("logout requires idle session".into())));
                        continue;
                    }
                    let outcome = timeout(opts.io_timeout, logout_connection(&mut conn, wire::RESERVED_TAG - 1))
                        .await.map_err(|_| Error::Timeout).and_then(|result| result);
                    let _ = request.done.send(outcome.map(|()| request.data));
                    return Ok(());
                }
                if !window_open(conn.cmdsn, conn.maxcmdsn) {
                    queued = Some(request);
                    break;
                }
                let Some(index) = free.pop() else { break; };
                let tag = generations[index] * opts.queue_depth as u32 + index as u32;
                // Never reuse a tag during the lifetime of a connection.
                generations[index] = generations[index].checked_add(1)
                    .filter(|g| u64::from(*g) * opts.queue_depth as u64 + (index as u64) < u64::from(wire::RESERVED_TAG - 1))
                    .ok_or_else(|| Error::Protocol("ITT space exhausted; reconnect required".into()))?;
                let mut pending = Pending {
                    tag, request, received: 0, sent: 0, datasn: 0, r2tsn: 0, start: Instant::now(),
                };
                encode_command(&mut conn, &mut pending, &mut outgoing)?;
                slots[index] = Some(pending);
                if outgoing.len() >= 1024 * 1024 { break; }
            }
            if !outgoing.is_empty() {
                timeout(opts.io_timeout, conn.socket.write_all(&outgoing)).await.map_err(|_| Error::Timeout)??;
                outgoing.clear();
            }
            tokio::select! {
                ready = conn.socket.readable() => {
                    ready?;
                    let closed = timeout(opts.io_timeout, receive(&mut conn, &mut slots, &mut free, &stats, &mut outgoing))
                        .await.map_err(|_| Error::Timeout)??;
                    if closed { return Ok(()); }
                }
                request = rx.recv(), if queued.is_none() && !free.is_empty() => {
                    match request {
                        Some(request) => queued = Some(request),
                        None => return Ok(()),
                    }
                }
                _ = timer.tick() => {
                    if closed.load(Ordering::Acquire)
                        || slots.iter().flatten().any(|p| p.start.elapsed() > opts.io_timeout || p.request.done.is_closed())
                    {
                        return Err(Error::Timeout);
                    }
                    if rx.is_closed() && queued.is_none() && slots.iter().all(Option::is_none) {
                        return Ok(());
                    }
                }
            }
        }
    }.await;
    closed.store(true, Ordering::Release);
    let failure = result.err();
    if let Some(error) = &failure {
        tracing::warn!(%error, "iSCSI connection terminated");
    }
    // Preserve the root failure for every waiting caller (without Clone on io::Error).
    let copy_error = || match &failure {
        Some(Error::Timeout) => Error::Timeout,
        Some(Error::Digest(what)) => Error::Digest(what),
        Some(Error::Transport(e)) => Error::Transport(std::io::Error::new(e.kind(), e.to_string())),
        Some(e) => Error::Protocol(e.to_string()),
        None => Error::Protocol("session closed".into()),
    };
    rx.close();
    for p in slots.into_iter().flatten() {
        stats.errors.fetch_add(1, Ordering::Relaxed);
        let _ = p.request.done.send(Err(copy_error()));
    }
    if let Some(request) = queued {
        let _ = request.done.send(Err(copy_error()));
    }
    while let Ok(request) = rx.try_recv() {
        let _ = request.done.send(Err(copy_error()));
    }
}

fn encode_command(
    conn: &mut Connection,
    pending: &mut Pending,
    outgoing: &mut Vec<u8>,
) -> Result<(), Error> {
    let request = &pending.request;
    let mut flags = 0x80 | 1; // F, simple task attribute.
    if request.direction == Direction::Read {
        flags |= 0x40;
    }
    if request.direction == Direction::Write {
        flags |= 0x20;
    }
    let mut h = Bhs::new(wire::SCSI_COMMAND, flags);
    h.0[8..16].copy_from_slice(&request.lun);
    h.put_u32(16, pending.tag);
    h.put_u32(20, request.data.len() as u32);
    h.put_u32(24, conn.cmdsn);
    h.put_u32(28, conn.expstatsn);
    h.0[32..48].copy_from_slice(&request.cdb);
    conn.cmdsn = conn.cmdsn.wrapping_add(1);
    let immediate = if request.direction == Direction::Write && conn.params.immediate_data {
        request
            .data
            .len()
            .min(conn.params.first_burst_length as usize)
            .min(conn.params.target_max_recv_data_segment_length as usize)
    } else {
        0
    };
    pending.sent = immediate;
    wire::encode(
        &h,
        &[],
        &request.data[..immediate],
        conn.params.header_digest,
        conn.params.data_digest,
        outgoing,
    )
}

fn status_sequence(conn: &mut Connection, h: &Bhs, advances: bool) -> Result<(), Error> {
    if h.u32(24) != conn.expstatsn {
        return Err(Error::Protocol(format!(
            "StatSN mismatch: got {}, expected {}",
            h.u32(24),
            conn.expstatsn
        )));
    }
    if advances {
        conn.expstatsn = conn.expstatsn.wrapping_add(1);
    }
    Ok(())
}

async fn receive(
    conn: &mut Connection,
    slots: &mut [Option<Pending>],
    free: &mut Vec<usize>,
    stats: &Stats,
    outgoing: &mut Vec<u8>,
) -> Result<bool, Error> {
    // Tokio readiness can be stale after login or the previous PDU. A
    // nonblocking probe clears stale readiness without blocking command intake.
    let mut header = Bhs([0; wire::BHS_LEN]);
    let received = match conn.socket.try_read(&mut header.0) {
        Ok(0) => return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into()),
        Ok(n) => n,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let h = wire::read_header_tail(
        &mut conn.socket,
        header,
        received,
        conn.params.header_digest,
        conn.params.max_recv_data_segment_length as usize,
    )
    .await?;
    conn.update_window(h.u32(28), h.u32(32));
    if h.opcode() == wire::NOP_IN {
        let mut data = vec![0; h.data_len()];
        wire::read_data(&mut conn.socket, &mut data, conn.params.data_digest).await?;
        status_sequence(conn, &h, h.itt() != wire::RESERVED_TAG)?;
        if h.u32(20) != wire::RESERVED_TAG {
            let mut response = Bhs::new(wire::NOP_OUT | 0x40, 0x80);
            response.0[8..16].copy_from_slice(&h.0[8..16]);
            response.put_u32(16, wire::RESERVED_TAG);
            response.put_u32(20, h.u32(20));
            response.put_u32(24, conn.cmdsn);
            response.put_u32(28, conn.expstatsn);
            wire::encode(
                &response,
                &[],
                &data,
                conn.params.header_digest,
                conn.params.data_digest,
                outgoing,
            )?;
        }
        return Ok(false);
    }
    if h.opcode() == wire::REJECT {
        return Err(Error::Protocol(format!(
            "target Reject, reason {:#x}",
            h.0[2]
        )));
    }
    let index = h.itt() as usize % slots.len();
    let p = slots[index]
        .as_mut()
        .filter(|p| p.tag == h.itt())
        .ok_or_else(|| Error::Protocol("unknown or stale ITT".into()))?;
    let completion = match h.opcode() {
        wire::DATA_IN => {
            if p.request.direction != Direction::Read
                || h.u32(36) != p.datasn
                || h.u32(40) as usize != p.received
                || p.received
                    .checked_add(h.data_len())
                    .is_none_or(|end| end > p.request.data.len())
            {
                return Err(Error::Protocol(
                    "invalid Data-In sequence, offset or length".into(),
                ));
            }
            // RFC 7143 11.7.2: ignore the A bit at ERL 0.
            wire::read_data(
                &mut conn.socket,
                &mut p.request.data[p.received..p.received + h.data_len()],
                conn.params.data_digest,
            )
            .await?;
            p.received += h.data_len();
            p.datasn = p.datasn.wrapping_add(1);
            if h.0[1] & 1 != 0 {
                if h.0[1] & 0x80 == 0 {
                    return Err(Error::Protocol("Data-In status without F".into()));
                }
                status_sequence(conn, &h, true)?;
                Some(scsi_result(&h, &[], p, h.0[1] & 2 != 0, h.0[1] & 4 != 0))
            } else {
                None
            }
        }
        wire::SCSI_RESPONSE => {
            let mut data = vec![0; h.data_len()];
            wire::read_data(&mut conn.socket, &mut data, conn.params.data_digest).await?;
            status_sequence(conn, &h, true)?;
            if h.0[2] != 0 {
                return Err(Error::Protocol(format!(
                    "iSCSI service response {}",
                    h.0[2]
                )));
            }
            if p.request.direction == Direction::Read && h.u32(36) != p.datasn {
                return Err(Error::Protocol("ExpDataSN mismatch".into()));
            }
            let sense = if data.is_empty() {
                &[][..]
            } else {
                if data.len() < 2 {
                    return Err(Error::Protocol("truncated sense length".into()));
                }
                let length = usize::from(u16::from_be_bytes([data[0], data[1]]));
                data.get(2..2 + length)
                    .ok_or_else(|| Error::Protocol("truncated sense".into()))?
            };
            Some(scsi_result(&h, sense, p, h.0[1] & 2 != 0, h.0[1] & 4 != 0))
        }
        wire::R2T => {
            if h.data_len() != 0
                || p.request.direction != Direction::Write
                || h.u32(36) != p.r2tsn
                || h.u32(40) as usize != p.sent
            {
                return Err(Error::Protocol("invalid R2T".into()));
            }
            status_sequence(conn, &h, false)?;
            let size = h.u32(44) as usize;
            let end = p
                .sent
                .checked_add(size)
                .ok_or_else(|| Error::Protocol("R2T overflow".into()))?;
            if size == 0
                || size > conn.params.max_burst_length as usize
                || end > p.request.data.len()
                || h.u32(20) == wire::RESERVED_TAG
            {
                return Err(Error::Protocol("invalid R2T transfer range".into()));
            }
            let mut datasn = 0;
            while p.sent < end {
                let count =
                    (end - p.sent).min(conn.params.target_max_recv_data_segment_length as usize);
                let mut out =
                    Bhs::new(wire::DATA_OUT, if p.sent + count == end { 0x80 } else { 0 });
                out.0[8..16].copy_from_slice(&p.request.lun);
                out.put_u32(16, p.tag);
                out.put_u32(20, h.u32(20));
                out.put_u32(28, conn.expstatsn);
                out.put_u32(36, datasn);
                out.put_u32(40, p.sent as u32);
                wire::encode(
                    &out,
                    &[],
                    &p.request.data[p.sent..p.sent + count],
                    conn.params.header_digest,
                    conn.params.data_digest,
                    outgoing,
                )?;
                p.sent += count;
                datasn += 1;
            }
            p.r2tsn += 1;
            None
        }
        other => return Err(Error::Protocol(format!("unexpected opcode {other:#x}"))),
    };
    if let Some(result) = completion
        && let Some(mut pending) = slots[index].take()
    {
        free.push(index);
        stats.commands.fetch_add(1, Ordering::Relaxed);
        match result {
            Ok(length) => {
                if pending.request.direction == Direction::Read {
                    pending.request.data.truncate(length);
                    stats.bytes_read.fetch_add(length as u64, Ordering::Relaxed);
                } else if pending.request.direction == Direction::Write {
                    stats
                        .bytes_written
                        .fetch_add(length as u64, Ordering::Relaxed);
                }
                let _ = pending.request.done.send(Ok(pending.request.data));
            }
            Err(error) => {
                stats.errors.fetch_add(1, Ordering::Relaxed);
                let _ = pending.request.done.send(Err(error));
            }
        }
    }
    Ok(false)
}

fn scsi_result(
    h: &Bhs,
    sense: &[u8],
    p: &Pending,
    underflow: bool,
    overflow: bool,
) -> Result<usize, Error> {
    if h.0[3] != 0 {
        return Err(Error::Scsi {
            status: h.0[3],
            sense: sense.to_vec(),
        });
    }
    if overflow || (underflow && !p.request.allow_short) {
        return Err(Error::Protocol("SCSI transfer residual".into()));
    }
    let expected = if underflow {
        p.request
            .data
            .len()
            .checked_sub(h.u32(44) as usize)
            .ok_or_else(|| Error::Protocol("invalid underflow residual".into()))?
    } else {
        p.request.data.len()
    };
    let actual = match p.request.direction {
        Direction::Read => p.received,
        Direction::Write => p.sent,
        _ => 0,
    };
    if actual != expected {
        return Err(Error::Protocol(format!(
            "short transfer: got {actual}, expected {expected}"
        )));
    }
    Ok(actual)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn serial_window_wraps() {
        assert!(window_open(u32::MAX, 2));
        assert!(!window_open(3, 2));
        assert!(window_open(2, 2));
    }

    #[tokio::test]
    async fn real_target_smoke_env_gated() {
        let Ok(host) = std::env::var("ISCSI_TEST_HOST") else {
            eprintln!("skipped: ISCSI_TEST_HOST is unset");
            return;
        };
        let iqn = std::env::var("ISCSI_TEST_IQN")
            .unwrap_or_else(|_| "iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836".into());
        let lun = std::env::var("ISCSI_TEST_LUN")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        let session = login(
            &Target {
                addr: TargetAddr { host, port: 3260 },
                iqn,
                lun,
                chap: None,
            },
            &SessionOptions::default(),
        )
        .await
        .unwrap();
        assert!(!session.inquiry().await.unwrap().vendor.is_empty());
        let capacity = session.capacity().await.unwrap();
        assert!(capacity.blocks > 0);
        assert_eq!(
            session.read(0, 1).await.unwrap().len(),
            capacity.block_size as usize
        );
        session.logout().await.unwrap();
    }
}
