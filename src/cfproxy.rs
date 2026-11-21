use crate::config::*;
use crate::ws::{ws_connect_once, RawWebSocket, WsError};
use crate::{ldebug, linfo, lwarn};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

use once_cell::sync::Lazy;
static CFPROXY_SEM: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(CFPROXY_GLOBAL_PARALLEL));

// ---------------------------------------------------------------------------
// Domain decoding
// ---------------------------------------------------------------------------

pub fn decode_cf_domain(s: &str) -> String {
    if !s.ends_with(".com") {
        return s.to_string();
    }
    let suffix = ".co.uk";
    let p = &s[..s.len() - 4];
    let mut n = 0i32;
    for c in p.chars() {
        if (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') {
            n += 1;
        }
    }
    let mut result: Vec<u8> = Vec::new();
    for &c in p.as_bytes() {
        if c >= b'a' && c <= b'z' {
            let v = (((c - b'a') as i32 - n % 26 + 26) % 26) as u8 + b'a';
            result.push(v);
        } else if c >= b'A' && c <= b'Z' {
            let v = (((c - b'A') as i32 - n % 26 + 26) % 26) as u8 + b'A';
            result.push(v);
        } else {
            result.push(c);
        }
    }
    let mut out = String::from_utf8_lossy(&result).to_string();
    out.push_str(suffix);
    out
}

pub fn normalize_cf_domain(s: &str) -> String {
    let mut decoded = decode_cf_domain(s.trim()).trim().to_lowercase();
    while decoded.ends_with('.') {
        decoded.pop();
    }
    if decoded.is_empty() || !decoded.ends_with(".co.uk") {
        return String::new();
    }
    decoded
}

pub fn default_cfproxy_domains() -> Vec<String> {
    let mut domains = Vec::with_capacity(CFPROXY_ENC.len());
    for enc in CFPROXY_ENC {
        let d = normalize_cf_domain(enc);
        if !d.is_empty() {
            domains.push(d);
        }
    }
    domains
}

pub fn merge_cfproxy_domains(lists: &[Vec<String>]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut merged = Vec::new();
    for list in lists {
        for raw in list {
            let d = normalize_cf_domain(raw);
            if d.is_empty() || seen.contains(&d) {
                continue;
            }
            // Дополнительно валидируем как в _normalize_domain_pool оригинала.
            if !crate::config::is_valid_domain(&d) {
                continue;
            }
            seen.insert(d.clone());
            merged.push(d);
        }
    }
    merged
}

// ---------------------------------------------------------------------------
// 429 cooldown logic
// ---------------------------------------------------------------------------

pub fn clear_cfproxy_429_cooldowns() {
    CFPROXY_429.write().clear();
}

pub fn prune_cfproxy_429_cooldowns(epoch: u64) {
    CFPROXY_429.write().retain(|(network, _), _| *network >= epoch);
}

pub fn clear_cfproxy_429_cooldown(epoch: u64, domain: &str) {
    let d = normalize_cf_domain(domain);
    if d.is_empty() {
        return;
    }
    CFPROXY_429.write().remove(&(epoch, d));
}

pub fn retry_after_delay(err: &WsError) -> Duration {
    let h = match err.handshake() {
        Some(h) => h,
        None => return Duration::ZERO,
    };
    let retry_after = h.headers.get("retry-after").map(|s| s.trim()).unwrap_or("");
    if retry_after.is_empty() {
        return Duration::ZERO;
    }
    if let Ok(seconds) = retry_after.parse::<i64>() {
        if seconds > 0 {
            return Duration::from_secs(seconds as u64);
        }
    }
    // http date parse (best-effort): пропускаем, как маловероятный кейс
    Duration::ZERO
}

pub fn next_cfproxy_429_cooldown_delay(prev: &Cfproxy429State, retry_after: Duration) -> Duration {
    if retry_after > Duration::ZERO {
        if retry_after > CFPROXY_429_MAX_COOLDOWN {
            return CFPROXY_429_MAX_COOLDOWN;
        }
        return retry_after;
    }
    let mut strikes = prev.strikes;
    let expired = match prev.until {
        None => true,
        Some(u) => u.elapsed() > CFPROXY_429_MAX_COOLDOWN,
    };
    if expired {
        strikes = 0;
    }
    let mut delay = CFPROXY_429_COOLDOWN;
    for _ in 0..strikes {
        delay *= 2;
        if delay >= CFPROXY_429_MAX_COOLDOWN {
            return CFPROXY_429_MAX_COOLDOWN;
        }
    }
    if delay > CFPROXY_429_MAX_COOLDOWN {
        return CFPROXY_429_MAX_COOLDOWN;
    }
    delay
}

