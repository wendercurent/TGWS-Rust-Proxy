pub mod cfproxy;
pub mod config;
pub mod crypto;
mod dial;
mod trace;
pub mod faketls;
pub mod proxy;
mod routing;
pub mod ws;
#[cfg(test)]
mod tls_hello;
pub mod balancer;

use config::*;
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use proxy::{apply_default_dc_redirects, parse_cidr_pool, run_proxy, ProxyPools};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

// Глобальный рантайм — никогда не дропается
static RUNTIME: OnceCell<Runtime> = OnceCell::new();

struct ProxyState {
    pools: Arc<ProxyPools>,
    handle: tokio::task::JoinHandle<()>,
    cancel_tasks: CancellationToken,
}

static STATE: OnceCell<Mutex<Option<ProxyState>>> = OnceCell::new();

fn state_cell() -> &'static Mutex<Option<ProxyState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        // Многопоточный рантайм, кол-во воркеров адекватно мобиле
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("tgwsproxy-rt")
            .enable_all()
            .build()
            .expect("failed to build global tokio runtime")
    })
}

fn cstr_to_string(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p).to_string_lossy().into_owned() }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    #[test]
    fn stop_releases_listener_and_pending_init_for_repeated_rebinds() {
        let addr = TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap();
        let host = CString::new("127.0.0.1").unwrap();
        let secret = CString::new("42424242424242424242424242424242").unwrap();
        let pool_size = POOL_SIZE.swap(0, Ordering::Relaxed);
        let cf_enabled = CFPROXY_ENABLED.swap(false, Ordering::Relaxed);
        struct Reset {
            pool_size: i32,
            cf_enabled: bool,
        }
        impl Drop for Reset {
            fn drop(&mut self) {
                StopProxy();
                POOL_SIZE.store(self.pool_size, Ordering::Relaxed);
                CFPROXY_ENABLED.store(self.cf_enabled, Ordering::Relaxed);
            }
        }
        let _reset = Reset { pool_size, cf_enabled };

        for _ in 0..3 {
            let result = unsafe {
                StartProxy(host.as_ptr(), i32::from(addr.port()), std::ptr::null(), secret.as_ptr(), 0)
            };
            assert_eq!(result, 0);
            let mut socket = TcpStream::connect(addr).unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            socket.write_all(&[0x42]).unwrap();
            assert_eq!(StopProxy(), 0);
            let mut byte = [0];
            match socket.read(&mut byte) {
                Ok(0) => {}
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
                result => panic!("pending init was not closed after stop: {result:?}"),
            }
        }
    }

    #[test]
    fn start_proxy_rejects_invalid_secret_without_binding_or_overwriting() {
        let addr = TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap();
        let host = CString::new("127.0.0.1").unwrap();
        let previous = PROXY_SECRET.read().clone();
        *PROXY_SECRET.write() = "11111111111111111111111111111111".to_string();

        for bad in ["", "short", "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz", "4242424242424242424242424242424200"] {
            let secret = CString::new(bad).unwrap();
            let result = unsafe {
                StartProxy(host.as_ptr(), i32::from(addr.port()), std::ptr::null(), secret.as_ptr(), 0)
            };
            assert_eq!(result, -5, "expected invalid secret rejection for len={}", bad.len());
            assert_eq!(*PROXY_SECRET.read(), "11111111111111111111111111111111");
            assert!(
                TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_err(),
                "listener must not bind when secret is invalid"
            );
        }

        *PROXY_SECRET.write() = previous;
    }
}

// ---------------------------------------------------------------------------
// Exports
// ---------------------------------------------------------------------------

/// # Safety
/// Указатели должны быть валидными C-строками (или null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn StartProxy(
    c_host: *const c_char,
    port: c_int,
    c_dc_ips: *const c_char,
    c_secret: *const c_char,
    verbose: c_int,
) -> c_int {
    let cell = state_cell();
    let mut guard = cell.lock();

    if guard.is_some() {
        return -1;
    }

    let host = cstr_to_string(c_host);
    let Ok(go_port) = u16::try_from(port) else { return -3 };
    let dc_ips_str = cstr_to_string(c_dc_ips);
    let secret_str = cstr_to_string(c_secret);
    let is_verbose = verbose != 0;

    let valid_secret = secret_str.len() == 32 && hex::decode(&secret_str).is_ok();
    if !valid_secret {
        return -5;
    }

    init_logging(is_verbose);
    cfproxy::clear_cfproxy_429_cooldowns();

    *PROXY_SECRET.write() = secret_str;

    cfproxy::init_cfproxy_domains();

    let dc_opt_map = apply_default_dc_redirects(parse_cidr_pool(&dc_ips_str));

    let rt = runtime();
    let cancel_tasks = CancellationToken::new();
    let pools = Arc::new(ProxyPools::new(cancel_tasks.clone()));

    // Канал готовности: ждём успешного bind перед возвратом
    let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<()>>();

    let pools_task = pools.clone();
    let host_task = host.clone();
    let map_task = dc_opt_map.clone();
    let cancel_root = cancel_tasks.clone();

    let handle = rt.spawn(async move {
        // Предварительный bind для сигнала готовности
        let addr = format!("{}:{}", host_task, go_port);
        match tokio::net::TcpListener::bind((host_task.as_str(), go_port)).await {
            Ok(listener) => {
                if tx.send(Ok(())).is_err() {
                    return;
                }
                if let Err(e) =
                    run_proxy(pools_task, host_task, go_port, map_task, cancel_root, listener).await
                {
                    lerror!("listen on {}: {}", addr, e);
                }
            }
            Err(e) => {
                let _ = tx.send(Err(e));
            }
        }
    });

    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::AddrInUse => return -4,
        _ => {
            cancel_tasks.cancel();
            handle.abort();
            return -3;
        }
    }

    *guard = Some(ProxyState {
        pools,
        handle,
        cancel_tasks,
    });

    0
}

