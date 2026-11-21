use super::route_policy_tests::{client_init, reply_is_res_pq, request_pq};
use super::*;
use rand::SeedableRng;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use std::collections::BTreeMap;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

const CASES: [i16; 9] = [1, -1, 2, -2, 4, -4, 5, -5, 203];
const KEY: [u8; 16] = [0x42; 16];

fn bytes_at(offset: usize, sample: &mut [u8]) {
    for (i, byte) in sample.iter_mut().enumerate() {
        *byte = ((offset + i) % 251) as u8;
    }
}

async fn check_media(length: usize) {
    let (websocket, mut upstream) = crate::ws::tests::loopback_ws().await;
    let (mut consumer, proxy) = tokio::io::duplex(16 * 1024);
    let (read, write) = tokio::io::split(proxy);
    let cancel = CancellationToken::new();
    let cipher = || new_aes_ctr(&[23; 32], &[12; 16]);
    let relay = bridge_ws(
        read,
        write,
        websocket,
        "fixture".into(),
        2,
        true,
        None,
        cipher(),
        cipher(),
        cipher(),
        cipher(),
        cancel.clone(),
        None,
        |_| {},
    );
    let source = async {
        let mut random = rand::rngs::StdRng::seed_from_u64(length as u64);
        let mut offset = 0;
        while offset < length {
            let n = random.gen_range(1024..65536).min(length - offset);
            let mut sample = vec![0; n];
            bytes_at(offset, &mut sample);
            // Alternate complete messages and fragmented messages.
            if offset % 3 == 0 {
                let mut first = build_frame(OP_BINARY, &sample[..n / 2], false);
                first[0] &= 0x7f;
                upstream.write_all(&first).await.unwrap();
                upstream
                    .write_all(&build_frame(OP_CONT, &sample[n / 2..], false))
                    .await
                    .unwrap();
            } else {
                upstream
                    .write_all(&build_frame(OP_BINARY, &sample, false))
                    .await
                    .unwrap();
            }
            offset += n;
        }
        // A mid-stream disconnect is also observable as EOF, with the exact prefix delivered.
        upstream.shutdown().await.unwrap();
    };
    let sink = async {
        let mut received = [0; 8192];
        let mut expected = [0; 8192];
        let mut offset = 0;
        loop {
            let n = consumer.read(&mut received).await.unwrap();
            if n == 0 {
                break;
            }
            bytes_at(offset, &mut expected[..n]);
            assert_eq!(&received[..n], &expected[..n]);
            offset += n;
            if offset / 8192 % 23 == 0 {
                tokio::task::yield_now().await;
            }
        }
        assert_eq!(offset, length);
    };
    tokio::time::timeout(Duration::from_secs(50), async {
        tokio::join!(relay, source, sink);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn simultaneous_large_media_relays_keep_the_exact_payload() {
    tokio::join!(
        check_media(1 << 20),
        check_media(10 << 20),
        check_media(50 << 20)
    );
}

async fn receive_masked<R: AsyncRead + Unpin>(read: &mut R) -> std::io::Result<(u8, Vec<u8>)> {
    let mut header = [0; 2];
    read.read_exact(&mut header).await?;
    let mut length = u16::from(header[1] & 0x7f);
    if length == 126 {
        length = read.read_u16().await?;
    } else if length == 127 {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    if header[1] & 0x80 == 0 {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut mask = [0; 4];
    read.read_exact(&mut mask).await?;
    let mut body = vec![0; usize::from(length)];
    read.read_exact(&mut body).await?;
    xor_mask_in_place(&mut body, &mask);
    Ok((header[0] & 15, body))
}

#[derive(Default)]
struct RouteScenario {
    sessions: parking_lot::Mutex<Vec<String>>,
    handover: std::sync::atomic::AtomicBool,
    reject_media: std::sync::atomic::AtomicBool,
}

async fn fixture_peer<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    scenario: Option<Arc<RouteScenario>>,
) -> std::io::Result<()> {
    let mut stream = BufReader::new(stream);
    let mut key = String::new();
    let mut host = String::new();
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        if let Some(value) = line.strip_prefix("Sec-WebSocket-Key: ") {
            key = value.trim().to_owned();
        }
        if let Some(value) = line.strip_prefix("Host: ") {
            host = value.trim().to_owned();
        }
        if line == "\r\n" {
            break;
        }
    }
    // Keep startup dials long enough for the concurrency challenge to exercise overlap.
    tokio::time::sleep(Duration::from_millis(18)).await;
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Protocol: binary\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        expected_ws_accept(&key)
    );
    if scenario
        .as_ref()
        .is_some_and(|fixture| fixture.handover.swap(false, Ordering::Relaxed))
    {
        crate::NotifyDefaultNetworkState(2, 0, 0);
    }
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;

    let mut ciphers = None;
    loop {
        let (opcode, mut body) = receive_masked(&mut stream).await?;
        match opcode {
            OP_CLOSE => return Ok(()),
            OP_PING => {
                stream
                    .write_all(&build_frame(OP_PONG, &body, false))
                    .await?
            }
            OP_BINARY if ciphers.is_none() && body.len() == 64 => {
                let reversed: Vec<_> = body[8..56].iter().rev().copied().collect();
                let mut decrypt = new_aes_ctr(&body[8..40], &body[40..56]);
                decrypt.xor(&mut body);
                let id = i16::from_le_bytes(body[60..62].try_into().unwrap());
                assert!(CASES.contains(&id));
                if let Some(scenario) = &scenario {
                    scenario.sessions.lock().push(host.clone());
                }
                assert_eq!(&body[56..60], &[0xdd; 4]);
                ciphers = Some((decrypt, new_aes_ctr(&reversed[..32], &reversed[32..])));
            }
            OP_BINARY if body.len() == 44 => {
                let Some((read_cipher, write_cipher)) = ciphers.as_mut() else {
                    return Err(std::io::ErrorKind::InvalidData.into());
                };
                read_cipher.xor(&mut body);
                assert_eq!(&body[24..28], &0xbe7e8ef1u32.to_le_bytes());
                if host == "kws2-1.web.telegram.org"
                    && scenario.as_ref().is_some_and(|fixture| fixture.reject_media.load(Ordering::Relaxed))
                {
                    let mut reply = [0; 24];
                    reply[..4].copy_from_slice(&8u32.to_le_bytes());
                    reply[4..8].copy_from_slice(&(-1i32).to_le_bytes());
                    reply[8..12].copy_from_slice(&0x8001_0203u32.to_le_bytes());
                    reply[12..16].copy_from_slice(&8u32.to_le_bytes());
                    reply[16..20].copy_from_slice(&(-404i32).to_le_bytes());
                    write_cipher.xor(&mut reply);
                    for chunk in [&reply[..7], &reply[7..17], &reply[17..]] {
                        stream
                            .write_all(&build_frame(OP_BINARY, chunk, false))
                            .await?;
                    }
                    stream.flush().await?;
                    return Ok(());
                }
                let mut reply = [0; 44];
                reply[..4].copy_from_slice(&40u32.to_le_bytes());
                reply[20..24].copy_from_slice(&20u32.to_le_bytes());
                reply[24..28].copy_from_slice(&0x05162463u32.to_le_bytes());
                reply[28..].copy_from_slice(&body[28..]);
                write_cipher.xor(&mut reply);
                stream
                    .write_all(&build_frame(OP_BINARY, &reply, false))
                    .await?;
            }
            _ => return Err(std::io::ErrorKind::InvalidData.into()),
        }
        stream.flush().await?;
    }
}

async fn fixture(scenario: Option<Arc<RouteScenario>>) -> tokio::task::JoinHandle<()> {
    let hostnames = CASES
        .iter()
        .flat_map(|dc| ws_domains(i32::from(*dc).abs(), *dc < 0))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let cert = rcgen::generate_simple_self_signed(hostnames).unwrap();
    let cert_der = cert.cert.der().clone();
    let key_der = rustls_pki_types::PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
    let server = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der.clone()], key_der)
        .unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(cert_der).unwrap();
    let client = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    *crate::ws::LOOPBACK_FIXTURE.lock() =
        Some((listener.local_addr().unwrap().port(), Arc::new(client)));
    let acceptor = TlsAcceptor::from(Arc::new(server));
    tokio::spawn(async move {
        let mut peers = JoinSet::new();
        loop {
            tokio::select! {
                _ = peers.join_next(), if !peers.is_empty() => {}
                incoming = listener.accept() => {
                    let (socket, _) = incoming.unwrap();
                    let acceptor = acceptor.clone();
                    let scenario = scenario.clone();
                    peers.spawn(async move {
                        if let Ok(tls) = acceptor.accept(socket).await {
                            let _ = fixture_peer(tls, scenario).await;
                        }
                    });
                }
            }
        }
    })
}

