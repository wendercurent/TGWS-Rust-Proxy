use crate::trace::{self, Route};
use crate::routing::{Candidate, Evidence, FailureClass, Manager, RouteUse, Scope, SkipReason};
use crate::cfproxy::*;
use crate::config::*;
use crate::crypto::*;
use crate::faketls;
use crate::ws::*;
use crate::{ldebug, linfo, lwarn};
use byteorder::{ByteOrder, LittleEndian};
use rand::{Rng, RngCore};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Target resolution
// ---------------------------------------------------------------------------

pub(crate) const SHARED_DIRECT_WSS_IP: &str = "149.154.167.220";
const MIN_FAILED_SESSION_TIME: Duration = Duration::from_millis(500);

pub fn resolve_configured_target(key: DcKey) -> Option<String> {
    select_direct_target(&DC_OPT.read(), key).map(str::to_owned)
}

fn configured_target(map: &HashMap<i32, String>, dc: i32, is_media: bool) -> Option<&String> {
    let media_target = is_media.then(|| map.get(&-dc)).flatten();
    media_target.filter(|target| !target.is_empty())
        .or_else(|| map.get(&dc).filter(|target| !target.is_empty()))
}

fn select_direct_target(map: &HashMap<i32, String>, key: DcKey) -> Option<&str> {
    configured_target(map, key.dc, key.is_media != 0)
        .map(String::as_str)
        .or_else(|| {
            // Telegram's DC203 WSS hostname alias uses the same verified DC2
            // service. The relay init keeps the original positive 203 ID.
            (key == DcKey::new(203, false, false)).then_some(SHARED_DIRECT_WSS_IP)
        })
}

pub fn resolve_fallback_target(dc: i32, is_media: bool, is_test: bool) -> String {
    if is_test {
        DC_TEST_IPS
            .get(&dc)
            .map(|s| s.to_string())
            .unwrap_or_default()
    } else {
        if is_media {
            if let Some(ip) = DC_OPT.read().get(&-dc) {
                return ip.clone();
            }
        }
        DC_DEFAULT_IPS
            .get(&dc)
            .map(|s| s.to_string())
            .unwrap_or_default()
    }
}

pub fn ws_domains(dc: i32, is_media: bool) -> Vec<String> {
    let mut effective_dc = dc;
    if let Some(o) = DC_OVERRIDES.get(&dc) {
        effective_dc = *o;
    }
    if is_media {
        // A media auth key on the main kwsN name completes WSS and then
        // returns MTProto transport -404. Device traces: signed DC -4 on
        // kws4 delivered 12 bytes and closed; the same DC on kws4-1 moved
        // megabytes. Voice notes and round videos use this media role.
        vec![format!("kws{}-1.web.telegram.org", effective_dc)]
    } else {
        vec![format!("kws{}.web.telegram.org", effective_dc)]
    }
}

fn direct_ws_domains(key: DcKey, target: &str, secure: bool) -> Vec<String> {
    let mut hosts = ws_domains(key.dc, key.is_media != 0);
    // On the inherited direct target the first-party -1 hostname returned
    // nonce-matched resPQ with a positive DC4 init. Keep that role unchanged.
    if secure && key == DcKey::new(4, false, false) && target == SHARED_DIRECT_WSS_IP {
        hosts.push("kws4-1.web.telegram.org".to_owned());
    }
    hosts
}

pub fn ws_path_for(is_test: bool) -> &'static str {
    if is_test {
        WS_PATH_TEST
    } else {
        WS_PATH
    }
}

pub fn media_tag(is_media: bool) -> &'static str {
    if is_media {
        " media"
    } else {
        ""
    }
}

pub fn is_media_int(b: bool) -> i32 {
    if b {
        1
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// WsPool — порт proxy/pool.py::_WsPool
// refilling-backoff (1s..3600s), rotation протухших, fronting-first.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DcSlot {
    pub dc: i32,
    pub is_media: i32,
}

pub struct PoolEntry {
    pub ws: RawWebSocket,
    pub created: f64, // monotonic, как time.monotonic() в оригинале
}

struct SlotState {
    queue: Mutex<VecDeque<PoolEntry>>,
    refilling: AtomicI32,
}

pub struct WsPool {
    slots: Mutex<HashMap<DcSlot, Arc<SlotState>>>,
    rotating: Mutex<HashSet<DcSlot>>,
    refill_failures: Mutex<HashMap<(u64, DcSlot), (u32, f64)>>,
    cancel_token: CancellationToken,
}

impl WsPool {
    pub fn new(cancel_token: CancellationToken) -> WsPool {
        WsPool {
            slots: Mutex::new(HashMap::new()),
            rotating: Mutex::new(HashSet::new()),
            refill_failures: Mutex::new(HashMap::new()),
            cancel_token,
        }
    }

    async fn get_slot(&self, slot: DcSlot) -> Arc<SlotState> {
        let mut map = self.slots.lock().await;
        map.entry(slot)
            .or_insert_with(|| {
                Arc::new(SlotState {
                    queue: Mutex::new(VecDeque::with_capacity(16)),
                    refilling: AtomicI32::new(0),
                })
            })
            .clone()
    }

    pub async fn get(
        self: &Arc<Self>,
        dc: i32,
        is_media: bool,
        preferred: Option<&str>,
    ) -> Option<RawWebSocket> {
        const MAX_CHECKOUT_PROBES: usize = 3;
        const CHECKOUT_BUDGET: Duration = Duration::from_millis(1200);

        let slot = DcSlot {
            dc,
            is_media: is_media_int(is_media),
        };
        let state = self.get_slot(slot).await;
        let started = std::time::Instant::now();
        let epoch = trace::network();
        let mut stale_skipped = 0usize;

        for _ in 0..MAX_CHECKOUT_PROBES {
            if started.elapsed() >= CHECKOUT_BUDGET || trace::network() != epoch {
                break;
            }
            let now = now_monotonic();
            let candidate = {
                let mut q = state.queue.lock().await;
                q.retain(|entry| {
                    now - entry.created < WS_POOL_REUSE_MAX_AGE
                        && !entry.ws.is_closed()
                        && entry.ws.network == epoch
                });
                let index = q
                    .iter()
                    .position(|entry| preferred.is_none_or(|host| entry.ws.peer_domain == host));
                index.and_then(|index| q.remove(index)).map(|entry| entry.ws)
            };

            let Some(ws) = candidate else {
                break;
            };
            if ws.check_idle().await && ws.network == trace::network() {
                trace::event(
                    "pool_checkout",
                    format_args!(
                        "result=hit wait_ms={} skipped_stale={stale_skipped}",
                        started.elapsed().as_millis()
                    ),
                );
                STATS.pool_hits.fetch_add(1, Ordering::Relaxed);
                return Some(ws);
            }
            stale_skipped += 1;
            drop(ws);
        }

        if stale_skipped > 0 {
            trace::event(
                "pool_checkout",
                format_args!(
                    "result=stale wait_ms={} skipped_stale={stale_skipped}",
                    started.elapsed().as_millis()
                ),
            );
        } else {
            trace::event("pool_checkout", format_args!("result=miss"));
        }
        STATS.pool_misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    pub async fn evict_stale_epoch(&self, epoch: u64) {
        let slots: Vec<Arc<SlotState>> = {
            let map = self.slots.lock().await;
            map.values().cloned().collect()
        };
        for state in slots {
            let mut q = state.queue.lock().await;
            q.retain(|entry| entry.ws.network == epoch && !entry.ws.is_closed());
        }
        self.refill_failures
            .lock()
            .await
            .retain(|(seen, _), _| *seen == epoch);
    }

    async fn schedule_refill(
        self: &Arc<Self>,
        slot: DcSlot,
        target_ip: String,
        domains: Vec<String>,
        limit: usize,
    ) {
        // Backoff как в оригинале: не раньше _refill_after.
        {
            let failures = self.refill_failures.lock().await;
            if let Some((_, after)) = failures.get(&(trace::network(), slot)) {
                if now_monotonic() < *after {
                    return;
                }
            }
        }
        let state = self.get_slot(slot).await;
        if state
            .refilling
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        // pool_size <= 0 — преконнект отключён (как pool_size=0 в оригинале).
        if self.cancel_token.is_cancelled() || !secure_transport_enabled()
            || POOL_SIZE.load(Ordering::Relaxed) <= 0 {
            state.refilling.store(0, Ordering::SeqCst);
            return;
        }
        let pool = self.clone();
        spawn_refill_task(pool, slot, state, target_ip, domains, limit);
    }

    async fn refill(
        self: Arc<Self>,
        slot: DcSlot,
        state: Arc<SlotState>,
        target_ip: String,
        domains: Vec<String>,
        limit: usize,
    ) {
        let _guard = RefillGuard {
            state: state.clone(),
        };
        let epoch = trace::network();
        let cur_len = {
            let mut queue = state.queue.lock().await;
            queue.retain(|entry| entry.ws.network == epoch && !entry.ws.is_closed()
                && now_monotonic() - entry.created < WS_POOL_REUSE_MAX_AGE);
            queue.len()
        };
        let pool_size = (POOL_SIZE.load(Ordering::Relaxed).clamp(0, MAX_POOL_SIZE) as usize).min(limit);
        if cur_len >= pool_size {
            return;
        }

        // The background dial budget allows one handshake per host. Filling
        // the same slot concurrently only caused sibling attempts to hit Busy.
        let mut connected = 0;
        for _ in cur_len..pool_size {
            let ws = tokio::select! {
                _ = self.cancel_token.cancelled() => return,
                _ = trace::network_changed(epoch) => return,
                ws = connect_one_ws(&target_ip, &domains) => ws,
            };
            let Some(ws) = ws else { break };
            if ws.network != epoch || epoch != trace::network() {
                return;
            }
            state.queue.lock().await.push_back(PoolEntry { ws, created: now_monotonic() });
            connected += 1;
        }

        if self.cancel_token.is_cancelled() || epoch != trace::network() {
            return;
        }
        if connected > 0 {
            self.clear_refill_failure(epoch, slot).await;
            self.schedule_rotation(slot).await;
        } else {
            // Экспоненциальный backoff 1s..3600s как в оригинале.
            let mut failures = self.refill_failures.lock().await;
            failures.retain(|(network, _), _| *network == epoch);
            let key = (epoch, slot);
            let count = failures.get(&key).map(|(c, _)| *c).unwrap_or(0) + 1;
            let shift = (count - 1).min(12);
            let delay = (WS_POOL_REFILL_BACKOFF_INITIAL * (2u64.pow(shift) as f64))
                .min(WS_POOL_REFILL_BACKOFF_MAX);
            failures.insert(key, (count, now_monotonic() + delay));
            drop(failures);
            linfo!(
                "WS pool refill failed for DC{}{}, retry in {:.0}s",
                slot.dc,
                if slot.is_media != 0 { "m" } else { "" },
                delay
            );
        }
        ldebug!(
            "WS pool refilled DC{}{}: {} ready",
            slot.dc,
            if slot.is_media != 0 { "m" } else { "" },
            state.queue.lock().await.len()
        );
    }

    async fn clear_refill_failure(&self, epoch: u64, slot: DcSlot) {
        self.refill_failures.lock().await.remove(&(epoch, slot));
    }

    async fn schedule_rotation(self: &Arc<Self>, slot: DcSlot) {
        {
            let mut rotating = self.rotating.lock().await;
            if rotating.contains(&slot) {
                return;
            }
            rotating.insert(slot);
        }
        let pool = self.clone();
        let cancel = self.cancel_token.clone();
        spawn_rotate_task(pool, slot, cancel);
    }

    async fn rotate(
        self: Arc<Self>,
        slot: DcSlot,
        cancel: CancellationToken,
    ) {
        loop {
            // Спим до ближайшего протухания, но не реже CHECK_INTERVAL.
            let sleep_dur = {
                let map = self.slots.lock().await;
                match map.get(&slot) {
                    Some(state) => {
                        let q = state.queue.lock().await;
                        if q.is_empty() {
                            None
                        } else {
                            let now = now_monotonic();
                            let min_expires = q
                                .iter()
                                .map(|e| e.created + WS_POOL_REUSE_MAX_AGE)
                                .fold(f64::INFINITY, f64::min);
                            Some((min_expires - now).clamp(0.0, WS_POOL_CHECK_INTERVAL))
                        }
                    }
                    None => None,
                }
            };
            let sleep_dur = match sleep_dur {
                Some(d) => d,
                None => break,
            };
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs_f64(sleep_dur)) => {}
            }

            let now = now_monotonic();
            let expired_count: usize;
            let ready_count: usize;
            {
                let map = self.slots.lock().await;
                let state = match map.get(&slot) {
                    Some(s) => s.clone(),
                    None => break,
                };
                let mut q = state.queue.lock().await;
                let mut ready = VecDeque::with_capacity(q.len());
                let mut expired = Vec::new();
                while let Some(e) = q.pop_front() {
                    if now - e.created >= WS_POOL_REUSE_MAX_AGE || e.ws.is_closed() || e.ws.network != trace::network() {
                        expired.push(e.ws);
                    } else {
                        ready.push_back(e);
                    }
                }
                expired_count = expired.len();
                *q = ready;
                ready_count = q.len();
                for ws in expired {
                    tokio::spawn(async move {
                        ws.close().await;
                    });
                }
            }
            if expired_count > 0 {
                ldebug!(
                    "WS pool rotated DC{}{}: {} stale, {} ready",
                    slot.dc,
                    if slot.is_media != 0 { "m" } else { "" },
                    expired_count,
                    ready_count
                );
            }
            if ready_count == 0 {
                break;
            }

        }
        self.rotating.lock().await.remove(&slot);
    }

    pub async fn warmup(self: &Arc<Self>, dc_opt_map: &HashMap<i32, String>) {
        let mut ndcs = 0;
        for &dc in dc_opt_map.keys() {
            let logical_dc = dc.abs();
            let is_media = dc < 0;
            let Some(target) = resolve_configured_target(DcKey::new(logical_dc, is_media, false)) else { continue };
            ndcs += 1;
            let slot = DcSlot { dc: logical_dc, is_media: is_media_int(is_media) };
            self.schedule_refill(slot, target, ws_domains(logical_dc, is_media), 1).await;
        }
        linfo!("WS pool warmup started for {} DC(s)", ndcs);
    }

    pub async fn idle_count(&self) -> usize {
        let map = self.slots.lock().await;
        let mut count = 0;
        for s in map.values() {
            count += s.queue.lock().await.len();
        }
        count
    }

    pub async fn close_all(&self) {
        let map = self.slots.lock().await;
        for s in map.values() {
            let mut q = s.queue.lock().await;
            for e in q.drain(..) {
                tokio::spawn(async move {
                    e.ws.close().await;
                });
            }
        }
    }

    pub async fn reset(&self) {
        self.close_all().await;
        self.slots.lock().await.clear();
        self.rotating.lock().await.clear();
        self.refill_failures.lock().await.clear();
    }
}

