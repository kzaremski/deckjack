//! What the state database promises, exercised through the public API only.
//!
//! The claims under test are the ones the design makes about `state.db`:
//! migrations run in order from any version this build ever wrote and never
//! lose a row, a file from the future is refused instead of guessed at, the
//! WAL is folded back so the directory can be carried, and a row that cannot
//! be read is skipped rather than taken down the whole load with it.
//!
//! Where a test needs a database at an older schema, it builds one with raw
//! SQL and then hands the file to `Store::open`. That is deliberate: it is
//! the only way to stand in for a file an older build left behind, and
//! shipped migration SQL is frozen by rule, so such a fixture cannot drift.

use std::net::IpAddr;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deckjack_core::auth::session::{Session, SessionHash, SessionId};
use deckjack_core::store::{DB_PATH, SCHEMA_VERSION, Store, StoreError};
use rusqlite::Connection;

// ---- helpers ------------------------------------------------------------

/// A second connection to the same file, so an assertion about what landed on
/// disk is made independently of the `Store` that wrote it.
fn raw(dir: &Path) -> Connection {
    Connection::open(dir.join(DB_PATH)).expect("the state file should be an sqlite database")
}

fn session(ip: &str, lasts_secs: u64) -> Session {
    let now = SystemTime::now();
    Session::new(
        ip.parse::<IpAddr>().unwrap(),
        now,
        now + Duration::from_secs(lasts_secs),
    )
}

/// A session as everything but the live request path sees it: the hash, not
/// the cookie. `insert_session` takes only this, which is what keeps a cookie
/// out of the file.
fn an_id() -> SessionHash {
    a_cookie().hash()
}

/// The cookie itself. Only the tests that are *about* the cookie want one.
fn a_cookie() -> SessionId {
    SessionId::generate().unwrap()
}

fn unix(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Every object the schema is supposed to define, as one sorted string. Used
/// to compare a database against itself across reopens.
fn schema_dump(conn: &Connection) -> String {
    let mut stmt = conn
        .prepare("SELECT type, name, COALESCE(sql, '') FROM sqlite_master ORDER BY type, name")
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok(format!(
                "{} {}\n{}\n",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?
            ))
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        > 0
}

fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
    conn.prepare(&format!("SELECT {column} FROM {table} LIMIT 0"))
        .is_ok()
}

/// The sessions table exactly as schema version 1 shipped it. Frozen SQL: a
/// migration that has shipped is never edited, so this cannot go stale.
const V1_SCHEMA: &str = r#"
CREATE TABLE sessions (
    id      TEXT    PRIMARY KEY,
    ip      TEXT    NOT NULL,
    created INTEGER NOT NULL,
    expires INTEGER NOT NULL
) STRICT;
CREATE INDEX sessions_expires ON sessions (expires);
PRAGMA user_version = 1;
"#;

/// Write a session row straight into the table, bypassing `insert_session`,
/// so a test can describe a row no current code path would produce.
fn seed_session(conn: &Connection, hash: &str, ip: &str, expires: i64) {
    conn.execute(
        "INSERT INTO sessions (hash, ip, created, expires) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![hash, ip, 0_i64, expires],
    )
    .unwrap();
}

/// The same, against the version 1 schema, whose primary key was the cookie
/// itself. Only the migration tests have a database in that shape.
fn seed_v1_session(conn: &Connection, id: &str, ip: &str, expires: i64) {
    conn.execute(
        "INSERT INTO sessions (id, ip, created, expires) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![id, ip, 0_i64, expires],
    )
    .unwrap();
}

/// No file under `.deckjack/` contains this cookie.
///
/// Every file, not just `state.db`: `DROP TABLE` leaves the bytes in freed
/// pages, and in WAL mode a plain `VACUUM` writes its result into `-wal`
/// while the main file keeps the old pages untouched — mtime and all.
/// Checking one file would pass while the cookie sat in the other.
fn assert_scrubbed(working_dir: &std::path::Path, cookie: &SessionId) {
    let plaintext = cookie.encode().into_bytes();
    let dir = working_dir.join(".deckjack");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("the hidden directory") {
        let path = entry.unwrap().path();
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        checked += 1;
        assert!(
            !bytes.windows(plaintext.len()).any(|w| w == plaintext),
            "a session cookie is still readable in {}",
            path.display()
        );
    }
    assert!(checked > 0, "nothing was checked in {}", dir.display());
}