pub fn mark_cfproxy_429_cooldown(epoch: u64, domain: &str, err: &WsError) {
    let d = normalize_cf_domain(domain);
    if d.is_empty() {
        return;
    }
    let retry_after = retry_after_delay(err);
    let mut map = CFPROXY_429.write();
    if epoch < crate::trace::network() {
        return;
    }
    let key = (epoch, d);
    let prev = map.get(&key).cloned().unwrap_or_default();
    let delay = next_cfproxy_429_cooldown_delay(&prev, retry_after);
    let mut strikes = prev.strikes + 1;
    let expired = match prev.until {
        None => true,
        Some(u) => u.elapsed() > CFPROXY_429_MAX_COOLDOWN,
    };
    if expired {
        strikes = 1;
    }
    map.insert(
        key,
        Cfproxy429State {
            until: Some(Instant::now() + delay),
            strikes,
        },
    );
    drop(map);
    linfo!(
        "stage=cf_http_rate_limit epoch={} retry_ms={}",
        epoch >> 3,
        delay.as_millis()
    );
}

pub fn cfproxy_429_cooldown_remaining(epoch: u64, domain: &str) -> Duration {
    let d = normalize_cf_domain(domain);
    if d.is_empty() {
        return Duration::ZERO;
    }
    let key = (epoch, d);
    let mut map = CFPROXY_429.write();
    let Some(until) = map.get(&key).and_then(|state| state.until) else {
        return Duration::ZERO;
    };
    let now = Instant::now();
    if until <= now {
        map.remove(&key);
        return Duration::ZERO;
    }
    until - now
}

pub fn acquire_cfproxy_attempt_slot() -> Option<tokio::sync::SemaphorePermit<'static>> {
    CFPROXY_SEM.try_acquire().ok()
}

// ---------------------------------------------------------------------------
// Cache files
// ---------------------------------------------------------------------------

fn cfproxy_cache_path() -> Option<PathBuf> {
    let dir = CFPROXY.read().cache_dir.trim().to_string();
    if dir.is_empty() {
        return None;
    }
    Some(PathBuf::from(dir).join(CFPROXY_CACHE_FILE_NAME))
}

// Активный домен больше не сохраняется в отдельный файл. Балансер работает в памяти.

fn load_cfproxy_domains_from_cache() -> Vec<String> {
    let path = match cfproxy_cache_path() {
        Some(p) => p,
        None => return Vec::new(),
    };
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let list: Vec<String> = data.split('\n').map(|s| s.to_string()).collect();
    merge_cfproxy_domains(&[list])
}



fn save_cfproxy_domains_to_cache(domains: &[String]) {
    let path = match cfproxy_cache_path() {
        Some(p) => p,
        None => return,
    };
    if domains.is_empty() {
        return;
    }
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            ldebug!(" CF: кеш создать не удалось: {}", e);
            return;
        }
    }
    let data = domains.join("\n");
    if let Err(e) = std::fs::write(&path, data) {
        ldebug!(" CF: кеш сохранить не удалось: {}", e);
    }
}



fn should_refresh_cfproxy_domains() -> bool {
    let path = match cfproxy_cache_path() {
        Some(p) => p,
        None => return true,
    };
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(_) => return true,
    };
    let modified = match meta.modified() {
        Ok(t) => t,
        Err(_) => return true,
    };
    match modified.elapsed() {
        Ok(elapsed) => elapsed >= CFPROXY_REFRESH_INTERVAL,
        Err(_) => true,
    }
}



pub fn configured_user_domains(raw: &str) -> Vec<String> {
    let mut domains = normalize_domain_pool(&coerce_domain_list_str(raw));
    // Leave space for the longest per-DC label, kws203.
    domains.retain(|domain| domain.len() <= 246);
    domains
}