// RefillGuard снимает флаг refilling при выходе (аналог finally: discard).
struct RefillGuard {
    state: Arc<SlotState>,
}
impl Drop for RefillGuard {
    fn drop(&mut self) {
        self.state.refilling.store(0, Ordering::SeqCst);
    }
}

// Синхронные спавн-хелперы: разрывают цикл типов async-future
// (schedule_refill → refill → schedule_rotation → rotate → schedule_refill),
// о который спотыкается rustc (E0391). Sync-fn не вкладывает future одной
// async-функции в другую — внутри только JoinHandle.
fn spawn_refill_task(
    pool: Arc<WsPool>,
    slot: DcSlot,
    state: Arc<SlotState>,
    target_ip: String,
    domains: Vec<String>,
    limit: usize,
) {
    tokio::spawn(async move {
        pool.refill(slot, state, target_ip, domains, limit).await;
    });
}

fn spawn_rotate_task(
    pool: Arc<WsPool>,
    slot: DcSlot,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        pool.rotate(slot, cancel).await;
    });
}

// ---------------------------------------------------------------------------
// CfWorkerPool — порт proxy/pool.py::_CfWorkerPool
// ---------------------------------------------------------------------------

pub struct ProxyPools {
    pub(crate) routes: parking_lot::Mutex<Manager>,
    pub ws: Arc<WsPool>,
}

impl ProxyPools {
    pub fn new(cancel_token: CancellationToken) -> Self {
        Self {
            routes: parking_lot::Mutex::default(),
            ws: Arc::new(WsPool::new(cancel_token)),
        }
    }

    pub async fn close_all(&self) {
        self.ws.close_all().await;
    }

    pub async fn reset(&self) {
        self.ws.reset().await;
    }
}

// ---------------------------------------------------------------------------
// HTTP transport detection
// ---------------------------------------------------------------------------

pub fn is_http_transport(data: &[u8]) -> bool {
    if data.len() < 4 {
        return false;
    }
    &data[..4] == b"POST"
        || &data[..3] == b"GET"
        || &data[..4] == b"HEAD"
        || (data.len() >= 7 && &data[..7] == b"OPTIONS")
}

// ---------------------------------------------------------------------------
// Bridge WS — generic core + TcpStream wrapper.
// Сессионная статистика как в bridge_ws_reencrypt оригинала.
// ---------------------------------------------------------------------------

async fn bridge_ws<R, W>(
    conn_read: R,
    conn_write: W,
    ws: RawWebSocket,
    label: String,
    dc: i32,
    is_media: bool,
    mut splitter: Option<MsgSplitter>,
    mut clt_dec: TrackedStream,
    mut clt_enc: TrackedStream,
    mut tg_enc: TrackedStream,
    mut tg_dec: TrackedStream,
    cancel_token: CancellationToken,
    probe_timeout: Option<Duration>,
    mut observe: impl FnMut(Evidence),
) -> bool
where
    R: AsyncReadExt + Unpin + Send + 'static,
    W: AsyncWriteExt + Unpin + Send + 'static,
{
    let started = std::time::Instant::now();
    let activity = parking_lot::Mutex::new(started);
    let mut conn_read = conn_read;
    let mut conn_write = conn_write;
    let mut up_bytes = 0;
    let sent_payload = AtomicBool::new(false);
    let (sent_tx, mut sent_rx) = tokio::sync::watch::channel(false);
    let mut down_bytes = 0;
    let mut upstream_read = 0;
    let mut replies = trace::ReplyObserver::for_session();
    let mut last_relay_update = None;
    let mut sustained = false;
    let mut has_response = false;
    let mut framed = false;
    let mut transport_error = false;
    let close_reason = {
        let up = async {
            let mut buf = vec![0; WS_BRIDGE_CHUNK_SIZE];
            loop {
                let n = match conn_read.read(&mut buf).await {
                    Ok(0) => {
                        if let Some(splitter) = splitter.as_mut() {
                            let tail = splitter.flush();
                            if ws.send_batch(&tail).await.is_err() {
                                return "upstream_write";
                            }
                        }
                        return "client_eof";
                    }
                    Ok(n) => n,
                    Err(_) => return "client_read",
                };
                let chunk = &mut buf[..n];
                clt_dec.xor(chunk);
                tg_enc.xor(chunk);
                let sent = if let Some(splitter) = splitter.as_mut() {
                    let parts = splitter.split(chunk);
                    let bytes = parts.iter().map(Vec::len).sum::<usize>();
                    ws.send_batch(&parts).await.map(|_| bytes)
                } else {
                    ws.send(chunk).await.map(|_| n)
                };
                let Ok(sent) = sent else { return "upstream_write" };
                if sent == 0 {
                    continue;
                }
                if up_bytes == 0 {
                    sent_payload.store(true, Ordering::Release);
                    let _ = sent_tx.send(true);
                    ldebug!("[{}] stage=first_upstream_payload elapsed_ms={}", label, started.elapsed().as_millis());
                }
                up_bytes += sent;
                STATS.bytes_up.fetch_add(sent as i64, Ordering::Relaxed);
                *activity.lock() = std::time::Instant::now();
            }
        };
        let down = async {
            let first_response_wait = async {
                let Some(timeout) = probe_timeout else {
                    std::future::pending::<()>().await;
                    return;
                };
                if !*sent_rx.borrow() {
                    let _ = sent_rx.wait_for(|sent| *sent).await;
                }
                tokio::time::sleep(timeout).await;
            };
            tokio::pin!(first_response_wait);
            loop {
                let incoming = tokio::select! {
                    _ = &mut first_response_wait, if probe_timeout.is_some() && !has_response && !transport_error => {
                        return "first_response_timeout";
                    }
                    incoming = ws.recv() => incoming,
                };
                let mut data = match incoming {
                    Ok(data) if data.is_empty() => continue,
                    Ok(data) => data,
                    Err(WsError::Closed(code)) => {
                        trace::event("upstream_end", format_args!("kind=ws_close code={code:?}"));
                        return "upstream_ws_close";
                    }
                    Err(WsError::Io(error)) => {
                        trace::event("upstream_end", format_args!("kind={} errno={:?}", error.kind(), error.raw_os_error()));
                        return if error.kind() == std::io::ErrorKind::UnexpectedEof { "upstream_eof" } else { "upstream_read" };
                    }
                    Err(_) => {
                        trace::event("upstream_end", format_args!("kind=ws_protocol_or_write"));
                        return "upstream_websocket";
                    }
                };
                if upstream_read == 0 {
                    trace::event("first_upstream_frame", format_args!("message_bytes={}", data.len()));
                }
                upstream_read += data.len();
                STATS.bytes_upstream_received.fetch_add(data.len() as i64, Ordering::Relaxed);
                tg_dec.xor(&mut data);
                replies.observe(&data, |class| {
                    match class {
                        trace::ReplyClass::TransportError(code) if !transport_error => {
                            transport_error = true;
                            trace::event("transport_error", format_args!("code={code} after_payload={framed}"));
                            if !framed {
                                observe(Evidence::TransportError(code));
                            }
                        }
                        trace::ReplyClass::FramedPayload => framed = true,
                        trace::ReplyClass::InvalidEnvelope => trace::event("transport_envelope_invalid", format_args!("direction=downstream")),
                        _ => {}
                    }
                });
                clt_enc.xor(&mut data);
                match tokio::time::timeout(WS_WRITE_TIMEOUT, conn_write.write_all(&data)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        trace::event("client_end", format_args!("kind={} errno={:?}", error.kind(), error.raw_os_error()));
                        return "client_write";
                    }
                    Err(_) => return "client_backpressure",
                }
                if down_bytes == 0 {
                    ldebug!("[{}] stage=first_downstream_payload elapsed_ms={}", label, started.elapsed().as_millis());
                }
                down_bytes += data.len();
                let verified = down_bytes >= 32 && framed && !transport_error
                    && sent_payload.load(Ordering::Relaxed);
                if verified && !has_response {
                    has_response = true;
                    trace::event("transport_response", format_args!("delivered_bytes={down_bytes}"));
                    observe(Evidence::ProtocolResponse);
                }
                if verified && last_relay_update.is_none_or(|last: Instant| last.elapsed() >= Duration::from_secs(3)) {
                    trace::mark_relay();
                    last_relay_update = Some(Instant::now());
                }
                if verified && down_bytes >= 32 * 1024 && !sustained {
                    sustained = true;
                    observe(Evidence::Sustained);
                }
                STATS.bytes_down.fetch_add(data.len() as i64, Ordering::Relaxed);
                *activity.lock() = std::time::Instant::now();
            }
        };
        let watchdog = async {
            let mut interval = tokio::time::interval(BRIDGE_PING_INTERVAL);
            interval.tick().await;
            loop {
                interval.tick().await;
                let idle = activity.lock().elapsed();
                if idle >= BRIDGE_READ_TIMEOUT {
                    return "idle_timeout";
                }
                if idle >= BRIDGE_PING_INTERVAL && ws.send_ping().await.is_err() {
                    return "upstream_ping_write";
                }
            }
        };

        // The first terminal event owns the session. Dropping the other
        // directions also interrupts a blocked write or a partial WS frame.
        tokio::select! {
            _ = cancel_token.cancelled() => "service_stop",
            reason = up => reason,
            reason = down => reason,
            reason = watchdog => reason,
        }
    };
    if replies.incomplete() {
        trace::event("transport_record_incomplete", format_args!("direction=downstream"));
    }
    trace::event("relay_end", format_args!("reason={close_reason} read_bytes={upstream_read} up_bytes={up_bytes} down_bytes={down_bytes}"));
    if !has_response && !transport_error && up_bytes > 0 {
        if matches!(close_reason, "probe_timeout" | "first_response_timeout" | "idle_timeout") {
            observe(Evidence::ResponseTimeout);
        } else if close_reason.starts_with("upstream_") {
            observe(Evidence::PeerFailure);
        }
    }
    let elapsed = started.elapsed();
    linfo!(
        "[{}] stage=close dc={} media={} transport=ws reason={} up_bytes={} down_bytes={} duration_ms={} down_bps={}",
        label, dc, is_media, close_reason, up_bytes, down_bytes, elapsed.as_millis(),
        (down_bytes as f64 / elapsed.as_secs_f64().max(0.001)) as u64
    );
    // A competing write may have been cancelled after sending part of a
    // frame. Drop the stream rather than appending a Close to partial bytes.
    up_bytes > 0 && down_bytes >= 32
}


