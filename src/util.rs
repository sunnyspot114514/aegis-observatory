use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const SITE_VERSION: &str = "1";
pub const SCHEMA_VERSION: u32 = 1;
/// Written into events.condition_version. Restore must compare this field, not only file hashes.
pub const CONDITION_VERSION: &str = "baseline";
pub const MAX_MESSAGES: i64 = 1000;
pub const MAX_SIM_EVENTS: i64 = 1000;
pub const MAX_EVENTS: i64 = 2000;
pub const MAX_BODY_BYTES: usize = 1024;
pub const MAX_REQUEST_BYTES: usize = 2048;
/// Form urlencoding can take 3 bytes per decoded byte (`%XX`) plus field names.
pub const MAX_FORM_REQUEST_BYTES: usize = 4096;
pub const MAX_URL_BYTES: usize = 1024;
pub const MAX_HEADERS: usize = 32;
pub const MAX_HEADER_BYTES: usize = 8192;
pub const MAX_CONNECTIONS: usize = 32;
pub const MAX_INFLIGHT: usize = 8;
/// POST handlers (including body read). Leaves inflight room for GET/HEAD.
pub const MAX_WRITE_INFLIGHT: usize = 4;
pub const DB_QUEUE: usize = 16;
pub const PAGE_LIMIT_MIN: i64 = 1;
pub const PAGE_LIMIT: i64 = 20;
pub const DB_PAGE_SIZE: i64 = 4096;
pub const DB_MAX_PAGES: i64 = 2048; // 8 MiB
pub const DATA_DIR_BUDGET: u64 = 16 * 1024 * 1024;
pub const MIN_FREE_BYTES: u64 = 128 * 1024 * 1024;
pub const WRITE_RATE: f64 = 6.0 / 60.0;
pub const WRITE_BURST: f64 = 3.0;
pub const REQUEST_RATE: f64 = 6.0 / 60.0;
pub const REQUEST_BURST: f64 = 8.0;
pub const REQUEST_TIMEOUT_SECS: u64 = 5;
pub const BODY_TIMEOUT_SECS: u64 = 5;
pub const LOG_CAP_BYTES: u64 = 1024 * 1024;
pub const ARCHIVE_HINT: f64 = 0.70;
pub const ARCHIVE_URGENT: f64 = 0.85;
pub const ORIGIN_SLOTS: usize = 64;
pub const ORIGIN_TTL_SECS: u64 = 120;
pub const ORIGIN_WRITE_RATE: f64 = 2.0 / 60.0;
pub const ORIGIN_WRITE_BURST: f64 = 2.0;

pub fn pressure_level(ratio: f64) -> &'static str {
    if ratio >= 1.0 {
        "full"
    } else if ratio >= ARCHIVE_URGENT {
        "urgent"
    } else if ratio >= ARCHIVE_HINT {
        "hint"
    } else {
        "ok"
    }
}

pub fn worst_pressure(levels: &[&str]) -> &'static str {
    if levels.iter().any(|s| *s == "full") {
        "full"
    } else if levels.iter().any(|s| *s == "urgent") {
        "urgent"
    } else if levels.iter().any(|s| *s == "hint") {
        "hint"
    } else {
        "ok"
    }
}

/// Token bucket. HTTP request limiter counts *attempts*.
/// The DB-thread write limiter counts *successful new inserts* (refund on insert fail).
pub struct Bucket {
    tokens: f64,
    last: Instant,
    rate: f64,
    burst: f64,
}

impl Bucket {
    pub fn new(rate: f64, burst: f64) -> Self {
        Self {
            tokens: burst,
            last: Instant::now(),
            rate,
            burst,
        }
    }
    /// None = token taken. Some(secs) = wait.
    pub fn take(&mut self) -> Option<u64> {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + dt * self.rate).min(self.burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            None
        } else {
            Some(((1.0 - self.tokens) / self.rate).ceil() as u64)
        }
    }
    pub fn give_back(&mut self) {
        self.tokens = (self.tokens + 1.0).min(self.burst);
    }
}

pub fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

pub fn skip_disk_requested() -> bool {
    std::env::var("AEGIS_SKIP_DISK_CHECK").ok().as_deref() == Some("1")
}

pub fn dev_mode() -> bool {
    std::env::var("AEGIS_DEV").ok().as_deref() == Some("1")
}

pub fn skip_disk_allowed() -> bool {
    skip_disk_requested() && dev_mode()
}

pub fn unlimited_writes() -> bool {
    #[cfg(debug_assertions)]
    {
        std::env::var("AEGIS_UNLIMITED_WRITES").ok().as_deref() == Some("1")
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

pub fn log_rate_off() -> bool {
    #[cfg(debug_assertions)]
    {
        std::env::var("AEGIS_LOG_RATE_OFF").ok().as_deref() == Some("1")
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

pub fn disk_query_force_fail() -> bool {
    #[cfg(debug_assertions)]
    {
        std::env::var("AEGIS_DISK_QUERY").ok().as_deref() == Some("fail")
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

pub fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_rfc3339(secs)
}

fn format_rfc3339(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let tod = secs % 86400;
    let hour = tod / 3600;
    let min = (tod % 3600) / 60;
    let sec = tod % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c if c.is_control() && c != '\n' && c != '\t' => {
                out.push_str(&format!("&#{};", u32::from(c)));
            }
            _ => out.push(c),
        }
    }
    out
}

pub fn body_has_illegal_controls(s: &str) -> bool {
    s.chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
}

pub fn valid_request_id(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=64).contains(&b.len())
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-' | b':'))
}

pub fn dir_size(path: &Path) -> u64 {
    fn walk(p: &Path, acc: &mut u64) {
        let Ok(rd) = std::fs::read_dir(p) else {
            return;
        };
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                walk(&path, acc);
            } else if let Ok(m) = e.metadata() {
                *acc = acc.saturating_add(m.len());
            }
        }
    }
    let mut n = 0u64;
    walk(path, &mut n);
    n
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskQueryFailed;

/// Fail-closed: `Err` means the caller must refuse writes.
/// `AEGIS_DISK_QUERY=fail` is a deny switch (never a skip).
pub fn available_bytes(path: &Path) -> Result<u64, DiskQueryFailed> {
    if disk_query_force_fail() {
        return Err(DiskQueryFailed);
    }
    let dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().unwrap_or(path).to_path_buf()
    };
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        extern "system" {
            fn GetDiskFreeSpaceExW(
                lp_directory_name: *const u16,
                lp_free_bytes_available_to_caller: *mut u64,
                lp_total_number_of_bytes: *mut u64,
                lp_total_number_of_free_bytes: *mut u64,
            ) -> i32;
        }
        let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut free = 0u64;
        let ok = unsafe {
            GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut())
        };
        if ok != 0 {
            Ok(free)
        } else {
            Err(DiskQueryFailed)
        }
    }
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c = CString::new(dir.as_os_str().as_bytes()).map_err(|_| DiskQueryFailed)?;
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(c.as_ptr(), &mut s) };
        if rc == 0 {
            let fr = s.f_frsize as u64;
            let avail = s.f_bavail as u64;
            Ok(avail.saturating_mul(fr))
        } else {
            Err(DiskQueryFailed)
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = dir;
        Err(DiskQueryFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_bytes_on_temp_succeeds() {
        std::env::remove_var("AEGIS_DISK_QUERY");
        let p = std::env::temp_dir();
        let n = available_bytes(&p).expect("temp dir disk query must succeed");
        assert!(n > 0);
    }
}
