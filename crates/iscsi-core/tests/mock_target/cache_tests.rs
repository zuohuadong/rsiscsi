use super::*;
use iscsi_core::{CacheConfig, CacheStats, Session, cache::SEGMENT_BYTES};
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

const SEG: usize = SEGMENT_BYTES;

#[derive(Clone, Debug, PartialEq, Eq)]
struct CommandRecord {
    opcode: u8,
    lba: u64,
    bytes: usize,
}

struct MemoryLun {
    bytes: Vec<u8>,
    commands: Vec<CommandRecord>,
    fail_sync: bool,
    fail_write: Option<u64>,
    short_read: bool,
}

struct Mock {
    session: Session,
    lun: Arc<Mutex<MemoryLun>>,
    server: JoinHandle<()>,
    block_size: usize,
}

impl Mock {
    async fn new(config: CacheConfig) -> Self {
        Self::with_geometry(config, 512).await
    }

    async fn with_geometry(config: CacheConfig, block_size: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let lun = Arc::new(Mutex::new(MemoryLun {
            bytes: (0..8 * 1024 * 1024).map(|i| (i / SEG) as u8).collect(),
            commands: Vec::new(),
            fail_sync: false,
            fail_write: None,
            short_read: false,
        }));
        let shared = lun.clone();
        let server = tokio::spawn(async move {
            let mut socket = accept_login(listener, false).await;
            let mut statsn = 2;
            loop {
                let command = recv(&mut socket, false).await;
                if command.header.opcode() == wire::LOGOUT_REQUEST {
                    send(
                        &mut socket,
                        &response(wire::LOGOUT_RESPONSE, 0x80, &command, statsn),
                        &[],
                        false,
                    )
                    .await;
                    return;
                }
                let opcode = command.header.0[32];
                let lba = u64::from_be_bytes(command.header.0[34..42].try_into().unwrap());
                let bytes = command.header.u32(20) as usize;
                shared
                    .lock()
                    .unwrap()
                    .commands
                    .push(CommandRecord { opcode, lba, bytes });
                match opcode {
                    0x25 => {
                        let mut capacity = [0; 8];
                        capacity[..4].copy_from_slice(
                            &((8 * 1024 * 1024 / block_size - 1) as u32).to_be_bytes(),
                        );
                        capacity[4..].copy_from_slice(&(block_size as u32).to_be_bytes());
                        data_in(&mut socket, &command, &mut statsn, &capacity, false).await;
                    }
                    0x88 => {
                        let (payload, short) = {
                            let state = shared.lock().unwrap();
                            let start = lba as usize * block_size;
                            (state.bytes[start..start + bytes].to_vec(), state.short_read)
                        };
                        let length = if short {
                            payload.len() - block_size
                        } else {
                            payload.len()
                        };
                        for (index, chunk) in payload[..length].chunks(65536).enumerate() {
                            let last = index * 65536 + chunk.len() == length;
                            let mut h = response(
                                wire::DATA_IN,
                                if last {
                                    if short { 0x83 } else { 0x81 }
                                } else {
                                    0
                                },
                                &command,
                                statsn,
                            );
                            h.put_u32(36, index as u32);
                            h.put_u32(40, (index * 65536) as u32);
                            if short && last {
                                h.put_u32(44, block_size as u32);
                            }
                            send(&mut socket, &h, chunk, false).await;
                        }
                        statsn += 1;
                    }
                    0x8a => {
                        let mut payload = command.data.clone();
                        let mut r2tsn = 0;
                        while payload.len() < bytes {
                            let count = (bytes - payload.len()).min(8192);
                            let mut h = response(wire::R2T, 0x80, &command, statsn);
                            h.put_u32(20, 42);
                            h.put_u32(36, r2tsn);
                            h.put_u32(40, payload.len() as u32);
                            h.put_u32(44, count as u32);
                            send(&mut socket, &h, &[], false).await;
                            for _ in 0..count.div_ceil(4096) {
                                let part = recv(&mut socket, false).await;
                                assert_eq!(part.header.opcode(), wire::DATA_OUT);
                                assert_eq!(part.header.u32(40) as usize, payload.len());
                                payload.extend_from_slice(&part.data);
                            }
                            r2tsn += 1;
                        }
                        let failed = {
                            let mut state = shared.lock().unwrap();
                            if state.fail_write == Some(lba) {
                                true
                            } else {
                                let start = lba as usize * block_size;
                                state.bytes[start..start + bytes].copy_from_slice(&payload);
                                false
                            }
                        };
                        let mut h = response(wire::SCSI_RESPONSE, 0x80, &command, statsn);
                        if failed {
                            h.0[3] = 2;
                        }
                        send(&mut socket, &h, &[], false).await;
                        statsn += 1;
                    }
                    0x91 => {
                        let mut h = response(wire::SCSI_RESPONSE, 0x80, &command, statsn);
                        if shared.lock().unwrap().fail_sync {
                            h.0[3] = 2;
                        }
                        send(&mut socket, &h, &[], false).await;
                        statsn += 1;
                    }
                    _ => panic!("unexpected mock command {opcode:#x}"),
                }
            }
        });
        let mut session = login(
            &target(port),
            &SessionOptions {
                io_timeout: Duration::from_secs(5),
                ..SessionOptions::default()
            },
        )
        .await
        .unwrap();
        session.enable_cache(config);
        session.capacity().await.unwrap();
        Self {
            session,
            lun,
            server,
            block_size,
        }
    }

