//! Bounded per-origin write fairness. Global caps stay in charge.
//! Trust CF-Connecting-IP only when the TCP peer is loopback (the tunnel).
//! Never honor client-supplied X-Forwarded-For.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use hyper::HeaderMap;

use crate::util::{Bucket, ORIGIN_SLOTS, ORIGIN_TTL_SECS, ORIGIN_WRITE_BURST, ORIGIN_WRITE_RATE};

pub struct OriginTable {
    slots: HashMap<IpAddr, Slot>,
}

struct Slot {
    bucket: Bucket,
    last: Instant,
}

impl OriginTable {
    pub fn new() -> Self {
        Self {
            slots: HashMap::with_capacity(ORIGIN_SLOTS),
        }
    }

    fn evict_expired(&mut self, now: Instant) {
        let ttl = Duration::from_secs(ORIGIN_TTL_SECS);
        self.slots.retain(|_, s| now.duration_since(s.last) < ttl);
    }

    /// None = no per-origin wait. Some(secs) = this origin should wait.
    /// If the table is full of live keys, skip per-origin (global limiter still applies).
    pub fn take_write(&mut self, ip: IpAddr) -> Option<u64> {
        let now = Instant::now();
        self.evict_expired(now);
        if let Some(s) = self.slots.get_mut(&ip) {
            s.last = now;
            return s.bucket.take();
        }
        if self.slots.len() >= ORIGIN_SLOTS {
            return None;
        }
        let mut b = Bucket::new(ORIGIN_WRITE_RATE, ORIGIN_WRITE_BURST);
        let wait = b.take();
        self.slots.insert(
            ip,
            Slot {
                bucket: b,
                last: now,
            },
        );
        wait
    }
}

/// Visitor origin for fairness. `None` means "do not apply a per-origin bucket".
pub fn visitor_ip(peer: SocketAddr, headers: &HeaderMap) -> Option<IpAddr> {
    if !peer.ip().is_loopback() {
        return Some(peer.ip());
    }
    let raw = headers.get("cf-connecting-ip")?.to_str().ok()?.trim();
    if raw.is_empty() || raw.contains(',') {
        return None;
    }
    let ip: IpAddr = raw.parse().ok()?;
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return None;
    }
    Some(ip)
}