pub async fn bridge_tcp<CR, CW>(
    c_read: CR,
    c_write: CW,
    remote: TcpStream,
    clt_dec: TrackedStream,
    clt_enc: TrackedStream,
    tg_enc: TrackedStream,
    tg_dec: TrackedStream,
    cancel_token: CancellationToken,
) where
    CR: AsyncReadExt + Unpin + Send + 'static,
    CW: AsyncWriteExt + Unpin + Send + 'static,
{
    bridge_tcp_observed(
        c_read,
        c_write,
        remote,
        clt_dec,
        clt_enc,
        tg_enc,
        tg_dec,
        cancel_token,
        Some(FIRST_RESPONSE_TIMEOUT),
        |_| {},
    )
    .await;
}

enum TcpClose {
    UpstreamEof,
    UpstreamRead,
    UpstreamWrite,
    UpstreamTimeout,
    ClientRead,
    ClientIdleTimeout,
    ClientWrite,
    ClientBackpressure,
    FirstResponseTimeout,
}

impl TcpClose {
    fn as_str(self) -> &'static str {
        match self {
            Self::UpstreamEof => "upstream_eof",
            Self::UpstreamRead => "upstream_read",
            Self::UpstreamWrite => "upstream_write",
            Self::UpstreamTimeout => "upstream_timeout",
            Self::ClientRead => "client_read",
            Self::ClientIdleTimeout => "client_idle_timeout",
            Self::ClientWrite => "client_write",
            Self::ClientBackpressure => "client_backpressure",
            Self::FirstResponseTimeout => "first_response_timeout",
        }
    }
}

async fn bridge_tcp_observed<CR, CW>(
    mut c_read: CR,
    mut c_write: CW,
    mut remote: TcpStream,
    mut clt_dec: TrackedStream,
    mut clt_enc: TrackedStream,
    mut tg_enc: TrackedStream,
    mut tg_dec: TrackedStream,
    cancel_token: CancellationToken,
    first_response_timeout: Option<Duration>,
    mut observe: impl FnMut(Evidence),
) where
    CR: AsyncReadExt + Unpin + Send + 'static,
    CW: AsyncWriteExt + Unpin + Send + 'static,
{
    let (mut r_read, mut r_write) = remote.split();
    let up_bytes = std::sync::atomic::AtomicUsize::new(0);
    let (sent_tx, mut sent_rx) = tokio::sync::watch::channel(false);
    let up = async {
        let mut buffer = vec![0; WS_BRIDGE_CHUNK_SIZE];
        loop {
            let n = match tokio::time::timeout(BRIDGE_READ_TIMEOUT, c_read.read(&mut buffer)).await {
                Ok(Ok(0)) => {
                    let _ = tokio::time::timeout(WS_WRITE_TIMEOUT, r_write.shutdown()).await;
                    return Ok::<(), TcpClose>(());
                }
                Ok(Ok(n)) => n,
                Ok(Err(_)) => return Err(TcpClose::ClientRead),
                Err(_) => return Err(TcpClose::ClientIdleTimeout),
            };
            let chunk = &mut buffer[..n];
            clt_dec.xor(chunk);
            tg_enc.xor(chunk);
            match tokio::time::timeout(WS_WRITE_TIMEOUT, r_write.write_all(chunk)).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return Err(TcpClose::UpstreamWrite),
                Err(_) => return Err(TcpClose::UpstreamTimeout),
            }
            if up_bytes.fetch_add(n, Ordering::Relaxed) == 0 {
                let _ = sent_tx.send(true);
            }
            STATS.bytes_up.fetch_add(n as i64, Ordering::Relaxed);
        }
    };

    let mut reply = trace::ReplyObserver::for_session();
    let mut delivered = 0usize;
    let mut framed = false;
    let mut reported_response = false;
    let mut reported_sustained = false;
    let mut transport_error = false;
    let down = async {
        let first_response_wait = async {
            let Some(timeout) = first_response_timeout else {
                std::future::pending::<()>().await;
                return;
            };
            if !*sent_rx.borrow() {
                let _ = sent_rx.wait_for(|sent| *sent).await;
            }
            tokio::time::sleep(timeout).await;
        };
        tokio::pin!(first_response_wait);
        let mut buffer = vec![0; WS_BRIDGE_CHUNK_SIZE];
        loop {
            let read_res = tokio::select! {
                _ = &mut first_response_wait, if first_response_timeout.is_some() && !reported_response && !transport_error => {
                    return Err::<(), TcpClose>(TcpClose::FirstResponseTimeout);
                }
                res = tokio::time::timeout(BRIDGE_READ_TIMEOUT, r_read.read(&mut buffer)) => res,
            };
            let n = match read_res {
                Ok(Ok(0)) => {
                    let _ = tokio::time::timeout(WS_WRITE_TIMEOUT, c_write.shutdown()).await;
                    return Err(TcpClose::UpstreamEof);
                }
                Ok(Ok(n)) => n,
                Ok(Err(error)) => {
                    trace::event("tcp_relay_error", format_args!("side=upstream kind={} errno={:?}", error.kind(), error.raw_os_error()));
                    return Err(TcpClose::UpstreamRead);
                }
                Err(_) => return Err(TcpClose::UpstreamTimeout),
            };
            let chunk = &mut buffer[..n];
            tg_dec.xor(chunk);
            STATS.bytes_upstream_received.fetch_add(n as i64, Ordering::Relaxed);
            let mut new_transport_error = None;
            reply.observe(chunk, |class| match class {
                trace::ReplyClass::TransportError(code) if !transport_error => {
                    transport_error = true;
                    trace::event("transport_error", format_args!("code={code} after_payload={framed}"));
                    if !framed {
                        new_transport_error = Some(code);
                    }
                }
                trace::ReplyClass::FramedPayload => framed = true,
                trace::ReplyClass::InvalidEnvelope => {
                    trace::event("transport_envelope_invalid", format_args!("direction=downstream"));
                }
                _ => {}
            });
            if let Some(code) = new_transport_error {
                observe(Evidence::TransportError(code));
            }
            clt_enc.xor(chunk);
            match tokio::time::timeout(WS_WRITE_TIMEOUT, c_write.write_all(chunk)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    trace::event("tcp_relay_error", format_args!("side=client kind={} errno={:?}", error.kind(), error.raw_os_error()));
                    return Err(TcpClose::ClientWrite);
                }
                Err(_) => return Err(TcpClose::ClientBackpressure),
            }
            STATS.bytes_down.fetch_add(n as i64, Ordering::Relaxed);
            delivered += n;
            let sent = up_bytes.load(Ordering::Relaxed) > 0;
            if sent && framed && !transport_error && !reported_response && delivered >= 32 {
                reported_response = true;
                observe(Evidence::ProtocolResponse);
                trace::mark_relay();
            }
            if sent && framed && !transport_error && !reported_sustained && delivered >= 32 * 1024 {
                reported_sustained = true;
                observe(Evidence::Sustained);
                trace::mark_relay();
            }
        }
    };

    let ended = tokio::select! {
        _ = cancel_token.cancelled() => "service_stop",
        result = async { tokio::try_join!(up, down) } => match result {
            Ok(_) => "eof",
            Err(close) => close.as_str(),
        },
    };
    if !reported_response && !transport_error && up_bytes.load(Ordering::Relaxed) > 0 {
        match ended {
            "first_response_timeout" | "upstream_timeout" => observe(Evidence::ResponseTimeout),
            "upstream_eof" | "upstream_read" | "upstream_write" => observe(Evidence::PeerFailure),
            _ => {}
        }
    }
    if reply.incomplete() {
        trace::event("transport_record_incomplete", format_args!("direction=downstream"));
    }
    trace::event(
        "relay_end",
        format_args!(
            "transport=tcp reason={ended} up_bytes={} down_bytes={delivered}",
            up_bytes.load(Ordering::Relaxed)
        ),
    );
}

pub async fn bridge_tcp_stream(
    client: TcpStream,
    remote: TcpStream,
    clt_dec: TrackedStream,
    clt_enc: TrackedStream,
    tg_enc: TrackedStream,
    tg_dec: TrackedStream,
    cancel_token: CancellationToken,
) {
    let (c_read, c_write) = tokio::io::split(client);
    bridge_tcp(
        c_read,
        c_write,
        remote,
        clt_dec,
        clt_enc,
        tg_enc,
        tg_dec,
        cancel_token,
    )
    .await;
}

// ---------------------------------------------------------------------------
// TCP fallback — порт bridge._tcp_fallback
// ---------------------------------------------------------------------------

async fn connect_tcp_candidate(dst: &str) -> Result<TcpStream, WsError> {
    let port = direct_dc_tcp_port();
    let endpoint = format!("tcp:{dst}:{port}");
    let _probe = crate::dial::try_cf_domain(&endpoint).ok_or(WsError::Busy)?;
    let _permit = crate::dial::acquire(dst, 2).await.map_err(|_| WsError::Busy)?;
    trace::attempt();
    let result = tokio::time::timeout(Duration::from_secs(4), TcpStream::connect((dst, port))).await;
    let remote = match result {
        Ok(Ok(remote)) => remote,
        Ok(Err(error)) => {
            trace::event("tcp_connect", format_args!("outcome=error port={port} kind={} errno={:?}", error.kind(), error.raw_os_error()));
            return Err(WsError::Tcp(error));
        }
        Err(_) => {
            trace::event("tcp_connect", format_args!("outcome=timeout port={port}"));
            return Err(WsError::Timeout);
        }
    };
    trace::event("tcp_connect", format_args!("outcome=connected port={port}"));
    let _ = remote.set_nodelay(true);
    Ok(remote)
}

// ---------------------------------------------------------------------------
// Cfproxy fallback — мобильная версия (параллель + 429 cooldown + DoH),
// путь и secure как в оригинале.
// ---------------------------------------------------------------------------

async fn try_cfproxy_base_domain(
    scope: Scope,
    candidate: &Candidate,
    ws_path: &str,
) -> Result<RawWebSocket, WsError> {
    let base_domain = &candidate.target;
    if !cfproxy_429_cooldown_remaining(scope.epoch, base_domain).is_zero() {
        return Err(WsError::Busy);
    }
    let _permit = acquire_cfproxy_attempt_slot().ok_or(WsError::Busy)?;
    let _host_permit = crate::dial::try_cf_domain(base_domain).ok_or(WsError::Busy)?;
    let result = cf_connect_domain(&candidate.host, ws_path, candidate.secure).await;
    match &result {
        Ok(_) => clear_cfproxy_429_cooldown(scope.epoch, base_domain),
        Err(error) if is_http_status_error(error, 429) => {
            mark_cfproxy_429_cooldown(scope.epoch, base_domain, error);
        }
        _ => {}
    }
    result
}

enum Upstream {
    WebSocket(RawWebSocket),
    Tcp(TcpStream),
}

async fn acquire_candidate(
    pools: &ProxyPools,
    scope: Scope,
    candidate: &Candidate,
    ws_path: &str,
) -> Result<Upstream, WsError> {
    let dc = scope.dc.dc;
    let media = scope.dc.is_media != 0;
    match candidate.kind {
        Route::DirectWss => {
            if scope.dc.is_test == 0 && candidate.secure {
                if let Some(ws) = pools.ws.get(dc, media, Some(&candidate.host)).await {
                    return Ok(Upstream::WebSocket(ws));
                }
            }
            ws_connect_full_opts(&candidate.target, &candidate.host, ws_path,
                WS_DIRECT_TIMEOUT, None, Some(candidate.secure)).await.map(Upstream::WebSocket)
        }
        Route::CfWss => try_cfproxy_base_domain(scope, candidate, ws_path)
            .await
            .map(Upstream::WebSocket),
        Route::WorkerWss => {
            let path = format!("/apiws?dst={}&dc={dc}", candidate.target);
            ws_connect_full_opts(&candidate.host, &candidate.host, &path, 10.0, None, Some(candidate.secure))
                .await.map(Upstream::WebSocket)
        }
        Route::Tcp => connect_tcp_candidate(&candidate.target).await.map(Upstream::Tcp),
    }
}

