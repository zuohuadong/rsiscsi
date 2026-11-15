//! Deterministic local targets: no LAN, credentials or remote writes.
use iscsi_core::{
    BenchOptions, BenchPattern, Error, SessionOptions, Target, TargetAddr, benchmark, login,
    wire::{self, Bhs, Pdu},
};
use std::time::Duration;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
};

async fn send(socket: &mut TcpStream, h: &Bhs, data: &[u8], digest: bool) {
    let mut bytes = Vec::new();
    wire::encode(h, &[], data, digest, digest, &mut bytes).unwrap();
    socket.write_all(&bytes).await.unwrap();
}

async fn recv(socket: &mut TcpStream, digest: bool) -> Pdu {
    wire::read_pdu(socket, digest, digest, wire::MAX_DATA_LEN)
        .await
        .unwrap()
}

async fn accept_login(listener: TcpListener, digest: bool) -> TcpStream {
    accept_login_window(listener, digest, 128).await
}

async fn accept_login_window(listener: TcpListener, digest: bool, maxcmdsn: u32) -> TcpStream {
    let (mut socket, _) = listener.accept().await.unwrap();
    for (stage, flags) in [(0, 0x81), (1, 0x87)] {
        let request = recv(&mut socket, false).await;
        assert_eq!(request.header.opcode(), wire::LOGIN_REQUEST);
        let mut h = Bhs::new(wire::LOGIN_RESPONSE, flags);
        h.0[8..14].copy_from_slice(&request.header.0[8..14]);
        h.0[15] = 1;
        h.put_u32(16, request.header.itt());
        h.put_u32(24, stage);
        h.put_u32(28, 1);
        h.put_u32(32, maxcmdsn);
        let ops = if digest {
            b"HeaderDigest=CRC32C\0DataDigest=CRC32C\0MaxRecvDataSegmentLength=4096\0MaxBurstLength=8192\0FirstBurstLength=4096\0".as_slice()
        } else {
            b"HeaderDigest=None\0DataDigest=None\0MaxRecvDataSegmentLength=4096\0MaxBurstLength=8192\0FirstBurstLength=4096\0".as_slice()
        };
        send(
            &mut socket,
            &h,
            if stage == 0 {
                b"AuthMethod=None\0"
            } else {
                ops
            },
            false,
        )
        .await;
    }
    socket
}

fn target(port: u16) -> Target {
    Target {
        addr: TargetAddr {
            host: "127.0.0.1".into(),
            port,
        },
        iqn: "iqn.2026-11.test:a:b:c".into(),
        lun: 1,
        chap: None,
    }
}

fn response(op: u8, flags: u8, command: &Pdu, statsn: u32) -> Bhs {
    let mut h = Bhs::new(op, flags);
    h.put_u32(16, command.header.itt());
    h.put_u32(24, statsn);
    h.put_u32(28, command.header.u32(24).wrapping_add(1));
    h.put_u32(32, command.header.u32(24).wrapping_add(128));
    h
}

async fn data_in(
    socket: &mut TcpStream,
    command: &Pdu,
    statsn: &mut u32,
    data: &[u8],
    digest: bool,
) {
    let expected = command.header.u32(20) as usize;
    let mut h = response(
        wire::DATA_IN,
        if data.len() < expected { 0x83 } else { 0x81 },
        command,
        *statsn,
    );
    if data.len() < expected {
        h.put_u32(44, (expected - data.len()) as u32);
    }
    // At ERL 0 an initiator MUST ignore the A bit, even when the peer sets it.
    h.0[1] |= 0x40;
    send(socket, &h, data, digest).await;
    *statsn += 1;
}

async fn capacity(socket: &mut TcpStream, statsn: &mut u32, digest: bool) {
    let command = recv(socket, digest).await;
    assert_eq!(command.header.0[32], 0x25);
    data_in(
        socket,
        &command,
        statsn,
        &[0, 0, 0x3f, 0xff, 0, 0, 2, 0],
        digest,
    )
    .await;
}

async fn logout(socket: &mut TcpStream, statsn: u32, digest: bool) {
    let command = recv(socket, digest).await;
    assert_eq!(command.header.opcode(), wire::LOGOUT_REQUEST);
    send(
        socket,
        &response(wire::LOGOUT_RESPONSE, 0x80, &command, statsn),
        &[],
        digest,
    )
    .await;
}