// ---- migrations ---------------------------------------------------------

#[test]
fn a_fresh_directory_arrives_at_the_current_schema_with_every_table_present() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    drop(store);

    // 0 -> SCHEMA_VERSION has to apply every migration, not just the first.
    // Naming the tables is the point: a migration that silently never runs
    // leaves a version number that looks right and a database that is empty.
    let conn = raw(dir.path());
    for table in [
        "sessions",    // 1
        "command_log", // 2
        "command_grants",
        "events",
        "grants",
        "inbox",
        "nodes",
        "windows",
        "window_desktop",
        "window_compact",
        "window_split",
        "window_shell",
        "passkey_user",
        "passkeys",
        "app_grants",
    ] {
        assert!(table_exists(&conn, table), "migrations left out {table}");
    }
    // And migration 3's column, which is the one an ALTER can quietly skip.
    assert!(
        has_column(&conn, "command_log", "issuer_session"),
        "migration 3 did not add command_log.issuer_session"
    );
}

#[test]
fn reopening_a_current_database_changes_neither_its_version_nor_its_schema() {
    let dir = tempfile::tempdir().unwrap();
    let first = Store::open(dir.path()).unwrap();
    drop(first);
    let after_first = schema_dump(&raw(dir.path()));

    // Idempotence means the second and third open are no-ops, not that they
    // merely fail to error: compare the whole schema, not the version.
    for _ in 0..2 {
        let again = Store::open(dir.path()).unwrap();
        assert_eq!(again.schema_version().unwrap(), SCHEMA_VERSION);
    }
    assert_eq!(
        after_first,
        schema_dump(&raw(dir.path())),
        "reopening changed the schema"
    );
}

#[test]
fn a_version_one_database_migrates_forward_and_drops_its_plaintext_sessions() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".deckjack")).unwrap();
    let cookie = a_cookie();
    let expires = SystemTime::now() + Duration::from_secs(3600);
    {
        let conn = raw(dir.path());
        conn.execute_batch(V1_SCHEMA).unwrap();
        seed_v1_session(&conn, &cookie.encode(), "100.69.236.100", unix(expires));
    }

    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);

    // Deliberately lost. The version 1 row's primary key was the cookie
    // itself, and carrying it across by hashing it in place would leave the
    // plaintext in the freed pages — which is the one thing migration 4 is
    // for. The cost is stated where it is paid: everyone signs in again once.
    assert!(
        store.live_sessions(SystemTime::now()).unwrap().is_empty(),
        "migration 4 kept a session row whose key was a live cookie"
    );
    drop(store);

    // And the bytes are gone from the files, not merely unreachable by SQL.
    assert_scrubbed(dir.path(), &cookie);

    let conn = raw(dir.path());
    assert!(table_exists(&conn, "events"), "migration 2 did not run");
    assert!(has_column(&conn, "command_log", "issuer_session"));
}

