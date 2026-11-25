use crate::config::DcKey;
use crate::trace::Route;
use parking_lot::Mutex;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CACHE_TTL: u64 = 12 * 60 * 60;
const MAX_HINTS: usize = 64;
const MAX_ROUTES: usize = 256;
static CACHE_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
static CACHE_IO: Mutex<()> = Mutex::new(());
static NETWORK_ID: Mutex<(u64, u64)> = Mutex::new((0, 0));

pub(crate) fn set_cache_dir(path: PathBuf) {
    *CACHE_DIR.lock() = Some(path);
}

pub(crate) fn set_network_identity(epoch: u64, identity: u64) {
    *NETWORK_ID.lock() = (epoch, identity);
}

fn network_identity(epoch: u64) -> u64 {
    let (observed, identity) = *NETWORK_ID.lock();
    if observed == epoch {
        identity
    } else {
        0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) struct Scope {
    pub epoch: u64,
    pub dc: DcKey,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct Candidate {
    pub kind: Route,
    pub host: String,
    pub target: String,
    pub secure: bool,
}

impl Candidate {
    pub(crate) fn new(kind: Route, host: String, target: String, secure: bool) -> Self {
        Self {
            kind,
            host,
            target,
            secure,
        }
    }

    fn cacheable(&self) -> bool {
        self.kind == Route::DirectWss
            && self.secure
            && self.host.ends_with(".web.telegram.org")
            && self.target == crate::proxy::SHARED_DIRECT_WSS_IP
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Evidence {
    Reachable,
    TransportVerified,
    ProtocolResponse,
    Sustained,
    DialFailure,
    PeerFailure,
    ResponseTimeout,
    TransportError(i32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Unknown,
    Probing,
    Healthy,
    Degraded,
    Cooldown,
    Unavailable,
}

struct Health {
    failures: u8,
    last_failure: Option<Instant>,
    cooldown: Option<Instant>,
    probation: bool,
    probing: bool,
    response: bool,
    sustained: u8,
    verified: bool,
    latency_ms: Option<u32>,
    endpoints: HashMap<SocketAddr, Evidence>,
    updated: Instant,
}

impl Health {
    fn new(now: Instant) -> Self {
        Self {
            failures: 0,
            last_failure: None,
            cooldown: None,
            probation: false,
            probing: false,
            response: false,
            sustained: 0,
            verified: false,
            latency_ms: None,
            endpoints: HashMap::new(),
            updated: now,
        }
    }

    fn state(&self, now: Instant) -> State {
        match self.cooldown {
            Some(until) if until > now && self.failures >= 5 => State::Unavailable,
            Some(until) if until > now => State::Cooldown,
            _ if self.probing => State::Probing,
            _ if self.failures > 0 || self.probation => State::Degraded,
            _ if self.response => State::Healthy,
            _ => State::Unknown,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Policy {
    pub failures_before_cooldown: u8,
    pub failure_window: Duration,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub min_tenure: Duration,
    pub preference_ttl: Duration,
    pub score_margin: u8,
    pub jitter_ms: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            failures_before_cooldown: 2,
            failure_window: Duration::from_secs(30),
            initial_backoff: Duration::from_secs(3),
            max_backoff: Duration::from_secs(45),
            min_tenure: Duration::from_secs(20),
            preference_ttl: Duration::from_secs(15 * 60),
            score_margin: 2,
            jitter_ms: 400,
        }
    }
}

#[derive(Clone)]
struct Preferred {
    candidate: Candidate,
    since: Instant,
    evidence: Instant,
}

#[derive(Clone, Serialize, Deserialize)]
struct Hint {
    network: u64,
    dc: i32,
    media: bool,
    host: String,
    valid_until: u64,
}

#[derive(Serialize, Deserialize)]
struct StoredHints {
    version: u32,
    hints: Vec<Hint>,
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn read_hints(path: &std::path::Path, now: u64) -> Vec<Hint> {
    use std::io::Read;
    let mut bytes = Vec::new();
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    if file.take(32 * 1024).read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    serde_json::from_slice::<StoredHints>(&bytes)
        .ok()
        .filter(|cache| cache.version == 1)
        .map(|cache| {
            cache
                .hints
                .into_iter()
                .take(MAX_HINTS)
                .filter(|hint| hint.valid_until > now && hint.valid_until <= now + CACHE_TTL)
                .collect()
        })
        .unwrap_or_default()
}

fn write_hint(path: PathBuf, hint: Hint) {
    let _guard = CACHE_IO.lock();
    let mut hints = read_hints(&path, unix_time());
    hints.retain(|old| (old.network, old.dc, old.media) != (hint.network, hint.dc, hint.media));
    hints.insert(0, hint);
    hints.truncate(MAX_HINTS);
    let tmp = path.with_extension("tmp");
    if let Ok(data) = serde_json::to_vec(&StoredHints { version: 1, hints }) {
        if std::fs::write(&tmp, data).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }
}

const FAMILY_THRESHOLD: u8 = 4;
const FAMILY_BACKOFF: Duration = Duration::from_secs(4);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum FailureClass {
    #[default]
    None,
    Timeout,
    Dns,
    Tcp,
    Tls,
    Websocket,
    Transport,
    Other,
}

impl FailureClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Timeout => "timeout",
            Self::Dns => "dns",
            Self::Tcp => "tcp",
            Self::Tls => "tls",
            Self::Websocket => "websocket",
            Self::Transport => "transport",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Family {
    consecutive: u8,
    open_until: Option<Instant>,
    in_flight: u8,
    last_failure: FailureClass,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FamilyAttempt {
    Dial,
    Probe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SkipReason {
    NetworkChanged,
    RouteCooling,
    RouteProbing,
    FamilyCooling,
    FamilyBudget,
}

#[derive(Clone)]
struct ExhaustedEntry {
    at: Instant,
    reason: &'static str,
    candidates: usize,
    attempts: u32,
}

pub(crate) struct Manager {
    policy: Policy,
    health: HashMap<(Scope, Candidate), Health>,
    preferred: HashMap<Scope, Preferred>,
    hints: Vec<Hint>,
    path: Option<PathBuf>,
    next_retry: Option<(u64, Instant)>,
    last_payload: Option<(u64, Instant)>,
    changes: VecDeque<(u64, serde_json::Value)>,
    family: HashMap<(u64, Route), Family>,
    exhausted: HashMap<Scope, ExhaustedEntry>,
}

impl Default for Manager {
    fn default() -> Self {
        let path = CACHE_DIR
            .lock()
            .as_ref()
            .map(|dir| dir.join("route-preferences-v1.json"));
        let hints = path
            .as_ref()
            .map(|path| read_hints(path, unix_time()))
            .unwrap_or_default();
        Self {
            hints,
            path,
            family: HashMap::new(),
            exhausted: HashMap::new(),
            ..Self::with_policy(Policy::default())
        }
    }
}

impl Manager {
    fn with_policy(policy: Policy) -> Self {
        Self {
            policy,
            health: HashMap::new(),
            preferred: HashMap::new(),
            hints: Vec::new(),
            path: None,
            next_retry: None,
            last_payload: None,
            changes: VecDeque::new(),
            family: HashMap::new(),
            exhausted: HashMap::new(),
        }
    }

    pub(crate) fn order(
        &mut self,
        scope: Scope,
        candidates: &mut [Candidate],
        now: Instant,
        automatic: bool,
    ) {
        // A reconnected network starts with fresh counters, even if it has a saved hint.
        self.health.retain(|(key, _), state| {
            key.epoch == scope.epoch
                && (state.probing
                    || now.saturating_duration_since(state.updated) <= self.policy.preference_ttl)
        });
        self.preferred.retain(|key, pref| {
            key.epoch == scope.epoch
                && now.saturating_duration_since(pref.evidence) <= self.policy.preference_ttl
        });
        self.changes.retain(|(epoch, _)| *epoch == scope.epoch);
        self.family.retain(|(epoch, _), _| *epoch == scope.epoch);
        self.exhausted.retain(|key, entry| {
            key.epoch == scope.epoch
                && now.saturating_duration_since(entry.at) <= Duration::from_secs(60)
        });
        if !automatic {
            return;
        }
        if let Some(first) = candidates.iter().position(|candidate| candidate.kind == Route::CfWss) {
            let count = candidates[first..].iter()
                .take_while(|candidate| candidate.kind == Route::CfWss)
                .count();
            candidates[first..first + count].sort_by_cached_key(|candidate| {
                // Share CF domain discovery and latency across DCs and main/media roles
                // on the same network epoch while keeping per-DC MTProto health separate.
                let mut usable = false;
                let mut degraded = false;
                let mut best_latency_ms = u32::MAX;
                let mut last_attempt: Option<Instant> = None;
                let domain_key = Self::cf_domain_key(candidate);
                for ((key, seen), state) in &self.health {
                    if key.epoch == scope.epoch
                        && seen.kind == Route::CfWss
                        && Self::cf_domain_key(seen) == domain_key
                        && seen.secure == candidate.secure
                    {
                        if (state.verified || state.response) && !state.probation {
                            usable = true;
                            if let Some(ms) = state.latency_ms {
                                best_latency_ms = best_latency_ms.min(ms);
                            }
                        } else if state.failures > 0 || state.probation {
                            degraded = true;
                        }
                        last_attempt = Some(last_attempt.map_or(state.updated, |prev| prev.max(state.updated)));
                    }
                }
                (!usable, !usable && degraded, best_latency_ms, last_attempt)
            });
        }
        let identity = network_identity(scope.epoch);
        let hint_host = self
            .hints
            .iter()
            .find(|hint| {
                hint.network == identity
                    && identity != 0
                    && hint.dc == scope.dc.dc
                    && hint.media == (scope.dc.is_media != 0)
                    && scope.dc.is_test == 0
                    && hint.valid_until > unix_time()
            })
            .map(|hint| hint.host.as_str());
        let preferred = self.preferred.get(&scope).map(|pref| &pref.candidate);
        let direct_degraded = self.direct_is_degraded(scope.epoch, now);
        let index = candidates
            .iter()
            .position(|candidate| preferred == Some(candidate))
            .or_else(|| {
                candidates.iter().position(|candidate| {
                    candidate.cacheable() && hint_host == Some(candidate.host.as_str())
                })
            });
        if let Some(index) = index {
            let kind = candidates[index].kind;
            if direct_degraded && matches!(kind, Route::CfWss | Route::WorkerWss) {
                // Direct WSS has repeatedly failed on this network epoch; promote the
                // proven healthy permitted fallback ahead of DirectWss to avoid repeated
                // 5-second timeouts before every new Telegram/media connection.
                candidates[..=index].rotate_right(1);
            } else {
                // Keep DirectWss ahead of fallbacks when DirectWss is healthy or untested.
                let first = candidates[..index]
                    .iter()
                    .rposition(|candidate| candidate.kind != kind)
                    .map_or(0, |previous| previous + 1);
                candidates[first..=index].rotate_right(1);
            }
        } else if direct_degraded {
            // Even for a newly contacted DC or media role on a cellular network where
            // DirectWss is already known failing, try a proven healthy CF domain first.
            if let Some(cf_idx) = candidates.iter().position(|candidate| {
                candidate.kind == Route::CfWss && self.has_healthy_cf_domain(scope.epoch, candidate)
            }) {
                candidates[..=cf_idx].rotate_right(1);
            }
        }
    }

    fn cf_domain_key(candidate: &Candidate) -> &str {
        candidate
            .host
            .split_once('.')
            .map(|(_, rest)| rest)
            .filter(|rest| !rest.is_empty())
            .unwrap_or(candidate.target.as_str())
    }

    fn has_healthy_route(&self, epoch: u64, kind: Route) -> bool {
        self.health.iter().any(|((scope, candidate), state)| {
            scope.epoch == epoch
                && candidate.kind == kind
                && state.response
                && !state.probation
        })
    }

    fn has_healthy_cf_domain(&self, epoch: u64, candidate: &Candidate) -> bool {
        let domain_key = Self::cf_domain_key(candidate);
        self.health.iter().any(|((scope, seen), state)| {
            scope.epoch == epoch
                && seen.kind == Route::CfWss
                && Self::cf_domain_key(seen) == domain_key
                && seen.secure == candidate.secure
                && state.response
                && !state.probation
        })
    }

    fn direct_is_degraded(&self, epoch: u64, now: Instant) -> bool {
        if self.has_healthy_route(epoch, Route::DirectWss) {
            return false;
        }
        let mut total_failures: u32 = 0;
        let mut cooling = false;
        for ((scope, candidate), state) in &self.health {
            if scope.epoch == epoch && candidate.kind == Route::DirectWss {
                total_failures = total_failures.saturating_add(u32::from(state.failures));
                if state.probation || state.cooldown.is_some_and(|until| until > now) {
                    cooling = true;
                }
            }
        }
        cooling && total_failures >= u32::from(self.policy.failures_before_cooldown)
    }

    fn start(
        &mut self,
        scope: Scope,
        candidate: &Candidate,
        now: Instant,
    ) -> Result<(bool, Option<FamilyAttempt>), SkipReason> {
        let family_key = (scope.epoch, candidate.kind);
        let has_healthy = matches!(candidate.kind, Route::CfWss | Route::WorkerWss)
            && self.has_healthy_route(scope.epoch, candidate.kind);
        let attempt = if matches!(candidate.kind, Route::CfWss | Route::WorkerWss) {
            let family = self.family.entry(family_key).or_default();
            if let Some(until) = family.open_until {
                if now < until || family.in_flight != 0 {
                    return Err(SkipReason::FamilyCooling);
                }
                Some(FamilyAttempt::Probe)
            } else if !has_healthy && family.consecutive + family.in_flight >= FAMILY_THRESHOLD {
                // Only gate concurrent dials against the failure budget while no
                // endpoint in this family has proven healthy on the current epoch.
                return Err(SkipReason::FamilyBudget);
            } else {
                Some(FamilyAttempt::Dial)
            }
        } else {
            None
        };
        let route_probe = self.start_route(scope, candidate, now)?;
        if let Some(family) = self.family.get_mut(&family_key) {
            family.in_flight = family.in_flight.saturating_add(1);
        }
        Ok((route_probe, attempt))
    }

    // A normal dial is settled at Upgrade. Only a half-open probe owns the
    // family slot through the first MTProto response. None releases a cancelled
    // or locally skipped attempt without changing its failure history.
    fn finish_family(
        &mut self,
        scope: Scope,
        kind: Route,
        outcome: Option<FailureClass>,
        now: Instant,
    ) {
        let has_healthy = self.has_healthy_route(scope.epoch, kind);
        let Some(family) = self.family.get_mut(&(scope.epoch, kind)) else {
            return;
        };
        family.in_flight -= 1;
        let Some(class) = outcome else { return };
        if class == FailureClass::None {
            family.consecutive = 0;
            family.open_until = None;
            family.last_failure = FailureClass::None;
            return;
        }
        family.last_failure = class;
        // If another endpoint of this family is already proven healthy on this network epoch,
        // cool down only the failing endpoint instead of pausing the entire family and stalling
        // active media/messaging downloads.
        if has_healthy {
            family.consecutive = family.consecutive.saturating_add(1).min(FAMILY_THRESHOLD - 1);
            return;
        }
        family.consecutive = family.consecutive.saturating_add(1).min(12);
        if family.consecutive < FAMILY_THRESHOLD {
            return;
        }
        let steps = u32::from(family.consecutive - FAMILY_THRESHOLD).min(4);
        let delay = FAMILY_BACKOFF.saturating_mul(1u32 << steps);
        let jitter = Duration::from_millis(rand::thread_rng().gen_range(0..=self.policy.jitter_ms));
        family.open_until = Some(now + (delay + jitter).min(Duration::from_secs(64)));
        let failures = family.consecutive;
        let backoff_s = delay.as_secs();
        crate::config::log_warn(&format!(
            "fallback paused kind={kind:?} failures={failures} class={} backoff_s={backoff_s}",
            class.as_str()
        ));
    }

    fn family_diag(&self, epoch: u64, kind: Route, now: Instant) -> serde_json::Value {
        let Some(family) = self.family.get(&(epoch, kind)) else {
            return serde_json::json!({"state": "closed", "consecutiveFailures": 0, "lastFailure": "none"});
        };
        let state = if family.open_until.is_some_and(|until| until > now) {
            "open"
        } else if family.open_until.is_some() {
            "half_open"
        } else {
            "closed"
        };
        serde_json::json!({
            "state": state,
            "consecutiveFailures": family.consecutive,
            "lastFailure": family.last_failure.as_str(),
            "inFlight": family.in_flight,
        })
    }

    fn start_route(&mut self, scope: Scope, candidate: &Candidate, now: Instant) -> Result<bool, SkipReason> {
        if !self.health.contains_key(&(scope, candidate.clone())) && self.health.len() >= MAX_ROUTES
        {
            let oldest = self
                .health
                .iter()
                .filter(|(_, health)| !health.probing)
                .min_by_key(|(_, health)| health.updated)
                .map(|(key, _)| key.clone())
                .ok_or(SkipReason::RouteProbing)?;
            self.health.remove(&oldest);
        }
        let state = self
            .health
            .entry((scope, candidate.clone()))
            .or_insert_with(|| Health::new(now));
        if state.probing {
            return Err(SkipReason::RouteProbing);
        }
        if state.cooldown.is_some_and(|until| until > now) {
            return Err(SkipReason::RouteCooling);
        }
        state.probing = state.probation;
        state.updated = now;
        Ok(state.probing)
    }

    fn record(
        &mut self,
        scope: Scope,
        candidate: &Candidate,
        evidence: Evidence,
        peer: Option<SocketAddr>,
        now: Instant,
    ) -> Option<(PathBuf, Hint)> {
        let state = self.health.get_mut(&(scope, candidate.clone()))?;
        state.updated = now;
        if let Some(peer) = peer {
            if state.endpoints.len() >= 8 && !state.endpoints.contains_key(&peer) {
                state.endpoints.clear();
            }
            state.endpoints.insert(peer, evidence);
        }
        let previous_state = state.state(now);
        match evidence {
            Evidence::Reachable => {}
            Evidence::TransportVerified => state.verified = true,
            Evidence::ProtocolResponse | Evidence::Sustained => {
                state.failures = 0;
                state.cooldown = None;
                state.probation = false;
                state.response = true;
                self.last_payload = Some((scope.epoch, now));
                self.exhausted.remove(&scope);
                if evidence == Evidence::Sustained {
                    state.sustained = state.sustained.saturating_add(1).min(8);
                }
            }
            Evidence::DialFailure | Evidence::PeerFailure | Evidence::ResponseTimeout | Evidence::TransportError(_) => {
                if state.last_failure.is_none_or(|previous| {
                    now.saturating_duration_since(previous) > self.policy.failure_window
                }) && !state.probation
                {
                    state.failures = 0;
                }
                state.last_failure = Some(now);
                state.failures = state.failures.saturating_add(1).min(8);
                if state.failures >= self.policy.failures_before_cooldown || state.probation {
                    let steps = state
                        .failures
                        .saturating_sub(self.policy.failures_before_cooldown)
                        .min(6);
                    let delay = self.policy.initial_backoff.saturating_mul(1 << steps);
                    let jitter = Duration::from_millis(
                        rand::thread_rng().gen_range(0..=self.policy.jitter_ms),
                    );
                    state.cooldown = Some(now + (delay + jitter).min(self.policy.max_backoff));
                    state.probation = true;
                }
            }
        }
        let new_state = state.state(now);
        if new_state != previous_state {
            crate::trace::event(
                "route_health",
                format_args!(
                    "kind={:?} host={} state={new_state:?} evidence={evidence:?}",
                    candidate.kind,
                    crate::trace::allowlisted_host(&candidate.host)
                ),
            );
        }

        if !matches!(evidence, Evidence::ProtocolResponse | Evidence::Sustained) {
            return None;
        }
        // Keep raw TCP in its configured fallback position even after it responds.
        if candidate.kind == Route::Tcp {
            return None;
        }
        let score = state.sustained;
        let current = self.preferred.get_mut(&scope);
        if let Some(preferred) = current {
            if preferred.candidate == *candidate {
                preferred.evidence = now;
            } else {
                let preferred_health = self.health.get(&(scope, preferred.candidate.clone()));
                let was_healthy =
                    preferred_health.is_some_and(|health| health.response && !health.probation);
                let exceeds_margin = evidence == Evidence::Sustained
                    && now.saturating_duration_since(preferred.since) >= self.policy.min_tenure
                    && score
                        >= preferred_health
                            .map_or(0, |health| health.sustained)
                            .saturating_add(self.policy.score_margin);
                if was_healthy && !exceeds_margin {
                    return None;
                }
                *preferred = Preferred {
                    candidate: candidate.clone(),
                    since: now,
                    evidence: now,
                };
                self.record_preference(
                    scope,
                    candidate,
                    if was_healthy {
                        "sustained"
                    } else {
                        "previous_unhealthy"
                    },
                );
            }
        } else {
            self.preferred.insert(
                scope,
                Preferred {
                    candidate: candidate.clone(),
                    since: now,
                    evidence: now,
                },
            );
            self.record_preference(scope, candidate, "first_response");
        }

        let identity = network_identity(scope.epoch);
        if evidence != Evidence::Sustained
            || identity == 0
            || scope.dc.is_test != 0
            || !candidate.cacheable()
        {
            return None;
        }
        let now = unix_time();
        if self.hints.iter().any(|hint| {
            hint.network == identity
                && hint.dc == scope.dc.dc
                && hint.media == (scope.dc.is_media != 0)
                && hint.host == candidate.host
                && hint.valid_until.saturating_sub(now) > CACHE_TTL / 2
        }) {
            return None;
        }
        let hint = Hint {
            network: identity,
            dc: scope.dc.dc,
            media: scope.dc.is_media != 0,
            host: candidate.host.clone(),
            valid_until: now + CACHE_TTL,
        };
        let path = self.path.clone()?;
        self.hints
            .retain(|old| (old.network, old.dc, old.media) != (hint.network, hint.dc, hint.media));
        self.hints.insert(0, hint.clone());
        self.hints.truncate(MAX_HINTS);
        Some((path, hint))
    }

    pub(crate) fn retry_cooled_routes(&mut self, epoch: u64, now: Instant) -> bool {
        if self.next_retry.is_some_and(|(seen, next)| seen == epoch && next > now) {
            return false;
        }
        self.next_retry = Some((epoch, now + Duration::from_secs(15)));
        for ((scope, _), health) in &mut self.health {
            if scope.epoch == epoch && health.probation && !health.probing {
                health.cooldown = Some(now);
            }
        }
        for ((seen, _), family) in &mut self.family {
            if *seen == epoch && family.open_until.is_some() && family.in_flight == 0 {
                family.open_until = Some(now);
            }
        }
        true
    }

    fn record_preference(&mut self, scope: Scope, candidate: &Candidate, reason: &str) {
        if self.changes.len() == 12 {
            self.changes.pop_back();
        }
        let dc = if scope.dc.is_media == 0 {
            scope.dc.dc
        } else {
            -scope.dc.dc
        };
        self.changes.push_front((
            scope.epoch,
            serde_json::json!({
                "dc": dc, "test": scope.dc.is_test != 0, "kind": candidate.kind, "reason": reason,
            }),
        ));
        crate::trace::event(
            "route_preferred",
            format_args!(
                "dc={} media={} test={} kind={:?} host={} reason={reason}",
                scope.dc.dc,
                scope.dc.is_media,
                scope.dc.is_test,
                candidate.kind,
                crate::trace::allowlisted_host(&candidate.host)
            ),
        );
    }

    pub(crate) fn record_exhausted(
        &mut self,
        scope: Scope,
        reason: &'static str,
        candidates: usize,
        attempts: u32,
        now: Instant,
    ) {
        self.exhausted.retain(|key, entry| {
            key.epoch == scope.epoch
                && now.saturating_duration_since(entry.at) <= Duration::from_secs(60)
        });
        if self.exhausted.len() >= 16 && !self.exhausted.contains_key(&scope) {
            if let Some(oldest) = self
                .exhausted
                .iter()
                .min_by_key(|(_, entry)| entry.at)
                .map(|(key, _)| *key)
            {
                self.exhausted.remove(&oldest);
            }
        }
        self.exhausted.insert(
            scope,
            ExhaustedEntry {
                at: now,
                reason,
                candidates,
                attempts,
            },
        );
    }

    pub(crate) fn diagnostics(&self, epoch: u64, now: Instant) -> serde_json::Value {
        let mut routes: Vec<_> = self.preferred.iter()
            .filter(|(scope, pref)| scope.epoch == epoch && now.saturating_duration_since(pref.evidence) <= self.policy.preference_ttl)
            .filter_map(|(scope, preferred)| {
                let health = self.health.get(&(*scope, preferred.candidate.clone()))?;
                Some((scope.dc.dc, scope.dc.is_media != 0, serde_json::json!({
                    "dc": scope.dc.dc, "media": scope.dc.is_media != 0, "test": scope.dc.is_test != 0,
                    "kind": preferred.candidate.kind, "state": format!("{:?}", health.state(now)),
                    "host": crate::trace::allowlisted_host(&preferred.candidate.host),
                    "tlsVerified": health.verified, "endpointCount": health.endpoints.len(),
                })))
            }).collect();
        routes.sort_by_key(|(dc, media, _)| (*dc, *media));
        let mut exhausted: Vec<_> = self
            .exhausted
            .iter()
            .filter(|(scope, entry)| {
                scope.epoch == epoch
                    && now.saturating_duration_since(entry.at) <= Duration::from_secs(60)
            })
            .map(|(scope, entry)| {
                (
                    scope.dc.dc,
                    scope.dc.is_media != 0,
                    serde_json::json!({
                        "dc": scope.dc.dc,
                        "media": scope.dc.is_media != 0,
                        "test": scope.dc.is_test != 0,
                        "reason": entry.reason,
                        "candidates": entry.candidates,
                        "attempts": entry.attempts,
                        "ageSeconds": now.saturating_duration_since(entry.at).as_secs(),
                    }),
                )
            })
            .collect();
        exhausted.sort_by_key(|(dc, media, _)| (*dc, *media));
        let secure_failures: u32 = self
            .health
            .iter()
            .filter(|((scope, candidate), _)| scope.epoch == epoch && candidate.secure)
            .map(|(_, state)| u32::from(state.failures))
            .sum();
        let working_routes = self
            .health
            .iter()
            .filter(|((scope, _), state)| scope.epoch == epoch && state.response && !state.probation)
            .count();
        serde_json::json!({
            "network": crate::trace::NetworkKind::from_epoch(epoch) as u8,
            "generation": crate::trace::NetworkKind::generation(epoch),
            "active": crate::config::STATS.connections_active.load(std::sync::atomic::Ordering::Relaxed),
            "lastPayloadSeconds": self.last_payload.filter(|(seen, _)| *seen == epoch)
                .map(|(_, time)| now.saturating_duration_since(time).as_secs()),
            "lastChange": self.changes.iter().find(|(seen_epoch, _)| *seen_epoch == epoch)
                .map(|(_, entry)| entry),
            "routes": routes.into_iter().take(12).map(|(_, _, entry)| entry).collect::<Vec<_>>(),
            "exhausted": exhausted.into_iter().take(12).map(|(_, _, entry)| entry).collect::<Vec<_>>(),
            "cloudflare": self.family_diag(epoch, Route::CfWss, now),
            "worker": self.family_diag(epoch, Route::WorkerWss, now),
            "directDegraded": self.direct_is_degraded(epoch, now),
            "secureFailures": secure_failures,
            "workingRoutes": working_routes,
            "cfPlainWs80": crate::config::CFPROXY_PLAIN_WS80.load(std::sync::atomic::Ordering::Relaxed),
            "cfEnabled": crate::config::CFPROXY_ENABLED.load(std::sync::atomic::Ordering::Relaxed),
        })
    }
}

pub(crate) struct RouteUse<'a> {
    manager: &'a Mutex<Manager>,
    scope: Scope,
    candidate: &'a Candidate,
    route_probe: bool,
    family: Option<FamilyAttempt>,
    started: Instant,
}

impl<'a> RouteUse<'a> {
    pub(crate) fn start(
        manager: &'a Mutex<Manager>,
        scope: Scope,
        candidate: &'a Candidate,
    ) -> Result<Self, SkipReason> {
        let now = Instant::now();
        let mut routes = manager.lock();
        if scope.epoch != crate::trace::network() {
            return Err(SkipReason::NetworkChanged);
        }
        let (route_probe, family) = routes.start(scope, candidate, now)?;
        if route_probe || family == Some(FamilyAttempt::Probe) {
            crate::trace::event(
                "route_probe",
                format_args!(
                    "kind={:?} host={}",
                    candidate.kind,
                    crate::trace::allowlisted_host(&candidate.host)
                ),
            );
        }
        Ok(Self {
            manager,
            scope,
            candidate,
            route_probe,
            family,
            started: now,
        })
    }

    pub(crate) fn observe_connect_failure(&mut self, class: FailureClass) {
        self.observe_at(Evidence::DialFailure, None, class, Instant::now());
    }

    pub(crate) fn observe(&mut self, evidence: Evidence, peer: Option<SocketAddr>) {
        let class = if evidence == Evidence::ResponseTimeout { FailureClass::Timeout } else { FailureClass::Transport };
        self.observe_at(evidence, peer, class, Instant::now());
    }

    fn observe_at(&mut self, evidence: Evidence, peer: Option<SocketAddr>, class: FailureClass, now: Instant) {
        let mut manager = self.manager.lock();
        if self.scope.epoch != crate::trace::network() {
            return;
        }
        // Per-DC/per-role MTProto TransportError (e.g. -404) proves the underlying
        // transport reached Telegram; it must only degrade this (Scope, Candidate)
        // and must NOT open the cross-DC family circuit breaker.
        if matches!(evidence, Evidence::TransportError(_)) {
            if self.family.take().is_some() {
                manager.finish_family(self.scope, self.candidate.kind, None, now);
            }
        } else {
            let outcome = match (self.family, evidence) {
                (_, Evidence::DialFailure | Evidence::PeerFailure | Evidence::ResponseTimeout) => Some(class),
                (Some(FamilyAttempt::Dial), Evidence::TransportVerified | Evidence::Reachable)
                | (_, Evidence::ProtocolResponse | Evidence::Sustained) => Some(FailureClass::None),
                _ => None,
            };
            if outcome.is_some() && self.family.take().is_some() {
                manager.finish_family(self.scope, self.candidate.kind, outcome, now);
            }
        }
        if matches!(evidence, Evidence::Reachable | Evidence::TransportVerified) {
            if let Some(health) = manager
                .health
                .get_mut(&(self.scope, self.candidate.clone()))
            {
                let ms = now
                    .saturating_duration_since(self.started)
                    .as_millis()
                    .min(u128::from(u32::MAX)) as u32;
                health.latency_ms = Some(ms);
            }
        }
        if let Some((path, hint)) =
            manager.record(self.scope, self.candidate, evidence, peer, now)
        {
            tokio::task::spawn_blocking(move || write_hint(path, hint));
        }
        if matches!(evidence, Evidence::ProtocolResponse | Evidence::Sustained)
            && std::mem::take(&mut self.route_probe)
        {
            if let Some(health) = manager
                .health
                .get_mut(&(self.scope, self.candidate.clone()))
            {
                health.probing = false;
            }
        }
    }
}

impl Drop for RouteUse<'_> {
    fn drop(&mut self) {
        if self.family.is_none() && !self.route_probe {
            return;
        }
        let mut manager = self.manager.lock();
        if self.family.take().is_some() {
            manager.finish_family(self.scope, self.candidate.kind, None, Instant::now());
        }
        if self.route_probe {
            if let Some(health) = manager.health
                .get_mut(&(self.scope, self.candidate.clone()))
            {
                health.probing = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(epoch: u64, media: bool, test: bool) -> Scope {
        Scope {
            epoch,
            dc: DcKey::new(2, media, test),
        }
    }

    fn route(kind: Route, host: &str) -> Candidate {
        Candidate::new(
            kind,
            host.to_owned(),
            crate::proxy::SHARED_DIRECT_WSS_IP.to_owned(),
            true,
        )
    }

    fn manager() -> Manager {
        Manager::with_policy(Policy {
            jitter_ms: 0,
            ..Policy::default()
        })
    }

    #[test]
    fn cloudflare_search_tries_fresh_domains_and_reuses_verified_transport_for_its_dc() {
        let mut routes = manager();
        let now = Instant::now();
        let main = key(9, false, false);
        let media = key(9, true, false);
        let direct = route(Route::DirectWss, "kws2-1.web.telegram.org");
        let failed = route(Route::CfWss, "kws2.failed.example");
        let fresh = route(Route::CfWss, "kws2.fresh.example");
        let verified = route(Route::CfWss, "kws2.verified.example");
        let tcp = route(Route::Tcp, "");
        let original = [direct.clone(), failed.clone(), fresh.clone(), verified.clone(), tcp.clone()];

        let _ = routes.start_route(media, &failed, now);
        routes.record(media, &failed, Evidence::DialFailure, None, now);
        let _ = routes.start_route(main, &verified, now);
        routes.record(main, &verified, Evidence::TransportVerified, None, now);

        let mut ordered = original.clone();
        routes.order(media, &mut ordered, now, true);
        assert_eq!(ordered, [direct, verified, fresh, failed, tcp]);
        assert!(!routes.health.contains_key(&(media, ordered[1].clone())));

        let mut ordered = original.clone();
        routes.order(key(17, true, false), &mut ordered, now, true);
        assert_eq!(ordered, original);
    }

    #[test]
    fn preference_is_scoped_and_a_single_timeout_keeps_it() {
        let mut routes = manager();
        let now = Instant::now();
        let media = key(9, true, false);
        let a = route(Route::DirectWss, "kws2-1.web.telegram.org");
        let b = route(Route::DirectWss, "kws2.web.telegram.org");
        assert_eq!(routes.start(media, &a, now), Ok((false, None)));
        routes.record(media, &a, Evidence::Sustained, None, now);
        routes.record(media, &a, Evidence::DialFailure, None, now);
        let _ = routes.start_route(media, &b, now);
        routes.record(media, &b, Evidence::Sustained, None, now);
        let mut ordered = [b.clone(), a.clone()];
        routes.order(media, &mut ordered, now, true);
        assert_eq!(ordered, [a.clone(), b.clone()]);
        for scope in [
            key(9, false, false),
            key(9, true, true),
            key(17, true, false),
        ] {
            let mut ordered = [b.clone(), a.clone()];
            routes.order(scope, &mut ordered, now, true);
            assert_eq!(ordered, [b.clone(), a.clone()]);
        }
    }

    #[test]
    fn after_repeated_failure_alternate_stays_preferred_and_half_open_is_exclusive() {
        let mut routes = manager();
        let now = Instant::now();
        let media = key(9, true, false);
        let a = route(Route::DirectWss, "kws2-1.web.telegram.org");
        let b = route(Route::DirectWss, "kws2.web.telegram.org");
        assert_eq!(routes.start(media, &a, now), Ok((false, None)));
        routes.record(media, &a, Evidence::ProtocolResponse, None, now);
        for _ in 0..2 {
            routes.record(media, &a, Evidence::DialFailure, None, now);
        }
        assert_eq!(routes.start(media, &a, now), Err(SkipReason::RouteCooling));
        let _ = routes.start_route(media, &b, now);
        routes.record(media, &b, Evidence::ProtocolResponse, None, now);
        let mut candidates = [a.clone(), b.clone()];
        routes.order(media, &mut candidates, now, true);
        assert_eq!(candidates, [b, a.clone()]);
        let after = now + Duration::from_secs(3);
        assert_eq!(routes.start(media, &a, after), Ok((true, None)));
        assert_eq!(routes.start(media, &a, after), Err(SkipReason::RouteProbing));
    }

    #[test]
    fn proven_cloudflare_endpoint_ranks_within_fallback_and_keeps_direct_first() {
        let mut routes = manager();
        let now = Instant::now();
        let scope = key(9, false, false);
        let direct = route(Route::DirectWss, "kws2.web.telegram.org");
        let first_cf = route(Route::CfWss, "first.example");
        let preferred_cf = route(Route::CfWss, "preferred.example");
        let worker = route(Route::WorkerWss, "worker.example");
        let tcp = route(Route::Tcp, "");

        let _ = routes.start_route(scope, &preferred_cf, now);
        routes.record(scope, &preferred_cf, Evidence::ProtocolResponse, None, now);
        let mut candidates = [direct.clone(), worker.clone(), first_cf.clone(), preferred_cf.clone(), tcp.clone()];
        routes.order(scope, &mut candidates, now, true);
        assert_eq!(candidates, [direct, worker, preferred_cf, first_cf, tcp]);
    }

    #[test]
    fn verified_upgrade_and_transport_error_never_promote_an_endpoint() {
        let mut routes = manager();
        let now = Instant::now();
        let media = key(9, true, false);
        let a = route(Route::DirectWss, "kws2-1.web.telegram.org");
        let peer = "127.0.0.1:443".parse().unwrap();
        let _ = routes.start(media, &a, now);
        routes.record(media, &a, Evidence::TransportVerified, Some(peer), now);
        routes.record(media, &a, Evidence::TransportError(-404), Some(peer), now);
        assert!(routes.preferred.get(&media).is_none());
        assert_eq!(
            routes.health[&(media, a.clone())].endpoints[&peer],
            Evidence::TransportError(-404)
        );
        assert_eq!(routes.start(media, &a, now), Ok((false, None)));
        assert!(routes.retry_cooled_routes(media.epoch, now));
        assert!(!routes.retry_cooled_routes(media.epoch, now));
    }

    #[test]
    fn configured_order_and_removed_candidates_remain_authoritative() {
        let mut routes = manager();
        let now = Instant::now();
        let media = key(9, false, false);
        let a = route(Route::DirectWss, "kws2.web.telegram.org");
        let b = route(Route::CfWss, "kws2.example");
        let _ = routes.start_route(media, &b, now);
        routes.record(media, &b, Evidence::ProtocolResponse, None, now);
        let mut candidates = [a.clone(), b.clone()];
        routes.order(media, &mut candidates, now, false);
        assert_eq!(candidates, [a.clone(), b]);
        let mut permitted = [a.clone()];
        routes.order(media, &mut permitted, now, true);
        assert_eq!(permitted, [a]);
    }

    #[test]
    fn a_responding_tcp_fallback_keeps_its_configured_position() {
        let mut routes = manager();
        let now = Instant::now();
        let scope = key(9, false, false);
        let wss = route(Route::DirectWss, "kws2.web.telegram.org");
        let tcp = route(Route::Tcp, "");
        let _ = routes.start(scope, &tcp, now);
        routes.record(scope, &tcp, Evidence::Sustained, None, now);
        let mut candidates = [wss.clone(), tcp.clone()];
        routes.order(scope, &mut candidates, now, true);
        assert_eq!(candidates, [wss, tcp]);
        assert!(!routes.preferred.contains_key(&scope));
    }

    #[test]
    fn hint_file_rejects_expiry_and_unknown_schema() {
        let dir = std::env::temp_dir().join(format!("tgws-hints-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hints.json");
        let now = unix_time();
        let hint = Hint {
            network: 91,
            dc: 2,
            media: true,
            host: "kws2-1.web.telegram.org".into(),
            valid_until: now + CACHE_TTL,
        };
        write_hint(path.clone(), hint);
        assert_eq!(read_hints(&path, now).len(), 1);
        assert!(read_hints(&path, now + CACHE_TTL).is_empty());
        std::fs::write(&path, br#"{"version":99,"hints":[]}"#).unwrap();
        assert!(read_hints(&path, now).is_empty());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    fn open_family(routes: &mut Manager, scope: Scope, now: Instant) -> Candidate {
        let candidate = route(Route::CfWss, "fallback.example");
        for _ in 0..FAMILY_THRESHOLD {
            assert_eq!(routes.start(scope, &candidate, now), Ok((false, Some(FamilyAttempt::Dial))));
        }
        assert_eq!(routes.start(scope, &candidate, now), Err(SkipReason::FamilyBudget));
        for _ in 0..FAMILY_THRESHOLD {
            routes.finish_family(scope, candidate.kind, Some(FailureClass::Timeout), now);
        }
        candidate
    }

    #[test]
    fn parallel_dials_share_the_failure_budget_and_keep_other_routes_available() {
        let mut routes = manager();
        let now = Instant::now();
        let dc2 = key(9, false, false);
        let dc5 = Scope { dc: DcKey::new(5, false, false), ..dc2 };
        let cf = open_family(&mut routes, dc2, now);
        assert_eq!(routes.start(dc5, &cf, now), Err(SkipReason::FamilyCooling));
        let direct = route(Route::DirectWss, "kws5.web.telegram.org");
        assert_eq!(routes.start(dc5, &direct, now), Ok((false, None)));
        let worker = route(Route::WorkerWss, "worker.example");
        assert_eq!(routes.start(dc5, &worker, now), Ok((false, Some(FamilyAttempt::Dial))));
        let new_epoch = Scope { epoch: 17, ..dc5 };
        routes.order(new_epoch, &mut [], now, true);
        // Completion from the previous network no longer has an entry.
        routes.finish_family(dc5, cf.kind, Some(FailureClass::Tls), now);
        assert_eq!(routes.start(new_epoch, &cf, now), Ok((false, Some(FamilyAttempt::Dial))));
        assert_eq!(routes.diagnostics(17, now)["cloudflare"]["state"], "closed");
    }

    #[test]
    fn repeated_half_open_failures_back_off_with_an_exclusive_probe() {
        let mut routes = manager();
        let mut now = Instant::now();
        let dc2 = key(9, false, false);
        let dc5 = Scope { dc: DcKey::new(5, false, false), ..dc2 };
        let cf = open_family(&mut routes, dc2, now);
        for delay in [4, 8, 16, 32, 64, 64] {
            assert_eq!(routes.start(dc2, &cf, now + Duration::from_secs(delay - 1)), Err(SkipReason::FamilyCooling));
            now += Duration::from_secs(delay);
            assert_eq!(routes.start(dc2, &cf, now), Ok((false, Some(FamilyAttempt::Probe))));
            assert_eq!(routes.start(dc5, &cf, now), Err(SkipReason::FamilyCooling));
            routes.finish_family(dc2, cf.kind, Some(FailureClass::Timeout), now);
        }
    }

    #[test]
    fn manual_retry_keeps_probe_ownership_and_cancelled_probe_releases_it() {
        let now = Instant::now();
        let scope = key(9, false, false);
        let mut routes = manager();
        let cf = open_family(&mut routes, scope, now);
        let after = now + Duration::from_secs(4);
        let (route_probe, family) = routes.start(scope, &cf, after).unwrap();
        assert_eq!(family, Some(FamilyAttempt::Probe));
        let routes = Mutex::new(routes);
        let probe = RouteUse { manager: &routes, scope, candidate: &cf, route_probe, family, started: after };
        assert!(routes.lock().retry_cooled_routes(scope.epoch, after));
        assert_eq!(routes.lock().start(scope, &cf, after), Err(SkipReason::FamilyCooling));
        drop(probe);
        assert_eq!(routes.lock().start(scope, &cf, after), Ok((false, Some(FamilyAttempt::Probe))));
    }

    #[test]
    fn simultaneous_half_open_requests_admit_one_probe_across_dcs() {
        use std::sync::{atomic::{AtomicUsize, Ordering}, Barrier};

        let now = Instant::now();
        let scope = key(9, false, false);
        let mut routes = manager();
        let cf = open_family(&mut routes, scope, now);
        let routes = Mutex::new(routes);
        let barrier = Barrier::new(8);
        let admitted = AtomicUsize::new(0);
        std::thread::scope(|threads| {
            for worker in 0..8 {
                let (routes, cf, barrier, admitted) = (&routes, &cf, &barrier, &admitted);
                threads.spawn(move || {
                    let scope = Scope { dc: DcKey::new(worker % 5 + 1, false, false), ..scope };
                    barrier.wait();
                    let attempt = routes.lock().start(scope, cf, now + Duration::from_secs(4));
                    let permit = attempt.ok().map(|(route_probe, family)| {
                        admitted.fetch_add(1, Ordering::Relaxed);
                        RouteUse { manager: routes, scope, candidate: cf, route_probe, family, started: now }
                    });
                    barrier.wait();
                    drop(permit);
                });
            }
        });
        assert_eq!(admitted.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn live_probe_keeps_ownership_until_protocol_response_or_timeout() {
        let now = Instant::now();
        let scope = key(crate::trace::network(), false, false);
        let mut routes = manager();
        let cf = open_family(&mut routes, scope, now);
        let routes = Mutex::new(routes);
        let attempt = |now| {
            let (route_probe, family) = routes.lock().start(scope, &cf, now).unwrap();
            RouteUse { manager: &routes, scope, candidate: &cf, route_probe, family, started: now }
        };
        let after = now + Duration::from_secs(4);
        let mut probe = attempt(after);
        probe.observe_at(Evidence::TransportVerified, None, FailureClass::None, after);
        assert_eq!(routes.lock().start(scope, &cf, after), Err(SkipReason::FamilyCooling));
        assert!(routes.lock().preferred.get(&scope).is_none());
        probe.observe_at(Evidence::ResponseTimeout, None, FailureClass::Timeout, after);
        assert_eq!(routes.lock().diagnostics(scope.epoch, after)["cloudflare"]["state"], "open");
        drop(probe);

        let retried = after + Duration::from_secs(8);
        let mut probe = attempt(retried);
        probe.observe_at(Evidence::ProtocolResponse, None, FailureClass::None, retried);
        assert_eq!(routes.lock().diagnostics(scope.epoch, retried)["cloudflare"]["state"], "closed");
        assert_eq!(routes.lock().preferred.get(&scope).unwrap().candidate, cf);
    }

    #[test]
    fn plain_ws80_cf_domain_with_protocol_response_is_prioritized_on_sibling_role() {
        let now = Instant::now();
        let main = key(crate::trace::network(), false, false);
        let media = key(crate::trace::network(), true, false);
        let mut routes = manager();
        let working = Candidate::new(
            Route::CfWss,
            "kws2.working.example".into(),
            "working.example".into(),
            false,
        );
        let untested = Candidate::new(
            Route::CfWss,
            "kws2.untested.example".into(),
            "untested.example".into(),
            false,
        );

        let _ = routes.start(main, &working, now).unwrap();
        routes.record(main, &working, Evidence::Reachable, None, now);
        routes.record(main, &working, Evidence::ProtocolResponse, None, now);

        let mut candidates = vec![untested.clone(), working.clone()];
        routes.order(media, &mut candidates, now + Duration::from_secs(1), true);
        assert_eq!(candidates[0], working);
        assert_eq!(
            routes.diagnostics(main.epoch, now)["routes"][0]["tlsVerified"],
            false
        );
    }

    #[test]
    fn cellular_degraded_direct_promotes_healthy_ws80_for_main_and_media_without_affecting_wifi() {
        let now = Instant::now();
        let cell_epoch = (1u64 << 3) | 2; // cellular
        let wifi_epoch = (2u64 << 3) | 1; // wifi
        let cell_main = key(cell_epoch, false, false);
        let cell_media = key(cell_epoch, true, false);
        let cell_dc4_media = Scope { epoch: cell_epoch, dc: DcKey::new(4, true, false) };
        let wifi_main = key(wifi_epoch, false, false);
        let mut routes = manager();

        let direct = route(Route::DirectWss, "kws2.web.telegram.org");
        let ws80 = Candidate::new(Route::CfWss, "kws2.good.example".into(), "good.example".into(), false);
        let ws80_dc4 = Candidate::new(Route::CfWss, "kws4.good.example".into(), "good.example".into(), false);
        let ws80_other = Candidate::new(Route::CfWss, "kws4.other.example".into(), "other.example".into(), false);

        // On cellular: DirectWss fails repeatedly and enters cooldown.
        let _ = routes.start(cell_main, &direct, now).unwrap();
        routes.record(cell_main, &direct, Evidence::DialFailure, None, now);
        routes.record(cell_main, &direct, Evidence::DialFailure, None, now);

        // Plaintext WS/80 succeeds on cellular main DC2.
        let _ = routes.start(cell_main, &ws80, now).unwrap();
        routes.record(cell_main, &ws80, Evidence::Reachable, None, now);
        routes.record(cell_main, &ws80, Evidence::ProtocolResponse, None, now);

        // Main DC2 promotes WS/80 ahead of failing DirectWss in automatic mode.
        let mut main_candidates = vec![direct.clone(), ws80.clone()];
        routes.order(cell_main, &mut main_candidates, now, true);
        assert_eq!(main_candidates[0], ws80);

        // Media DC2 and DC4 also promote the proven WS/80 domain ahead of failing DirectWss.
        let mut media_candidates = vec![direct.clone(), ws80_other.clone(), ws80_dc4.clone()];
        routes.order(cell_dc4_media, &mut media_candidates, now, true);
        assert_eq!(media_candidates[0], ws80_dc4);

        // Manual mode keeps configured order intact.
        let mut manual = vec![direct.clone(), ws80.clone()];
        routes.order(cell_media, &mut manual, now, false);
        assert_eq!(manual[0], direct);

        // Switching to Wi-Fi (new epoch) starts clean and keeps DirectWss first.
        let mut wifi_candidates = vec![direct.clone(), ws80.clone()];
        routes.order(wifi_main, &mut wifi_candidates, now + Duration::from_secs(1), true);
        assert_eq!(wifi_candidates[0], direct);
    }

    #[test]
    fn failing_secondary_cf_domain_does_not_pause_family_while_healthy_ws80_exists() {
        let now = Instant::now();
        let scope = key((1u64 << 3) | 2, false, false);
        let media = key((1u64 << 3) | 2, true, false);
        let mut routes = manager();
        let good = Candidate::new(Route::CfWss, "kws2.good.example".into(), "good.example".into(), false);
        let bad = Candidate::new(Route::CfWss, "kws2.bad.example".into(), "bad.example".into(), false);

        // Establish a healthy WS/80 route.
        let (_, _) = routes.start(scope, &good, now).unwrap();
        routes.finish_family(scope, Route::CfWss, Some(FailureClass::None), now);
        routes.record(scope, &good, Evidence::ProtocolResponse, None, now);

        // Repeated failures on another CF domain must not pause the CfWss family
        // or block concurrent dials for other DCs/media roles.
        for _ in 0..6 {
            let _ = routes.start(media, &bad, now);
            routes.finish_family(media, Route::CfWss, Some(FailureClass::Timeout), now);
        }
        assert_eq!(routes.diagnostics(scope.epoch, now)["cloudflare"]["state"], "closed");
        let dc4_media = Scope { epoch: scope.epoch, dc: DcKey::new(4, true, false) };
        let good_dc4 = Candidate::new(Route::CfWss, "kws4.good.example".into(), "good.example".into(), false);
        // Two concurrent in-flight dials must both be admitted while healthy CF exists.
        assert!(routes.start(media, &good, now).is_ok());
        assert!(routes.start(dc4_media, &good_dc4, now).is_ok());
    }

    #[test]
    fn mtproto_transport_error_on_one_dc_never_opens_family_breaker_for_other_dcs() {
        let now = Instant::now();
        let epoch = crate::trace::network();
        let dc1 = Scope { epoch, dc: DcKey::new(1, false, false) };
        let dc2 = Scope { epoch, dc: DcKey::new(2, false, false) };
        let cf_dc1 = Candidate::new(Route::CfWss, "kws1.cf.example".into(), "cf.example".into(), true);
        let cf_dc2 = Candidate::new(Route::CfWss, "kws2.cf.example".into(), "cf.example".into(), true);
        let routes = Mutex::new(manager());

        for _ in 0..6 {
            if let Ok(mut ru) = RouteUse::start(&routes, dc1, &cf_dc1) {
                ru.observe_at(Evidence::TransportError(-404), None, FailureClass::Transport, now);
            }
        }
        assert_eq!(routes.lock().diagnostics(epoch, now)["cloudflare"]["state"], "closed");
        assert!(RouteUse::start(&routes, dc2, &cf_dc2).is_ok());
    }

    #[test]
    fn diagnostics_reports_healthy_dc2_alongside_exhausted_dc1() {
        let now = Instant::now();
        let epoch = (3u64 << 3) | 1;
        let dc1 = Scope { epoch, dc: DcKey::new(1, false, false) };
        let dc2 = Scope { epoch, dc: DcKey::new(2, false, false) };
        let direct_dc2 = route(Route::DirectWss, "kws2.web.telegram.org");
        let mut routes = manager();

        let _ = routes.start(dc2, &direct_dc2, now).unwrap();
        routes.record(dc2, &direct_dc2, Evidence::ProtocolResponse, None, now);
        routes.record_exhausted(dc1, "family_cooling", 2, 0, now);

        let diag = routes.diagnostics(epoch, now);
        assert_eq!(diag["workingRoutes"], 1);
        assert_eq!(diag["routes"][0]["dc"], 2);
        assert_eq!(diag["exhausted"][0]["dc"], 1);
        assert_eq!(diag["exhausted"][0]["reason"], "family_cooling");
        assert_eq!(diag["exhausted"][0]["attempts"], 0);
    }

    #[test]
    fn diagnostics_exposes_secure_failures_and_working_routes_for_cellular_suggestion() {
        let now = Instant::now();
        let epoch = (1u64 << 3) | 2;
        let scope = key(epoch, false, false);
        let mut routes = manager();
        let direct = route(Route::DirectWss, "kws2.web.telegram.org");
        let cf_wss = route(Route::CfWss, "kws2.cf.example");

        let _ = routes.start(scope, &direct, now).unwrap();
        routes.record(scope, &direct, Evidence::DialFailure, None, now);
        routes.record(scope, &direct, Evidence::DialFailure, None, now);
        let _ = routes.start(scope, &cf_wss, now).unwrap();
        routes.record(scope, &cf_wss, Evidence::DialFailure, None, now);

        let diag = routes.diagnostics(epoch, now);
        assert_eq!(diag["network"], 2);
        assert_eq!(diag["secureFailures"], 3);
        assert_eq!(diag["workingRoutes"], 0);
        assert_eq!(diag["directDegraded"], true);
    }
}