#[unsafe(no_mangle)]
pub extern "C" fn NotifyDefaultNetwork(kind: c_int) {
    NotifyDefaultNetworkWithIdentity(kind, 0);
}

#[unsafe(no_mangle)]
pub extern "C" fn NotifyDefaultNetworkWithIdentity(kind: c_int, identity: i64) {
    NotifyDefaultNetworkState(kind, identity, 0);
}

#[unsafe(no_mangle)]
pub extern "C" fn NotifyDefaultNetworkState(kind: c_int, identity: i64, replaced: c_int) {
    let epoch = trace::change_network(kind.clamp(0, 4) as u8, identity as u64, replaced != 0);
    cfproxy::prune_cfproxy_429_cooldowns(epoch);
}

#[unsafe(no_mangle)]
pub extern "C" fn SetStickyRouting(enabled: c_int) {
    AUTO_STICKY_ROUTING.store(enabled != 0, Ordering::Relaxed);
}

#[unsafe(no_mangle)]
pub extern "C" fn RetryFailedRoutes() -> c_int {
    let guard = state_cell().lock();
    i32::from(guard.as_ref().is_some_and(|state| state.pools.routes.lock()
        .retry_cooled_routes(trace::network(), std::time::Instant::now())))
}

#[unsafe(no_mangle)]
pub extern "C" fn GetRouteDiagnostics() -> *mut c_char {
    let guard = state_cell().lock();
    let Some(state) = guard.as_ref() else { return std::ptr::null_mut() };
    let data = state.pools.routes.lock().diagnostics(trace::network(), std::time::Instant::now());
    CString::new(data.to_string()).map_or(std::ptr::null_mut(), CString::into_raw)
}

#[unsafe(no_mangle)]
pub extern "C" fn GetProxyStatus() -> c_int {
    if state_cell().lock().as_ref().is_none_or(|state| state.handle.is_finished()) {
        return 0;
    }
    trace::current_status(STATS.connections_validated_active.load(Ordering::Relaxed) > 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn StopProxy() -> c_int {
    let cell = state_cell();
    let mut guard = cell.lock();

    let state = match guard.take() {
        Some(s) => s,
        None => return -1,
    };

    // graceful shutdown — НЕ дропаем рантайм
    linfo!("StopProxy: cancelling all tasks");
    state.cancel_tasks.cancel();

    let rt = runtime();
    let pools = state.pools.clone();
    let mut handle = state.handle;
    rt.block_on(async move {
        linfo!("StopProxy: waiting for proxy tasks to finish (max 2s)");
        if tokio::time::timeout(std::time::Duration::from_secs(2), &mut handle).await.is_err() {
            handle.abort();
            let _ = handle.await;
        }
        linfo!("StopProxy: closing pool connections");
        pools.close_all().await;
        pools.reset().await;
        linfo!("StopProxy: done");
    });

    STATS.reset();
    trace::reset_status();
    cfproxy::clear_cfproxy_429_cooldowns();

    linfo!("StopProxy: exit");
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn SetPoolSize(size: c_int) {
    // Оригинал: pool_size = max(0, N); 0 отключает преконнект пула.
    POOL_SIZE.store(size.clamp(0, MAX_POOL_SIZE), Ordering::Relaxed);
}

#[unsafe(no_mangle)]
pub extern "C" fn SetBufferSizeKb(kb: c_int) {
    // proxy --buf-kb: размер SO_RCVBUF/SO_SNDBUF, минимум 4KB.
    let mut n = kb;
    if n < 4 {
        n = 4;
    }
    if n > 4096 {
        n = 4096;
    }
    BUFFER_SIZE.store(n * 1024, Ordering::Relaxed);
    RECV_BUF.store(n * 1024, Ordering::Relaxed);
    SEND_BUF.store(n * 1024, Ordering::Relaxed);
}

/// # Safety
/// `c_cache_dir` — валидная C-строка или null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SetCfProxyCacheDir(c_cache_dir: *const c_char) {
    let dir = cstr_to_string(c_cache_dir);
    CFPROXY.write().cache_dir = dir.trim().to_string();
}

/// # Safety
/// Path must be a valid C string or null. Android supplies its private files directory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SetRouteCacheDir(path: *const c_char) {
    let path = cstr_to_string(path);
    if !path.is_empty() {
        routing::set_cache_dir(path.into());
    }
}

/// # Safety
/// `c_user_domain` — валидная C-строка или null (можно списком через , ; пробел).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SetCfProxyConfig(
    enabled: c_int,
    _priority: c_int,
    c_user_domain: *const c_char,
) {
    CFPROXY_ENABLED.store(enabled != 0, Ordering::Relaxed);
    let user_domain = cstr_to_string(c_user_domain);
    // Несколько доменов, как --cfproxy-domain. Невалидные имена отбрасываются,
    // чтобы одна ошибочная строка не подменяла весь пул.
    let list = cfproxy::configured_user_domains(&user_domain);
    let mut cfg = CFPROXY.write();
    cfg.user_domain = list.join(",");
    if !list.is_empty() {
        cfg.domains = list.clone();
        cfg.active = list[0].clone();
        crate::balancer::BALANCER
            .write()
            .update_domains_list(&cfg.domains);
    }
}