#[test]
fn a_version_two_database_migrates_forward_with_its_rows_intact() {
    let dir = tempfile::tempdir().unwrap();
    let cookie = a_cookie();
    let expires = SystemTime::now() + Duration::from_secs(3600);

    // Build a genuine v2 file by taking a current one and reversing every
    // migration after 2. Reversing beats transcribing 200 lines of migration
    // 2 into this file — but it has a running cost, and this is the comment
    // that has to be read when it bites:
    //
    //   EVERY NEW MIGRATION MUST BE UNDONE HERE.
    //
    // Forget one and this test fails on the re-run with a message about a
    // duplicate column, which says nothing about the real problem. Adding
    // migration 5 did exactly that.
    {
        let store = Store::open(dir.path()).unwrap();
        drop(store);
        let conn = raw(dir.path());
        conn.execute_batch(
            // 7 and 4: the sessions table is dropped and rebuilt in its
            // version 1 shape below, so `kind` goes with it.
            // 6: the grant owner column.
            "DROP INDEX grants_owner;
             ALTER TABLE grants DROP COLUMN owner;
             -- 5: the four columns that made `events` the audit log.
             DROP INDEX events_dispatch;
             ALTER TABLE events DROP COLUMN dispatch;
             ALTER TABLE events DROP COLUMN risk;
             ALTER TABLE events DROP COLUMN issuer_session;
             ALTER TABLE events DROP COLUMN phase;
             -- 4: the sessions table and the known-address list.
             DROP TABLE sessions;
             DROP TABLE known_addresses;
             -- 3: the issuing session on a command log row.
             DROP INDEX command_log_session;
             ALTER TABLE command_log DROP COLUMN issuer_session;
             CREATE TABLE sessions (
                id      TEXT    PRIMARY KEY,
                ip      TEXT    NOT NULL,
                created INTEGER NOT NULL,
                expires INTEGER NOT NULL
             ) STRICT;
             CREATE INDEX sessions_expires ON sessions (expires);
             PRAGMA user_version = 2;",
        )
        .unwrap();
        assert!(!has_column(&conn, "command_log", "issuer_session"));
        seed_v1_session(&conn, &cookie.encode(), "100.69.236.100", unix(expires));
        // A populated v2 database: a log row an older build wrote.
        conn.execute(
            "INSERT INTO command_log (dispatch, at, node, command, risk, issuer, decision)
             VALUES ('d1', 10, 'n1', 'terminal.write', 'write', 'operator', 'ran')",
            [],
        )
        .unwrap();
    }

    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    // The sessions go, for the reason migration 4 exists; everything else
    // carries forward, which is what the rest of this test checks.
    assert!(
        store.live_sessions(SystemTime::now()).unwrap().is_empty(),
        "migration 4 kept a session row whose key was a live cookie"
    );
    drop(store);

    assert_scrubbed(dir.path(), &cookie);

    let conn = raw(dir.path());
    // The pre-existing row survived the ALTER and reads as "no session known"
    // rather than as some invented value.
    let (command, issuer_session) = conn
        .query_row(
            "SELECT command, issuer_session FROM command_log WHERE dispatch = 'd1'",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .expect("migration 2 -> 3 dropped a command_log row");
    assert_eq!(command, "terminal.write");
    assert_eq!(issuer_session, None);
}

#[test]
fn a_database_written_by_a_newer_schema_is_refused_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let id = an_id();
    {
        let store = Store::open(dir.path()).unwrap();
        store
            .insert_session(&id, &session("127.0.0.1", 3600))
            .unwrap();
        store.checkpoint().unwrap();
        drop(store);
        raw(dir.path())
            .execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
            .unwrap();
    }

    match Store::open(dir.path()) {
        Err(StoreError::FromTheFuture {
            found, supported, ..
        }) => {
            assert_eq!(found, SCHEMA_VERSION + 1);
            assert_eq!(supported, SCHEMA_VERSION);
        }
        Err(other) => panic!("refused for the wrong reason: {other}"),
        Ok(_) => panic!("a database from the future was opened"),
    }

    // Refusing means not touching it: the version is not helpfully reset and
    // the rows are still there for the build that can read them.
    let conn = raw(dir.path());
    let version: u32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        version,
        SCHEMA_VERSION + 1,
        "the refused file was rewritten"
    );
    let count: i64 = conn
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1, "the refused file lost rows");
}

#[test]
fn an_empty_state_file_is_treated_as_a_new_database_not_a_damaged_one() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".deckjack")).unwrap();
    std::fs::write(dir.path().join(DB_PATH), b"").unwrap();

    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
}

#[test]
fn a_state_file_that_is_not_a_database_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".deckjack")).unwrap();
    std::fs::write(dir.path().join(DB_PATH), b"this is not an sqlite file").unwrap();

    let Err(err) = Store::open(dir.path()) else {
        panic!("garbage was accepted as a database");
    };
    // The operator has to be able to tell which file is the problem, and the
    // message must not blame a migration for a file that was never a database.
    let message = err.to_string();
    assert!(
        message.contains(DB_PATH),
        "the error does not name the file: {message}"
    );
    assert!(
        !message.contains("already exists"),
        "a corrupt file was reported as a migration conflict: {message}"
    );
}

