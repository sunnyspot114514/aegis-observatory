#!/usr/bin/env python3
"""Re-read SHA-256, write manifest, restore a batch into isolated temp SQLite. Does not prune."""
import hashlib
import json
import os
import sqlite3
import sys
import tempfile


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: archive_verify.py BATCH_DIR")
        return 2
    dest = sys.argv[1]
    path = os.path.join(dest, "batch.json")
    raw1 = open(path, "rb").read()
    digest1 = hashlib.sha256(raw1).hexdigest()
    digest2 = hashlib.sha256(open(path, "rb").read()).hexdigest()
    if digest1 != digest2:
        print("re-read hash mismatch")
        return 1
    body = json.loads(raw1.decode("utf-8"))
    events = body.get("events") or []
    if len(events) != int(body.get("count") or 0):
        print("count mismatch")
        return 1
    seqs = [int(e["event_seq"]) for e in events]
    if seqs != sorted(seqs) or (seqs and seqs != list(range(seqs[0], seqs[-1] + 1))):
        print("event_seq not contiguous")
        return 1
    open(path + ".sha256", "w", encoding="utf-8").write(digest1 + "\n")
    man = {
        "from_seq": body.get("from_seq"),
        "to_seq": body.get("to_seq"),
        "count": body.get("count"),
        "message_id_min": body.get("message_id_min"),
        "message_id_max": body.get("message_id_max"),
        "condition_version": body.get("condition_version"),
        "instance_id": body.get("instance_id"),
        "batch_sha256": digest1,
        "reread_ok": True,
        "isolated_restore_ok": False,
        "prune": False,
        "note": "originals only; do not upload visitor bodies to GitHub",
    }
    tmp = tempfile.NamedTemporaryFile(suffix=".sqlite", delete=False)
    tmp.close()
    try:
        conn = sqlite3.connect(tmp.name)
        conn.execute(
            "CREATE TABLE messages(id INTEGER PRIMARY KEY, created_at TEXT, body TEXT, request_id TEXT)"
        )
        conn.execute(
            "CREATE TABLE events(event_seq INTEGER PRIMARY KEY, created_at TEXT, source_kind TEXT, action TEXT, result TEXT, condition_version TEXT, message_id INTEGER)"
        )
        for e in events:
            mid = e.get("message_id")
            if mid is not None:
                conn.execute(
                    "INSERT OR REPLACE INTO messages(id, created_at, body, request_id) VALUES(?,?,?,?)",
                    (mid, e.get("created_at"), e.get("body"), e.get("request_id")),
                )
            conn.execute(
                "INSERT INTO events(event_seq, created_at, source_kind, action, result, condition_version, message_id) VALUES(?,?,?,?,?,?,?)",
                (
                    e["event_seq"],
                    e.get("created_at"),
                    e.get("source_kind"),
                    e.get("action"),
                    e.get("result"),
                    e.get("condition_version"),
                    mid,
                ),
            )
        conn.commit()
        n = conn.execute("SELECT COUNT(*) FROM events").fetchone()[0]
        row = conn.execute("SELECT MIN(event_seq), MAX(event_seq) FROM events").fetchone()
        conn.close()
        if n != len(events):
            print("isolated restore count mismatch")
            return 1
        if events and (row[0] != body.get("from_seq") or row[1] != body.get("to_seq")):
            print("isolated restore seq range mismatch")
            return 1
        man["isolated_restore_ok"] = True
    finally:
        try:
            os.unlink(tmp.name)
        except OSError:
            pass
    open(os.path.join(dest, "manifest.json"), "w", encoding="utf-8").write(
        json.dumps(man, indent=2) + "\n"
    )
    print("VERIFY_OK", dest, "sha256", digest1, "count", man["count"])
    print("This batch is restore-verified. Prune still requires a separate operator confirm.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