    async fn read(&self, segment: usize, count: usize) -> Vec<u8> {
        self.session
            .read_cached(
                (segment * SEG / self.block_size) as u64,
                (count * SEG / self.block_size) as u32,
            )
            .await
            .unwrap()
    }

    fn records(&self, opcode: u8) -> Vec<CommandRecord> {
        self.lun
            .lock()
            .unwrap()
            .commands
            .iter()
            .filter(|c| c.opcode == opcode)
            .cloned()
            .collect()
    }

    async fn finish(self) {
        self.session.logout().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), self.server)
            .await
            .unwrap()
            .unwrap();
    }
}

fn read_cache(segments: usize) -> CacheConfig {
    CacheConfig {
        read_capacity_bytes: segments * SEG,
        ..CacheConfig::default()
    }
}

fn write_cache(segments: usize) -> CacheConfig {
    CacheConfig {
        write_back_capacity_bytes: segments * SEG,
        ..read_cache(8)
    }
}

#[tokio::test]
async fn hits_skip_wire_reads_lru_evicts_and_misses_merge_at_one_mib() {
    let mock = Mock::new(read_cache(2)).await;
    assert_eq!(mock.read(0, 2).await, [vec![0; SEG], vec![1; SEG]].concat());
    mock.read(0, 1).await;
    mock.read(2, 1).await;
    mock.read(0, 1).await;
    mock.read(1, 1).await;
    assert_eq!(mock.records(0x88).len(), 3);
    let stats = mock.session.cache_stats();
    assert_eq!(stats.hits, (2 * SEG) as u64);
    assert_eq!(stats.misses, (4 * SEG) as u64);
    assert_eq!(stats.bytes_served, stats.hits);
    assert_eq!(stats.evictions, 2);
    mock.finish().await;

    let mock = Mock::new(read_cache(32)).await;
    mock.read(0, 17).await;
    assert_eq!(
        mock.records(0x88)
            .iter()
            .map(|r| r.bytes)
            .collect::<Vec<_>>(),
        vec![1024 * 1024, SEG]
    );
    mock.read(0, 17).await;
    assert_eq!(mock.records(0x88).len(), 2);
    mock.finish().await;
}

#[tokio::test]
async fn unaligned_requests_bypass_without_filling_or_splitting() {
    let mock = Mock::new(read_cache(8)).await;
    for _ in 0..2 {
        assert_eq!(
            mock.session.read_cached(1, 256).await.unwrap().len(),
            2 * SEG
        );
        mock.session.read_cached(0, 1).await.unwrap();
    }
    assert_eq!(
        mock.records(0x88)
            .iter()
            .map(|r| (r.lba, r.bytes))
            .collect::<Vec<_>>(),
        vec![(1, 2 * SEG), (0, 512), (1, 2 * SEG), (0, 512)]
    );
    mock.read(0, 2).await;
    assert_eq!(mock.records(0x88).len(), 5);
    assert_eq!(mock.session.cache_stats().hits, 0);
    mock.finish().await;
}