/// Controls whether new sessions may use raw MTProto TCP fallback.
#[unsafe(no_mangle)]
pub extern "C" fn SetTcpFallbackEnabled(enabled: c_int) {
    TCP_FALLBACK_ENABLED.store(enabled != 0, Ordering::Relaxed);
}

/// # Safety
/// `c_domains` — валидная C-строка или null (--cfproxy-worker-domain, repeatable).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SetCfWorkerDomains(c_domains: *const c_char) {
    let raw = cstr_to_string(c_domains);
    *CF_WORKER_DOMAINS.write() = normalize_domain_pool(&coerce_domain_list_str(&raw));
}

/// # Safety
/// `c_domain` — валидная C-строка или null (--fake-tls-domain, ee-secret).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SetFakeTlsDomain(c_domain: *const c_char) {
    let d = cstr_to_string(c_domain);
    *FAKE_TLS_DOMAIN.write() = d.trim().to_string();
}

#[unsafe(no_mangle)]
pub extern "C" fn SetDisableSecure(v: c_int) {
    // --no-secure: порт 80 для CF proxy/worker.
    DISABLE_SECURE.store(v != 0, Ordering::Relaxed);
}

/// Cloudflare-only plaintext WebSocket on port 80. Direct WSS is unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn SetCfPlainWs80(enabled: c_int) {
    CFPROXY_PLAIN_WS80.store(enabled != 0, Ordering::Relaxed);
    if enabled != 0 {
        log_warn("CF WS/80 enabled: plaintext, no TLS, no server authentication");
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn SetForceTestDc(v: c_int) {
    // --force-test-dc: весь трафик на тестовые DC.
    FORCE_TEST_DC.store(v != 0, Ordering::Relaxed);
}

#[unsafe(no_mangle)]
pub extern "C" fn SetProxyProtocol(v: c_int) {
    // --proxy-protocol: принимать PROXY protocol v1.
    PROXY_PROTOCOL.store(v != 0, Ordering::Relaxed);
}

/// # Safety
/// `c_secret` — валидная C-строка или null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SetSecret(c_secret: *const c_char) {
    let s = cstr_to_string(c_secret);
    if s.len() != 32 {
        return;
    }
    if hex::decode(&s).is_err() {
        return;
    }
    *PROXY_SECRET.write() = s;
}

#[unsafe(no_mangle)]
pub extern "C" fn GetStats() -> *mut c_char {
    let s = STATS.summary();
    CString::new(s).unwrap_or_default().into_raw()
}

#[unsafe(no_mangle)]
pub extern "C" fn GetSecretWithPrefix() -> *mut c_char {
    // ee-secret при включённом FakeTLS: ee{secret}{domain_hex} — как в оригинале.
    let sec = PROXY_SECRET.read().clone();
    let ftls = FAKE_TLS_DOMAIN.read().clone();
    let full = if ftls.is_empty() {
        format!("dd{}", sec)
    } else {
        format!("ee{}{}", sec, ftls.as_bytes().iter().map(|b| format!("{:02x}", b)).collect::<String>())
    };
    CString::new(full).unwrap_or_default().into_raw()
}

/// # Safety
/// `p` должен быть указателем, ранее возвращённым из этой библиотеки.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn FreeString(p: *mut c_char) {
    if p.is_null() {
        return;
    }
    unsafe {
        let _ = CString::from_raw(p);
    }
}
