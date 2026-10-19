use std::io::{Error, ErrorKind};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::elog::elog;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptClass {
    Retry,
    Resource,
    Fatal,
}

pub fn classify_accept(err: &Error) -> AcceptClass {
    match err.kind() {
        ErrorKind::Interrupted | ErrorKind::WouldBlock | ErrorKind::TimedOut => {
            AcceptClass::Retry
        }
        ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset => AcceptClass::Retry,
        ErrorKind::OutOfMemory => AcceptClass::Resource,
        _ => classify_os(err),
    }
}

fn classify_os(err: &Error) -> AcceptClass {
    let Some(code) = err.raw_os_error() else {
        return AcceptClass::Retry;
    };
    #[cfg(windows)]
    {
        // WSAEMFILE / WSAENOBUFS / WSAEWOULDBLOCK
        match code {
            10024 | 10055 => AcceptClass::Resource,
            10035 | 10004 | 10053 | 10054 => AcceptClass::Retry,
            10038 | 10022 | 10047 => AcceptClass::Fatal, // notsock / inval / afnosupport
            _ => AcceptClass::Retry,
        }
    }
    #[cfg(unix)]
    {
        match code {
            libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM => AcceptClass::Resource,
            libc::EBADF | libc::EINVAL => AcceptClass::Fatal,
            _ => AcceptClass::Retry,
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = code;
        AcceptClass::Retry
    }
}

pub fn resource_backoff(streak: u32) -> Duration {
    let ms = 10u64.saturating_mul(1u64 << streak.min(6));
    Duration::from_millis(ms.min(500))
}

static LAST_RESOURCE_LOG: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
static RESOURCE_DROPPED: AtomicU64 = AtomicU64::new(0);

pub fn log_accept_resource() {
    RESOURCE_DROPPED.fetch_add(1, Ordering::Relaxed);
    let mut last = LAST_RESOURCE_LOG.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    if last.map(|t| now.duration_since(t) >= Duration::from_secs(1)).unwrap_or(true) {
        *last = Some(now);
        elog("accept_resource");
    }
}

#[cfg(test)]
pub fn accept_resource_dropped() -> u64 {
    RESOURCE_DROPPED.load(Ordering::Relaxed)
}

pub struct ConnSlot(Arc<AtomicUsize>);

impl ConnSlot {
    pub fn try_acquire(counter: &Arc<AtomicUsize>, max: usize) -> Option<Self> {
        let prev = counter.fetch_add(1, Ordering::SeqCst);
        if prev >= max {
            counter.fetch_sub(1, Ordering::SeqCst);
            None
        } else {
            Some(Self(counter.clone()))
        }
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_is_retry() {
        let e = Error::from(ErrorKind::Interrupted);
        assert_eq!(classify_accept(&e), AcceptClass::Retry);
    }

    #[test]
    fn oom_is_resource() {
        let e = Error::from(ErrorKind::OutOfMemory);
        assert_eq!(classify_accept(&e), AcceptClass::Resource);
    }

    #[test]
    fn backoff_is_capped() {
        assert_eq!(resource_backoff(0), Duration::from_millis(10));
        assert_eq!(resource_backoff(20), Duration::from_millis(500));
    }

    #[test]
    fn conn_slot_releases() {
        let c = Arc::new(AtomicUsize::new(0));
        {
            let a = ConnSlot::try_acquire(&c, 1).unwrap();
            assert!(ConnSlot::try_acquire(&c, 1).is_none());
            drop(a);
        }
        assert_eq!(c.load(Ordering::SeqCst), 0);
        assert!(ConnSlot::try_acquire(&c, 1).is_some());
    }

    #[test]
    fn resource_counter_increments_without_requiring_elog() {
        let before = accept_resource_dropped();
        for _ in 0..20 {
            log_accept_resource();
        }
        assert!(accept_resource_dropped() >= before + 20);
    }

    #[test]
    fn conn_slot_releases_when_unpolled_future_is_dropped() {
        let c = Arc::new(AtomicUsize::new(0));
        let slot = ConnSlot::try_acquire(&c, 1).unwrap();
        let fut = async move {
            let _slot = slot;
            std::future::pending::<()>().await;
        };
        drop(fut);
        assert_eq!(c.load(Ordering::SeqCst), 0);
        assert!(ConnSlot::try_acquire(&c, 1).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn linux_emfile_enfile_are_resource() {
        assert_eq!(
            classify_accept(&Error::from_raw_os_error(libc::EMFILE)),
            AcceptClass::Resource
        );
        assert_eq!(
            classify_accept(&Error::from_raw_os_error(libc::ENFILE)),
            AcceptClass::Resource
        );
        assert_eq!(
            classify_accept(&Error::from_raw_os_error(libc::ENOBUFS)),
            AcceptClass::Resource
        );
    }

    #[cfg(unix)]
    #[test]
    fn linux_ebadf_einval_are_fatal() {
        assert_eq!(
            classify_accept(&Error::from_raw_os_error(libc::EBADF)),
            AcceptClass::Fatal
        );
        assert_eq!(
            classify_accept(&Error::from_raw_os_error(libc::EINVAL)),
            AcceptClass::Fatal
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_emfile_is_resource() {
        let e = Error::from_raw_os_error(10024);
        assert_eq!(classify_accept(&e), AcceptClass::Resource);
    }

    #[cfg(windows)]
    #[test]
    fn windows_notsock_is_fatal() {
        let e = Error::from_raw_os_error(10038);
        assert_eq!(classify_accept(&e), AcceptClass::Fatal);
    }
}