#[test]
fn the_state_database_is_created_inside_the_hidden_directory() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!dir.path().join(".deckjack").exists());

    let store = Store::open(dir.path()).unwrap();
    // Hidden, because the file API, WebDAV, previews and agent tools all
    // refuse `.deckjack/` by prefix.
    assert!(Path::new(DB_PATH).starts_with(".deckjack"));
    assert_eq!(
        store.path().strip_prefix(dir.path()).unwrap(),
        Path::new(DB_PATH)
    );
    assert!(dir.path().join(DB_PATH).is_file());
}

#[test]
#[cfg(unix)]
fn the_state_file_is_readable_only_by_the_user_running_the_server() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .insert_session(&an_id(), &session("100.69.236.100", 3600))
        .unwrap();

    // The sessions table holds the cookies a browser presents, so "another
    // account on this host can read it" is the same as "can sign in as me".
    // The side files count: `-wal` holds the rows that have not been folded
    // back yet, which during a session is most of them.
    for name in ["state.db", "state.db-wal", "state.db-shm"] {
        let file = dir.path().join(".deckjack").join(name);
        if !file.exists() {
            continue;
        }
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode & 0o077,
            0,
            "{name} is mode {mode:o}: group or other can read the state database"
        );
    }
}

#[test]
fn a_live_session_cookie_is_never_written_to_the_state_file_in_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let id = a_cookie();
    let store = Store::open(dir.path()).unwrap();
    store
        .insert_session(&id.hash(), &session("100.69.236.100", 3600))
        .unwrap();
    store.checkpoint().unwrap();

    // The rule this file sets for itself: the command log stores a hash of
    // the session id "never the cookie, because this file is readable from
    // the terminal". The sessions table holds a digest for the same reason,
    // so a copy of the directory — a backup, a sync target, another local
    // account that can read it — is state rather than live credentials.
    let bytes = std::fs::read(dir.path().join(DB_PATH)).unwrap();
    let cookie = id.encode().into_bytes();
    assert!(
        !bytes.windows(cookie.len()).any(|w| w == cookie),
        "a live session cookie is sitting in {DB_PATH} in plaintext"
    );
}

#[test]
fn the_migrations_that_turn_the_unused_tables_on_are_actually_applied() {
    // A migration that silently never runs leaves a version number that looks
    // right and a database that is missing a column, so each of the three is
    // checked by the thing it added rather than by the version alone.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    drop(store);
    let conn = raw(dir.path());

    // 5: `events` becomes the audit log. `phase` is the one that matters most
    // — without it a dispatch logged on admission and again on completion is
    // two rows nothing can tell apart.
    for column in ["dispatch", "risk", "issuer_session", "phase"] {
        assert!(
            has_column(&conn, "events", column),
            "migration 5 did not add events.{column}"
        );
    }

    // 6: a standing grant records who holds it, so one sessionless issuer
    // does not inherit the union of every standing grant.
    assert!(
        has_column(&conn, "grants", "owner"),
        "migration 6 did not add grants.owner"
    );

    // 7: a session records how it was proved, which is what step-up needs.
    assert!(
        has_column(&conn, "sessions", "kind"),
        "migration 7 did not add sessions.kind"
    );
}

#[test]
fn an_existing_session_is_read_as_the_weaker_kind_not_the_stronger_one() {
    // Every session that exists when migration 7 runs was created by the
    // one-time code. Defaulting the column to 'passkey' would hand an old
    // row access it never earned.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .insert_session(&an_id(), &session("127.0.0.1", 3600))
        .unwrap();
    drop(store);
    let kind: String = raw(dir.path())
        .query_row("SELECT kind FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(kind, "tether");
}

// ---- WAL and checkpoint -------------------------------------------------

#[test]
fn the_file_itself_is_left_in_wal_mode() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .insert_session(&an_id(), &session("127.0.0.1", 3600))
        .unwrap();
    drop(store);

    // Asked of a connection the Store knows nothing about: WAL has to be a
    // property of the file, or a reader would still block the writer.
    let mode: String = raw(dir.path())
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
}