pub fn init_cfproxy_domains() {
    let defaults = default_cfproxy_domains();
    let cached = load_cfproxy_domains_from_cache();

    let mut cfg = CFPROXY.write();
    let user = configured_user_domains(&cfg.user_domain);
    if !user.is_empty() {
        cfg.user_domain = user.join(",");
        cfg.domains = user.clone();
        crate::balancer::BALANCER.write().update_domains_list(&cfg.domains);
        return;
    }
    if !cfg.user_domain.trim().is_empty() {
        lwarn!(" CF: пользовательские домены не прошли проверку, используется общий список");
        cfg.user_domain.clear();
    }

    if !cached.is_empty() {
        let n = cached.len();
        cfg.domains = merge_cfproxy_domains(&[cached, defaults]);
        crate::balancer::BALANCER.write().update_domains_list(&cfg.domains);
        drop(cfg);
        linfo!(" CF: кеш доменов загружен ({} шт.)", n);
    } else {
        cfg.domains = defaults;
        crate::balancer::BALANCER.write().update_domains_list(&cfg.domains);
    }
}

pub fn start_cfproxy_refresh(cancel: tokio_util::sync::CancellationToken) {
    // Порт proxy/config.py::start_cfproxy_domain_refresh: сначала пробуем
    // обновить сразу (до 3 попыток), затем раз в час фоновым таском.
    // На мобиле уважаем свежесть дискового кеша для первой попытки.
    if !should_refresh_cfproxy_domains() {
        ldebug!(" CF: кеш свежий, откладываю обновление списка на час");
    }
    tokio::spawn(async move {
        tokio::select! {
            _ = cancel.cancelled() => {}
            _ = async {
        if should_refresh_cfproxy_domains() {
            for _ in 0..3 {
                if try_refresh_cfproxy_domains().await {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            if CFPROXY.read().domains.is_empty() {
                ldebug!(" CF: обновить список доменов не удалось, остаюсь на кеше/встроенном списке");
            }
        }
        loop {
            tokio::time::sleep(CFPROXY_REFRESH_INTERVAL).await;
            try_refresh_cfproxy_domains().await;
        }
            } => {}
        }
    });
}

pub async fn try_refresh_cfproxy_domains() -> bool {
    let has_user = !CFPROXY.read().user_domain.is_empty();
    if has_user {
        return true;
    }

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    // Cache-buster как в оригинале ("?" + 7 случайных букв), UA tg-ws-proxy.
    let nonce: String = {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        (0..7)
            .map(|_| rng.gen_range(b'a'..=b'z') as char)
            .collect()
    };
    let url = format!("{}?{}", CFPROXY_DOMAINS_URL, nonce);
    let resp = match client
        .get(&url)
        .header("User-Agent", "tg-ws-proxy")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            ldebug!(" CF: GitHub недоступен: {}", e);
            return false;
        }
    };
    if resp.status().as_u16() != 200 {
        ldebug!(" CF: GitHub вернул {}", resp.status().as_u16());
        return false;
    }
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            ldebug!(" CF: список доменов прочитать не удалось: {}", e);
            return false;
        }
    };

    let mut fetched_raw = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        fetched_raw.push(line.to_string());
    }

    // Валидация пула как в оригинале: минимум CFPROXY_MIN_VALID_DOMAINS
    // валидных доменов, иначе игнорируем payload и остаёмся на текущем пуле.
    let decoded: Vec<String> = fetched_raw.iter().map(|d| normalize_cf_domain(d)).collect();
    let pool = normalize_domain_pool(&decoded);
    if pool.len() >= CFPROXY_MIN_VALID_DOMAINS {
        let merged = merge_cfproxy_domains(&[pool.clone(), default_cfproxy_domains()]);
        {
            let mut cfg = CFPROXY.write();
            if !cfg.user_domain.is_empty() {
                return true;
            }
            cfg.domains = merged.clone();
        }
        crate::balancer::BALANCER.write().update_domains_list(&merged);
        save_cfproxy_domains_to_cache(&merged);
        linfo!(" CF: список доменов обновлен ({} шт.)", pool.len());
        return true;
    }

    if !fetched_raw.is_empty() {
        lwarn!(
            " CF: игнорирую список доменов (total={}, valid={}, need>={}); остаюсь на текущем пуле",
            fetched_raw.len(),
            pool.len(),
            CFPROXY_MIN_VALID_DOMAINS
        );
    } else {
        ldebug!(" CF: обновить список доменов не удалось (пустой ответ), остаюсь на кеше/встроенном списке");
    }
    false
}

