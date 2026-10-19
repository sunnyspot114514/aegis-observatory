# Archive and capacity (stick + D: originals)

Hard caps stay: **1000 messages**, 2000 events, 8 MiB SQLite pages, 16 MiB data dir, 128 MiB free disk. Do not raise them to “keep the board up”.

## Pressure

`GET /healthz` (loopback; public edge 404s this) reports `capacity.archive_hint`:

| hint | meaning |
|------|---------|
| `ok` | every dimension below 70% |
| `hint` | at least one dimension ≥ 70% — pull a batch |
| `urgent` | ≥ 85% — pull soon |
| `full` | a hard cap rejected writes (`507`, `retryable=false`; do not retry every second) |

Disk query failure still **forbids writes** (`disk_query_failed`).

## Pull, do not copy the live file

PC tool: `tools/archive_pull.sh` / `tools/archive_pull.ps1`. It asks the DB worker for an **event_seq range** via `GET /ops/export?from=&limit=` on **loopback** (USB SSH + `127.0.0.1:8787`, never the public hostname). That is a consistent batch (join events→messages). `tools/archive_verify.py` re-reads SHA-256 and restores the batch into a **temp SQLite** that is deleted afterwards. Do **not** `cp aegis.sqlite` while the writer is live and call it a backup.

Public Cloudflare ingress must 404 `/ops/` and `/healthz`. The process also 404s those paths when `CF-Connecting-IP` is present. Visitor bodies in a batch are **untrusted user content**, not MIT, and must not go to GitHub.

## D: originals

Set `AEGIS_ARCHIVE_ROOT` (example: `D:\AEGIS-Archive`). Each batch directory stores:

- `batch.json` (the export)
- `manifest.json` (from_seq, to_seq, count, condition_version, instance_id, pulled_at, sha256 of batch.json)
- SHA-256 of those files

Re-read and hash-check. Isolation restore = copy into a **new empty data dir**, start the binary `--dev` only if needed, `GET /ops/export` and compare seq range. Only a fully verified batch is **eligible** to prune.

Do not prune when: D: is offline, transfer interrupted, hash mismatch, or the only copy is the GitHub desensitized tree.

## Prune (not enabled)

Auto-clean stays **off** until a separate confirmation. When enabled later:

- Delete only message rows whose `id` is in a **verified** batch, and only events in that seq range.
- Do **not** reset AUTOINCREMENT; do **not** renumber from 1.
- Rows inserted **after** `to_seq` stay.
- Pagination cursors are message ids: pruned ids simply disappear; clients follow `next_before` / `next_after`.

### Idempotency window

`request_id` lives **on the message row**. Retention = messages still on the stick (≤ 1000). No unbounded tombstone table.

After a verified prune, a pruned `request_id` is **unknown**. A later POST with that id and a valid body is a **new 201** (new id). Clients must not assume idempotency across archive boundaries. This is the expired-request behavior.

## GitHub

Daily public material: redacted logs and counts only. Never the SQLite file, credentials, full client IPs, or visitor bodies.
