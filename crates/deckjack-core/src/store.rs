//! System state: one SQLite file in the working directory.
//!
//! Windows, layouts, grants, settings and sessions all end up here. User data
//! never does — that stays in plain files and open formats, so if Deckjack
//! disappeared the files would still work with other tools.
//!
//! **Never exposed.** The file API, WebDAV, search, previews, downloads and
//! agent tools all refuse `.deckjack/`. The terminal is the documented
//! escape hatch: it is a real shell as the host user, so like any shell on a
//! real computer it can read this file. Secrets inside it are encrypted for
//! that reason.
//!
//! **WAL mode**, so a reader never blocks the writer. The cost is that the
//! database is three files rather than one, and copying it while the server
//! runs is unsafe — which is what the lock file is there to tell you.
//! Graceful shutdown truncates the WAL back into the main file so the
//! directory is safe to carry.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

use crate::auth::session::{Session, SessionId};

/// Where the database lives, relative to the working directory.
pub const DB_PATH: &str = ".deckjack/state.db";

/// Bumped whenever [`MIGRATIONS`] grows. Checked at startup against what the
/// file says, so a database written by a newer Deckjack is refused rather
/// than quietly half-understood.
pub const SCHEMA_VERSION: u32 = 3;

/// Applied in order; index + 1 is the version each one produces. Never edit a
/// migration that has shipped — add another.
const MIGRATIONS: &[&str] = &[
    // 1
    r#"
    CREATE TABLE sessions (
        id      TEXT    PRIMARY KEY,
        ip      TEXT    NOT NULL,
        created INTEGER NOT NULL,
        expires INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX sessions_expires ON sessions (expires);
    "#,
    // 2 — the rest of core: the command log and grants, the window set and its
    // two per-device-class layouts, the attribution log, the node registry,
    // passkey credentials, and installed apps. Written as one migration
    // because they landed together; from here each change gets its own.
    r#"
-- ===== from command.rs =====
-- Command bus: attribution log and per-session grants.

-- Append-only. One row per decision about a dispatch, so a single dispatch id
-- appears twice in the normal case (`ran` then `completed`/`failed`). Only ever
-- INSERTed: SQLite cannot enforce that, which is why the design ships this log
-- to the VPS as it is written rather than relying on the host keeping it honest.
CREATE TABLE command_log (
    row       INTEGER PRIMARY KEY AUTOINCREMENT,
    dispatch  TEXT    NOT NULL,
    at        INTEGER NOT NULL,
    node      TEXT    NOT NULL,
    command   TEXT    NOT NULL,
    -- NULL when the command was never found, so no backend had declared a risk.
    risk      TEXT,
    issuer    TEXT    NOT NULL,   -- 'operator' | 'agent' | 'cli'
    -- The agent session's name; NULL for the operator and the CLI. The session
    -- cookie is deliberately not here: this file is readable from the terminal.
    issuer_name TEXT,
    decision  TEXT    NOT NULL,   -- ran | queued | awaiting_confirmation | refused | completed | failed
    -- Parameters as JSON, on admission only, and '"<redacted>"' for a command
    -- whose spec sets redact_params.
    params    TEXT,
    detail    TEXT
) STRICT;
CREATE INDEX command_log_at ON command_log (at);
CREATE INDEX command_log_dispatch ON command_log (dispatch);
-- The lockdown trigger "a burst of refused agent commands" is a count over one
-- session in a time window, so it gets its own index.
CREATE INDEX command_log_refusals ON command_log (issuer_name, at) WHERE decision = 'refused';

CREATE TABLE command_grants (
    session    TEXT    PRIMARY KEY,
    -- One pattern per line, as Pattern writes itself: 'git.*', 'files.read.status'.
    namespaces TEXT    NOT NULL,
    -- 'this' | 'any' | one node id per line.
    nodes      TEXT    NOT NULL,
    ceiling    TEXT    NOT NULL,
    created    INTEGER NOT NULL,
    expires    INTEGER
) STRICT;

-- ===== from events.rs =====
-- Append as MIGRATIONS[1] in store.rs (produces schema version 2) and bump
-- SCHEMA_VERSION to 2. Do not edit it after it ships.
--
-- 2
CREATE TABLE events (
    seq           INTEGER PRIMARY KEY AUTOINCREMENT,
    at_ms         INTEGER NOT NULL,
    issuer_kind   TEXT    NOT NULL,
    issuer_name   TEXT,
    issuer_window TEXT,
    command       TEXT    NOT NULL,
    target_node   TEXT    NOT NULL,
    params        TEXT    NOT NULL,
    outcome       TEXT    NOT NULL,
    detail        TEXT,
    path          TEXT,
    from_ip       TEXT
) STRICT;

CREATE INDEX events_at ON events (at_ms);
CREATE INDEX events_issuer ON events (issuer_kind, issuer_name, seq);
CREATE INDEX events_command ON events (command, seq);

CREATE TRIGGER events_no_update BEFORE UPDATE ON events BEGIN
    SELECT RAISE(ABORT, 'the audit log is append-only: an entry is never changed');
END;

CREATE TRIGGER events_no_delete BEFORE DELETE ON events BEGIN
    SELECT RAISE(ABORT, 'the audit log is append-only: an entry is never removed');
END;

-- Notes for whoever reviews this:
--   * AUTOINCREMENT is deliberate: rowids are never reused, so a gap in `seq`
--     is evidence that a row was removed rather than being papered over.
--   * at_ms is MILLISECONDS, unlike the seconds the sessions table uses. A
--     command log records several entries per second and a timeline that
--     showed them at one instant would be a worse log; the column name
--     carries the unit so it cannot be misread at the SQL prompt.
--   * The triggers make append-only real against hand-written SQL through
--     another connection. They are not tamper-proofing: a shell as my uid can
--     DROP TRIGGER. See integration_notes.

-- ===== from grants.rs =====
-- Migration 2: scoped grants and the approval Inbox.
--
-- No foreign key from inbox.grant_id to grants.id: an Inbox item is an audit
-- record of what was asked, and it must survive the grant being revoked and
-- cleaned up.
CREATE TABLE grants (
    id         TEXT    PRIMARY KEY,
    label      TEXT    NOT NULL,
    -- Owning session id (SessionId::encode()), or NULL for a standing grant
    -- such as the always-on assistant's.
    session    TEXT,
    project    TEXT,
    -- One pattern per line, exactly as written: "git.*\\nfiles.read.*".
    namespaces TEXT    NOT NULL,
    -- '*' for every node, otherwise newline-separated node ids.
    nodes      TEXT    NOT NULL,
    issued_at  INTEGER NOT NULL,
    expires_at INTEGER,
    revoked_at INTEGER
) STRICT;
CREATE INDEX grants_session ON grants (session);
CREATE INDEX grants_expires ON grants (expires_at);

CREATE TABLE inbox (
    id           TEXT    PRIMARY KEY,
    grant_id     TEXT,
    -- Issuer as JSON (serde of grants::Issuer), so the CLI, the operator and
    -- a named session all round-trip without three nullable columns.
    issuer       TEXT    NOT NULL,
    command      TEXT    NOT NULL,
    target_node  TEXT    NOT NULL,
    risk         TEXT    NOT NULL,  -- Risk::as_str
    reason       TEXT    NOT NULL,  -- QueueReason as JSON (Bulk carries a count)
    scale_kind   TEXT    NOT NULL,  -- 'single' | 'count' | 'unknown'
    scale_count  INTEGER,           -- NULL exactly when scale_kind = 'unknown'
    title        TEXT    NOT NULL,
    detail       TEXT,
    -- Command parameters as JSON. Data, never re-read as instructions.
    params       TEXT    NOT NULL,
    created      INTEGER NOT NULL,
    expires      INTEGER,
    status       TEXT    NOT NULL,  -- ItemStatus::as_str
    decided_at   INTEGER,
    decided_note TEXT
) STRICT;
-- The Inbox reads as "pending, oldest first"; the badge counts the same index.
CREATE INDEX inbox_status ON inbox (status, created);
CREATE INDEX inbox_grant ON inbox (grant_id);

-- ===== from registry.rs =====
-- 2: the node registry. Registration is durable so a restart can say "the
-- R620 is offline" instead of pretending the machine never existed;
-- heartbeats are deliberately NOT written here (synchronous = FULL makes a
-- disk write per node every few seconds, for a value that is meaningless the
-- moment the process dies). `last_seen` is the final beat before shutdown,
-- shown as "last seen" and never treated as liveness.
CREATE TABLE nodes (
    node_id      TEXT    PRIMARY KEY,
    host_name    TEXT    NOT NULL,
    address      TEXT    NOT NULL,
    protocol     INTEGER NOT NULL,
    capabilities TEXT    NOT NULL,
    backends     TEXT    NOT NULL,
    first_seen   INTEGER NOT NULL,
    last_seen    INTEGER NOT NULL
) STRICT;
CREATE INDEX nodes_last_seen ON nodes (last_seen);

-- ===== from window.rs =====
-- Migration 2 (append to store::MIGRATIONS, bump SCHEMA_VERSION to 2).
-- Columns match deckjack_core::window::{WindowRow, DesktopRow, CompactRow,
-- SplitRow, ShellRow} field for field.

CREATE TABLE windows (
    id        INTEGER PRIMARY KEY,
    app       TEXT    NOT NULL,
    instance  TEXT    NOT NULL,
    title     TEXT    NOT NULL,
    node_id   TEXT    NOT NULL,
    min_width INTEGER NOT NULL,
    opened_at INTEGER NOT NULL
) STRICT;

-- The desktop layout: one row per window. `z` is 0 at the back. `snap` is
-- Snap::as_str(); the zone is stored rather than only the frame so a window
-- re-snaps correctly when the work area changes.
CREATE TABLE window_desktop (
    window_id      INTEGER PRIMARY KEY REFERENCES windows (id) ON DELETE CASCADE,
    x              INTEGER NOT NULL,
    y              INTEGER NOT NULL,
    width          INTEGER NOT NULL,
    height         INTEGER NOT NULL,
    snap           TEXT    NOT NULL,
    restore_x      INTEGER,
    restore_y      INTEGER,
    restore_width  INTEGER,
    restore_height INTEGER,
    minimized      INTEGER NOT NULL,
    popped_out     INTEGER NOT NULL,
    z              INTEGER NOT NULL
) STRICT;
-- Not UNIQUE on purpose: SQLite checks uniqueness per statement, so a unique
-- index would make rewriting the stack row by row fail halfway. The snapshot
-- is what guarantees distinct z values, and restore breaks ties by id.
CREATE INDEX window_desktop_z ON window_desktop (z);

-- The compact layout: switcher position, 0 = most recently focused.
CREATE TABLE window_compact (
    window_id  INTEGER PRIMARY KEY REFERENCES windows (id) ON DELETE CASCADE,
    focus_rank INTEGER NOT NULL
) STRICT;
CREATE INDEX window_compact_rank ON window_compact (focus_rank);

-- At most one split exists at a time, hence the pinned id.
CREATE TABLE window_split (
    id               INTEGER PRIMARY KEY CHECK (id = 1),
    primary_window   INTEGER NOT NULL REFERENCES windows (id) ON DELETE CASCADE,
    secondary_window INTEGER NOT NULL REFERENCES windows (id) ON DELETE CASCADE,
    divider          REAL    NOT NULL,
    CHECK (primary_window <> secondary_window)
) STRICT;

-- Shell state that belongs to no single window. NULL mode_override is
-- automatic mode.
CREATE TABLE window_shell (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    desktop_focus INTEGER REFERENCES windows (id) ON DELETE SET NULL,
    mode_override TEXT
) STRICT;

-- ===== from passkey.rs =====
-- 2
CREATE TABLE passkey_user (
    -- One operator, so one row, enforced by the database rather than by
    -- whichever code path happens to write it.
    id      INTEGER PRIMARY KEY CHECK (id = 1),
    handle  TEXT    NOT NULL,
    created INTEGER NOT NULL
) STRICT;

CREATE TABLE passkeys (
    -- base64url of the authenticator's credential id.
    cred_id         TEXT    PRIMARY KEY,
    nickname        TEXT    NOT NULL,
    -- webauthn-rs `Passkey` as JSON: public key material only, which is why
    -- it is not encrypted like a secret would be.
    key_json        TEXT    NOT NULL,
    -- Highest sign count seen. 0 means no assertion yet, or an authenticator
    -- that does not count; a value that fails to move forward is a clone.
    sign_count      INTEGER NOT NULL,
    backup_eligible INTEGER NOT NULL,
    backup_state    INTEGER NOT NULL,
    created         INTEGER NOT NULL,
    last_used       INTEGER
) STRICT;
CREATE INDEX passkeys_created ON passkeys (created);

-- ===== from apps.rs =====
-- Appended to store.rs MIGRATIONS as migration 2 (bump SCHEMA_VERSION to match).
--
-- Approvals, not the app inventory: binaries live in the install directory,
-- which never travels, so what is installed must never be written into the
-- working directory's state.db. What the operator approved is a decision
-- about an app, and decisions travel with the directory.
--
-- `fingerprint` is apps::Manifest::fingerprint() at the moment of approval.
-- A manifest that later asks for more has a different fingerprint, so the
-- row stops matching and the app needs approving again rather than
-- inheriting consent it was never given.
CREATE TABLE app_grants (
    app         TEXT    PRIMARY KEY,
    fingerprint TEXT    NOT NULL,
    needs       TEXT    NOT NULL,
    granted     INTEGER NOT NULL
) STRICT;
"#,
    // 3 — which browser session issued a command. A `command::SessionTag`:
    // 16 hex characters of SHA-256 over the session id, never the cookie,
    // because this file is readable from the terminal. NULL for agents, the
    // CLI, and commands the server issued itself.
    r#"
    ALTER TABLE command_log ADD COLUMN issuer_session TEXT;
    CREATE INDEX command_log_session ON command_log (issuer_session, at)
        WHERE issuer_session IS NOT NULL;
    "#,
];

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("cannot open {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error(
        "{path} was written by a newer Deckjack (schema {found}, this build understands {supported}); \
         upgrade rather than risk writing a database you do not fully understand"
    )]
    FromTheFuture {
        path: PathBuf,
        found: u32,
        supported: u32,
    },

    #[error(
        "{path}: the write-ahead log could not be folded back into the main file, because \
         another connection still holds a read lock; this directory is not safe to copy yet"
    )]
    WalNotFolded { path: PathBuf },

    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

