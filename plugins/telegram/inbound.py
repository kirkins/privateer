"""Durable Telegram update custody and bounded notice delivery (adapter-local).

SQLite commits use FULL synchronous durability. The fetched updates and their
next poll offset commit together; offset.json is only a compatibility mirror.
Completed receipts contain metadata only and are bounded, as is admission.
"""
import contextlib
import json
import os
import sqlite3
import sys
import time

sys.dont_write_bytecode = True

import tg  # noqa: E402

LIMIT = 1000
RECEIPTS = 5000
ATTEMPTS = 3
RETRY_SECONDS = 15


def metadata(update):
    kind = next((k for k in ("message", "edited_message", "callback_query") if k in update), "unknown")
    msg = update.get(kind) or {}
    text = msg.get("data") if kind == "callback_query" else msg.get("text") or msg.get("caption")
    return kind, len(text or "")


class Spool:
    def __init__(self):
        self.path = tg.state_path("inbound.sqlite3")
        os.makedirs(tg.STATE_DIR, exist_ok=True)
        fd = os.open(self.path, os.O_RDWR | os.O_CREAT, 0o600)
        os.close(fd)
        with self.db() as db:
            db.executescript("""
                CREATE TABLE IF NOT EXISTS pending (
                    id INTEGER PRIMARY KEY, data TEXT NOT NULL,
                    attempts INTEGER NOT NULL DEFAULT 0, next_at REAL NOT NULL DEFAULT 0,
                    outcome TEXT, submission TEXT, result TEXT,
                    initial_skip INTEGER NOT NULL DEFAULT 0);
                CREATE TABLE IF NOT EXISTS notices (
                    update_id INTEGER NOT NULL, slot INTEGER NOT NULL, params TEXT NOT NULL,
                    attempts INTEGER NOT NULL DEFAULT 0, next_at REAL NOT NULL DEFAULT 0,
                    done INTEGER NOT NULL DEFAULT 0, message_id INTEGER,
                    PRIMARY KEY(update_id,slot));
                CREATE TABLE IF NOT EXISTS receipts (
                    id INTEGER PRIMARY KEY, kind TEXT NOT NULL, length INTEGER NOT NULL,
                    outcome TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS checkpoint (id INTEGER PRIMARY KEY, offset INTEGER NOT NULL);
                CREATE TABLE IF NOT EXISTS joins (
                    member INTEGER PRIMARY KEY, leader INTEGER NOT NULL);
            """)
            columns = {r[1] for r in db.execute("PRAGMA table_info(pending)")}
            if "admitted_at" not in columns:
                db.execute("ALTER TABLE pending ADD COLUMN admitted_at REAL NOT NULL DEFAULT 0")

    @contextlib.contextmanager
    def db(self):
        db = sqlite3.connect(self.path, timeout=30)
        db.row_factory = sqlite3.Row
        try:
            db.execute("PRAGMA synchronous=FULL")
            with db:
                yield db
        finally:
            db.close()

    def offset(self):
        with self.db() as db:
            row = db.execute("SELECT offset FROM checkpoint WHERE id=1").fetchone()
        return row[0] if row else tg.read_state("offset.json", {}).get("offset")

    def admit(self, updates, initial_skip=False):
        """False applies backpressure: never confirm updates we cannot retain."""
        if not updates and not initial_skip:
            return True
        with self.db() as db:
            db.execute("BEGIN IMMEDIATE")
            count = db.execute("SELECT count(*) FROM pending").fetchone()[0]
            if count + len(updates) > LIMIT:
                return False
            for update in updates:
                ident = update["update_id"]
                db.execute("INSERT OR IGNORE INTO pending(id,data,initial_skip,admitted_at) SELECT ?,?,?,? WHERE NOT EXISTS "
                           "(SELECT 1 FROM receipts WHERE id=?)",
                           (ident, json.dumps(update), int(initial_skip), time.time(), ident))
            offset = max((u["update_id"] + 1 for u in updates), default=0)
            db.execute("INSERT INTO checkpoint VALUES(1,?) ON CONFLICT(id) DO UPDATE SET "
                       "offset=max(offset,excluded.offset)", (offset,))
        return True

    def ready(self):
        with self.db() as db:
            # Preserve order: a retry must not change the routing of earlier mail.
            return db.execute("SELECT * FROM pending WHERE outcome IS NULL ORDER BY id LIMIT 1").fetchone()

    def following(self, ident):
        with self.db() as db:
            return db.execute("SELECT * FROM pending WHERE outcome IS NULL AND id>=? ORDER BY id",
                              (ident,)).fetchall()

    def joined(self, ident):
        with self.db() as db:
            return db.execute("SELECT p.* FROM pending p JOIN joins j ON j.member=p.id "
                              "WHERE j.leader=? ORDER BY p.id", (ident,)).fetchall()

    def join(self, ids):
        # Freeze membership before any external effect. A crash reuses exactly
        # this group, even if more updates arrive during recovery.
        with self.db() as db:
            db.executemany("INSERT INTO joins(member,leader) VALUES(?,?)",
                           [(i, ids[0]) for i in ids])

    def begin(self, ident):
        with self.db() as db:
            db.execute("UPDATE pending SET attempts=attempts+1,next_at=? WHERE id=?",
                       (time.time() + RETRY_SECONDS, ident))

    def submission(self, ident, args, msg_id, label):
        with self.db() as db:
            db.execute("UPDATE pending SET submission=coalesce(submission,?) WHERE id=?",
                       (json.dumps([args, msg_id, label]), ident))
            return json.loads(db.execute("SELECT submission FROM pending WHERE id=?", (ident,)).fetchone()[0])

    def replace_submission(self, ident, args, msg_id, label):
        with self.db() as db:
            db.execute("UPDATE pending SET submission=? WHERE id=?",
                       (json.dumps([args, msg_id, label]), ident))

    def result(self, ident, result=None):
        with self.db() as db:
            if result is not None:
                db.execute("UPDATE pending SET result=? WHERE id=?", (json.dumps(result), ident))
            value = db.execute("SELECT result FROM pending WHERE id=?", (ident,)).fetchone()[0]
            return json.loads(value) if value else None

    def finish(self, ident, outcome):
        with self.db() as db:
            rows = db.execute("SELECT id,data FROM pending WHERE id=? OR id IN "
                              "(SELECT member FROM joins WHERE leader=?) ORDER BY id",
                              (ident, ident)).fetchall()
            facts = []
            for row in rows:
                kind, length = metadata(json.loads(row["data"]))
                facts.append((row["id"], kind, length))
                db.execute("UPDATE pending SET outcome=? WHERE id=?", (outcome, row["id"]))
                db.execute("INSERT OR REPLACE INTO receipts VALUES(?,?,?,?)",
                           (row["id"], kind, length, outcome))
            db.execute("DELETE FROM receipts WHERE id NOT IN (SELECT id FROM receipts ORDER BY id DESC LIMIT ?)",
                       (RECEIPTS,))
        for member, kind, length in facts:
            tg.log(f"inbound update_id={member} type={kind} length={length} outcome={outcome}")
        self.collect()

    def notice(self, ident, slot, params):
        with self.db() as db:
            db.execute("INSERT OR IGNORE INTO notices(update_id,slot,params) VALUES(?,?,?)",
                       (ident, slot, json.dumps(params)))
            return db.execute("SELECT * FROM notices WHERE update_id=? AND slot=?", (ident, slot)).fetchone()

    def notice_attempt(self, row):
        # Reserve the attempt before the external call, including process crashes.
        with self.db() as db:
            db.execute("UPDATE notices SET attempts=attempts+1,next_at=? WHERE update_id=? AND slot=?",
                       (time.time() + RETRY_SECONDS, row["update_id"], row["slot"]))

    def notice_result(self, row, message_id, err):
        exhausted = row["attempts"] + 1 >= ATTEMPTS
        with self.db() as db:
            db.execute("UPDATE notices SET done=?,message_id=? WHERE update_id=? AND slot=?",
                       (int(not err or exhausted), message_id, row["update_id"], row["slot"]))
        if err:
            code = "notice_exhausted" if exhausted else "notice_retry"
            with self.db() as db:
                update = db.execute("SELECT data FROM pending WHERE id=?", (row["update_id"],)).fetchone()
            kind, length = metadata(json.loads(update[0]))
            tg.log(f"inbound update_id={row['update_id']} type={kind} length={length} outcome={code}")

    def notices_ready(self):
        with self.db() as db:
            return db.execute("SELECT * FROM notices WHERE done=0 AND next_at<=? ORDER BY update_id,slot LIMIT 1",
                              (time.time(),)).fetchall()

    def collect(self):
        with self.db() as db:
            db.execute("DELETE FROM notices WHERE done=1 AND update_id IN "
                       "(SELECT id FROM pending WHERE outcome IS NOT NULL)")
            db.execute("DELETE FROM joins WHERE leader IN (SELECT id FROM pending WHERE outcome IS NOT NULL)")
            db.execute("DELETE FROM pending WHERE outcome IS NOT NULL AND NOT EXISTS "
                       "(SELECT 1 FROM notices WHERE update_id=pending.id)")