#[tokio::test]
async fn all_write_apis_invalidate_only_overlapping_segments() {
    let mock = Mock::new(read_cache(8)).await;
    mock.read(0, 4).await;
    mock.session.write_cached(127, &[0x5a; 1024]).await.unwrap();
    mock.read(2, 2).await;
    assert_eq!(mock.records(0x88).len(), 1);
    assert_eq!(&mock.read(0, 2).await[SEG - 512..SEG + 512], &[0x5a; 1024]);
    assert_eq!(mock.records(0x88).len(), 2);
    mock.session.write(256, &[0x6b; 512]).await.unwrap();
    assert_eq!(&mock.read(2, 1).await[..512], &[0x6b; 512]);
    let mut buffer = mock.session.buffer_pool().acquire(512);
    buffer.fill(0x7c);
    mock.session.write_buffer(384, buffer).await.unwrap();
    assert_eq!(&mock.read(3, 1).await[..512], &[0x7c; 512]);
    assert_eq!(mock.records(0x88).len(), 4);
    mock.finish().await;
}

#[tokio::test]
async fn writeback_reads_dirty_orders_and_groups_writes_before_one_sync() {
    let mock = Mock::new(write_cache(8)).await;
    mock.read(0, 4).await;
    for index in [3, 0, 1] {
        mock.session
            .write_cached(index * 128, &vec![0xa0 + index as u8; SEG])
            .await
            .unwrap();
    }
    assert!(mock.records(0x8a).is_empty());
    assert_eq!(mock.session.cache_stats().dirty_bytes, 3 * SEG);
    assert_eq!(
        mock.read(0, 4).await,
        [
            vec![0xa0; SEG],
            vec![0xa1; SEG],
            vec![2; SEG],
            vec![0xa3; SEG]
        ]
        .concat()
    );
    mock.session.flush().await.unwrap();
    assert_eq!(
        mock.records(0x8a),
        vec![
            CommandRecord {
                opcode: 0x8a,
                lba: 0,
                bytes: 2 * SEG
            },
            CommandRecord {
                opcode: 0x8a,
                lba: 384,
                bytes: SEG
            },
        ]
    );
    assert_eq!(mock.records(0x91).len(), 1);
    {
        let state = mock.lun.lock().unwrap();
        assert_eq!(state.commands.last().unwrap().opcode, 0x91);
    }
    assert_eq!(mock.session.cache_stats().dirty_bytes, 0);
    mock.read(0, 2).await; // Dirty writes never promote into the clean cache.
    assert_eq!(mock.records(0x88).len(), 2);
    mock.finish().await;
}

#[tokio::test]
async fn write_and_sync_failures_retain_all_dirty_data_and_prevent_logout() {
    for fail_write in [true, false] {
        let mut mock = Mock::new(write_cache(8)).await;
        for index in [0, 2] {
            mock.session
                .write_cached(index * 128, &vec![0x99; SEG])
                .await
                .unwrap();
        }
        {
            let mut state = mock.lun.lock().unwrap();
            state.fail_write = fail_write.then_some(256);
            state.fail_sync = !fail_write;
        }
        assert!(mock.session.flush().await.is_err());
        assert_eq!(mock.session.cache_stats().dirty_bytes, 2 * SEG);
        assert_eq!(mock.session.cache_stats().flushes, 0);
        assert!(mock.session.cache_flush_error().is_some());
        assert!(
            mock.session
                .try_enable_cache(CacheConfig::default())
                .is_err()
        );
        assert!(mock.session.clone().logout().await.is_err());
        assert!(!mock.session.is_closed());
        assert_eq!(mock.read(0, 1).await, vec![0x99; SEG]);
        {
            let mut state = mock.lun.lock().unwrap();
            state.fail_write = None;
            state.fail_sync = false;
        }
        mock.session.flush().await.unwrap();
        assert_eq!(mock.session.cache_stats().dirty_bytes, 0);
        assert!(mock.session.cache_flush_error().is_none());
        mock.finish().await;
    }
}

#[tokio::test]
async fn cache_off_matches_raw_bytes_and_command_counts() {
    let mock = Mock::new(CacheConfig::default()).await;
    let raw = mock.session.read(0, 256).await.unwrap();
    let cached = mock.session.read_cached(0, 256).await.unwrap();
    assert_eq!(raw, cached);
    mock.session.write(0, &[0x42; 512]).await.unwrap();
    mock.session.write_cached(1, &[0x42; 512]).await.unwrap();
    mock.session.flush().await.unwrap();
    assert_eq!(mock.records(0x88).len(), 2);
    assert_eq!(mock.records(0x8a).len(), 2);
    assert_eq!(mock.records(0x91).len(), 1);
    assert_eq!(mock.session.cache_stats(), CacheStats::default());
    mock.finish().await;
}