pub struct Store {
    conn: Connection,
    path: PathBuf,
}

impl Store {
    /// Open (creating if needed) the state database for a working directory.
    pub fn open(working_dir: &Path) -> Result<Store, StoreError> {
        let path = working_dir.join(DB_PATH);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::Open {
                path: path.clone(),
                source: rusqlite::Error::ToSqlConversionFailure(Box::new(e)),
            })?;
        }
        // Before the connection, not after: sqlite would create the file 0644
        // under the usual umask, and it takes the mode of `-wal` and `-shm`
        // from whatever the main file's mode is when they are created.
        owner_only(&path);
        let conn = Connection::open(&path).map_err(|source| StoreError::Open {
            path: path.clone(),
            source,
        })?;
        Store::from_connection(conn, path)
    }

    /// An in-memory database, for tests.
    pub fn in_memory() -> Result<Store, StoreError> {
        Store::from_connection(Connection::open_in_memory()?, PathBuf::from(":memory:"))
    }

    fn from_connection(conn: Connection, path: PathBuf) -> Result<Store, StoreError> {
        // `Connection::open` only records the filename; the first pragma is
        // where a file that is not a database says so. Those failures are
        // about this file, so they carry its name rather than surfacing as a
        // bare "file is not a database" with nothing to act on.
        let prepare = || -> Result<(), rusqlite::Error> {
            // WAL survives in the file, but setting it is cheap and makes a
            // database created elsewhere behave the same way.
            conn.pragma_update(None, "journal_mode", "WAL")?;
            // Without this, a crash between the write and the flush can lose
            // the last transaction. FULL is the honest setting for state we
            // intend to carry between machines.
            conn.pragma_update(None, "synchronous", "FULL")?;
            conn.pragma_update(None, "foreign_keys", true)
        };
        prepare().map_err(|source| StoreError::Open {
            path: path.clone(),
            source,
        })?;

        let mut store = Store { conn, path };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<(), StoreError> {
        // Not `unwrap_or(0)`: a new or empty file genuinely reports 0, so
        // defaulting buys nothing, and it turned "this file is not a database"
        // and "the file is locked" into "version 0" — which then ran migration
        // 1 against a populated file and blamed the collision on the schema
        // instead of saying which file is wrong.
        let found: u32 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|source| StoreError::Open {
                path: self.path.clone(),
                source,
            })?;

        if found > SCHEMA_VERSION {
            return Err(StoreError::FromTheFuture {
                path: self.path.clone(),
                found,
                supported: SCHEMA_VERSION,
            });
        }
        if found == SCHEMA_VERSION {
            return Ok(());
        }

        let tx = self.conn.transaction()?;
        for (i, sql) in MIGRATIONS.iter().enumerate() {
            let produces = i as u32 + 1;
            if produces > found {
                tx.execute_batch(sql)?;
            }
        }
        // `pragma_update` cannot run inside a transaction on this pragma, so
        // it goes in the batch.
        tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        tx.commit()?;
        tracing::info!(from = found, to = SCHEMA_VERSION, "state schema migrated");
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn schema_version(&self) -> Result<u32, StoreError> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    // ---- sessions -------------------------------------------------------

    pub fn insert_session(&self, id: &SessionId, s: &Session) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sessions (id, ip, created, expires) VALUES (?1, ?2, ?3, ?4)",
            params![
                id.encode(),
                s.ip.to_string(),
                to_unix(s.created),
                to_unix(s.expires),
            ],
        )?;
        Ok(())
    }

    pub fn delete_session(&self, id: &SessionId) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![id.encode()])?;
        Ok(())
    }

    pub fn delete_all_sessions(&self) -> Result<usize, StoreError> {
        Ok(self.conn.execute("DELETE FROM sessions", [])?)
    }

    /// Every session that has not expired, for restoring state at startup.
    /// Expired rows are dropped on the way past rather than loaded and then
    /// pruned, so a long-dormant directory does not come back with thousands
    /// of dead sessions in memory.
    pub fn live_sessions(&self, now: SystemTime) -> Result<Vec<(SessionId, Session)>, StoreError> {
        let cutoff = to_unix(now);
        self.conn
            .execute("DELETE FROM sessions WHERE expires <= ?1", params![cutoff])?;

        let mut stmt = self
            .conn
            .prepare("SELECT id, ip, created, expires FROM sessions")?;
        let rows = stmt.query_map([], |r| {
            let id: String = r.get(0)?;
            let ip: String = r.get(1)?;
            let created: i64 = r.get(2)?;
            let expires: i64 = r.get(3)?;
            Ok((id, ip, created, expires))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (id, ip, created, expires) = row?;
            // A row we cannot parse is a row we cannot honor. Skipping it is
            // right: the worst case is someone signs in again.
            let (Some(id), Ok(ip)) = (SessionId::decode(&id), ip.parse()) else {
                tracing::warn!("skipping unreadable session row");
                continue;
            };
            out.push((
                id,
                Session {
                    ip,
                    created: from_unix(created),
                    expires: from_unix(expires),
                },
            ));
        }
        Ok(out)
    }

    /// Fold the WAL back into the main file so the directory can be copied.
    /// Called on graceful shutdown, before the lock is released.
    pub fn checkpoint(&self) -> Result<(), StoreError> {
        // The pragma answers (busy, log, checkpointed). SQLite does not call
        // a blocked checkpoint an error — busy = 1 means "a reader was still
        // in the file, so some committed rows live only in the `-wal`". That
        // is precisely the case the caller must not mistake for success:
        // shutdown would release the lock and declare the directory copyable
        // while a third of the state sat in a file nobody copies.
        let busy: i64 = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))
            .optional()?
            .unwrap_or(0);
        if busy != 0 {
            return Err(StoreError::WalNotFolded {
                path: self.path.clone(),
            });
        }
        Ok(())
    }
}