#[derive(Debug, Eq, PartialEq)]
enum FixtureReply {
    ResPq,
    TransportError(i32),
    Invalid,
}

async fn client_reply(addr: std::net::SocketAddr, dc: i16) -> FixtureReply {
    let Ok(mut stream) = TcpStream::connect(addr).await else {
        return FixtureReply::Invalid;
    };
    client_reply_on(&mut stream, dc).await
}

async fn client_reply_on(stream: &mut TcpStream, dc: i16) -> FixtureReply {
    client_reply_with_budget(stream, dc, Duration::from_secs(3)).await
}

async fn client_reply_with_budget(stream: &mut TcpStream, dc: i16, budget: Duration) -> FixtureReply {
    let result = tokio::time::timeout(budget, async {
        let init = client_init(&KEY, dc);
        let (_, mut tx, _) = decode_client_init(&init, &KEY, false).unwrap();
        let reversed: Vec<_> = init[8..56].iter().rev().copied().collect();
        let key = Sha256::digest([&reversed[..32], KEY.as_slice()].concat());
        let mut rx = new_aes_ctr(&key, &reversed[32..]);
        let (mut request, nonce) = request_pq();
        tx.xor(&mut request);
        stream.write_all(&init).await?;
        stream.write_all(&request).await?;
        let mut response = [0; 44];
        for _ in 0..2 {
            stream.read_exact(&mut response[..4]).await?;
            rx.xor(&mut response[..4]);
            let size = u32::from_le_bytes(response[..4].try_into().unwrap()) as usize;
            if !matches!(size, 8 | 40) {
                break;
            }
            stream.read_exact(&mut response[4..4 + size]).await?;
            rx.xor(&mut response[4..4 + size]);
            if size == 8 {
                let control = i32::from_le_bytes(response[4..8].try_into().unwrap());
                if control == -1 {
                    continue;
                }
                return Ok::<_, std::io::Error>(FixtureReply::TransportError(control));
            }
            if reply_is_res_pq(&response, &nonce) {
                return Ok(FixtureReply::ResPq);
            }
        }
        Ok(FixtureReply::Invalid)
    })
    .await;
    result
        .ok()
        .and_then(Result::ok)
        .unwrap_or(FixtureReply::Invalid)
}