#[tokio::test]
async fn threshold_interval_and_logout_flush_dirty_data() {
    let mock = Mock::new(write_cache(1)).await;
    mock.session
        .write_cached(128, &vec![0x11; SEG])
        .await
        .unwrap();
    assert!(mock.records(0x91).is_empty());
    mock.session
        .write_cached(0, &vec![0x22; SEG])
        .await
        .unwrap();
    assert_eq!(mock.session.cache_stats().dirty_bytes, 0);
    assert_eq!(mock.records(0x8a)[0].bytes, 2 * SEG);
    mock.finish().await;

    let mock = Mock::new(CacheConfig {
        flush_interval: Some(Duration::from_millis(20)),
        ..write_cache(8)
    })
    .await;
    mock.session
        .write_cached(0, &vec![0x33; SEG])
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while mock.session.cache_stats().dirty_bytes != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(mock.records(0x91).len(), 1);
    mock.finish().await;

    let mock = Mock::new(write_cache(8)).await;
    mock.session
        .write_cached(0, &vec![0x44; SEG])
        .await
        .unwrap();
    let lun = mock.lun.clone();
    mock.finish().await;
    assert_eq!(lun.lock().unwrap().commands.last().unwrap().opcode, 0x91);
}

#[tokio::test]
async fn unaligned_and_raw_io_preserve_writeback_order_and_geometry() {
    for block_size in [512, 4096] {
        let mock = Mock::with_geometry(write_cache(8), block_size).await;
        let blocks = SEG / block_size;
        mock.session
            .write_cached(0, &vec![0x44; SEG])
            .await
            .unwrap();
        // The bypass remains a single read after one write/sync batch.
        assert_eq!(
            mock.session.read_cached(1, 1).await.unwrap(),
            vec![0x44; block_size]
        );
        mock.session
            .write_cached(0, &vec![0x55; SEG])
            .await
            .unwrap();
        mock.session
            .write(1, &vec![0x66; block_size])
            .await
            .unwrap();
        let bytes = mock.session.read_cached(0, blocks as u32).await.unwrap();
        assert_eq!(&bytes[..block_size], &vec![0x55; block_size]);
        assert_eq!(&bytes[block_size..2 * block_size], &vec![0x66; block_size]);
        assert_eq!(mock.session.cache_stats().dirty_bytes, 0);
        assert_eq!(mock.records(0x91).len(), 2);
        mock.finish().await;
    }
}

#[tokio::test]
async fn incomplete_read_never_populates_cache_and_closed_hits_fail() {
    let mock = Mock::new(read_cache(8)).await;
    mock.lun.lock().unwrap().short_read = true;
    assert!(mock.session.read_cached(0, 128).await.is_err());
    assert_eq!(mock.session.cache_stats().hits, 0);
    mock.lun.lock().unwrap().short_read = false;
    mock.read(0, 1).await;
    assert_eq!(mock.records(0x88).len(), 2);
    let session = mock.session.clone();
    mock.finish().await;
    assert!(session.read_cached(0, 128).await.is_err());
    assert!(session.write_cached(0, &vec![0; SEG]).await.is_err());
}

#[tokio::test]
async fn mixed_hits_and_dirty_segments_split_only_contiguous_miss_runs() {
    let mock = Mock::new(write_cache(32)).await;
    mock.read(1, 1).await;
    mock.read(3, 1).await;
    mock.session
        .write_cached(5 * 128, &vec![0xaa; SEG])
        .await
        .unwrap();
    let bytes = mock.read(0, 8).await;
    assert_eq!(&bytes[5 * SEG..6 * SEG], &vec![0xaa; SEG]);
    assert_eq!(
        mock.records(0x88)
            .iter()
            .map(|c| (c.lba, c.bytes))
            .collect::<Vec<_>>(),
        vec![
            (128, SEG),
            (384, SEG),
            (0, SEG),
            (256, SEG),
            (512, SEG),
            (768, 2 * SEG)
        ]
    );
    assert_eq!(mock.session.cache_stats().hits, (3 * SEG) as u64);
    assert_eq!(mock.session.cache_stats().misses, (7 * SEG) as u64);
    mock.finish().await;
}