/// Make the state file readable only by the user running the server,
/// creating it empty if it is not there yet (sqlite reads an empty file as an
/// empty database, so creating it costs nothing).
///
/// This file holds the session table, and a session id in it is the cookie a
/// browser presents — so another local account that can read the file can
/// present those cookies too. The terminal is the documented escape hatch for
/// *this* user; nothing says the next user on the host gets one.
///
/// A failure is warned about rather than returned: some hosts the design
/// targets (a working directory on Android shared storage) have no unix modes
/// at all, and refusing to start there would be the wrong trade.
fn owner_only(path: &Path) {
    #[cfg(unix)]
    {
        use std::fs::{OpenOptions, Permissions, set_permissions};
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let result = if path.exists() {
            set_permissions(path, Permissions::from_mode(0o600))
        } else {
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .open(path)
                .map(|_| ())
        };
        if let Err(e) = result {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "cannot restrict the state database to this user; it may be readable by others on this host"
            );
        }
    }
}

fn to_unix(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        // Before the epoch is not a time we expect; clamping is better than
        // panicking on a clock that has gone backwards.
        Err(_) => 0,
    }
}

fn from_unix(secs: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn session(ip: &str, lasts: u64) -> Session {
        let now = SystemTime::now();
        Session {
            ip: ip.parse::<IpAddr>().unwrap(),
            created: now,
            expires: now + Duration::from_secs(lasts),
        }
    }

    fn id(byte: u8) -> SessionId {
        SessionId::from_bytes([byte; 32])
    }

    #[test]
    fn a_new_database_is_at_the_current_schema() {
        let s = Store::in_memory().unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn migration_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let first = Store::open(dir.path()).unwrap();
        assert_eq!(first.schema_version().unwrap(), SCHEMA_VERSION);
        drop(first);

        let again = Store::open(dir.path()).unwrap();
        assert_eq!(again.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn a_newer_schema_is_refused_rather_than_guessed_at() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = Store::open(dir.path()).unwrap();
            s.conn
                .execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 5))
                .unwrap();
        }
        match Store::open(dir.path()) {
            Err(StoreError::FromTheFuture {
                found, supported, ..
            }) => {
                assert_eq!(found, SCHEMA_VERSION + 5);
                assert_eq!(supported, SCHEMA_VERSION);
            }
            Err(other) => panic!("wrong error: {other}"),
            Ok(_) => panic!("a future schema was accepted"),
        }
    }

    #[test]
    fn sessions_round_trip() {
        let s = Store::in_memory().unwrap();
        let sess = session("100.69.236.100", 3600);
        s.insert_session(&id(1), &sess).unwrap();

        let live = s.live_sessions(SystemTime::now()).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].0, id(1));
        assert_eq!(live[0].1.ip, sess.ip);
        // Stored to the second, so compare at that resolution.
        assert_eq!(to_unix(live[0].1.expires), to_unix(sess.expires));
    }

    #[test]
    fn expired_sessions_are_dropped_not_returned() {
        let s = Store::in_memory().unwrap();
        s.insert_session(&id(1), &session("127.0.0.1", 3600))
            .unwrap();

        let mut dead = session("127.0.0.1", 0);
        dead.expires = SystemTime::now() - Duration::from_secs(60);
        s.insert_session(&id(2), &dead).unwrap();

        let live = s.live_sessions(SystemTime::now()).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].0, id(1));

        // And it is gone from the file, not merely filtered on the way out.
        let count: i64 = s
            .conn
            .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn deleting_a_session_removes_it() {
        let s = Store::in_memory().unwrap();
        s.insert_session(&id(1), &session("127.0.0.1", 3600))
            .unwrap();
        s.delete_session(&id(1)).unwrap();
        assert!(s.live_sessions(SystemTime::now()).unwrap().is_empty());
    }

    #[test]
    fn sessions_survive_reopening_the_file() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = Store::open(dir.path()).unwrap();
            s.insert_session(&id(7), &session("100.69.236.100", 3600))
                .unwrap();
            s.checkpoint().unwrap();
        }
        let s = Store::open(dir.path()).unwrap();
        let live = s.live_sessions(SystemTime::now()).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].0, id(7));
    }

    #[test]
    fn the_database_lives_in_the_hidden_directory() {
        assert!(Path::new(DB_PATH).starts_with(".deckjack"));
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(
            s.path().strip_prefix(dir.path()).unwrap(),
            Path::new(DB_PATH)
        );
    }

    #[test]
    fn wal_mode_is_on() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let mode: String = s
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }
}

