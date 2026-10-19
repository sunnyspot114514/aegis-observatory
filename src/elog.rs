use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::util::{env_usize, log_rate_off, LOG_CAP_BYTES};

pub struct ErrorLog {
    path: PathBuf,
    written: AtomicU64,
    dropped: AtomicU64,
    last: Mutex<Instant>,
    fourxx: AtomicU64,
    last_4xx: Mutex<Option<Instant>>,
}

static LOG: OnceLock<ErrorLog> = OnceLock::new();

impl ErrorLog {
    pub fn init(path: PathBuf) {
        let written = path.metadata().map(|m| m.len()).unwrap_or(0);
        let _ = LOG.set(Self {
            path,
            written: AtomicU64::new(written),
            dropped: AtomicU64::new(0),
            last: Mutex::new(Instant::now() - Duration::from_secs(1)),
            fourxx: AtomicU64::new(0),
            last_4xx: Mutex::new(None),
        });
    }

    fn emit(&self, msg: &str) {
        // Never include request bodies, tokens, or raw headers.
        let line = format!("{msg}\n");
        let cap = env_usize("AEGIS_LOG_CAP_BYTES", LOG_CAP_BYTES as usize) as u64;
        if self.written.load(Ordering::Relaxed) >= cap {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let min_gap = if log_rate_off() {
            Duration::ZERO
        } else {
            Duration::from_millis(100)
        };
        if let Ok(mut last) = self.last.lock() {
            if last.elapsed() < min_gap {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            *last = Instant::now();
        }
        let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.path) else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if f.write_all(line.as_bytes()).is_ok() {
            self.written.fetch_add(line.len() as u64, Ordering::Relaxed);
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub fn elog(msg: &str) {
    if let Some(log) = LOG.get() {
        log.emit(msg);
    }
}

pub fn note_4xx(code: u16, error: &str) {
    let Some(log) = LOG.get() else {
        return;
    };
    let n = log.fourxx.fetch_add(1, Ordering::Relaxed) + 1;
    let emit_now = log_rate_off()
        || log
            .last_4xx
            .lock()
            .map(|mut last| {
                let now = Instant::now();
                let ok = last
                    .map(|t| now.duration_since(t) >= Duration::from_secs(1))
                    .unwrap_or(true);
                if ok {
                    *last = Some(now);
                }
                ok
            })
            .unwrap_or(true);
    if emit_now {
        elog(&format!("http_4xx n={n} last={code}/{error}"));
    } else {
        log.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn dropped() -> u64 {
    LOG.get().map(|l| l.dropped.load(Ordering::Relaxed)).unwrap_or(0)
}

pub fn written() -> u64 {
    LOG.get().map(|l| l.written.load(Ordering::Relaxed)).unwrap_or(0)
}

pub fn fourxx() -> u64 {
    LOG.get().map(|l| l.fourxx.load(Ordering::Relaxed)).unwrap_or(0)
}

pub fn cap_exhausted() -> bool {
    let cap = env_usize("AEGIS_LOG_CAP_BYTES", LOG_CAP_BYTES as usize) as u64;
    written() >= cap
}