#[tokio::test]
async fn flush_batches_at_one_mib_and_concurrent_callers_share_one_commit() {
    let mock = Mock::new(write_cache(32)).await;
    mock.session
        .write_cached(0, &vec![0x77; 17 * SEG])
        .await
        .unwrap();
    let (first, second) = tokio::join!(mock.session.flush(), mock.session.flush());
    first.unwrap();
    second.unwrap();
    assert_eq!(
        mock.records(0x8a)
            .iter()
            .map(|c| c.bytes)
            .collect::<Vec<_>>(),
        vec![16 * SEG, SEG]
    );
    assert_eq!(mock.records(0x91).len(), 1);
    assert_eq!(mock.session.cache_stats().flushes, 1);
    mock.finish().await;
}

#[tokio::test]
async fn read_cache_benchmark_uses_cache_while_default_wire_path_stays_pooled() {
    let mock = Mock::new(read_cache(8)).await;
    let report = benchmark(
        &mock.session,
        BenchOptions {
            block_size: SEG,
            queue_depths: vec![1],
            duration: Duration::from_millis(20),
            warmup: Duration::ZERO,
            range_blocks: Some(128),
            ..BenchOptions::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(mock.records(0x88).len(), 1);
    assert!(report.cache.hits > 0);
    assert_eq!(report.cache.misses, SEG as u64);
    assert_eq!(
        report.results[0].bytes,
        report.cache.hits + report.cache.misses
    );
    mock.finish().await;
}

#[tokio::test]
async fn concurrent_write_waits_for_read_fill_then_invalidates_it() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (seen, read_seen) = tokio::sync::oneshot::channel();
    let (release, wait) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut socket = accept_login(listener, false).await;
        let mut statsn = 2;
        capacity(&mut socket, &mut statsn, false).await;
        let read = recv(&mut socket, false).await;
        assert_eq!(read.header.0[32], 0x88);
        seen.send(()).unwrap();
        wait.await.unwrap();
        data_in(&mut socket, &read, &mut statsn, &vec![0; SEG], false).await;
        let write = recv(&mut socket, false).await;
        assert_eq!(write.header.0[32], 0x8a);
        assert_eq!(write.data, vec![0x88; 512]);
        send(
            &mut socket,
            &response(wire::SCSI_RESPONSE, 0x80, &write, statsn),
            &[],
            false,
        )
        .await;
        statsn += 1;
        let fresh = recv(&mut socket, false).await;
        assert_eq!(fresh.header.0[32], 0x88);
        let mut bytes = vec![0; SEG];
        bytes[..512].fill(0x88);
        data_in(&mut socket, &fresh, &mut statsn, &bytes, false).await;
        logout(&mut socket, statsn, false).await;
    });
    let mut session = login(&target(port), &SessionOptions::default())
        .await
        .unwrap();
    session.enable_cache(read_cache(8));
    session.capacity().await.unwrap();
    let reader = session.clone();
    let read = tokio::spawn(async move { reader.read_cached(0, 128).await });
    read_seen.await.unwrap();
    assert!(session.try_enable_cache(CacheConfig::default()).is_err());
    let writer = session.clone();
    let write = tokio::spawn(async move { writer.write(0, &[0x88; 512]).await });
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!write.is_finished());
    release.send(()).unwrap();
    read.await.unwrap().unwrap();
    write.await.unwrap().unwrap();
    assert_eq!(
        &session.read_cached(0, 128).await.unwrap()[..512],
        &[0x88; 512]
    );
    session.logout().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn cancelling_sync_retains_dirty_and_rejects_cached_success_on_uncertain_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (seen, sync_seen) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        for (stage, flags) in [(0, 0x81), (1, 0x87)] {
            let command = recv(&mut socket, false).await;
            let mut h = response(wire::LOGIN_RESPONSE, flags, &command, stage);
            h.0[8..14].copy_from_slice(&command.header.0[8..14]);
            h.put_u32(28, 1);
            send(&mut socket, &h, if stage == 0 { b"AuthMethod=None\0" } else { b"FirstBurstLength=65536\0MaxBurstLength=65536\0MaxRecvDataSegmentLength=65536\0" }, false).await;
        }
        let mut statsn = 2;
        capacity(&mut socket, &mut statsn, false).await;
        let write = recv(&mut socket, false).await;
        assert_eq!(write.header.0[32], 0x8a);
        assert_eq!(write.data.len(), SEG);
        send(
            &mut socket,
            &response(wire::SCSI_RESPONSE, 0x80, &write, statsn),
            &[],
            false,
        )
        .await;
        let sync = recv(&mut socket, false).await;
        assert_eq!(sync.header.0[32], 0x91);
        seen.send(()).unwrap();
        // No completion: actor must close after the flush waiter is cancelled.
        use tokio::io::AsyncReadExt;
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    });
    let mut session = login(&target(port), &SessionOptions::default())
        .await
        .unwrap();
    session.enable_cache(write_cache(8));
    session.write_cached(0, &vec![0x66; SEG]).await.unwrap();
    let writer = session.clone();
    let flush = tokio::spawn(async move { writer.flush().await });
    sync_seen.await.unwrap();
    flush.abort();
    assert!(flush.await.unwrap_err().is_cancelled());
    assert!(session.is_closed());
    assert_eq!(session.cache_stats().dirty_bytes, SEG);
    assert!(session.read_cached(0, 128).await.is_err());
    assert!(session.write_cached(0, &vec![0x77; SEG]).await.is_err());
    server.await.unwrap();
}