#[tokio::test]
async fn cooperative_benchmark_cancel_drains_workers_before_logout_without_flush() {
    use iscsi_core::{BenchmarkCancellation, benchmark_cancellable};
    for pattern in [BenchPattern::SeqRead, BenchPattern::SeqWrite] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancellation = BenchmarkCancellation::default();
        let stop = cancellation.clone();
        let server = tokio::spawn(async move {
            let mut socket = accept_login(listener, false).await;
            let mut statsn = 2;
            capacity(&mut socket, &mut statsn, false).await;
            let mut commands = Vec::new();
            for _ in 0..4 {
                let command = recv(&mut socket, false).await;
                assert_eq!(
                    command.header.0[32],
                    if pattern.is_write() { 0x8a } else { 0x88 }
                );
                commands.push(command);
            }
            stop.cancel();
            for command in commands {
                if pattern.is_write() {
                    send(
                        &mut socket,
                        &response(wire::SCSI_RESPONSE, 0x80, &command, statsn),
                        &[],
                        false,
                    )
                    .await;
                    statsn += 1;
                } else {
                    data_in(&mut socket, &command, &mut statsn, &[0; 512], false).await;
                }
            }
            // The very next command must be Logout, never another I/O or flush.
            logout(&mut socket, statsn, false).await;
        });
        let session = login(
            &target(port),
            &SessionOptions {
                queue_depth: 4,
                ..SessionOptions::default()
            },
        )
        .await
        .unwrap();
        let report = tokio::time::timeout(
            Duration::from_secs(3),
            benchmark_cancellable(
                &session,
                BenchOptions {
                    pattern,
                    block_size: 512,
                    queue_depths: vec![4],
                    duration: Duration::from_secs(60),
                    warmup: Duration::ZERO,
                    range_blocks: Some(1024),
                    allow_destructive_write: pattern.is_write(),
                    ..BenchOptions::default()
                },
                cancellation,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(report.is_none());
        session.clone().logout().await.unwrap();
        assert!(session.is_closed());
        server.await.unwrap();
    }
}

#[tokio::test]
async fn pipelined_reads_complete_out_of_order_and_keepalive() {
    for digest in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let mut socket = accept_login(listener, digest).await;
            let mut statsn = 2;
            capacity(&mut socket, &mut statsn, digest).await;
            // Reading all commands before responding proves command pipelining.
            let mut commands = Vec::new();
            for _ in 0..3 {
                let command = recv(&mut socket, digest).await;
                assert_eq!(command.header.0[32], 0x88);
                commands.push(command);
            }
            let mut nop = Bhs::new(wire::NOP_IN, 0x80);
            nop.put_u32(16, wire::RESERVED_TAG);
            nop.put_u32(20, 0x1234);
            nop.put_u32(24, statsn);
            nop.put_u32(32, 128);
            send(&mut socket, &nop, b"ping", digest).await;
            let pong = recv(&mut socket, digest).await;
            assert_eq!(pong.header.opcode(), wire::NOP_OUT);
            assert_eq!(pong.header.u32(20), 0x1234);
            assert_eq!(pong.data, b"ping");
            for command in commands.iter().rev() {
                let lba = command.header.0[41];
                data_in(&mut socket, command, &mut statsn, &vec![lba; 512], digest).await;
            }
            logout(&mut socket, statsn, digest).await;
        });
        let session = login(
            &target(port),
            &SessionOptions {
                header_digest: digest,
                data_digest: digest,
                queue_depth: 3,
                io_timeout: Duration::from_secs(3),
                ..SessionOptions::default()
            },
        )
        .await
        .unwrap();
        session.capacity().await.unwrap();
        let (a, b, c) = tokio::join!(session.read(1, 1), session.read(2, 1), session.read(3, 1));
        assert_eq!(a.unwrap(), vec![1; 512]);
        assert_eq!(b.unwrap(), vec![2; 512]);
        assert_eq!(c.unwrap(), vec![3; 512]);
        assert_eq!(session.stats().bytes_read, 8 + 1536);
        session.logout().await.unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn immediate_write_and_multiple_r2t_bursts() {
    for digest in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let mut socket = accept_login(listener, digest).await;
            let mut statsn = 2;
            capacity(&mut socket, &mut statsn, digest).await;
            let command = recv(&mut socket, digest).await;
            assert_eq!(command.header.0[32], 0x8a);
            assert_eq!(command.data, vec![0x5a; 4096]);
            let mut offset = 4096;
            for (r2tsn, count) in [(0, 8192), (1, 4096)] {
                let mut h = response(wire::R2T, 0x80, &command, statsn);
                h.put_u32(20, 0xabcd);
                h.put_u32(36, r2tsn);
                h.put_u32(40, offset);
                h.put_u32(44, count);
                send(&mut socket, &h, &[], digest).await;
                for datasn in 0..count / 4096 {
                    let data = recv(&mut socket, digest).await;
                    assert_eq!(data.header.opcode(), wire::DATA_OUT);
                    assert_eq!(data.header.u32(36), datasn);
                    assert_eq!(data.header.u32(40), offset);
                    assert_eq!(data.header.u32(20), 0xabcd);
                    assert_eq!(data.header.0[1] & 0x80 != 0, datasn + 1 == count / 4096);
                    assert_eq!(data.data, vec![0x5a; 4096]);
                    offset += 4096;
                }
            }
            send(
                &mut socket,
                &response(wire::SCSI_RESPONSE, 0x80, &command, statsn),
                &[],
                digest,
            )
            .await;
            statsn += 1;
            let small = recv(&mut socket, digest).await;
            assert_eq!(small.header.0[32], 0x8a);
            assert_eq!(small.data, vec![0x6b; 512]);
            send(
                &mut socket,
                &response(wire::SCSI_RESPONSE, 0x80, &small, statsn),
                &[],
                digest,
            )
            .await;
            statsn += 1;
            let flush = recv(&mut socket, digest).await;
            assert_eq!(flush.header.0[32], 0x91);
            send(
                &mut socket,
                &response(wire::SCSI_RESPONSE, 0x80, &flush, statsn),
                &[],
                digest,
            )
            .await;
            statsn += 1;
            logout(&mut socket, statsn, digest).await;
        });
        let session = login(
            &target(port),
            &SessionOptions {
                header_digest: digest,
                data_digest: digest,
                io_timeout: Duration::from_secs(3),
                ..SessionOptions::default()
            },
        )
        .await
        .unwrap();
        session.write(0, &vec![0x5a; 16384]).await.unwrap();
        session.write(32, &[0x6b; 512]).await.unwrap();
        session.flush().await.unwrap();
        assert_eq!(session.stats().bytes_written, 16384 + 512);
        session.logout().await.unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn malformed_data_in_fails_all_waiters_without_hanging() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let mut socket = accept_login(listener, false).await;
        capacity(&mut socket, &mut 2, false).await;
        let command = recv(&mut socket, false).await;
        let mut h = response(wire::DATA_IN, 0x81, &command, 3);
        h.put_u32(40, 512); // Invalid first offset.
        send(&mut socket, &h, &[0; 512], false).await;
    });
    let session = login(&target(port), &SessionOptions::default())
        .await
        .unwrap();
    assert!(matches!(session.read(0, 1).await, Err(Error::Protocol(_))));
    server.await.unwrap();
}

