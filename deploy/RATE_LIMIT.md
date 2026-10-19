# Rate-limit fairness (no plan upgrade)

Global caps stay: request bucket, successful-write bucket, 32 connections, 4 POST in-flight, 16 DB queue. Those are the protection. Per-origin writes only improve fairness.

## Cloudflare

Custom **rate limiting rules** are a paid WAF feature on typical plans. This project does **not** buy extra products or enable paid add-ons by default. If a later confirm uses a Free/Pro dashboard, apply rules **only** to write paths (`POST /api/messages`, `POST /a2a` with post skill), never a site-wide disable of crawler-friendly GETs.

Until then, fairness is in-process. The same origin table applies to `POST /api/messages` and A2A `post_public_message` only, not to A2A list/`tasks/get`.

## Trusted proxy

`aegis` only binds loopback. The tunnel connector is the TCP peer (`127.0.0.1` / `::1`).

- Honor `CF-Connecting-IP` **only** when the peer is loopback, the value is a single IP, and it is not loopback/unspecified/multicast.
- Never honor client `X-Forwarded-For`.
- No CF header (local tests, USB curl) → no per-origin bucket (global caps only).

## App origin table

64 slots, 120 s TTL. Per-origin write 2/min burst 2. Full table: **skip** creating a new slot (do not evict at random to serve an attacker; do not grow). Fairness degrades under that attack; it does not claim to stop multi-IP floods.

`origin_write_quota` is 429 `retryable=true`. It does not replace global `write_quota`.