fn route_candidates(scope: Scope) -> Vec<Candidate> {
    let dc_key = scope.dc;
    let dc = dc_key.dc;
    let media = dc_key.is_media != 0;
    let test = dc_key.is_test != 0;
    let secure = direct_websocket_secure();
    let cf_secure = match trace::NetworkKind::from_epoch(scope.epoch) {
        // Plaintext WS/80 is strictly mobile-only; Wi-Fi, Ethernet, Other, and Unknown
        // always stay on verified TLS/443 even when cellular WS/80 consent is saved.
        trace::NetworkKind::Cellular => cloudflare_websocket_secure(),
        _ => true,
    };
    let mut candidates = Vec::new();
    if let Some(target) = resolve_configured_target(dc_key) {
        candidates.extend(direct_ws_domains(dc_key, &target, secure).into_iter()
            .map(|host| Candidate::new(Route::DirectWss, host, target.clone(), secure)));
    }
    let fallback = resolve_fallback_target(dc, media, test);
    if !test {
        let mut workers = CF_WORKER_DOMAINS.read().clone();
        if !fallback.is_empty() && !workers.is_empty() {
            use rand::seq::SliceRandom;
            workers.shuffle(&mut rand::thread_rng());
            candidates.extend(workers.into_iter()
                .take(CF_ATTEMPTS_PER_SESSION)
                .map(|host| Candidate::new(Route::WorkerWss, host, fallback.clone(), secure)));
        }
        if CFPROXY_ENABLED.load(Ordering::Relaxed) {
            let signed_dc = if media { -dc } else { dc };
            candidates.extend(crate::balancer::BALANCER.read().get_domains_for_dc(signed_dc)
                .into_iter().filter(|domain| cfproxy_429_cooldown_remaining(scope.epoch, domain).is_zero())
                .map(|domain| Candidate::new(Route::CfWss, format!("kws{dc}.{domain}"), domain, cf_secure)));
        }
    }
    if tcp_fallback_allowed(TCP_FALLBACK_ENABLED.load(Ordering::Relaxed), &fallback) {
        candidates.push(Candidate::new(Route::Tcp, String::new(), fallback, false));
    }
    candidates
}

// ---------------------------------------------------------------------------
// Client serving — порт tg_ws_proxy._handle_client (dd + ee/FakeTLS).
// ---------------------------------------------------------------------------

async fn read_proxy_protocol_line(conn: &mut TcpStream) -> Option<String> {
    let mut line = Vec::with_capacity(108);
    let mut byte = [0u8; 1];
    let res = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let n = conn.read(&mut byte).await.map_err(|_| ())?;
            if n == 0 {
                return Err(());
            }
            line.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
            if line.len() > 108 {
                break;
            }
        }
        Ok::<(), ()>(())
    })
    .await;
    match res {
        Ok(Ok(())) => Some(String::from_utf8_lossy(&line).to_string()),
        _ => None,
    }
}