#[tokio::test]
async fn write_benchmark_requires_explicit_range_and_confirmation_before_io() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let mut socket = accept_login(listener, false).await;
        logout(&mut socket, 2, false).await;
    });
    let session = login(&target(port), &SessionOptions::default())
        .await
        .unwrap();
    let result = benchmark(
        &session,
        BenchOptions {
            pattern: BenchPattern::SeqWrite,
            ..BenchOptions::default()
        },
    )
    .await;
    assert!(matches!(result, Err(Error::Config(_))));
    session.logout().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn chap_login_uses_binary_challenge_and_configured_identity() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        for (statsn, flags, reply) in [
            (0, 0, b"AuthMethod=CHAP\0".as_slice()),
            (1, 0, b"CHAP_A=5\0CHAP_I=1\0CHAP_C=0bAQIDBA==\0".as_slice()),
            (2, 0x81, b"".as_slice()),
            (3, 0x87, b"HeaderDigest=None\0DataDigest=None\0".as_slice()),
        ] {
            let request = recv(&mut socket, false).await;
            let pairs = wire::parse_text(&request.data).unwrap();
            if statsn == 1 {
                assert!(pairs.contains(&("chap_a".into(), "5".into())));
            }
            if statsn == 2 {
                assert!(pairs.contains(&("chap_n".into(), "test-user".into())));
                assert!(
                    pairs.contains(&("chap_r".into(), "0xafd81c66f2d5f7fc733e1b2d88bb2630".into()))
                );
            }
            let mut h = response(wire::LOGIN_RESPONSE, flags, &request, statsn);
            h.0[8..14].copy_from_slice(&request.header.0[8..14]);
            h.put_u32(28, 1);
            send(&mut socket, &h, reply, false).await;
        }
        logout(&mut socket, 4, false).await;
    });
    let mut target = target(port);
    target.chap = Some(iscsi_core::ChapCredentials {
        user: "test-user".into(),
        secret: "secret".into(),
    });
    let session = login(&target, &SessionOptions::default()).await.unwrap();
    session.logout().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn benchmarks_account_for_all_bytes_and_drain_all_four_patterns() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let mut socket = accept_login(listener, false).await;
        let mut statsn = 2;
        let mut flushes = 0;
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
                assert_eq!(flushes, 4); // two write patterns, two queue depths.
                return;
            }
            match command.header.0[32] {
                0x25 => {
                    data_in(
                        &mut socket,
                        &command,
                        &mut statsn,
                        &[0, 0, 0x3f, 0xff, 0, 0, 2, 0],
                        false,
                    )
                    .await
                }
                0x88 => data_in(&mut socket, &command, &mut statsn, &[0x4a; 512], false).await,
                0x8a | 0x91 => {
                    if command.header.0[32] == 0x8a {
                        assert_eq!(command.data.len(), 512);
                    } else {
                        flushes += 1;
                    }
                    send(
                        &mut socket,
                        &response(wire::SCSI_RESPONSE, 0x80, &command, statsn),
                        &[],
                        false,
                    )
                    .await;
                    statsn += 1;
                }
                op => panic!("unexpected CDB {op:#x}"),
            }
        }
    });
    let session = login(&target(port), &SessionOptions::default())
        .await
        .unwrap();
    for pattern in [
        BenchPattern::SeqRead,
        BenchPattern::RandRead,
        BenchPattern::SeqWrite,
        BenchPattern::RandWrite,
    ] {
        let report = benchmark(
            &session,
            BenchOptions {
                pattern,
                block_size: 512,
                queue_depths: vec![1, 4],
                duration: Duration::from_millis(25),
                warmup: Duration::from_millis(5),
                range_blocks: Some(1024),
                allow_destructive_write: true,
                ..BenchOptions::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(report.results.len(), 2);
        for result in report.results {
            assert!(result.commands > 0);
            assert_eq!(result.bytes, result.commands * 512);
            assert!(result.elapsed_seconds >= 0.025);
            assert!(result.p99_latency_us >= result.p50_latency_us);
        }
        if pattern.is_write() {
            assert!(report.flush_seconds > 0.0);
        }
    }
    session.logout().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn timed_out_command_closes_session_instead_of_reusing_tags() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let mut socket = accept_login(listener, false).await;
        let command = recv(&mut socket, false).await;
        assert_eq!(command.header.0[32], 0);
        tokio::time::sleep(Duration::from_millis(150)).await;
        let mut data = [0; 1];
        use tokio::io::AsyncReadExt;
        assert_eq!(socket.read(&mut data).await.unwrap(), 0);
    });
    let session = login(
        &target(port),
        &SessionOptions {
            io_timeout: Duration::from_millis(50),
            ..SessionOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        session.test_unit_ready().await,
        Err(Error::Timeout)
    ));
    assert!(session.test_unit_ready().await.is_err());
    server.await.unwrap();
}