#[tokio::test]
async fn writeback_rejects_nonexistent_blocks_before_acknowledging_or_staging() {
    let mock = Mock::new(write_cache(8)).await;
    for lba in [8 * 1024 * 1024 / 512, u64::MAX - 127] {
        assert!(matches!(
            mock.session.write_cached(lba, &vec![0x55; SEG]).await,
            Err(Error::Config(_))
        ));
        assert_eq!(mock.session.cache_stats().dirty_bytes, 0);
        assert_eq!(mock.session.cache_stats().write_bytes, 0);
    }
    assert!(mock.records(0x8a).is_empty());
    mock.finish().await;
}

#[tokio::test]
async fn cancelled_default_write_keeps_actor_owned_enable_guard_until_retired() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (seen, write_seen) = tokio::sync::oneshot::channel();
    let (release, wait) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut socket = accept_login(listener, false).await;
        let mut statsn = 2;
        capacity(&mut socket, &mut statsn, false).await;
        let write = recv(&mut socket, false).await;
        assert_eq!(write.header.0[32], 0x8a);
        seen.send(()).unwrap();
        wait.await.unwrap();
        send(
            &mut socket,
            &response(wire::SCSI_RESPONSE, 0x80, &write, statsn),
            &[],
            false,
        )
        .await;
        statsn += 1;
        logout(&mut socket, statsn, false).await;
    });
    let mut session = login(&target(port), &SessionOptions::default())
        .await
        .unwrap();
    session.capacity().await.unwrap();
    let writer = session.clone();
    let write = tokio::spawn(async move { writer.write(0, &[0x22; 512]).await });
    write_seen.await.unwrap();
    write.abort();
    assert!(write.await.unwrap_err().is_cancelled());
    assert!(session.try_enable_cache(read_cache(8)).is_err());
    assert_eq!(session.cache_config(), CacheConfig::default());
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while session.try_enable_cache(read_cache(8)).is_err() {
            assert!(!session.is_closed());
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    session.logout().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn cache_reconfiguration_resets_stats_but_not_commit_receipts() {
    let mut mock = Mock::new(read_cache(8)).await;
    mock.session.flush().await.unwrap();
    assert_eq!(mock.session.cache_stats().flushes, 1);
    mock.session.try_enable_cache(read_cache(16)).unwrap();
    assert_eq!(mock.session.cache_stats().flushes, 0);
    mock.session.write(0, &[0x55; 512]).await.unwrap();
    mock.session.flush().await.unwrap();
    assert_eq!(mock.records(0x91).len(), 2);
    assert_eq!(mock.session.cache_stats().flushes, 1);
    mock.finish().await;
}

#[tokio::test]
async fn waiting_flush_does_not_coalesce_across_a_newer_write_through() {
    let mock = Mock::new(write_cache(8)).await;
    mock.session
        .write_cached(128, &vec![0x33; SEG])
        .await
        .unwrap();
    let (write, sync) = tokio::join!(mock.session.write(0, &[0x44; 512]), mock.session.flush(),);
    write.unwrap();
    sync.unwrap();
    assert_eq!(mock.records(0x91).len(), 2);
    assert_eq!(
        mock.lun.lock().unwrap().commands.last().unwrap().opcode,
        0x91
    );
    mock.finish().await;
}