#[test]
fn checkpoint_folds_the_wal_back_so_the_main_file_alone_carries_the_rows() {
    let dir = tempfile::tempdir().unwrap();
    let id = an_id();
    let store = Store::open(dir.path()).unwrap();
    store
        .insert_session(&id, &session("100.69.236.100", 3600))
        .unwrap();
    store.checkpoint().unwrap();

    // The real claim is about carrying the directory, so copy what a carry
    // would copy — the main file, without `-wal` or `-shm` — and read it.
    let carried = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(carried.path().join(".deckjack")).unwrap();
    std::fs::copy(dir.path().join(DB_PATH), carried.path().join(DB_PATH)).unwrap();
    assert!(!carried.path().join(".deckjack/state.db-wal").exists());

    let moved = Store::open(carried.path()).unwrap();
    let live = moved.live_sessions(SystemTime::now()).unwrap();
    assert_eq!(live.len(), 1, "the checkpointed file lost the session");
    assert_eq!(live[0].0, id);
}

#[test]
fn a_checkpoint_that_could_not_fold_the_wal_is_not_reported_as_success() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .insert_session(&an_id(), &session("127.0.0.1", 3600))
        .unwrap();

    // A second connection parked inside a read transaction is exactly the
    // shutdown race: a browser tab still reading while the server folds the
    // WAL. TRUNCATE cannot complete, and reporting success here would release
    // the lock on a directory whose rows are still only in the `-wal` file.
    let reader = raw(dir.path());
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM sessions;")
        .unwrap();

    let result = store.checkpoint();
    reader.execute_batch("COMMIT").unwrap();
    assert!(
        result.is_err(),
        "a blocked checkpoint claimed the directory was safe to copy"
    );

    // With the reader gone it succeeds, so the failure is about the race and
    // not a database the Store has given up on.
    store.checkpoint().unwrap();
}

#[test]
fn checkpointing_an_in_memory_database_is_not_an_error() {
    // The test and single-node paths use `in_memory`, which has no WAL at
    // all; shutdown must not treat that as a failed checkpoint.
    let store = Store::in_memory().unwrap();
    store
        .insert_session(&an_id(), &session("127.0.0.1", 3600))
        .unwrap();
    store.checkpoint().unwrap();
}

// ---- sessions -----------------------------------------------------------

#[test]
fn an_expired_session_is_deleted_from_the_file_not_merely_filtered_out() {
    let dir = tempfile::tempdir().unwrap();
    let live_id = an_id();
    let dead_id = an_id();
    let store = Store::open(dir.path()).unwrap();
    store
        .insert_session(&live_id, &session("127.0.0.1", 3600))
        .unwrap();

    let mut dead = session("127.0.0.1", 0);
    dead.expires = SystemTime::now() - Duration::from_secs(60);
    store.insert_session(&dead_id, &dead).unwrap();

    let returned = store.live_sessions(SystemTime::now()).unwrap();
    assert_eq!(returned.len(), 1);
    assert_eq!(returned[0].0, live_id);

    // Asked of another connection: a dormant directory must not come back
    // years later carrying every session it ever issued.
    store.checkpoint().unwrap();
    let remaining: i64 = raw(dir.path())
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining, 1, "the expired row was filtered, not deleted");
}

#[test]
fn a_session_whose_expiry_has_just_arrived_is_not_live() {
    let store = Store::in_memory().unwrap();
    let now = SystemTime::now();
    let id = an_id();
    store
        .insert_session(
            &id,
            &Session::new(
                "127.0.0.1".parse().unwrap(),
                now - Duration::from_secs(60),
                now,
            ),
        )
        .unwrap();

    // Expiry is an instant the session does not survive. Getting this
    // backwards hands out one extra second of access per session.
    assert!(
        store.live_sessions(now).unwrap().is_empty(),
        "a session was honored at the instant it expired"
    );
}

#[test]
fn an_expiry_before_the_epoch_counts_as_expired_not_as_far_future() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    drop(store);
    seed_session(&raw(dir.path()), &an_id().hex(), "127.0.0.1", -1);

    // A negative stamp read as unsigned would become an expiry 292 billion
    // years out — a session that never dies.
    let store = Store::open(dir.path()).unwrap();
    assert!(
        store.live_sessions(SystemTime::now()).unwrap().is_empty(),
        "a negative expiry was treated as a live session"
    );
}

