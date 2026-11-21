use crate::config::{PROTO_ABRIDGED_INT, PROTO_INTERMEDIATE_INT, PROTO_PADDED_INTERMEDIATE_INT};
use std::cell::Cell;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// Low three bits hold the transport kind. The upper bits count default-network changes.
static NETWORK: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum NetworkKind {
    Unknown = 0,
    Wifi = 1,
    Cellular = 2,
    Ethernet = 3,
    Other = 4,
}

impl NetworkKind {
    pub(crate) fn from_epoch(epoch: u64) -> Self {
        match epoch & 7 {
            1 => Self::Wifi,
            2 => Self::Cellular,
            3 => Self::Ethernet,
            4 => Self::Other,
            _ => Self::Unknown,
        }
    }

    pub(crate) fn generation(epoch: u64) -> u64 {
        epoch >> 3
    }
}

#[derive(Default)]
struct NetworkChange {
    epoch: u64,
    handover: u64,
}

static NETWORK_CHANGES: once_cell::sync::Lazy<tokio::sync::watch::Sender<NetworkChange>> =
    once_cell::sync::Lazy::new(|| tokio::sync::watch::channel(NetworkChange::default()).0);

pub(crate) async fn network_changed(epoch: u64) {
    let mut changes = NETWORK_CHANGES.subscribe();
    let _ = changes.wait_for(|current| current.epoch != epoch).await;
}

pub(crate) fn network() -> u64 {
    NETWORK.load(Ordering::Acquire)
}

pub(crate) fn network_handover() -> impl std::future::Future<Output = ()> + Send + 'static {
    let mut changes = NETWORK_CHANGES.subscribe();
    let handover = changes.borrow().handover;
    async move {
        let _ = changes.wait_for(|current| current.handover != handover).await;
    }
}

pub(crate) fn change_network(kind: u8, identity: u64, replaced: bool) -> u64 {
    let kind = kind.min(4);
    let mut epoch = 0;
    NETWORK_CHANGES.send_modify(|current| {
        epoch = ((current.epoch & !7) + 8) | u64::from(kind);
        current.epoch = epoch;
        if replaced {
            current.handover = current.handover.wrapping_add(1);
        }
        crate::routing::set_network_identity(epoch, identity);
        NETWORK.store(epoch, Ordering::Release);
    });
    crate::linfo!(
        "stage=network_changed epoch={} network={kind} handover={replaced}",
        epoch >> 3
    );
    epoch
}

#[derive(Default)]
struct Status {
    network: u64,
    verified: Option<Instant>,
    relay: Option<Instant>,
    exhausted: Option<Instant>,
}

static STATUS: parking_lot::Mutex<Status> = parking_lot::const_mutex(Status {
    network: 0,
    verified: None,
    relay: None,
    exhausted: None,
});

fn update_status(update: impl FnOnce(&mut Status, Instant)) {
    let network = session_network();
    let mut status = STATUS.lock();
    if network != self::network() {
        return;
    }
    if status.network != network {
        *status = Status {
            network,
            ..Status::default()
        };
    }
    update(&mut status, Instant::now());
}

pub(crate) fn mark_verified() {
    update_status(|status, now| status.verified = Some(now));
}

pub(crate) fn mark_relay() {
    update_status(|status, now| status.relay = Some(now));
}

pub(crate) fn mark_exhausted() {
    update_status(|status, now| status.exhausted = Some(now));
}

pub(crate) fn reset_status() {
    *STATUS.lock() = Status::default();
}