#[tokio::test]
#[ignore = "requires an isolated loopback reference proxy with a synthetic secret"]
async fn reference_loopback_proxy_probe() {
    let port: u16 = std::env::var("TGWS_REFERENCE_PORT").unwrap().parse().unwrap();
    let (main, media) = tokio::join!(probe_reference(port, 2), probe_reference(port, -4));
    assert!(main && media, "reference proxy did not return both nonce-matched responses");
}

async fn probe_reference(port: u16, dc: i16) -> bool {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let reply = client_reply_with_budget(&mut socket, dc, Duration::from_secs(22)).await;
    println!("reference_proxy dc={dc} result={reply:?}");
    reply == FixtureReply::ResPq
}

async fn client(addr: std::net::SocketAddr, dc: i16) -> (i16, bool, u128) {
    let start = Instant::now();
    let reply = client_reply(addr, dc).await;
    (
        dc,
        reply == FixtureReply::ResPq,
        start.elapsed().as_millis(),
    )
}

fn report(mode: i32, phase: &str, sample: &[(i16, bool, u128)]) {
    let mut times = sample
        .iter()
        .filter_map(|(_, ok, ms)| ok.then_some(*ms))
        .collect::<Vec<_>>();
    times.sort_unstable();
    let (median, max) = (times.get(times.len() / 2), times.last());
    let mut by_dc = BTreeMap::<i16, [usize; 2]>::new();
    for &(dc, ok, _) in sample {
        let totals = by_dc.entry(dc).or_default();
        totals[0] += usize::from(ok);
        totals[1] += 1;
    }
    println!("cohort pool={mode} phase={phase} valid={}/{} median_ms={median:?} max_ms={max:?} by_signed_dc={by_dc:?}",
        times.len(), sample.len());
    assert_eq!(times.len(), sample.len(), "{phase} pool={mode}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "loopback load benchmark with a deliberate idle interval"]
async fn cold_warm_idle_matrix() {
    let idle = std::env::var("TGWS_IDLE_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(180);
    let modes = std::env::var("TGWS_POOL_MODES").unwrap_or_else(|_| "0,1,4".into());
    CFPROXY_ENABLED.store(false, Ordering::Relaxed);
    TCP_FALLBACK_ENABLED.store(false, Ordering::Relaxed);
    assert!(CF_WORKER_DOMAINS.read().is_empty());
    *PROXY_SECRET.write() = hex::encode(KEY);
    let server = fixture(None).await;

    for mode in modes.split(',').map(|word| word.parse::<i32>().unwrap()) {
        POOL_SIZE.store(mode, Ordering::Relaxed);
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let endpoints = CASES
            .into_iter()
            .map(|dc| (i32::from(dc), "127.0.0.1".to_owned()))
            .collect();
        let cancel = CancellationToken::new();
        let pools = Arc::new(ProxyPools::new(cancel.clone()));
        let proxy = tokio::spawn(run_proxy(
            pools.clone(),
            "127.0.0.1".into(),
            addr.port(),
            endpoints,
            cancel.clone(),
            listener,
        ));
        let mut sequential = Vec::new();
        for i in 0..30 {
            sequential.push(client(addr, CASES[i % CASES.len()]).await);
        }
        report(mode, "cold_first_10", &sequential[..10]);
        report(mode, "cold_next_20", &sequential[10..]);

        tokio::time::sleep(Duration::from_millis(350)).await;
        sequential.clear();
        for i in 0..30 {
            sequential.push(client(addr, CASES[i % CASES.len()]).await);
        }
        report(mode, "warm", &sequential);

        tokio::time::sleep(Duration::from_secs(idle)).await;
        sequential.clear();
        for i in 0..30 {
            sequential.push(client(addr, CASES[i % CASES.len()]).await);
        }
        report(mode, &format!("idle_{idle}s"), &sequential);

        for count in [1, 5, 10, 20] {
            let mut work = JoinSet::new();
            for i in 0..count {
                work.spawn(client(addr, CASES[i % CASES.len()]));
            }
            let mut overlap = Vec::new();
            while let Some(result) = work.join_next().await {
                overlap.push(result.unwrap());
            }
            report(mode, &format!("concurrent_{count}"), &overlap);
        }
        cancel.cancel();
        proxy.await.unwrap().unwrap();
        pools.close_all().await;
    }
    server.abort();
    let _ = server.await;
    *crate::ws::LOOPBACK_FIXTURE.lock() = None;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "isolates the production listener's process-wide route settings"]
async fn sticky_listener_pins_one_init_and_rejects_cross_epoch_upgrade() {
    CFPROXY_ENABLED.store(false, Ordering::Relaxed);
    TCP_FALLBACK_ENABLED.store(false, Ordering::Relaxed);
    POOL_SIZE.store(0, Ordering::Relaxed);
    *PROXY_SECRET.write() = hex::encode(KEY);
    let scenario = Arc::new(RouteScenario::default());
    scenario.reject_media.store(true, Ordering::Relaxed);
    let server = fixture(Some(scenario.clone())).await;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let pools = Arc::new(ProxyPools::new(cancel.clone()));
    let proxy = tokio::spawn(run_proxy(
        pools,
        "127.0.0.1".into(),
        address.port(),
        HashMap::from([(-2, "127.0.0.1".into())]),
        cancel.clone(),
        listener,
    ));

    let primary = "kws2-1.web.telegram.org";
    for expected_count in 1..=2 {
        assert_eq!(
            client_reply(address, -2).await,
            FixtureReply::TransportError(-404)
        );
        assert_eq!(
            scenario.sessions.lock().as_slice(),
            vec![primary; expected_count]
        );
    }
    // Media DC2 must not jump to the main hostname when the media route cools.
    let failed_at = Instant::now();
    assert_eq!(client_reply(address, -2).await, FixtureReply::Invalid);
    assert!(failed_at.elapsed() >= MIN_FAILED_SESSION_TIME);
    assert_eq!(scenario.sessions.lock().as_slice(), [primary, primary]);

    scenario.handover.store(true, Ordering::Relaxed);
    assert!(matches!(
        ws_connect_full_opts("127.0.0.1", primary, WS_PATH, 2.0, None, Some(true)).await,
        Err(WsError::NetworkChanged)
    ));
    assert_eq!(scenario.sessions.lock().as_slice(), [primary, primary]);
    scenario.reject_media.store(false, Ordering::Relaxed);
    let mut media = TcpStream::connect(address).await.unwrap();
    assert_eq!(client_reply_on(&mut media, -2).await, FixtureReply::ResPq);
    assert_eq!(scenario.sessions.lock().as_slice(), [primary, primary, primary]);

    // A link/DNS update invalidates new dials but preserves the established relay.
    crate::NotifyDefaultNetworkState(2, 1, 0);
    let mut byte = [0];
    assert!(tokio::time::timeout(Duration::from_millis(150), media.read(&mut byte)).await.is_err());
    // A replaced Android default network releases the old client promptly.
    crate::NotifyDefaultNetworkState(1, 2, 1);
    assert_eq!(tokio::time::timeout(Duration::from_secs(1), media.read(&mut byte)).await.unwrap().unwrap(), 0);
    assert_eq!(client_reply(address, -2).await, FixtureReply::ResPq);

    cancel.cancel();
    proxy.await.unwrap().unwrap();
    server.abort();
    let _ = server.await;
    *crate::ws::LOOPBACK_FIXTURE.lock() = None;
}