#[test]
fn an_absurd_expiry_does_not_take_down_the_load() {
    let dir = tempfile::tempdir().unwrap();
    let good = an_id();
    {
        let store = Store::open(dir.path()).unwrap();
        store
            .insert_session(&good, &session("127.0.0.1", 3600))
            .unwrap();
    }
    let conn = raw(dir.path());
    seed_session(&conn, &an_id().hex(), "127.0.0.1", i64::MAX);
    drop(conn);

    // Nothing stops a shell as the host user from writing this row, and
    // startup is not the place to find out that it panics.
    let store = Store::open(dir.path()).unwrap();
    let live = store.live_sessions(SystemTime::now()).unwrap();
    assert!(
        live.iter().any(|(hash, _)| *hash == good),
        "an absurd expiry hid the sessions around it"
    );
}

#[test]
fn an_unreadable_session_row_is_skipped_and_the_readable_ones_still_load() {
    let dir = tempfile::tempdir().unwrap();
    let good = an_id();
    {
        let store = Store::open(dir.path()).unwrap();
        store
            .insert_session(&good, &session("100.69.236.100", 3600))
            .unwrap();
    }
    let future = unix(SystemTime::now() + Duration::from_secs(3600));
    {
        let conn = raw(dir.path());
        // Three ways a row can be unreadable, all of them reachable from the
        // terminal: a truncated digest, a well-formed digest with something
        // that is not an address beside it, and non-hex junk.
        seed_session(&conn, "too-short", "127.0.0.1", future);
        seed_session(&conn, &"a".repeat(64), "not-an-ip", future);
        seed_session(&conn, &"z".repeat(64), "127.0.0.1", future);
    }

    let store = Store::open(dir.path()).unwrap();
    let live = store
        .live_sessions(SystemTime::now())
        .expect("one bad row failed the whole load");
    assert_eq!(
        live.len(),
        1,
        "a bad row was honored or a good one was lost"
    );
    assert_eq!(live[0].0, good);
}

#[test]
fn reinserting_a_session_id_replaces_it_rather_than_duplicating_it() {
    let dir = tempfile::tempdir().unwrap();
    let id = an_id();
    let store = Store::open(dir.path()).unwrap();
    store
        .insert_session(&id, &session("100.69.236.100", 60))
        .unwrap();
    store
        .insert_session(&id, &session("100.69.236.101", 3600))
        .unwrap();

    let live = store.live_sessions(SystemTime::now()).unwrap();
    assert_eq!(live.len(), 1, "one session id produced two rows");
    // The later write wins: a session that moved address or was extended must
    // not be shadowed by what it used to be.
    assert_eq!(live[0].1.ip.to_string(), "100.69.236.101");
}

#[test]
fn signing_out_a_session_that_is_already_gone_is_not_an_error() {
    let store = Store::in_memory().unwrap();
    let id = an_id();
    store
        .insert_session(&id, &session("127.0.0.1", 3600))
        .unwrap();
    store.delete_session(&id).unwrap();
    // Sign-out arriving twice, or after the session expired, is ordinary.
    store.delete_session(&id).unwrap();
    store.delete_session(&an_id()).unwrap();
    assert!(store.live_sessions(SystemTime::now()).unwrap().is_empty());
}

#[test]
fn the_kill_switch_empties_the_table_and_says_how_many_it_took() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    for _ in 0..3 {
        store
            .insert_session(&an_id(), &session("127.0.0.1", 3600))
            .unwrap();
    }

    assert_eq!(store.delete_all_sessions().unwrap(), 3);
    assert!(store.live_sessions(SystemTime::now()).unwrap().is_empty());
    store.checkpoint().unwrap();
    let remaining: i64 = raw(dir.path())
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining, 0, "revoked sessions were left in the file");
    // Revoking when there is nothing to revoke is still a success.
    assert_eq!(store.delete_all_sessions().unwrap(), 0);
}

#[test]
fn a_second_store_on_the_same_file_sees_what_the_first_one_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let writer = Store::open(dir.path()).unwrap();
    let id = an_id();
    writer
        .insert_session(&id, &session("100.69.236.100", 3600))
        .unwrap();

    // Opening a second Store runs migrate again against a live database. It
    // must be a no-op, and WAL means this read does not wait on the writer.
    let reader = Store::open(dir.path()).unwrap();
    assert_eq!(reader.schema_version().unwrap(), SCHEMA_VERSION);
    let live = reader.live_sessions(SystemTime::now()).unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].0, id);
}