pub(crate) fn current_status(active: bool) -> i32 {
    let status = STATUS.lock();
    let latest = |event: Option<Instant>, within: u64| {
        status.network == network()
            && event.is_some_and(|t| t.elapsed() < Duration::from_secs(within))
    };
    let verified = latest(status.verified, 15);
    let relay = latest(status.relay, 8);
    let exhausted = latest(status.exhausted, 10);
    match (verified || relay, relay, exhausted, active) {
        (true, _, true, _) => 5, // degraded
        (_, _, true, _) => 6,    // no available route in recent sessions
        (_, true, _, _) => 4,    // recently relayed downstream bytes
        (true, _, _, _) => 3,    // established upstream transport
        (_, _, _, true) => 2,    // connecting
        _ => 1,                  // listening
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) enum Route {
    DirectWss,
    WorkerWss,
    CfWss,
    Tcp,
}

pub(crate) struct Session {
    run: u64,
    ordinal: u64,
    opened: Instant,
    network: u64,
    route: Cell<Option<Route>>,
    attempt: Cell<u32>,
    proto: Cell<u32>,
}

tokio::task_local! {
    pub(crate) static SESSION: Session;
}

impl Session {
    pub(crate) fn new(run: u64, ordinal: u64) -> Self {
        Self {
            run,
            ordinal,
            opened: Instant::now(),
            network: network(),
            route: Cell::new(None),
            attempt: Cell::new(0),
            proto: Cell::new(0),
        }
    }

    fn log(&self, level: bool, stage: &str, detail: fmt::Arguments<'_>) {
        let message = format!(
            "[run={:016x} ordinal={} epoch={} network={}] stage={} route={:?} attempt={} elapsed_ms={} {}",
            self.run, self.ordinal, self.network >> 3, self.network & 7, stage, self.route.get(),
            self.attempt.get(), self.opened.elapsed().as_millis(), detail
        );
        if level {
            crate::config::log_info(&message);
        } else {
            crate::config::log_debug(&message);
        }
    }
}

pub(crate) fn event(stage: &str, detail: fmt::Arguments<'_>) {
    let _ = SESSION.try_with(|session| session.log(false, stage, detail));
}

pub(crate) fn end(reason: &str) {
    let _ = SESSION.try_with(|session| {
        if session.proto.get() != 0 || log_unverified(session.ordinal) {
            session.log(true, "session_end", format_args!("reason={reason}"));
        }
    });
}

fn log_unverified(ordinal: u64) -> bool {
    ordinal <= 8 || ordinal.is_multiple_of(128)
}

pub(crate) fn sample_unverified() -> bool {
    SESSION.try_with(|session| log_unverified(session.ordinal)).unwrap_or(true)
}

pub(crate) fn label() -> Option<String> {
    SESSION
        .try_with(|s| {
            format!(
                "run={:016x} ordinal={} epoch={}",
                s.run,
                s.ordinal,
                s.network >> 3
            )
        })
        .ok()
}

pub(crate) fn set_route(route: Route) {
    let _ = SESSION.try_with(|s| s.route.set(Some(route)));
}

pub(crate) fn set_protocol(proto: u32) {
    let _ = SESSION.try_with(|s| s.proto.set(proto));
}

pub(crate) fn attempt() {
    let _ = SESSION.try_with(|s| s.attempt.set(s.attempt.get() + 1));
}

pub(crate) fn session_network() -> u64 {
    SESSION
        .try_with(|s| s.network)
        .unwrap_or_else(|_| network())
}

pub(crate) fn allowlisted_host(host: &str) -> &str {
    if host.ends_with(".web.telegram.org") {
        host
    } else {
        "configured_or_cf"
    }
}

pub(crate) fn allowlisted_peer(peer: Option<std::net::SocketAddr>) -> String {
    let Some(peer) = peer else {
        return "unknown".into();
    };
    let ip = peer.ip().to_string();
    if ip == crate::proxy::SHARED_DIRECT_WSS_IP
        || crate::config::DC_DEFAULT_IPS
            .values()
            .any(|known| *known == ip)
        || crate::config::DC_TEST_IPS
            .values()
            .any(|known| *known == ip)
    {
        peer.to_string()
    } else if peer.is_ipv6() {
        "masked_ipv6".into()
    } else {
        "masked_ipv4".into()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplyClass {
    TransportError(i32),
    QuickAck,
    ShortUnknown,
    FramedPayload,
    InvalidEnvelope,
    Unclassified,
}

// The observer never allocates or buffers an MTProto message. Beyond this
// conservative bound it stops classifying and leaves relay bytes untouched.
const MAX_OBSERVED_PAYLOAD: usize = 16 * 1024 * 1024;
const PADDED_CONTROL_MAX: usize = 19;

struct RecordBody {
    len: usize,
    remaining: usize,
    control_prefix: [u8; 4],
}

/// Walks transport envelopes after obfuscation is removed. Frame boundaries
/// here are independent of WebSocket message and TCP read boundaries.
pub(crate) struct ReplyObserver {
    proto: u32,
    header: [u8; 4],
    header_used: usize,
    body: Option<RecordBody>,
    first: bool,
    disabled: bool,
}

impl ReplyObserver {
    pub(crate) fn for_session() -> Self {
        Self::new(SESSION.try_with(|s| s.proto.get()).unwrap_or_default())
    }

    fn new(proto: u32) -> Self {
        Self {
            proto,
            header: [0; 4],
            header_used: 0,
            body: None,
            first: true,
            disabled: false,
        }
    }

    pub(crate) fn observe(&mut self, mut input: &[u8], mut visit: impl FnMut(ReplyClass)) {
        if self.disabled || input.is_empty() {
            return;
        }
        if !matches!(
            self.proto,
            PROTO_ABRIDGED_INT | PROTO_INTERMEDIATE_INT | PROTO_PADDED_INTERMEDIATE_INT
        ) {
            self.disabled = true;
            self.report(ReplyClass::Unclassified, &mut visit);
            return;
        }
        while !input.is_empty() {
            if let Some(body) = &mut self.body {
                let consumed = body.remaining.min(input.len());
                let offset = body.len - body.remaining;
                if body.len <= PADDED_CONTROL_MAX && offset < body.control_prefix.len() {
                    let copy = consumed.min(body.control_prefix.len() - offset);
                    body.control_prefix[offset..offset + copy].copy_from_slice(&input[..copy]);
                }
                body.remaining -= consumed;
                input = &input[consumed..];
                if body.remaining == 0 {
                    let body = self.body.take().expect("record in progress");
                    self.report(self.classify_body(body), &mut visit);
                }
                continue;
            }

            let abridged = self.proto == PROTO_ABRIDGED_INT;
            let header_len = if abridged {
                if self.header_used == 0 {
                    self.header[0] = input[0];
                    self.header_used = 1;
                    input = &input[1..];
                }
                if self.header[0] < 0x7f {
                    1
                } else {
                    4
                }
            } else {
                4
            };
            let consumed = (header_len - self.header_used).min(input.len());
            self.header[self.header_used..self.header_used + consumed]
                .copy_from_slice(&input[..consumed]);
            self.header_used += consumed;
            input = &input[consumed..];
            if self.header_used != header_len {
                continue;
            }
            self.header_used = 0;
            let word = u32::from_le_bytes(self.header);
            if (abridged && self.header[0] >= 0x80)
                || (self.proto == PROTO_INTERMEDIATE_INT && word & 0x8000_0000 != 0)
            {
                self.report(ReplyClass::QuickAck, &mut visit);
                continue;
            }
            let len = if abridged {
                if header_len == 1 {
                    usize::from(self.header[0]) * 4
                } else {
                    (word >> 8) as usize * 4
                }
            } else {
                word as usize
            };
            if !(4..=MAX_OBSERVED_PAYLOAD).contains(&len)
                || (self.proto == PROTO_INTERMEDIATE_INT && len % 4 != 0)
            {
                self.disabled = true;
                self.report(ReplyClass::InvalidEnvelope, &mut visit);
                return;
            }
            self.body = Some(RecordBody {
                len,
                remaining: len,
                control_prefix: [0; 4],
            });
        }
    }

    fn classify_body(&self, body: RecordBody) -> ReplyClass {
        let control_max = if self.proto == PROTO_PADDED_INTERMEDIATE_INT {
            PADDED_CONTROL_MAX
        } else {
            4
        };
        if body.len > control_max {
            return if body.len >= 20 {
                ReplyClass::FramedPayload
            } else {
                ReplyClass::ShortUnknown
            };
        }
        match i32::from_le_bytes(body.control_prefix) {
            -1 if self.proto == PROTO_PADDED_INTERMEDIATE_INT && (8..=16).contains(&body.len) => {
                ReplyClass::QuickAck
            }
            code @ (-404 | -403 | -429 | -444) => ReplyClass::TransportError(code),
            _ => ReplyClass::ShortUnknown,
        }
    }

    fn report(&mut self, class: ReplyClass, visit: &mut impl FnMut(ReplyClass)) {
        if self.first {
            event("first_transport_record", format_args!("class={class:?}"));
            self.first = false;
        }
        visit(class);
    }

    pub(crate) fn incomplete(&self) -> bool {
        self.header_used != 0 || self.body.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACK: [u8; 12] = [8, 0, 0, 0, 255, 255, 255, 255, 1, 2, 3, 4];
    const ERROR: [u8; 12] = [8, 0, 0, 0, 108, 254, 255, 255, 0, 0, 0, 0];

    fn observe(reply: &mut ReplyObserver, data: &[u8]) -> Vec<ReplyClass> {
        let mut events = Vec::new();
        reply.observe(data, |event| events.push(event));
        events
    }

    #[test]
    fn recognizes_a_transport_error_after_a_quick_ack() {
        let mut reply = ReplyObserver::new(PROTO_PADDED_INTERMEDIATE_INT);
        assert_eq!(observe(&mut reply, &ACK), [ReplyClass::QuickAck]);
        assert!(observe(&mut reply, &ERROR[..7]).is_empty());
        assert_eq!(
            observe(&mut reply, &ERROR[7..]),
            [ReplyClass::TransportError(-404)]
        );
    }

    #[test]
    fn detects_complete_padded_error_under_all_split_positions() {
        let mut record = 12u32.to_le_bytes().to_vec();
        record.extend_from_slice(&(-404i32).to_le_bytes());
        record.extend_from_slice(&[0xaa; 8]);
        for at in 1..record.len() {
            let mut observer = ReplyObserver::new(PROTO_PADDED_INTERMEDIATE_INT);
            assert!(observe(&mut observer, &record[..at]).is_empty());
            assert!(observer.incomplete());
            assert_eq!(
                observe(&mut observer, &record[at..]),
                [ReplyClass::TransportError(-404)]
            );
            assert!(!observer.incomplete());
        }
    }

    #[test]
    fn only_complete_frames_count_and_following_errors_are_visible() {
        let mut reply = ReplyObserver::new(PROTO_PADDED_INTERMEDIATE_INT);
        assert!(observe(&mut reply, &4096u32.to_le_bytes()).is_empty());
        assert!(observe(&mut reply, &[0x41; 4095]).is_empty());
        assert!(reply.incomplete());
        let mut tail = vec![0x41];
        tail.extend_from_slice(&ACK);
        tail.extend_from_slice(&ERROR);
        assert_eq!(
            observe(&mut reply, &tail),
            [
                ReplyClass::FramedPayload,
                ReplyClass::QuickAck,
                ReplyClass::TransportError(-404)
            ]
        );
        assert!(!reply.incomplete());
    }

    #[test]
    fn keeps_ack_and_error_boundaries_for_unpadded_transports() {
        let mut intermediate = ReplyObserver::new(PROTO_INTERMEDIATE_INT);
        assert_eq!(
            observe(
                &mut intermediate,
                &[0, 0, 0, 128, 4, 0, 0, 0, 68, 254, 255, 255]
            ),
            [ReplyClass::QuickAck, ReplyClass::TransportError(-444)]
        );
        let mut abridged = ReplyObserver::new(PROTO_ABRIDGED_INT);
        assert_eq!(
            observe(&mut abridged, &[128, 0, 0, 0, 1, 83, 254, 255, 255]),
            [ReplyClass::QuickAck, ReplyClass::TransportError(-429)]
        );
    }

    #[test]
    fn oversized_or_invalid_envelope_cannot_verify_the_route() {
        let mut reply = ReplyObserver::new(PROTO_PADDED_INTERMEDIATE_INT);
        assert_eq!(
            observe(&mut reply, &u32::MAX.to_le_bytes()),
            [ReplyClass::InvalidEnvelope]
        );
        assert!(observe(&mut reply, &ERROR).is_empty());
        assert!(!reply.incomplete());
    }
}