#[tokio::test]
async fn immediate_logout_does_not_require_an_open_command_window() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let mut socket = accept_login_window(listener, false, 0).await;
        logout(&mut socket, 2, false).await;
    });
    let session = login(
        &target(port),
        &SessionOptions {
            io_timeout: Duration::from_secs(1),
            ..SessionOptions::default()
        },
    )
    .await
    .unwrap();
    session.logout().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn cancelled_write_is_not_sent_when_window_reopens() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let mut socket = accept_login_window(listener, false, 1).await;
        let command = recv(&mut socket, false).await;
        assert_eq!(command.header.0[32], 0x25);
        let mut capacity = response(wire::DATA_IN, 0x81, &command, 2);
        capacity.put_u32(32, 1); // ExpCmdSN=2, MaxCmdSN=1: zero window.
        send(
            &mut socket,
            &capacity,
            &[0, 0, 0x3f, 0xff, 0, 0, 2, 0],
            false,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(80)).await;
        let mut nop = Bhs::new(wire::NOP_IN, 0x80);
        nop.put_u32(16, wire::RESERVED_TAG);
        nop.put_u32(20, wire::RESERVED_TAG);
        nop.put_u32(24, 3);
        nop.put_u32(28, 2);
        nop.put_u32(32, 128);
        send(&mut socket, &nop, &[], false).await;
        // The next transmitted PDU must be Logout, never WRITE(16).
        logout(&mut socket, 3, false).await;
    });
    let session = login(
        &target(port),
        &SessionOptions {
            io_timeout: Duration::from_secs(1),
            ..SessionOptions::default()
        },
    )
    .await
    .unwrap();
    session.capacity().await.unwrap();
    let writer_session = session.clone();
    let writer = tokio::spawn(async move { writer_session.write(0, &[0x5a; 512]).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    writer.abort();
    assert!(writer.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(100)).await;
    session.logout().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn burst_keys_can_arrive_in_separate_login_responses() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        for (statsn, flags, reply) in [
            (0, 0x81, b"AuthMethod=None\0".as_slice()),
            (1, 0x04, b"MaxBurstLength=65536\0".as_slice()),
            (2, 0x87, b"FirstBurstLength=65536\0".as_slice()),
        ] {
            let command = recv(&mut socket, false).await;
            let mut h = response(wire::LOGIN_RESPONSE, flags, &command, statsn);
            h.0[8..14].copy_from_slice(&command.header.0[8..14]);
            h.put_u32(28, 1);
            send(&mut socket, &h, reply, false).await;
        }
        logout(&mut socket, 3, false).await;
    });
    let session = login(&target(port), &SessionOptions::default())
        .await
        .unwrap();
    assert_eq!(session.negotiated().max_burst_length, 65536);
    assert_eq!(session.negotiated().first_burst_length, 65536);
    session.logout().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn target_can_disable_immediate_data_and_request_entire_write() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        for (statsn, flags, reply) in [
            (0, 0x81, b"AuthMethod=None\0".as_slice()),
            (1, 0x87, b"ImmediateData=No\0".as_slice()),
        ] {
            let command = recv(&mut socket, false).await;
            let mut h = response(wire::LOGIN_RESPONSE, flags, &command, statsn);
            h.0[8..14].copy_from_slice(&command.header.0[8..14]);
            h.put_u32(28, 1);
            send(&mut socket, &h, reply, false).await;
        }
        let mut statsn = 2;
        capacity(&mut socket, &mut statsn, false).await;
        let command = recv(&mut socket, false).await;
        assert_eq!(command.header.0[32], 0x8a);
        assert!(command.data.is_empty());
        let mut h = response(wire::R2T, 0x80, &command, statsn);
        h.put_u32(20, 0x5678);
        h.put_u32(44, 512);
        send(&mut socket, &h, &[], false).await;
        let payload = recv(&mut socket, false).await;
        assert_eq!(payload.header.opcode(), wire::DATA_OUT);
        assert_eq!(payload.header.u32(20), 0x5678);
        assert_eq!(payload.data, vec![0x7c; 512]);
        send(
            &mut socket,
            &response(wire::SCSI_RESPONSE, 0x80, &command, statsn),
            &[],
            false,
        )
        .await;
        logout(&mut socket, statsn + 1, false).await;
    });
    let session = login(&target(port), &SessionOptions::default())
        .await
        .unwrap();
    assert!(!session.negotiated().immediate_data);
    session.write(0, &[0x7c; 512]).await.unwrap();
    session.logout().await.unwrap();
    server.await.unwrap();
}
