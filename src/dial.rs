use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, SemaphorePermit};

const SLOT_WAIT: Duration = Duration::from_millis(750);
const ADDRESS_FALLBACK_DELAY: Duration = Duration::from_millis(250);
static TOTAL: Semaphore = Semaphore::const_new(10);
static PRECONNECT_TOTAL: Semaphore = Semaphore::const_new(2);
static HOSTS: Lazy<Mutex<HashMap<(bool, String), Weak<Semaphore>>>> = Lazy::new(Mutex::default);

type DialPermit = (OwnedSemaphorePermit, SemaphorePermit<'static>);

fn host_slots(host: &str, limit: usize, background: bool) -> Arc<Semaphore> {
    let mut hosts = HOSTS.lock();
    hosts.retain(|_, slots| slots.strong_count() != 0);
    let key = (background, host.to_owned());
    if let Some(slots) = hosts.get(&key).and_then(Weak::upgrade) {
        return slots;
    }
    let slots = Arc::new(Semaphore::new(limit));
    hosts.insert(key, Arc::downgrade(&slots));
    slots
}

pub async fn acquire(host: &str, limit: usize) -> io::Result<DialPermit> {
    let host_slots = host_slots(host, limit, false);
    tokio::time::timeout(SLOT_WAIT, async {
        let host_permit = host_slots
            .acquire_owned()
            .await
            .expect("host slots remain open");
        let total_permit = TOTAL.acquire().await.expect("dial slots remain open");
        (host_permit, total_permit)
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "dial budget busy"))
}

pub fn try_preconnect(host: &str) -> Option<DialPermit> {
    let host_permit = host_slots(host, 1, true).try_acquire_owned().ok()?;
    let total_permit = PRECONNECT_TOTAL.try_acquire().ok()?;
    Some((host_permit, total_permit))
}

pub fn try_cf_domain(host: &str) -> Option<OwnedSemaphorePermit> {
    host_slots(host, 1, false).try_acquire_owned().ok()
}

pub(crate) async fn connect_addresses(addresses: &[SocketAddr]) -> io::Result<TcpStream> {
    let [preferred, rest @ ..] = addresses else {
        return Err(io::ErrorKind::AddrNotAvailable.into());
    };
    if rest.is_empty() {
        return TcpStream::connect(preferred).await;
    }

    let mut fallback = rest.to_vec();
    if let Some(other_family) = fallback
        .iter()
        .position(|addr| addr.is_ipv4() != preferred.is_ipv4())
    {
        fallback[..=other_family].rotate_right(1);
    }
    first_connected(
        TcpStream::connect(preferred),
        TcpStream::connect(fallback.as_slice()),
    )
    .await
}

async fn first_connected<T>(
    preferred: impl Future<Output = io::Result<T>>,
    fallback: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    tokio::pin!(preferred);
    tokio::select! {
        result = &mut preferred => return match result {
            Ok(socket) => Ok(socket),
            Err(_) => fallback.await,
        },
        _ = tokio::time::sleep(ADDRESS_FALLBACK_DELAY) => {}
    }
    tokio::pin!(fallback);
    tokio::select! {
        result = &mut preferred => match result {
            Ok(socket) => Ok(socket),
            Err(_) => fallback.await,
        },
        result = &mut fallback => match result {
            Ok(socket) => Ok(socket),
            Err(_) => preferred.await,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn host_dials_are_bounded_and_a_dropped_attempt_releases_its_slot() {
        let slot = acquire("one-slot.example", 1).await.unwrap();
        assert!(acquire("one-slot.example", 1).await.is_err());
        assert!(acquire("other.example", 1).await.is_ok());
        drop(slot);
        assert!(acquire("one-slot.example", 1).await.is_ok());
    }

    #[tokio::test]
    async fn preconnect_cannot_take_the_interactive_host_slot() {
        let _background = try_preconnect("shared.example").unwrap();
        assert!(try_preconnect("shared.example").is_none());
        assert!(acquire("shared.example", 1).await.is_ok());
    }

    #[tokio::test]
    async fn unresponsive_first_address_does_not_exhaust_the_dial_budget() {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            first_connected(std::future::pending(), std::future::ready(Ok(2))),
        )
        .await;
        assert_eq!(result.unwrap().unwrap(), 2);
    }
}