pub async fn handle_client(
    pools: Arc<ProxyPools>,
    mut conn: TcpStream,
    cancel_token: CancellationToken,
) {
    STATS.connections_total.fetch_add(1, Ordering::Relaxed);
    STATS.connections_active.fetch_add(1, Ordering::Relaxed);
    struct ActiveGuard;
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            if STATS.connections_active.load(Ordering::Relaxed) > 0 {
                STATS.connections_active.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
    let _guard = ActiveGuard;

    let label = trace::label().unwrap_or_else(|| "standalone".into());
    if trace::sample_unverified() {
        ldebug!("[{}] stage=accepted", label);
    }

    let _ = conn.set_nodelay(true);

    let current_secret = PROXY_SECRET.read().clone();
    let secret_bytes = hex::decode(&current_secret).unwrap_or_default();

    // PROXY protocol (proxy_config.proxy_protocol в оригинале).
    if PROXY_PROTOCOL.load(Ordering::Relaxed) {
        match read_proxy_protocol_line(&mut conn).await {
            Some(line) => {
                if !line.starts_with("PROXY ") {
                    ldebug!("[{}] stage=invalid_proxy_header", label);
                    return;
                }
            }
            None => {
                ldebug!("[{}] disconnected during PROXY header", label);
                return;
            }
        }
    }

    // FakeTLS (ee-secret): первый байт решает всё — как _read_client_init.
    let masking = FAKE_TLS_DOMAIN.read().clone();
    if !masking.is_empty() {
        let mut first = [0u8; 1];
        match tokio::time::timeout(Duration::from_secs(10), conn.read_exact(&mut first)).await
        {
            Ok(Ok(_)) => {}
            _ => {
                ldebug!("[{}] client disconnected before handshake", label);
                return;
            }
        }
        if first[0] == faketls::TLS_RECORD_HANDSHAKE {
            match faketls::server_handshake(conn, first[0], &secret_bytes, &masking, &label).await
            {
                Some(plain) => {
                    serve_core(
                        &pools,
                        plain.read,
                        plain.write,
                        label,
                        secret_bytes,
                        cancel_token,
                    )
                    .await;
                }
                None => {}
            }
            return;
        } else {
            ldebug!("[{}] non-TLS byte 0x{:02X} -> HTTP redirect", label, first[0]);
            let redirect = format!(
                "HTTP/1.1 301 Moved Permanently\r\nLocation: https://{}/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                masking
            );
            let _ = conn.write_all(redirect.as_bytes()).await;
            return;
        }
    }

    let (conn_read, conn_write) = tokio::io::split(conn);
    serve_core(
        &pools,
        conn_read,
        conn_write,
        label,
        secret_bytes,
        cancel_token,
    )
    .await;
}

fn decode_client_init(
    init: &[u8; HANDSHAKE_LEN],
    secret: &[u8],
    force_test: bool,
) -> Option<(u32, TrackedStream, DcKey)> {
    if secret.len() != 16 {
        return None;
    }
    let mut hash = Sha256::new();
    hash.update(&init[SKIP_LEN..SKIP_LEN + PREKEY_LEN]);
    hash.update(secret);
    let mut stream = new_aes_ctr(&hash.finalize(), &init[40..56]);
    let mut plaintext = *init;
    stream.xor(&mut plaintext);

    let proto = LittleEndian::read_u32(&plaintext[PROTO_TAG_POS..]);
    let signed = i16::from_le_bytes([plaintext[DC_IDX_POS], plaintext[DC_IDX_POS + 1]]);
    let absolute = i32::from(signed).abs();
    let test = absolute >= 10000;
    let dc = if test { absolute - 10000 } else { absolute };
    if !valid_proto(proto) || dc == 0 {
        return None;
    }
    Some((proto, stream, DcKey::new(dc, signed < 0, force_test || test)))
}

fn relay_init(proto_tag: [u8; 4], dc_idx: i16) -> ([u8; 64], TrackedStream, TrackedStream) {
    let mut relay_init = [0u8; 64];
    loop {
        rand::thread_rng().fill_bytes(&mut relay_init);
        if relay_init[0] == RESERVED_FIRST_BYTE {
            continue;
        }
        if is_reserved_start(&relay_init[..4]) {
            continue;
        }
        if relay_init[4] == 0 && relay_init[5] == 0 && relay_init[6] == 0 && relay_init[7] == 0 {
            continue;
        }
        break;
    }

    let mut tg_dec_prekey_and_iv = [0u8; 48];
    for i in 0..48 {
        tg_dec_prekey_and_iv[i] = relay_init[8 + 47 - i];
    }

    let mut tg_encryptor = new_aes_ctr(&relay_init[8..40], &relay_init[40..56]);
    let tg_decryptor = new_aes_ctr(&tg_dec_prekey_and_iv[..32], &tg_dec_prekey_and_iv[32..]);

    let mut dc_bytes = [0u8; 2];
    dc_bytes.copy_from_slice(&dc_idx.to_le_bytes());

    let mut tail_plain = [0u8; 8];
    tail_plain[0..4].copy_from_slice(&proto_tag);
    tail_plain[4..6].copy_from_slice(&dc_bytes);
    rand::thread_rng().fill_bytes(&mut tail_plain[6..8]);

    let mut encrypted_full = relay_init;
    tg_encryptor.xor(&mut encrypted_full);

    let mut keystream_tail = [0u8; 8];
    for i in 0..8 {
        keystream_tail[i] = encrypted_full[56 + i] ^ relay_init[56 + i];
        relay_init[56 + i] = tail_plain[i] ^ keystream_tail[i];
    }

    (relay_init, tg_encryptor, tg_decryptor)
}

/// Ядро обслуживания соединения: 64-байтный handshake → маршрут → bridge.
/// Generic над транспортом (TcpStream-halves или FakeTLS-duplex-halves).
async fn serve_core<R, W>(
    pools: &ProxyPools,
    mut conn_read: R,
    mut conn_write: W,
    label: String,
    secret_bytes: Vec<u8>,
    cancel_token: CancellationToken,
) where
    R: AsyncReadExt + Unpin + Send + 'static,
    W: AsyncWriteExt + Unpin + Send + 'static,
{
    // Account for partial headers and keep the original handshake deadline.
    let mut handshake = [0u8; 64];
    let mut read = 0;
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        while read < handshake.len() {
            let count = conn_read.read(&mut handshake[read..]).await?;
            if count == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            read += count;
        }
        Ok::<(), std::io::Error>(())
    }).await;
    STATS.bytes_client_init.fetch_add(read as i64, Ordering::Relaxed);
    if !matches!(result, Ok(Ok(()))) {
        if trace::sample_unverified() {
            trace::event("client_init_incomplete", format_args!("bytes={read} timeout={}", result.is_err()));
        }
        return;
    }

    if is_http_transport(&handshake) {
        STATS.connections_http_reject.fetch_add(1, Ordering::Relaxed);
        let _ = conn_write
            .write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n")
            .await;
        let _ = conn_write.shutdown().await;
        return;
    }

    let Some((proto, clt_decryptor, dc_key)) =
        decode_client_init(&handshake, &secret_bytes, FORCE_TEST_DC.load(Ordering::Relaxed))
    else {
        STATS.connections_bad.fetch_add(1, Ordering::Relaxed);
        if trace::sample_unverified() {
            lwarn!("[{}] stage=invalid_init bytes={read}", label);
        }
        return;
    };
    STATS.connections_validated.fetch_add(1, Ordering::Relaxed);
    STATS.connections_validated_active.fetch_add(1, Ordering::Relaxed);
    struct ValidatedGuard;
    impl Drop for ValidatedGuard {
        fn drop(&mut self) {
            STATS.connections_validated_active.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let _validated = ValidatedGuard;
    let dc = dc_key.dc;
    let is_media = dc_key.is_media != 0;
    let is_test_dc = dc_key.is_test != 0;
    let proto_tag = proto.to_le_bytes();
    trace::set_protocol(proto);
    trace::event("client_init_received", format_args!("bytes={read}"));
    trace::event("mtproto_init", format_args!("signed_dc={} dc={} media={} test={}",
        if is_media { -(dc + 10000 * i32::from(is_test_dc)) } else { dc + 10000 * i32::from(is_test_dc) }, dc, is_media, is_test_dc));

    let proto_int = proto; // u32 tag; MsgSplitter маппит через proto_tag_to_type
    let dc_idx = if is_media { -dc } else { dc };

    ldebug!(
        "[{}] stage=init_verified dc={} role={} proto=0x{:08X}{}",
        label,
        dc,
        if is_media { "media" } else { "main" },
        proto_int,
        if is_test_dc { " test" } else { "" }
    );

    let (relay_init, tg_encryptor, tg_decryptor) = relay_init(proto_tag, dc_idx as i16);

    // clt_encryptor: prekey/iv = reverse(client_dec_prekey_iv), key=SHA256+secret.
    let mut clt_enc_prekey_and_iv = [0u8; 48];
    for i in 0..48 {
        clt_enc_prekey_and_iv[i] = handshake[8 + 47 - i];
    }
    let mut hash_enc = Sha256::new();
    hash_enc.update(&clt_enc_prekey_and_iv[..32]);
    hash_enc.update(&secret_bytes);
    let clt_encryptor = new_aes_ctr(&hash_enc.finalize(), &clt_enc_prekey_and_iv[32..]);

    let scope = Scope { epoch: trace::session_network(), dc: dc_key };
    let failed_session_deadline = tokio::time::Instant::now() + MIN_FAILED_SESSION_TIME;
    let ws_path = ws_path_for(is_test_dc);
    let mut candidates = route_candidates(scope);
    let direct_target = resolve_configured_target(dc_key);
    if scope.epoch != trace::network() {
        return;
    }
    let automatic = AUTO_STICKY_ROUTING.load(Ordering::Relaxed);
    {
        let mut routes = pools.routes.lock();
        if scope.epoch != trace::network() {
            return;
        }
        routes.order(scope, &mut candidates, Instant::now(), automatic);
    }
    trace::event("candidate_plan", format_args!("automatic={automatic} first={:?} count={}",
        candidates.first().map(|candidate| candidate.kind), candidates.len()));

    let mut cf_attempts = 0;
    let mut dial_attempts = 0u32;
    let mut route_cooling = 0u32;
    let mut route_probing = 0u32;
    let mut family_cooling = 0u32;
    let mut family_budget = 0u32;
    let mut busy_count = 0u32;
    let mut dial_failed = 0u32;
    let mut busy_indices: Vec<usize> = Vec::new();
    let mut pass = 0u8;

    loop {
        let indices: Vec<usize> = if pass == 0 {
            (0..candidates.len()).collect()
        } else {
            std::mem::take(&mut busy_indices)
        };
        for idx in indices {
            let candidate = &candidates[idx];
            if candidate.kind == Route::CfWss && cf_attempts >= CF_ATTEMPTS_PER_SESSION {
                continue;
            }
            let mut route_use = match RouteUse::start(&pools.routes, scope, candidate) {
                Ok(route_use) => route_use,
                Err(SkipReason::NetworkChanged) => return,
                Err(SkipReason::RouteCooling) => {
                    route_cooling += 1;
                    continue;
                }
                Err(SkipReason::RouteProbing) => {
                    route_probing += 1;
                    continue;
                }
                Err(SkipReason::FamilyCooling) => {
                    family_cooling += 1;
                    continue;
                }
                Err(SkipReason::FamilyBudget) => {
                    family_budget += 1;
                    continue;
                }
            };
            trace::set_route(candidate.kind);
            let acquired = tokio::select! {
                _ = cancel_token.cancelled() => return,
                _ = trace::network_changed(scope.epoch) => {
                    trace::event("route_aborted", format_args!("reason=network_changed"));
                    return;
                }
                result = acquire_candidate(pools, scope, candidate, ws_path) => result,
            };
            if scope.epoch != trace::network() {
                trace::event("route_aborted", format_args!("reason=network_changed"));
                return;
            }
            let upstream = match acquired {
                Ok(upstream) => upstream,
                Err(WsError::NetworkChanged) => return,
                Err(WsError::Busy) => {
                    busy_count += 1;
                    if pass == 0 {
                        busy_indices.push(idx);
                    }
                    continue;
                }
                Err(WsError::Canceled) => continue,
                Err(error) => {
                    dial_attempts += 1;
                    dial_failed += 1;
                    if candidate.kind == Route::CfWss {
                        cf_attempts += 1;
                    }
                    STATS.ws_errors.fetch_add(i64::from(candidate.kind != Route::Tcp), Ordering::Relaxed);
                    trace::event("candidate_failed", format_args!("kind={:?} host={} error={} class={}",
                        candidate.kind, trace::allowlisted_host(&candidate.host), error.compact(),
                        connect_failure_class(&error).as_str()));
                    route_use.observe_connect_failure(connect_failure_class(&error));
                    continue;
                }
            };

            // After the first write is attempted the init may already be partially on
            // the wire. All exits below close this client and never use another route.
            match upstream {
                Upstream::WebSocket(ws) => {
                    if ws.network != scope.epoch || scope.epoch != trace::network() {
                        return;
                    }
                    let peer = ws.peer;
                    route_use.observe(if ws.secure { Evidence::TransportVerified } else { Evidence::Reachable }, peer);
                    if ws.send(&relay_init).await.is_err() {
                        route_use.observe(Evidence::PeerFailure, peer);
                        trace::event("relay_init_failed", format_args!("reason=write"));
                        return;
                    }
                    trace::mark_verified();
                    trace::event("relay_init_sent", format_args!("route={:?} dc={dc} media={is_media}", candidate.kind));
                    trace::event("route_pinned", format_args!("kind={:?} host={} dc={dc} media={is_media} test={is_test_dc}",
                        candidate.kind, trace::allowlisted_host(&candidate.host)));
                    let splitter = if candidate.kind == Route::WorkerWss { None } else { MsgSplitter::new(&relay_init, proto_int) };
                    if candidate.kind == Route::DirectWss {
                        STATS.connections_ws.fetch_add(1, Ordering::Relaxed);
                    } else {
                        STATS.connections_cfproxy.fetch_add(1, Ordering::Relaxed);
                    }
                    let probe_timeout = Some(FIRST_RESPONSE_TIMEOUT);
                    bridge_ws(conn_read, conn_write, ws, label, dc, is_media, splitter,
                        clt_decryptor, clt_encryptor, tg_encryptor, tg_decryptor, cancel_token, probe_timeout,
                        |evidence| {
                            route_use.observe(evidence, peer);
                            if evidence == Evidence::ProtocolResponse && candidate.kind == Route::DirectWss && candidate.secure
                                && scope.epoch == trace::network() && !is_test_dc {
                                if let Some(target) = direct_target.clone() {
                                    let pool = pools.ws.clone();
                                    let host = candidate.host.clone();
                                    tokio::spawn(async move {
                                        if scope.epoch == trace::network() {
                                            let slot = DcSlot { dc, is_media: is_media_int(is_media) };
                                            pool.clear_refill_failure(scope.epoch, slot).await;
                                            pool.schedule_refill(slot, target, vec![host], MAX_POOL_SIZE as usize).await;
                                        }
                                    });
                                }
                            }
                        }).await;
                }
                Upstream::Tcp(mut remote) => {
                    if scope.epoch != trace::network() { return; }
                    let peer = remote.peer_addr().ok();
                    route_use.observe(Evidence::Reachable, peer);
                    if !matches!(tokio::time::timeout(WS_WRITE_TIMEOUT, remote.write_all(&relay_init)).await, Ok(Ok(()))) {
                        route_use.observe(Evidence::PeerFailure, peer);
                        trace::event("relay_init_failed", format_args!("reason=write"));
                        return;
                    }
                    STATS.connections_tcp_fallback.fetch_add(1, Ordering::Relaxed);
                    trace::event("relay_init_sent", format_args!("route=Tcp dc={dc} media={is_media}"));
                    trace::event("route_pinned", format_args!("kind=Tcp dc={dc} media={is_media} test={is_test_dc}"));
                    bridge_tcp_observed(conn_read, conn_write, remote, clt_decryptor, clt_encryptor,
                        tg_encryptor, tg_decryptor, cancel_token, Some(FIRST_RESPONSE_TIMEOUT), |evidence| route_use.observe(evidence, peer)).await;
                }
            }
            return;
        }
        if pass == 0 && !busy_indices.is_empty() {
            pass = 1;
            tokio::select! {
                _ = cancel_token.cancelled() => return,
                _ = trace::network_changed(scope.epoch) => return,
                _ = tokio::time::sleep(Duration::from_millis(150)) => {}
            }
            continue;
        }
        break;
    }

    let primary_reason = if candidates.is_empty() {
        "no_candidates"
    } else if dial_failed > 0 {
        "dial_failed"
    } else if family_cooling > 0 {
        "family_cooling"
    } else if route_cooling > 0 {
        "route_cooling"
    } else if busy_count > 0 {
        "busy"
    } else if family_budget > 0 {
        "family_budget"
    } else if route_probing > 0 {
        "route_probing"
    } else {
        "skipped"
    };
    pools.routes.lock().record_exhausted(
        scope,
        primary_reason,
        candidates.len(),
        dial_attempts,
        Instant::now(),
    );
    // An immediate EOF while all routes are on cooldown makes Telegram reconnect in a tight loop.
    tokio::select! {
        _ = cancel_token.cancelled() => return,
        _ = trace::network_changed(scope.epoch) => return,
        _ = tokio::time::sleep_until(failed_session_deadline) => {}
    }
    trace::mark_exhausted();
    lwarn!(
        "[{}] stage=routes_exhausted dc={} media={} test={} reason={} candidates={} attempts={} route_cooling={} route_probing={} family_cooling={} family_budget={} busy={} dial_failed={}",
        label, dc, is_media, is_test_dc, primary_reason, candidates.len(), dial_attempts,
        route_cooling, route_probing, family_cooling, family_budget, busy_count, dial_failed
    );
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub async fn run_proxy(
    pools: Arc<ProxyPools>,
    host: String,
    port: u16,
    dc_opt_map: HashMap<i32, String>,
    cancel_root: CancellationToken,
    listener: TcpListener,
) -> std::io::Result<()> {
    {
        let mut m = DC_OPT.write();
        *m = dc_opt_map.clone();
    }

    if CFPROXY_ENABLED.load(Ordering::Relaxed) {
        start_cfproxy_refresh(cancel_root.clone());
    }

    {
        let p = pools.clone();
        let map = dc_opt_map.clone();
        tokio::spawn(async move {
            p.ws.warmup(&map).await;
        });
    }

    linfo!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    linfo!("  TG WS Proxy запущен");
    linfo!("  Адрес: {}:{}", host, port);
    linfo!("  Target DC IPs:");
    let mut dcs: Vec<i32> = dc_opt_map.keys().copied().collect();
    dcs.sort();
    for dc in dcs {
        if let Some(ip) = dc_opt_map.get(&dc) {
            linfo!("    DC{}: {}", dc, ip);
        }
    }
    if CFPROXY_ENABLED.load(Ordering::Relaxed) {
        linfo!("  CF proxy:      enabled");
    }
    if !CF_WORKER_DOMAINS.read().is_empty() {
        linfo!(
            "  CF worker:     enabled ({})",
            CF_WORKER_DOMAINS.read().join(", ")
        );
    }
    if DISABLE_SECURE.load(Ordering::Relaxed) {
        linfo!("  No secure:     enabled (port 80 for CF proxy/worker)");
    }
    if CFPROXY_PLAIN_WS80.load(Ordering::Relaxed) {
        lwarn!("  CF WS/80:      plaintext experiment, no TLS and no server authentication");
    }
    if !FAKE_TLS_DOMAIN.read().is_empty() {
        linfo!("  Fake TLS:      {}", FAKE_TLS_DOMAIN.read().clone());
    }

    let cancel_stats = cancel_root.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.tick().await;
        loop {
            tokio::select! {
                _ = cancel_stats.cancelled() => return,
                _ = interval.tick() => {
                    linfo!(" {}", STATS.summary_ru());
                    ldebug!("stage=session_totals {}", STATS.summary());
                }
            }
        }
    });

    let sessions = Arc::new(tokio::sync::Semaphore::new(64));
    let run_id = rand::thread_rng().gen::<u64>();
    let mut ordinal = 0;
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = cancel_root.cancelled() => {
                break;
            }
            _ = tasks.join_next(), if !tasks.is_empty() => {}
            accept = listener.accept() => {
                match accept {
                    Ok((conn, _)) => {
                        ordinal += 1;
                        let handover = trace::network_handover();
                        let trace = trace::Session::new(run_id, ordinal);
                        let Ok(permit) = sessions.clone().try_acquire_owned() else {
                            trace::SESSION.scope(trace, async { trace::end("admission_busy"); }).await;
                            continue;
                        };
                        let p = pools.clone();
                        let admitted = 64 - sessions.available_permits();
                        let cancel = cancel_root.child_token();
                        tasks.spawn(trace::SESSION.scope(trace, async move {
                            let _permit = permit;
                            if trace::sample_unverified() {
                                trace::event("session_admitted", format_args!("active={admitted}"));
                            }
                            let reason = tokio::select! {
                                biased;
                                _ = cancel.cancelled() => "service_stop",
                                _ = handover => "network_handover",
                                _ = handle_client(p, conn, cancel.clone()) => "finished",
                            };
                            cancel.cancel();
                            trace::end(reason);
                        }));
                    }
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }

    drop(listener);
    cancel_root.cancel();
    while tasks.join_next().await.is_some() {}
    Ok(())
}

/// Flowseal `dc_redirects` defaults. Auto mode sends no explicit map;
/// DC2 and DC4 still have a direct WebSocket target. Other DCs stay
/// unconfigured and use CF/TCP fallback. An explicit IP is not replaced.
pub fn apply_default_dc_redirects(mut map: HashMap<i32, String>) -> HashMap<i32, String> {
    map.entry(2).or_insert_with(|| SHARED_DIRECT_WSS_IP.to_owned());
    map.entry(4).or_insert_with(|| SHARED_DIRECT_WSS_IP.to_owned());
    map
}

fn connect_failure_class(error: &WsError) -> FailureClass {
    use std::io::ErrorKind::TimedOut;
    match error {
        WsError::Timeout => FailureClass::Timeout,
        WsError::Dns(err) | WsError::Tcp(err) | WsError::Tls(err) | WsError::Io(err)
            if err.kind() == TimedOut => FailureClass::Timeout,
        WsError::Upgrade(err) if matches!(err.as_ref(), WsError::Timeout) => FailureClass::Timeout,
        WsError::Dns(_) => FailureClass::Dns,
        WsError::Tcp(_) => FailureClass::Tcp,
        WsError::Tls(_) => FailureClass::Tls,
        WsError::Handshake(_) | WsError::Upgrade(_) | WsError::Closed(_) => FailureClass::Websocket,
        _ => FailureClass::Other,
    }
}

fn tcp_fallback_allowed(enabled: bool, destination: &str) -> bool {
    enabled && !destination.is_empty()
}

pub const CF_ATTEMPTS_PER_SESSION: usize = 2;

pub fn limit_cf_domains(domains: Vec<String>) -> Vec<String> {
    domains.into_iter().take(CF_ATTEMPTS_PER_SESSION).collect()
}

#[cfg(test)]
#[path = "cold_probe.rs"]
mod cold_probe;

#[cfg(test)]
mod route_policy_tests {
    use super::*;

    #[test]
    fn alternate_positive_dc4_hostname_is_scoped_to_verified_direct_candidate() {
        let main = DcKey::new(4, false, false);
        let media = DcKey::new(4, true, false);
        assert_eq!(direct_ws_domains(main, SHARED_DIRECT_WSS_IP, true),
            ["kws4.web.telegram.org", "kws4-1.web.telegram.org"]);
        assert_eq!(direct_ws_domains(media, SHARED_DIRECT_WSS_IP, true),
            ["kws4-1.web.telegram.org"]);
        assert_eq!(direct_ws_domains(DcKey::new(4, false, true), SHARED_DIRECT_WSS_IP, true),
            ["kws4.web.telegram.org"]);
        assert_eq!(direct_ws_domains(main, "192.0.2.4", true), ["kws4.web.telegram.org"]);
        assert_eq!(direct_ws_domains(main, SHARED_DIRECT_WSS_IP, false), ["kws4.web.telegram.org"]);
        assert_eq!(direct_ws_domains(DcKey::new(2, false, false), SHARED_DIRECT_WSS_IP, true),
            ["kws2.web.telegram.org"]);
    }

    pub(super) fn client_init(secret: &[u8; 16], signed_dc: i16) -> [u8; 64] {
        let mut init = [0x49; 64];
        init[56..60].fill(0xdd);
        init[60..62].copy_from_slice(&signed_dc.to_le_bytes());
        let key = Sha256::digest([&init[8..40], secret.as_slice()].concat());
        let mut cipher = ctr::Ctr128BE::<aes::Aes256>::new_from_slices(&key, &init[40..56]).unwrap();
        let mut encrypted = init;
        cipher.apply_keystream(&mut encrypted);
        init[56..].copy_from_slice(&encrypted[56..]);
        init
    }

    use aes::cipher::{KeyIvInit, StreamCipher};

    #[test]
    fn client_init_validates_secret_and_keeps_media_and_test_independent() {
        let secret = [0x42; 16];
        for (signed, dc, media, test) in [(2, 2, false, false), (-2, 2, true, false),
            (203, 203, false, false), (-10004, 4, true, true), (10005, 5, false, true)] {
            let init = client_init(&secret, signed);
            let (proto, _, key) = decode_client_init(&init, &secret, false).unwrap();
            assert_eq!(proto, PROTO_PADDED_INTERMEDIATE_INT);
            assert_eq!(key, DcKey::new(dc, media, test));
            assert!(decode_client_init(&init, &[0x43; 16], false).is_none());
        }
        assert!(decode_client_init(&client_init(&secret, 0), &secret, false).is_none());
    }

    #[test]
    fn media_override_and_host_alias_leave_logical_dc_unchanged() {
        let map = parse_cidr_pool("2:192.0.2.2,-2:192.0.2.3");
        assert_eq!(configured_target(&map, 2, true).unwrap(), "192.0.2.3");
        assert_eq!(configured_target(&map, 2, false).unwrap(), "192.0.2.2");
        assert_eq!(ws_domains(203, false), ["kws2.web.telegram.org"]);
        assert_eq!(ws_domains(203, true)[0], "kws2-1.web.telegram.org");

        let (init, _, _) = relay_init([0xdd; 4], -203);
        let mut cipher = ctr::Ctr128BE::<aes::Aes256>::new_from_slices(&init[8..40], &init[40..56]).unwrap();
        let mut plaintext = init;
        cipher.apply_keystream(&mut plaintext);
        assert_eq!(i16::from_le_bytes(plaintext[60..62].try_into().unwrap()), -203);
    }

    #[test]
    fn validated_dc203_alias_only_supplies_a_missing_production_main_target() {
        let mut map = HashMap::new();
        let main = DcKey::new(203, false, false);
        let media = DcKey::new(203, true, false);
        let test = DcKey::new(203, false, true);
        assert_eq!(select_direct_target(&map, main), Some("149.154.167.220"));
        assert_eq!(select_direct_target(&map, media), None);
        assert_eq!(select_direct_target(&map, test), None);
        map.insert(203, "192.0.2.3".into());
        map.insert(-203, "192.0.2.4".into());
        assert_eq!(select_direct_target(&map, main), Some("192.0.2.3"));
        assert_eq!(select_direct_target(&map, media), Some("192.0.2.4"));
    }

    #[test]
    fn explicit_policy_can_forbid_tcp_even_with_a_destination() {
        assert!(!tcp_fallback_allowed(false, "192.0.2.1"));
        assert!(!tcp_fallback_allowed(true, ""));
        assert!(tcp_fallback_allowed(true, "192.0.2.1"));
    }

    pub(super) fn request_pq() -> ([u8; 44], [u8; 16]) {
        let mut request = [0u8; 44];
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
        let id = (now.as_secs() << 32) | (u64::from(now.subsec_nanos()) << 2);
        let mut nonce = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut nonce);
        request[..4].copy_from_slice(&40u32.to_le_bytes());
        request[12..20].copy_from_slice(&id.to_le_bytes());
        request[20..24].copy_from_slice(&20u32.to_le_bytes());
        request[24..28].copy_from_slice(&0xbe7e8ef1u32.to_le_bytes());
        request[28..].copy_from_slice(&nonce);
        (request, nonce)
    }

    pub(super) fn reply_is_res_pq(reply: &[u8], nonce: &[u8; 16]) -> bool {
        reply.len() >= 44
            && reply[4..12] == [0; 8]
            && LittleEndian::read_u32(&reply[24..28]) == 0x05162463
            && &reply[28..44] == nonce
    }

    #[test]
    #[ignore = "requires network access to Telegram public WebSocket endpoints"]
    fn isolated_proxy_req_pq() {
        use std::ffi::CString;

        let address = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap();
        let host = CString::new("127.0.0.1").unwrap();
        let secret = [0x42; 16];
        let secret_argument = CString::new(hex::encode(secret)).unwrap();
        let selected = std::env::var("TGWS_PROBE_DC").ok().and_then(|value| value.parse::<i16>().ok());
        let candidates: Vec<_> = [2i16, 4, -2, -4, 203].into_iter()
            .filter(|dc| selected.is_none_or(|selected| selected == *dc)).collect();
        assert!(!candidates.is_empty(), "select one of the supported signed DCs");
        let expected = candidates.len();
        crate::SetTcpFallbackEnabled(0);
        CFPROXY_ENABLED.store(false, Ordering::Relaxed);
        let result = unsafe {
            crate::StartProxy(host.as_ptr(), i32::from(address.port()), std::ptr::null(), secret_argument.as_ptr(), 1)
        };
        assert_eq!(result, 0);
        struct StopGuard;
        impl Drop for StopGuard {
            fn drop(&mut self) {
                crate::StopProxy();
                crate::SetTcpFallbackEnabled(1);
                CFPROXY_ENABLED.store(true, Ordering::Relaxed);
            }
        }
        let _guard = StopGuard;

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let passed = runtime.block_on(async {
            let mut wrong = TcpStream::connect(address).await.unwrap();
            wrong.write_all(&client_init(&[0x43; 16], 2)).await.unwrap();
            let rejected = tokio::time::timeout(Duration::from_secs(2), wrong.read(&mut [0])).await;
            assert!(matches!(rejected, Ok(Ok(0))), "wrong secret was not rejected");

            let mut probes = tokio::task::JoinSet::new();
            for dc in candidates {
                probes.spawn(async move {
                    let started = std::time::Instant::now();
                    let result = tokio::time::timeout(Duration::from_secs(14), async {
                        let mut socket = TcpStream::connect(address).await?;
                        let init = client_init(&secret, dc);
                        let (_, mut up, _) = decode_client_init(&init, &secret, false).unwrap();
                        let reversed: Vec<_> = init[8..56].iter().rev().copied().collect();
                        let key = Sha256::digest([&reversed[..32], secret.as_slice()].concat());
                        let mut down = new_aes_ctr(&key, &reversed[32..]);
                        let (mut request, nonce) = request_pq();
                        up.xor(&mut request);
                        socket.write_all(&init).await?;
                        socket.write_all(&request).await?;
                        let mut reply = [0; 44];
                        socket.read_exact(&mut reply).await?;
                        down.xor(&mut reply);
                        Ok::<_, std::io::Error>(reply_is_res_pq(&reply, &nonce))
                    }).await;
                    println!("local_proxy dc={dc} elapsed_ms={} result={result:?}", started.elapsed().as_millis());
                    matches!(result, Ok(Ok(true)))
                });
            }
            let mut successes = 0;
            while let Some(result) = probes.join_next().await {
                successes += usize::from(result.unwrap());
            }
            successes
        });
        assert_eq!(passed, expected, "a DC did not return a validated resPQ through the isolated proxy");
    }

    async fn public_wss_response(
        dc: i16,
        ip: &str,
        domain: &str,
    ) -> Result<Result<usize, String>, tokio::time::error::Elapsed> {
        tokio::time::timeout(Duration::from_secs(12), async {
        let ws = ws_connect_full_opts(ip, domain, WS_PATH, 4.0, None, Some(true)).await
            .map_err(|e| format!("dial {}", e.compact()))?;
        wss_req_pq(ws, dc).await
        }).await
    }

    async fn wss_req_pq(ws: RawWebSocket, dc: i16) -> Result<usize, String> {
        let (init, mut up, mut down) = relay_init([0xdd; 4], dc);
        ws.send(&init).await.map_err(|e| format!("init {}", e.compact()))?;
        let (mut request, nonce) = request_pq();
        up.xor(&mut request);
        ws.send(&request).await.map_err(|e| format!("send {}", e.compact()))?;
        let mut reply = Vec::new();
        loop {
            let mut chunk = ws.recv().await.map_err(|e| format!("reply {} bytes={}", e.compact(), reply.len()))?;
            down.xor(&mut chunk);
            reply.extend_from_slice(&chunk);
            if reply.len() >= 44 { break; }
            if reply.len() == 4 && LittleEndian::read_i32(&reply) < 0 {
                return Err(format!("transport_error={}", LittleEndian::read_i32(&reply)));
            }
        }
        if !reply_is_res_pq(&reply, &nonce) {
            return Err(format!("invalid_resPQ bytes={}", reply.len()));
        }
        Ok::<_, String>(reply.len())
    }

    #[tokio::test]
    #[ignore = "requires public Cloudflare fallback connectivity"]
    async fn cloudflare_wss_probe() {
        init_logging(true);
        let domains = std::env::var("TGWS_CF_PROBE_DOMAIN").ok()
            .map(|domain| configured_user_domains(&domain))
            .unwrap_or_else(default_cfproxy_domains);
        let mut successes = 0;
        for (index, base_domain) in domains.iter().take(2).enumerate() {
            let host = format!("kws2.{base_domain}");
            let result = trace::SESSION.scope(trace::Session::new(1, index as u64 + 1), async {
                trace::set_route(Route::CfWss);
                tokio::time::timeout(Duration::from_secs(20), async {
                    let ws = cf_connect_domain(&host, WS_PATH, true).await.map_err(|error| error.compact())?;
                    wss_req_pq(ws, 2).await
                }).await
            }).await;
            println!("cloudflare_wss candidate={index} result={result:?}");
            successes += usize::from(matches!(result, Ok(Ok(_))));
        }
        assert_ne!(successes, 0, "no Cloudflare fallback returned a nonce-matched resPQ");
    }

    #[tokio::test]
    #[ignore = "compares public Cloudflare TLS and MTProto on the device under test"]
    async fn cloudflare_client_hello_comparison() {
        init_logging(true);
        let domains = std::env::var("TGWS_CF_PROBE_DOMAIN").ok()
            .map(|domain| configured_user_domains(&domain))
            .unwrap_or_else(default_cfproxy_domains);
        let mut cases = tokio::task::JoinSet::new();
        for (index, domain) in domains.into_iter().take(2).enumerate() {
            for dc in [2i16, -4] {
                let host = format!("kws{}.{domain}", dc.abs());
                cases.spawn(async move {
                    let lookup = tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host((&*host, 443))).await;
                    let Ok(Ok(mut addresses)) = lookup else {
                        println!("AB domain={index} dc={dc} step=dns failed");
                        return false;
                    };
                    let Some(address) = addresses.find(|address| public_fallback_ip(address.ip())) else {
                        println!("AB domain={index} dc={dc} step=dns no_public_address");
                        return false;
                    };
                    let ip = address.ip().to_string();
                    let mut replied = false;
                    use crate::ws::WsDialProfile::{SplitClientHello, Standard, Tls12};
                    for (profile_index, profile) in [Standard, SplitClientHello, Tls12].into_iter().enumerate() {
                        let trial = index * 6 + usize::from(dc < 0) * 3 + profile_index + 1;
                        let result = trace::SESSION.scope(trace::Session::new(2, trial as u64), async {
                            trace::set_route(Route::CfWss);
                            tokio::time::timeout(Duration::from_secs(16), async {
                                let ws = crate::ws::ws_connect_profile(&ip, &host, WS_PATH, CFPROXY_DIAL_PHASE_TIMEOUT, profile)
                                    .await.map_err(|error| error.compact())?;
                                wss_req_pq(ws, dc).await
                            }).await
                        }).await;
                        replied |= matches!(result, Ok(Ok(_)));
                        println!("AB domain={index} dc={dc} profile={profile:?} result={result:?}");
                    }
                    replied
                });
            }
        }
        let mut successes = 0;
        while let Some(result) = cases.join_next().await {
            successes += usize::from(result.unwrap());
        }
        assert_ne!(successes, 0, "no case returned a nonce-matched resPQ");
    }

    #[tokio::test]
    #[ignore = "requires the documented named Telegram WSS endpoints"]
    async fn telegram_named_wss_probe() {
        init_logging(true);
        let candidates = [
            (1, "pluto.web.telegram.org"),
            (-1, "pluto-1.web.telegram.org"),
            (5, "flora.web.telegram.org"),
            (-5, "flora-1.web.telegram.org"),
            (2, "venus.web.telegram.org"),
            (-2, "venus-1.web.telegram.org"),
        ];
        let mut successes = 0;
        for (i, (dc, host)) in candidates.into_iter().enumerate() {
            let started = std::time::Instant::now();
            let result = trace::SESSION.scope(trace::Session::new(1, (i + 1) as u64), async {
                trace::set_route(Route::DirectWss);
                public_wss_response(dc, "", host).await
            }).await;
            println!("named_wss signed_dc={dc} host={host} elapsed_ms={} result={result:?}",
                started.elapsed().as_millis());
            successes += usize::from(matches!(result, Ok(Ok(_))));
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        assert!(successes != 0, "no named endpoint returned a validated resPQ");
    }

    // Run explicitly on each network under test. Uses an unauthenticated
    // Telegram request and a fresh nonce, with no dependency on app settings.
    #[tokio::test]
    #[ignore = "requires network access to Telegram public WebSocket endpoints"]
    async fn telegram_wss_probe() {
        init_logging(true);
        let cases = [(1, "", false), (2, "149.154.167.220", false), (2, "149.154.167.220", true),
            (-2, "149.154.167.220", true), (-2, "149.154.167.220", false),
            (4, "149.154.167.220", false), (4, "149.154.167.220", true), (-4, "149.154.167.220", true),
            (-4, "149.154.167.220", false), (5, "", false), (201, "", false), (203, "", false),
            (1, "149.154.167.220", false), (5, "149.154.167.220", false),
            (203, "149.154.167.220", false), (203, "91.105.192.100", false)];
        let selected_dc = std::env::var("TGWS_PROBE_DC").ok().and_then(|dc| dc.parse::<i16>().ok());
        let mut probes = tokio::task::JoinSet::new();
        for (index, (dc, ip, media_host)) in cases.into_iter().enumerate() {
            if selected_dc.is_some_and(|id| id != dc) {
                continue;
            }
            probes.spawn(async move {
                let started = std::time::Instant::now();
                let domain = ws_domains(i32::from(dc).abs(), media_host).remove(0);
                let result = trace::SESSION.scope(trace::Session::new(1, (index + 1) as u64), async {
                    trace::set_route(Route::DirectWss);
                    public_wss_response(dc, ip, &domain).await
                }).await;
                let endpoint = match ip {
                    "" => "public_dns",
                    "91.105.192.100" => "dc203_legacy_ip",
                    _ => "shared_direct_override",
                };
                println!("dc={dc} media_host={media_host} endpoint={endpoint} elapsed_ms={} result={result:?}",
                    started.elapsed().as_millis());
                matches!(result, Ok(Ok(_)))
            });
        }
        let mut passed = 0;
        while let Some(result) = probes.join_next().await {
            passed += usize::from(result.unwrap());
        }
        assert!(passed != 0, "no endpoint returned a validated resPQ");
    }

    #[tokio::test]
    #[ignore = "compares Cloudflare WSS/443 with opt-in plaintext WS/80"]
    async fn cf_wss443_and_plain_ws80_return_res_pq() {
        init_logging(true);
        let domain = crate::cfproxy::default_cfproxy_domains().into_iter().next().expect("cf domain");
        let host = format!("kws2.{domain}");
        for (label, secure) in [("wss443", true), ("ws80", false)] {
            let started = std::time::Instant::now();
            let result = async {
                let ws = crate::cfproxy::cf_connect_domain(&host, "/apiws", secure).await.map_err(|e| e.compact())?;
                let (init, mut up, mut down) = relay_init([0xdd; 4], 2);
                let (mut request, nonce) = request_pq();
                up.xor(&mut request);
                ws.send(&init).await.map_err(|e| e.compact())?;
                ws.send(&request).await.map_err(|e| e.compact())?;
                let mut reply = Vec::new();
                let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
                while reply.len() < 44 {
                    let frame = tokio::time::timeout_at(deadline, ws.recv()).await
                        .map_err(|_| "reply_timeout".to_string())?
                        .map_err(|e| e.compact())?;
                    reply.extend_from_slice(&frame);
                }
                down.xor(&mut reply[..44]);
                if reply_is_res_pq(&reply[..44], &nonce) {
                    Ok(())
                } else {
                    Err(format!("invalid_resPQ bytes={}", reply.len()))
                }
            }.await;
            println!("transport={label} port={} elapsed_ms={} result={result:?}",
                if secure { 443 } else { 80 }, started.elapsed().as_millis());
        }
    }

    #[tokio::test]
    #[ignore = "requires network access to Telegram public TCP endpoints"]
    async fn telegram_tcp_probe() {
        let mut probes = tokio::task::JoinSet::new();
        for dc in [1i16, 2, -2, 3, 4, -4, 5, 203] {
            let ip = DC_DEFAULT_IPS[&i32::from(dc).abs()];
            probes.spawn(async move {
                let started = std::time::Instant::now();
                let result = async {
                    let mut socket = tokio::time::timeout(Duration::from_secs(4), TcpStream::connect((ip, 443)))
                        .await.map_err(|_| "tcp_timeout")?.map_err(|_| "tcp_error")?;
                    tokio::time::timeout(Duration::from_secs(4), async {
                        let (init, mut up, mut down) = relay_init([0xdd; 4], dc);
                        let (mut request, nonce) = request_pq();
                        up.xor(&mut request);
                        socket.write_all(&init).await.map_err(|_| "init_write_error")?;
                        socket.write_all(&request).await.map_err(|_| "request_write_error")?;
                        let mut reply = [0; 44];
                        socket.read_exact(&mut reply).await.map_err(|_| "reply_read_error")?;
                        down.xor(&mut reply);
                        reply_is_res_pq(&reply, &nonce).then_some(()).ok_or("invalid_resPQ")
                    }).await.map_err(|_| "reply_timeout")?
                }.await;
                println!("raw_tcp dc={dc} elapsed_ms={} result={result:?}", started.elapsed().as_millis());
                result.is_ok()
            });
        }
        let mut passed = 0;
        while let Some(result) = probes.join_next().await {
            passed += usize::from(result.unwrap());
        }
        assert!(passed != 0, "no raw TCP destination returned a validated resPQ");
    }

    #[tokio::test]
    async fn tcp_half_close_preserves_a_delayed_response() {
        use tokio::io::duplex;

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (remote, upstream) = tokio::join!(TcpStream::connect(addr), listener.accept());
        let (mut upstream, _) = upstream.unwrap();
        let (mut client, proxy) = duplex(32);
        let (read, write) = tokio::io::split(proxy);
        let cipher = || new_aes_ctr(&[0; 32], &[0; 16]);
        let relay = bridge_tcp(read, write, remote.unwrap(), cipher(), cipher(), cipher(), cipher(), CancellationToken::new());
        let exchange = async {
            client.write_all(b"request").await.unwrap();
            client.shutdown().await.unwrap();
            let mut request = Vec::new();
            upstream.read_to_end(&mut request).await.unwrap();
            assert_eq!(request, b"request");
            let response = vec![0x42; 1024 * 1024];
            let (sent, received) = tokio::join!(
                async { upstream.write_all(&response).await?; upstream.shutdown().await },
                async { let mut body = Vec::new(); client.read_to_end(&mut body).await?; Ok::<_, std::io::Error>(body) }
            );
            sent.unwrap();
            let received = received.unwrap();
            assert_eq!(received.len(), response.len());
            assert!(received == response);
        };
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(relay, exchange); })
            .await.unwrap();
    }

    #[tokio::test]
    async fn websocket_cancellation_releases_a_backpressured_client() {
        let (ws, mut upstream) = crate::ws::tests::loopback_ws().await;
        let (mut client, proxy) = tokio::io::duplex(16);
        let (read, write) = tokio::io::split(proxy);
        let cipher = || new_aes_ctr(&[0; 32], &[0; 16]);
        let cancel = CancellationToken::new();
        let relay = bridge_ws(read, write, ws, "test".into(), 2, true, None,
            cipher(), cipher(), cipher(), cipher(), cancel.clone(), None, |_| {});
        let exchange = async {
            upstream.write_all(&build_frame(OP_BINARY, &[0x51; 4096], false)).await.unwrap();
            let mut byte = [0];
            client.read_exact(&mut byte).await.unwrap();
            assert_eq!(byte, [0x51]);
            cancel.cancel();
        };
        tokio::time::timeout(Duration::from_secs(1), async { tokio::join!(relay, exchange); })
            .await.unwrap();
        let mut discarded = Vec::new();
        client.read_to_end(&mut discarded).await.unwrap();
        assert!(discarded.len() <= 16);
    }

    #[test]
    fn signed_ipv6_override_survives_configuration_parsing() {
        let map = parse_cidr_pool("2:192.0.2.2,-2:2001:db8::2");
        assert_eq!(map.get(&2).map(String::as_str), Some("192.0.2.2"));
        assert_eq!(map.get(&-2).map(String::as_str), Some("2001:db8::2"));
    }

    #[test]
    fn empty_auto_map_uses_flowseal_dc2_and_dc4_direct_targets() {
        let map = apply_default_dc_redirects(parse_cidr_pool(""));
        assert_eq!(map.get(&2).map(String::as_str), Some("149.154.167.220"));
        assert_eq!(map.get(&4).map(String::as_str), Some("149.154.167.220"));
        assert!(map.get(&1).is_none());
        assert!(map.get(&3).is_none());
    }

    #[test]
    fn explicit_dc2_is_not_replaced_by_the_default() {
        let map = apply_default_dc_redirects(parse_cidr_pool("2:1.2.3.4"));
        assert_eq!(map.get(&2).map(String::as_str), Some("1.2.3.4"));
        assert_eq!(map.get(&4).map(String::as_str), Some("149.154.167.220"));
    }

    #[test]
    fn connect_errors_keep_timeout_tls_and_websocket_distinct() {
        assert_eq!(super::connect_failure_class(&WsError::Timeout).as_str(), "timeout");
        assert_eq!(
            super::connect_failure_class(&WsError::Tls(std::io::ErrorKind::InvalidData.into())).as_str(),
            "tls"
        );
        assert_eq!(
            super::connect_failure_class(&WsError::Upgrade(Box::new(WsError::Other("invalid headers".into())))).as_str(),
            "websocket"
        );
    }

    #[test]
    fn cf_fallback_tries_only_two_domains_per_session() {
        let many: Vec<String> = (0..20).map(|i| format!("d{i}.example")).collect();
        let limited = limit_cf_domains(many);
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0], "d0.example");
        assert_eq!(limited[1], "d1.example");
    }

    #[tokio::test]
    async fn half_open_relay_times_out_when_the_peer_accepts_but_never_replies() {
        let (ws, _peer) = crate::ws::tests::loopback_ws().await;
        let (mut client, proxy) = tokio::io::duplex(32);
        client.write_all(b"request").await.unwrap();
        let (read, write) = tokio::io::split(proxy);
        let cipher = || new_aes_ctr(&[0; 32], &[0; 16]);
        let mut events = Vec::new();
        let result = bridge_ws(read, write, ws, "probe".into(), 2, false, None,
            cipher(), cipher(), cipher(), cipher(), CancellationToken::new(),
            Some(Duration::from_millis(100)), |evidence| events.push(evidence)).await;
        assert!(!result);
        assert_eq!(events, [Evidence::ResponseTimeout]);
    }

    #[tokio::test]
    async fn ws_idle_without_client_request_does_not_trigger_first_response_timeout() {
        let (ws, _peer) = crate::ws::tests::loopback_ws().await;
        let (_client, proxy) = tokio::io::duplex(32);
        let (read, write) = tokio::io::split(proxy);
        let cipher = || new_aes_ctr(&[0; 32], &[0; 16]);
        let cancel = CancellationToken::new();
        let cancel_soon = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(180)).await;
            cancel_soon.cancel();
        });
        let mut events = Vec::new();
        let result = bridge_ws(
            read, write, ws, "idle".into(), 2, false, None,
            cipher(), cipher(), cipher(), cipher(), cancel,
            Some(Duration::from_millis(60)),
            |evidence| events.push(evidence),
        )
        .await;
        assert!(!result);
        assert!(events.is_empty(), "idle socket without client request must not record timeout: {events:?}");
    }

    #[tokio::test]
    async fn tcp_silent_upstream_after_request_records_response_timeout() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 32];
            let _ = socket.read(&mut buf).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let remote = TcpStream::connect(addr).await.unwrap();
        let (mut client, proxy) = tokio::io::duplex(64);
        client.write_all(b"client-request").await.unwrap();
        let (read, write) = tokio::io::split(proxy);
        let cipher = || new_aes_ctr(&[0; 32], &[0; 16]);
        let mut events = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            bridge_tcp_observed(
                read, write, remote,
                cipher(), cipher(), cipher(), cipher(),
                CancellationToken::new(),
                Some(Duration::from_millis(100)),
                |evidence| events.push(evidence),
            ),
        )
        .await
        .unwrap();
        assert_eq!(events, [Evidence::ResponseTimeout]);
    }

    #[tokio::test]
    async fn tcp_early_upstream_eof_after_request_records_peer_failure() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 32];
            let _ = socket.read(&mut buf).await;
            drop(socket);
        });
        let remote = TcpStream::connect(addr).await.unwrap();
        let (mut client, proxy) = tokio::io::duplex(64);
        client.write_all(b"client-request").await.unwrap();
        let (read, write) = tokio::io::split(proxy);
        let cipher = || new_aes_ctr(&[0; 32], &[0; 16]);
        let mut events = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            bridge_tcp_observed(
                read, write, remote,
                cipher(), cipher(), cipher(), cipher(),
                CancellationToken::new(),
                Some(Duration::from_secs(5)),
                |evidence| events.push(evidence),
            ),
        )
        .await
        .unwrap();
        assert_eq!(events, [Evidence::PeerFailure]);
    }

    #[tokio::test]
    async fn tcp_client_write_failure_does_not_record_protocol_response_or_upstream_failure() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 32];
            let _ = socket.read(&mut buf).await;
            // Send a valid intermediate-framed 36-byte payload (len=32 + 32 bytes)
            let mut frame = Vec::with_capacity(36);
            frame.extend_from_slice(&32u32.to_le_bytes());
            frame.extend_from_slice(&[0xAB; 32]);
            let _ = socket.write_all(&frame).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        let remote = TcpStream::connect(addr).await.unwrap();
        let (mut client, proxy) = tokio::io::duplex(64);
        client.write_all(b"client-request").await.unwrap();
        drop(client); // Client disconnects before receiving response
        let (read, write) = tokio::io::split(proxy);
        let cipher = || new_aes_ctr(&[0; 32], &[0; 16]);
        let mut events = Vec::new();
        let session = trace::Session::new(1, 1);
        trace::SESSION
            .scope(session, async {
                trace::set_protocol(PROTO_INTERMEDIATE_INT);
                tokio::time::timeout(
                    Duration::from_secs(2),
                    bridge_tcp_observed(
                        read, write, remote,
                        cipher(), cipher(), cipher(), cipher(),
                        CancellationToken::new(),
                        Some(Duration::from_secs(2)),
                        |evidence| events.push(evidence),
                    ),
                )
                .await
                .unwrap();
            })
            .await;
        assert!(
            !events.contains(&Evidence::ProtocolResponse) && !events.contains(&Evidence::PeerFailure),
            "client write failure must not emit ProtocolResponse or PeerFailure: {events:?}"
        );
    }

    #[tokio::test]
    async fn ws_pool_get_skips_stale_socket_and_returns_healthy_second_socket() {
        let pool = Arc::new(WsPool::new(CancellationToken::new()));
        let (stale_ws, stale_peer) = crate::ws::tests::loopback_ws().await;
        drop(stale_peer); // peer closed -> check_idle fails immediately

        let (healthy_ws, mut healthy_peer) = crate::ws::tests::loopback_ws().await;
        let pong_task = tokio::spawn(async move {
            // Read client-masked 8-byte PING frame: 2 header bytes + 4 mask bytes + 8 payload bytes = 14 bytes
            let mut frame = [0u8; 14];
            healthy_peer.read_exact(&mut frame).await.unwrap();
            let mask = &frame[2..6];
            let mut nonce = [0u8; 8];
            for i in 0..8 {
                nonce[i] = frame[6 + i] ^ mask[i % 4];
            }
            healthy_peer
                .write_all(&build_frame(OP_PONG, &nonce, false))
                .await
                .unwrap();
        });

        let slot = DcSlot { dc: 2, is_media: 1 };
        let state = pool.get_slot(slot).await;
        {
            let mut q = state.queue.lock().await;
            q.push_back(PoolEntry { ws: stale_ws, created: now_monotonic() });
            q.push_back(PoolEntry { ws: healthy_ws, created: now_monotonic() });
        }

        let got = pool.get(2, true, Some("loopback")).await;
        assert!(got.is_some(), "pool checkout must skip stale socket and return live second entry");
        pong_task.await.unwrap();
    }

    #[tokio::test]
    async fn ws_pool_evict_stale_epoch_removes_old_network_entries_and_keeps_current() {
        let pool = Arc::new(WsPool::new(CancellationToken::new()));
        let (mut old_ws, _old_peer) = crate::ws::tests::loopback_ws().await;
        let current_epoch = trace::network();
        old_ws.network = current_epoch.wrapping_add(8);
        let (cur_ws, _cur_peer) = crate::ws::tests::loopback_ws().await;

        let slot = DcSlot { dc: 4, is_media: 1 };
        let state = pool.get_slot(slot).await;
        {
            let mut q = state.queue.lock().await;
            q.push_back(PoolEntry { ws: old_ws, created: now_monotonic() });
            q.push_back(PoolEntry { ws: cur_ws, created: now_monotonic() });
        }

        pool.evict_stale_epoch(current_epoch).await;
        let q = state.queue.lock().await;
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].ws.network, current_epoch);
    }

    #[test]
    fn ten_consecutive_network_transitions_keep_ws80_strictly_cellular_and_generations_distinct() {
        static NETWORK_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::const_mutex(());
        let _guard = NETWORK_TEST_LOCK.lock();
        let prev_plain = crate::config::CFPROXY_PLAIN_WS80.swap(true, Ordering::Relaxed);
        let prev_cf = crate::config::CFPROXY_ENABLED.swap(true, Ordering::Relaxed);
        let prev_domains = crate::balancer::BALANCER.read().get_domains_for_dc(2);
        crate::balancer::BALANCER.write().update_domains_list(&["cf.example".into()]);

        let transitions: &[(u8, trace::NetworkKind, bool)] = &[
            (1, trace::NetworkKind::Wifi, true),      // 1: Wi-Fi -> TLS/443
            (1, trace::NetworkKind::Wifi, true),      // 2: repeated Wi-Fi -> TLS/443
            (2, trace::NetworkKind::Cellular, false), // 3: Cellular -> WS/80 (opted in)
            (2, trace::NetworkKind::Cellular, false), // 4: repeated Cellular -> WS/80
            (0, trace::NetworkKind::Unknown, true),   // 5: Unknown -> TLS/443
            (1, trace::NetworkKind::Wifi, true),      // 6: Wi-Fi -> TLS/443
            (2, trace::NetworkKind::Cellular, false), // 7: Cellular -> WS/80
            (1, trace::NetworkKind::Wifi, true),      // 8: Wi-Fi -> TLS/443
            (3, trace::NetworkKind::Ethernet, true),  // 9: Ethernet -> TLS/443
            (2, trace::NetworkKind::Cellular, false), // 10: Cellular -> WS/80
        ];

        let mut prev_gen = trace::NetworkKind::generation(trace::network());
        for (idx, &(raw_kind, expected_kind, expected_cf_secure)) in transitions.iter().enumerate() {
            let epoch = trace::change_network(raw_kind, (idx as u64) + 100, true);
            let gen = trace::NetworkKind::generation(epoch);
            assert!(gen > prev_gen, "generation must strictly increase on transition {idx}");
            prev_gen = gen;
            assert_eq!(trace::NetworkKind::from_epoch(epoch), expected_kind);

            let scope = Scope { epoch, dc: DcKey::new(2, false, false) };
            let candidates = route_candidates(scope);
            let direct = candidates.iter().filter(|c| c.kind == Route::DirectWss).collect::<Vec<_>>();
            assert!(direct.iter().all(|c| c.secure), "DirectWss must always stay TLS/443");
            let cf = candidates.iter().filter(|c| c.kind == Route::CfWss).collect::<Vec<_>>();
            assert!(!cf.is_empty(), "expected CF candidates");
            assert!(
                cf.iter().all(|c| c.secure == expected_cf_secure),
                "transition {idx} ({expected_kind:?}) expected cf_secure={expected_cf_secure}"
            );
        }

        // With WS/80 consent disabled, cellular must also stay TLS/443.
        crate::config::CFPROXY_PLAIN_WS80.store(false, Ordering::Relaxed);
        let cell_epoch = trace::change_network(2, 999, true);
        let cell_candidates = route_candidates(Scope { epoch: cell_epoch, dc: DcKey::new(2, false, false) });
        assert!(cell_candidates.iter().filter(|c| c.kind == Route::CfWss).all(|c| c.secure));

        crate::balancer::BALANCER.write().update_domains_list(&prev_domains);
        crate::config::CFPROXY_PLAIN_WS80.store(prev_plain, Ordering::Relaxed);
        crate::config::CFPROXY_ENABLED.store(prev_cf, Ordering::Relaxed);
    }
}

pub fn parse_cidr_pool(cidrs_str: &str) -> HashMap<i32, String> {
    let mut result = HashMap::new();
    if cidrs_str.trim().is_empty() {
        return result;
    }
    for pair in cidrs_str.split(',') {
        let Some((dc, ip)) = pair.split_once(':') else { continue };
        let ip = ip.trim();
        let ip = ip.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(ip);
        if let (Ok(dc), Ok(ip)) = (dc.trim().parse::<i32>(), ip.parse::<std::net::IpAddr>()) {
            result.insert(dc, ip.to_string());
        }
    }
    result
}