// ---------------------------------------------------------------------------
// the command log
// ---------------------------------------------------------------------------

impl Store {
    /// Append one decision about a dispatch.
    ///
    /// INSERT only. There is no update and no delete here, and that is the
    /// guarantee the type is making — SQLite cannot enforce it, which is why
    /// the design ships this log offsite as it is written rather than
    /// trusting the host to keep it honest.
    pub fn append_command_log(&self, entry: &crate::command::LogEntry) -> Result<(), StoreError> {
        let issuer_name = entry.issuer.name().map(str::to_string);
        // The TAG, never the cookie: this file is readable from the terminal
        // by design, and a tag is enough to tell two browsers apart.
        let issuer_session = entry
            .issuer
            .browser_session()
            .map(|b| b.tag().as_str().to_string());
        let params = entry
            .params
            .as_ref()
            .map(|p| serde_json::to_string(p).unwrap_or_else(|_| "\"<unserializable>\"".into()));

        self.conn.execute(
            "INSERT INTO command_log
               (dispatch, at, node, command, risk, issuer, issuer_name,
                issuer_session, decision, params, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                entry.dispatch.to_string(),
                to_unix(entry.at),
                entry.node.as_str(),
                entry.command.to_string(),
                entry.risk.map(|r| r.as_str()),
                entry.issuer.kind(),
                issuer_name,
                issuer_session,
                decision_name(entry.decision),
                params,
                entry.detail.as_deref(),
            ],
        )?;
        Ok(())
    }