// ---------------------------------------------------------------------------
// DNS over HTTPS (DoH) resolve
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct DohAnswer {
    #[serde(rename = "data")]
    data: String,
    #[serde(rename = "type")]
    type_: i32,
}
#[derive(Deserialize)]
struct DohResponse {
    #[serde(rename = "Answer", default)]
    answer: Vec<DohAnswer>,
}

static DOH_CACHE: Lazy<parking_lot::RwLock<std::collections::HashMap<(u64, String), (String, Instant)>>> =
    Lazy::new(|| parking_lot::RwLock::new(std::collections::HashMap::new()));

pub(crate) fn public_fallback_ip(ip: std::net::IpAddr) -> bool {
    use std::net::{IpAddr, Ipv4Addr};
    match ip {
        IpAddr::V4(ip) => {
            let [first, second, ..] = ip.octets();
            first != 0
                && first < 224
                && !(first == 100 && (64..128).contains(&second))
                && !(first == 198 && matches!(second, 18 | 19))
                && !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_documentation()
        }
        IpAddr::V6(ip) => {
            let [a, b, c, d, e, f, g, h] = ip.segments();
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return public_fallback_ip(mapped.into());
            }
            if [a, b, c, d, e, f] == [0x64, 0xff9b, 0, 0, 0, 0] {
                return public_fallback_ip(Ipv4Addr::from((u32::from(g) << 16) | u32::from(h)).into());
            }
            a & 0xe000 == 0x2000 && [a, b] != [0x2001, 0xdb8]
        }
    }
}

fn pick_preferred_ip(candidates: &[String]) -> String {
    let mut fallback_v6 = String::new();
    for c in candidates {
        let c = c.trim();
        if let Ok(ip) = c.parse::<std::net::IpAddr>() {
            if !public_fallback_ip(ip) {
                continue;
            }
            match ip {
                std::net::IpAddr::V4(v4) => return v4.to_string(),
                std::net::IpAddr::V6(v6) => {
                    if fallback_v6.is_empty() {
                        fallback_v6 = v6.to_string();
                    }
                }
            }
        }
    }
    fallback_v6
}

pub async fn resolve_doh(domain: &str) -> Option<String> {
    let epoch = crate::trace::network();
    let key = (epoch, domain.to_owned());
    if let Some((ip, exp)) = DOH_CACHE.read().get(&key).cloned() {
        if Instant::now() < exp {
            return Some(ip);
        }
    }

    let endpoints = [
        "https://cloudflare-dns.com/dns-query",
        "https://dns.google/dns-query",
        "https://dns.quad9.net/dns-query",
        "https://dns.adguard-dns.com/dns-query",
    ];

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()
        .ok()?;

    let (tx, mut rx) = tokio::sync::mpsc::channel(endpoints.len() + 1);
    let mut tasks = tokio::task::JoinSet::new();

    for u in endpoints {
        let client = client.clone();
        let domain = domain.to_string();
        let tx = tx.clone();
        tasks.spawn(async move {
            let full = format!("{}?name={}&type=A", u, domain);
            if let Ok(resp) = client
                .get(&full)
                .header("Accept", "application/dns-json")
                .send()
                .await
            {
                if resp.status().as_u16() == 200 {
                    if let Ok(r) = resp.json::<DohResponse>().await {
                        for ans in r.answer {
                            if ans.type_ == 1 && ans.data.parse::<std::net::IpAddr>().is_ok_and(public_fallback_ip) {
                                let _ = tx.send(Some(ans.data)).await;
                                return;
                            }
                        }
                    }
                }
            }
            let _ = tx.send(None).await;
        });
    }

    // UDP-резолв через системный resolver как дополнительный кандидат
    {
        let domain2 = domain.to_string();
        let tx = tx.clone();
        tasks.spawn(async move {
            let host = format!("{}:443", domain2);
            if let Ok(Ok(addrs)) = tokio::time::timeout(
                Duration::from_millis(1500),
                tokio::net::lookup_host(host),
            )
            .await
            {
                let ips: Vec<String> = addrs.map(|a| a.ip().to_string()).collect();
                let p = pick_preferred_ip(&ips);
                if !p.is_empty() {
                    let _ = tx.send(Some(p)).await;
                    return;
                }
            }
            let _ = tx.send(None).await;
        });
    }

    drop(tx); // Чтобы rx.recv() завершился, когда все таски завершатся

    let deadline = tokio::time::sleep(Duration::from_millis(1500));
    tokio::pin!(deadline);

    let mut final_ip = None;
    loop {
        tokio::select! {
            _ = &mut deadline => {
                break;
            }
            msg = rx.recv() => {
                match msg {
                    Some(Some(ip)) => {
                        final_ip = Some(ip);
                        break;
                    }
                    Some(None) => {} // Таска ничего не нашла
                    None => break,   // Все таски завершились
                }
            }
        }
    }

    // Отменяем все незавершенные фоновые таски (исправление утечки)
    tasks.shutdown().await;

    let mut cache = DOH_CACHE.write();
    if epoch != crate::trace::network() {
        return None;
    }
    if let Some(ip) = &final_ip {
        let now = Instant::now();
        cache.retain(|(seen, _), (_, expires)| *seen == epoch && *expires > now);
        if cache.len() < 128 {
            cache.insert(key, (ip.clone(), now + Duration::from_secs(300)));
        }
    }
    final_ip
}

// ---------------------------------------------------------------------------
// cfConnectDomain
// ---------------------------------------------------------------------------

pub async fn cf_connect_domain(domain: &str, path: &str, secure: bool) -> Result<RawWebSocket, WsError> {
    let timeout = CFPROXY_DIAL_PHASE_TIMEOUT;
    match ws_connect_once(domain, domain, path, timeout, secure).await {
        Err(error @ WsError::Dns(_)) => {
            let Some(ip) = resolve_doh(domain).await else { return Err(error) };
            ws_connect_once(&ip, domain, path, timeout, secure).await
        }
        result => result,
    }
}

// активный домен set/save
pub fn set_active_domain_and_save(_chosen: &str) {
    // Больше не используется для файлов. Балансер обновляется внутри proxy.rs
}
#[cfg(test)]
mod rate_limit_tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn fallback_dns_rejects_local_addresses_including_embedded_ipv4() {
        for address in ["127.0.0.1", "10.0.0.1", "100.64.0.1", "169.254.169.254", "::1", "fd00::1",
            "::ffff:127.0.0.1", "64:ff9b::a00:1"] {
            assert!(!public_fallback_ip(address.parse().unwrap()), "{address}");
        }
        for address in ["1.1.1.1", "2606:4700::1111", "64:ff9b::101:101"] {
            assert!(public_fallback_ip(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn user_domains_keep_each_valid_name_and_drop_the_rest() {
        assert_eq!(
            configured_user_domains("A.Example.com, bad, A.Example.com; second.example"),
            vec!["a.example.com".to_string(), "second.example".to_string()]
        );
        assert!(configured_user_domains("not a domain").is_empty());
        assert!(configured_user_domains("a.example.com, b.example.com").len() == 2);
    }

    #[test]
    fn late_cloudflare_status_cannot_poison_a_new_network() {
        let old = 0x111;
        let current = 0x119;
        let domain = "rate-limit-test.example.co.uk";
        let limited = WsError::Handshake(crate::ws::WsHandshakeError {
            status_code: 429,
            status_line: "HTTP/1.1 429 Too Many Requests".to_owned(),
            headers: HashMap::from([("retry-after".to_owned(), "15".to_owned())]),
            location: String::new(),
        });
        mark_cfproxy_429_cooldown(old, domain, &limited);
        assert!(cfproxy_429_cooldown_remaining(old, domain) > Duration::ZERO);
        assert_eq!(cfproxy_429_cooldown_remaining(current, domain), Duration::ZERO);
        mark_cfproxy_429_cooldown(current, domain, &limited);
        clear_cfproxy_429_cooldown(old, domain);
        assert!(cfproxy_429_cooldown_remaining(current, domain) > Duration::ZERO);
        clear_cfproxy_429_cooldown(current, domain);
    }
}