    /// The timeline, newest first.
    pub fn command_log(&self, limit: usize) -> Result<Vec<LoggedCommand>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT at, command, risk, issuer, issuer_name, issuer_session, decision, detail
               FROM command_log ORDER BY row DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(LoggedCommand {
                at: from_unix(r.get(0)?),
                command: r.get(1)?,
                risk: r.get(2)?,
                issuer: r.get(3)?,
                issuer_name: r.get(4)?,
                issuer_session: r.get(5)?,
                decision: r.get(6)?,
                detail: r.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }
}

/// One row of the timeline panel.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LoggedCommand {
    pub at: SystemTime,
    pub command: String,
    pub risk: Option<String>,
    pub issuer: String,
    pub issuer_name: Option<String>,
    pub issuer_session: Option<String>,
    pub decision: String,
    pub detail: Option<String>,
}

fn decision_name(d: crate::command::Decision) -> &'static str {
    use crate::command::Decision as D;
    match d {
        D::Ran => "ran",
        D::Queued => "queued",
        D::AwaitingConfirmation => "awaiting_confirmation",
        D::Refused => "refused",
        D::Completed => "completed",
        D::Failed => "failed",
    }
}

/// The command log, written to the state database.
///
/// A failure to write is reported and swallowed by the bus, which is correct:
/// losing an audit row is bad, but refusing to run a command because the disk
/// is full is worse, and the bus already treats the journal as advisory to
/// routing.
pub struct StoreJournal {
    store: std::sync::Arc<std::sync::Mutex<Store>>,
}

impl StoreJournal {
    pub fn new(store: std::sync::Arc<std::sync::Mutex<Store>>) -> StoreJournal {
        StoreJournal { store }
    }
}

impl crate::command::Journal for StoreJournal {
    fn record(&self, entry: crate::command::LogEntry) -> Result<(), crate::command::JournalError> {
        self.store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .append_command_log(&entry)
            .map_err(|e| crate::command::JournalError(e.to_string()))
    }
}
