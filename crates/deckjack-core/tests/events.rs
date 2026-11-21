//! The attribution log, through its public surface only.
//!
//! The claims under test are the ones `events.rs` makes in prose: the log is
//! append-only, the caller cannot choose an entry's time, entries come back in
//! the order they were appended, the three queries the timeline panel uses
//! return exactly what they say, an entry names the issuer without ever
//! carrying a credential, and a megabyte of parameters becomes a summary
//! rather than a megabyte of row.
//!
//! No test sleeps: the clock is injected, the same way `lock.rs` injects its
//! `Env`. Several tests reach the `events` table with hand-written SQL through
//! a second connection, which is deliberate — "append-only" is a claim about
//! what *other* writers can do to the file, and the only way to test it is to
//! be one of them.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deckjack_core::events::{
    Clock, CommandFilter, Entry, EntryId, EventsError, Issuer, IssuerFilter, Log, MAX_LIMIT, Order,
    Outcome, ParamsSummary, Query, Record, SqliteSink,
};
use deckjack_core::store::{DB_PATH, Store};
use rusqlite::{Connection, params};

// ---- fixtures -----------------------------------------------------------

/// A clock the test drives. `Mutex` rather than `RefCell` because [`Clock`]
/// is `Send + Sync`: the log is shared by handlers.
struct TestClock(Mutex<SystemTime>);

impl TestClock {
    fn at(ms: u64) -> Arc<TestClock> {
        Arc::new(TestClock(Mutex::new(at_ms(ms))))
    }

    fn set(&self, ms: u64) {
        *self.0.lock().unwrap() = at_ms(ms);
    }
}

impl Clock for TestClock {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap()
    }
}

/// Millisecond-aligned so an expected timestamp is exact: the column is
/// milliseconds, and a test that used `SystemTime::now()` could not tell
/// truncation from correctness.
fn at_ms(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

/// A migrated, empty state database in a temporary directory. The store owns
/// the schema — the log refuses a database it did not find migrated — so the
/// fixture goes through `Store::open` exactly as the server does.
struct Db {
    dir: tempfile::TempDir,
}

impl Db {
    fn new() -> Db {
        let dir = tempfile::tempdir().unwrap();
        Store::open(dir.path()).unwrap();
        Db { dir }
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join(DB_PATH)
    }

    /// A log on this database, on a clock the test controls.
    fn log(&self, clock: Arc<TestClock>) -> Log {
        Log::new(Arc::new(SqliteSink::open(&self.path()).unwrap()), clock)
    }

    /// A second connection to the same file: what a buggy Deckjack, a
    /// migration or a hand at the SQL prompt would have.
    fn raw(&self) -> Connection {
        Connection::open(self.path()).unwrap()
    }
}

fn rec(issuer: Issuer, command: &str) -> Record {
    Record::new(issuer, command, "node-a", Outcome::Ok)
}

fn commands(entries: &[Entry]) -> Vec<&str> {
    entries.iter().map(|e| e.command.as_str()).collect()
}

fn oldest_first() -> Query {
    Query::default().order(Order::OldestFirst)
}

// ---- append-only --------------------------------------------------------

#[test]
fn the_log_offers_no_way_to_change_or_remove_an_entry() {
    let db = Db::new();
    let log = db.log(TestClock::at(1_000));

    let first = log
        .append(rec(Issuer::Operator, "files.delete").path("/home/me/old.md"))
        .unwrap();

    // Handlers hold a clone of the log. A clone must be able to do exactly
    // what the original can and nothing more — one sink, one append method.
    let handler = log.clone();
    handler.append(rec(Issuer::Cli, "files.read")).unwrap();

    // There is no `log.update(..)` or `log.delete(..)` to call, so the only
    // remaining way to rewrite history is SQL from somewhere else. The
    // migration's triggers refuse it.
    let conn = db.raw();
    let update = conn.execute(
        "UPDATE events SET command = 'files.read' WHERE seq = ?1",
        params![first.inner()],
    );
    assert!(
        update.is_err(),
        "an UPDATE against the audit log must be refused, not silently applied"
    );
    let delete = conn.execute("DELETE FROM events WHERE seq = ?1", params![first.inner()]);
    assert!(
        delete.is_err(),
        "a DELETE against the audit log must be refused, not silently applied"
    );

    // Both entries are still there, unaltered, in order.
    let entries = log.timeline(&oldest_first()).unwrap();
    assert_eq!(commands(&entries), ["files.delete", "files.read"]);
    assert_eq!(
        entries[0].path.as_deref(),
        Some(Path::new("/home/me/old.md"))
    );
}

#[test]
fn a_caller_cannot_choose_or_backdate_an_entrys_time() {
    let db = Db::new();
    let clock = TestClock::at(1_700_000_000_000);
    let log = db.log(clock.clone());

    // `Record` has no `at` field: these two calls can differ only in what the
    // clock says between them.
    log.append(rec(Issuer::Operator, "one")).unwrap();
    clock.set(1_700_000_000_007);
    log.append(rec(Issuer::Operator, "two")).unwrap();

    let entries = log.timeline(&oldest_first()).unwrap();
    assert_eq!(entries[0].at, at_ms(1_700_000_000_000));
    // Milliseconds, not seconds: two commands inside one second are not
    // recorded at the same instant.
    assert_eq!(entries[1].at, at_ms(1_700_000_000_007));

    // The stamp came from the injected clock, not from the wall clock: a
    // window around the real "now" contains nothing.
    let now = SystemTime::now();
    let recent = log
        .timeline(&Query::default().since(now - Duration::from_secs(60)))
        .unwrap();
    assert!(
        recent.is_empty(),
        "entries were stamped from somewhere other than the log's own clock"
    );
}

#[test]
fn entries_come_back_in_append_order_across_a_reopen() {
    let db = Db::new();
    // One frozen instant for every entry, so the only thing that can order
    // them is the sequence. A log that sorted by timestamp would fail here.
    let clock = TestClock::at(5_000);

    let log = db.log(clock.clone());
    for command in ["one", "two", "three"] {
        log.append(rec(Issuer::Operator, command)).unwrap();
    }
    let before_reopen = log.newest().unwrap().unwrap().seq;
    drop(log);

    let reopened = db.log(clock.clone());
    for command in ["four", "five"] {
        reopened.append(rec(Issuer::Operator, command)).unwrap();
    }

    let oldest = reopened.timeline(&oldest_first()).unwrap();
    assert_eq!(commands(&oldest), ["one", "two", "three", "four", "five"]);
    assert!(
        oldest.windows(2).all(|w| w[0].seq < w[1].seq),
        "sequence numbers must increase with append order"
    );
    // A reopen continues the sequence instead of restarting it: a reused
    // number would make two different actions indistinguishable in the log.
    assert!(oldest[3].seq > before_reopen);

    let newest = reopened.timeline(&Query::default()).unwrap();
    assert_eq!(commands(&newest), ["five", "four", "three", "two", "one"]);
}

// ---- the three panel queries -------------------------------------------

#[test]
fn a_time_range_query_returns_only_entries_inside_it() {
    let db = Db::new();
    let clock = TestClock::at(0);
    let log = db.log(clock.clone());

    for (ms, command) in [
        (1_000, "one"),
        (2_000, "two"),
        (3_000, "three"),
        (4_000, "four"),
    ] {
        clock.set(ms);
        log.append(rec(Issuer::Operator, command)).unwrap();
    }

    let inside = log
        .timeline(&oldest_first().range(at_ms(2_000), at_ms(3_000)))
        .unwrap();
    assert_eq!(commands(&inside), ["two", "three"]);

    // An open end means "everything from here on", not "everything".
    let since = log.timeline(&oldest_first().since(at_ms(3_000))).unwrap();
    assert_eq!(commands(&since), ["three", "four"]);

    let until = log.timeline(&oldest_first().until(at_ms(1_000))).unwrap();
    assert_eq!(commands(&until), ["one"]);

    // A window with nothing in it is empty, not the whole log.
    let empty = log
        .timeline(&oldest_first().range(at_ms(2_001), at_ms(2_999)))
        .unwrap();
    assert!(empty.is_empty());
}

#[test]
fn an_issuer_query_returns_only_that_issuer() {
    let db = Db::new();
    let log = db.log(TestClock::at(1_000));

    log.append(rec(Issuer::Operator, "me")).unwrap();
    log.append(rec(Issuer::Cli, "cli")).unwrap();
    log.append(rec(Issuer::System, "reminder")).unwrap();
    log.append(rec(Issuer::agent("claude"), "claude")).unwrap();
    log.append(rec(Issuer::agent_via_window("scribe", "3"), "scribe"))
        .unwrap();
    log.append(rec(
        Issuer::AppUi {
            app: "filer".into(),
        },
        "filer",
    ))
    .unwrap();

    let only = |f: IssuerFilter| {
        let entries = log.timeline(&oldest_first().issuer(f)).unwrap();
        commands(&entries)
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
    };

    assert_eq!(only(IssuerFilter::Operator), ["me"]);
    assert_eq!(only(IssuerFilter::Cli), ["cli"]);
    assert_eq!(only(IssuerFilter::System), ["reminder"]);
    // Every agent, whether or not it is working inside an invited window.
    assert_eq!(only(IssuerFilter::any_agent()), ["claude", "scribe"]);
    assert_eq!(only(IssuerFilter::agent("claude")), ["claude"]);
    // A named session means that session. Not a prefix of it, and not a
    // different case of it — attribution that matched loosely would blame the
    // wrong agent.
    assert!(only(IssuerFilter::agent("clau")).is_empty());
    assert!(only(IssuerFilter::agent("CLAUDE")).is_empty());
    // An app's window is its own issuer, by app or all of them.
    assert_eq!(only(IssuerFilter::AppUi { app: None }), ["filer"]);
    assert_eq!(
        only(IssuerFilter::AppUi {
            app: Some("filer".into())
        }),
        ["filer"]
    );
    assert!(
        only(IssuerFilter::AppUi {
            app: Some("scribe".into())
        })
        .is_empty()
    );
}

#[test]
fn a_command_query_matches_the_command_or_its_namespace_and_nothing_else() {
    let db = Db::new();
    let log = db.log(TestClock::at(1_000));

    for command in [
        "files",
        "files.delete",
        "files.read.status",
        "filesystem.mount",
        "git.push",
    ] {
        log.append(rec(Issuer::Operator, command)).unwrap();
    }

    let matched = |f: CommandFilter| {
        let entries = log.timeline(&oldest_first().command(f)).unwrap();
        commands(&entries)
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
    };

    assert_eq!(matched(CommandFilter::exact("files")), ["files"]);
    assert!(matched(CommandFilter::exact("file")).is_empty());

    // A namespace covers its own bare id and everything under the dot, which
    // is how a grant is written. `filesystem.mount` is a different command
    // that merely starts with the same letters.
    assert_eq!(
        matched(CommandFilter::namespace("files")),
        ["files", "files.delete", "files.read.status"]
    );
    assert_eq!(
        matched(CommandFilter::namespace("files.read")),
        ["files.read.status"]
    );
    assert!(matched(CommandFilter::namespace("gi")).is_empty());
}

// ---- who did it ---------------------------------------------------------

#[test]
fn an_entry_names_the_agent_session_and_the_window_it_was_invited_into() {
    let db = Db::new();
    let log = db.log(TestClock::at(1_000));

    log.append(Record::new(
        Issuer::agent_via_window("Assistant", "3"),
        "files.delete",
        "node-a",
        Outcome::Queued,
    ))
    .unwrap();

    let entry = log.newest().unwrap().unwrap();
    assert_eq!(
        entry.issuer,
        Issuer::Agent {
            session: "Assistant".to_string(),
            window: Some("3".to_string()),
        }
    );
    // The design's own words for the timeline panel.
    assert_eq!(entry.issuer.label(), "Assistant via window 3");
    assert_eq!(entry.outcome, Outcome::Queued);
}

#[test]
fn a_credential_in_the_parameters_is_never_written_to_the_log() {
    let db = Db::new();
    let log = db.log(TestClock::at(1_000));

    // Every one of these is a value that must not reach a file that ships to
    // the VPS. The keys are shapes the bus really produces: a session cookie,
    // a bearer header, a nested passphrase, an API key one level down.
    let secrets = [
        "dj_session_cookie_v1_aaaa",
        "Bearer bbbb_header_value",
        "hunter2_the_passphrase",
        "sk-cccc-api-key",
    ];
    let value = serde_json::json!({
        "cookie": secrets[0],
        "headers": { "authorization": secrets[1] },
        "nested": { "deep": { "passphrase": secrets[2] } },
        "config": { "api_key": secrets[3] },
        "path": "/home/me/notes.md",
    });

    let summary = ParamsSummary::from_json(&value);
    for secret in secrets {
        assert!(
            !summary.as_str().contains(secret),
            "the summary kept a credential: {summary}"
        );
    }
    // Redacting is not the same as summarizing nothing: the harmless
    // parameter is still there, or the panel shows an empty row.
    assert!(summary.as_str().contains("/home/me/notes.md"));

    log.append(
        rec(Issuer::agent("claude"), "auth.login")
            .params(summary)
            .from("fd7a:115c:a1e0::1".parse::<IpAddr>().unwrap()),
    )
    .unwrap();

    // Not just the summary: no column of the stored row carries it. The
    // issuer is a session *name*, never the session id, so the whole row is
    // fair game to check.
    let conn = db.raw();
    let row: String = conn
        .query_row(
            "SELECT COALESCE(issuer_kind,'') || '|' || COALESCE(issuer_name,'') || '|' \
             || COALESCE(issuer_window,'') || '|' || command || '|' || target_node || '|' \
             || params || '|' || outcome || '|' || COALESCE(detail,'') || '|' \
             || COALESCE(path,'') || '|' || COALESCE(from_ip,'') FROM events",
            [],
            |r| r.get(0),
        )
        .unwrap();
    for secret in secrets {
        assert!(!row.contains(secret), "a credential reached the row: {row}");
    }

    // And nothing carrying one survives into what the panel renders.
    let entry = log.newest().unwrap().unwrap();
    let rendered = format!("{entry:?}");
    for secret in secrets {
        assert!(!rendered.contains(secret));
    }
}

// ---- size ---------------------------------------------------------------

#[test]
fn a_huge_parameter_blob_is_summarized_rather_than_stored() {
    let db = Db::new();
    let log = db.log(TestClock::at(1_000));

    let blob = |strings: usize, chars: usize| {
        let items: Vec<serde_json::Value> = (0..strings)
            .map(|i| serde_json::json!(format!("/home/me/documents/file-{i:06}.txt")))
            .collect();
        serde_json::json!({ "text": "x".repeat(chars), "paths": items })
    };

    let megabyte = ParamsSummary::from_json(&blob(20_000, 1_000_000));
    let four_megabytes = ParamsSummary::from_json(&blob(80_000, 4_000_000));

    // The point is not the exact cap but that the row does not grow with the
    // parameters: four times the input is not four times the summary.
    assert!(
        megabyte.as_str().len() < 2_048,
        "a megabyte of parameters produced a {}-byte summary",
        megabyte.as_str().len()
    );
    assert!(four_megabytes.as_str().len() <= megabyte.as_str().len());

    // A single long value is cut too, not only the whole rendering: a 5,000
    // character note in a two-field object must not reach the log intact.
    let note = ParamsSummary::from_json(&serde_json::json!({ "note": "y".repeat(5_000) }));
    assert!(note.as_str().len() < 256, "{}", note.as_str().len());

    log.append(rec(Issuer::Operator, "files.delete").params(megabyte.clone()))
        .unwrap();
    let stored: i64 = db
        .raw()
        .query_row("SELECT length(params) FROM events", [], |r| r.get(0))
        .unwrap();
    assert!(stored < 2_048, "the stored summary is {stored} bytes");
    assert_eq!(log.newest().unwrap().unwrap().params, megabyte);
}

#[test]
fn a_query_cannot_ask_for_more_entries_than_the_cap() {
    let db = Db::new();
    let log = db.log(TestClock::at(1_000));

    // One over the cap, written through the public API: a panel that asks for
    // everything must not be able to pull the whole log into memory.
    for i in 0..=MAX_LIMIT {
        log.append(rec(Issuer::Operator, &format!("c{i}"))).unwrap();
    }

    let panel = log.timeline(&Query::default().limit(usize::MAX)).unwrap();
    assert_eq!(panel.len(), MAX_LIMIT);
    // Newest first, so the cap drops the oldest entry rather than the newest.
    assert_eq!(panel[0].command, format!("c{MAX_LIMIT}"));

    let shipper = log.after(EntryId::START, usize::MAX).unwrap();
    assert_eq!(shipper.len(), MAX_LIMIT);
    assert_eq!(shipper[0].command, "c0");
}

// ---- the shipper's reads ------------------------------------------------

#[test]
fn a_watermark_read_returns_everything_after_it_and_nothing_before() {
    let db = Db::new();
    let clock = TestClock::at(1_000);
    let log = db.log(clock.clone());

    assert!(
        log.newest().unwrap().is_none(),
        "an empty log has no newest entry"
    );

    for command in ["one", "two", "three", "four"] {
        log.append(rec(Issuer::Operator, command)).unwrap();
    }

    let all = log.after(EntryId::START, 100).unwrap();
    assert_eq!(commands(&all), ["one", "two", "three", "four"]);

    // What a shipper does on its second pass: the VPS acknowledged through
    // entry two, so it asks for what followed.
    let rest = log.after(all[1].seq, 100).unwrap();
    assert_eq!(commands(&rest), ["three", "four"]);

    let newest = log.newest().unwrap().unwrap();
    assert_eq!(newest.command, "four");
    assert!(
        log.after(newest.seq, 100).unwrap().is_empty(),
        "a caught-up shipper must get nothing, not the last entry again"
    );
}

#[test]
fn a_newest_first_panel_pages_back_without_repeating_or_skipping_an_entry() {
    let db = Db::new();
    // Every entry in the same millisecond, which is the normal case for a
    // burst of commands and the case a timestamp cursor cannot page.
    let log = db.log(TestClock::at(5_000));
    for i in 0..6 {
        log.append(rec(Issuer::Operator, &format!("c{i}"))).unwrap();
    }

    // Scroll back through the whole log two entries at a time, the way the
    // panel does: each page asks for the entries older than the oldest one
    // it is already showing.
    let mut seen: Vec<String> = Vec::new();
    let mut cursor = None;
    loop {
        let mut query = Query::default().limit(2);
        if let Some(seq) = cursor {
            query = query.before(seq);
        }
        let page = log.timeline(&query).unwrap();
        if page.is_empty() {
            break;
        }
        cursor = Some(page.last().unwrap().seq);
        seen.extend(page.iter().map(|e| e.command.clone()));
        assert!(seen.len() <= 6, "paging repeated entries: {seen:?}");
    }

    // Every entry exactly once, newest first, and the walk terminates.
    assert_eq!(seen, ["c5", "c4", "c3", "c2", "c1", "c0"]);
}

#[test]
fn an_entry_survives_the_json_round_trip_the_shipper_uses() {
    let db = Db::new();
    let log = db.log(TestClock::at(1_700_000_000_123));

    log.append(
        Record::new(
            Issuer::agent_via_window("Assistant", "3"),
            "files.delete",
            "node-r620",
            Outcome::refused("outside the grant"),
        )
        .params(ParamsSummary::text(
            "{\"path\":\"/home/me/notes/ünicode.md\"}",
        ))
        .path("/home/me/notes/ünicode.md")
        .from("fd7a:115c:a1e0::1".parse::<IpAddr>().unwrap()),
    )
    .unwrap();
    log.append(Record::new(
        Issuer::System,
        "sync.run",
        "node-a",
        Outcome::failed("the remote refused"),
    ))
    .unwrap();
    log.append(rec(Issuer::Cli, "window.open")).unwrap();
    log.append(Record::new(
        Issuer::Operator,
        "files.write",
        "node-a",
        Outcome::Queued,
    ))
    .unwrap();

    for entry in log.timeline(&oldest_first()).unwrap() {
        let json = serde_json::to_string(&entry).unwrap();
        let back: Entry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry, "entry did not survive {json}");
    }

    // The wire shape itself, because the VPS end is a different program: a
    // millisecond integer for the time, the outcome flattened beside its
    // detail, and the issuer tagged by kind.
    let refused = log.timeline(&oldest_first()).unwrap().remove(0);
    let value = serde_json::to_value(&refused).unwrap();
    assert_eq!(value["at"], 1_700_000_000_123_i64);
    assert_eq!(value["outcome"], "refused");
    assert_eq!(value["reason"], "outside the grant");
    assert_eq!(value["issuer"]["kind"], "agent");
    assert_eq!(value["issuer"]["session"], "Assistant");
    assert_eq!(value["issuer"]["window"], "3");
    assert_eq!(value["from"], "fd7a:115c:a1e0::1");
}

#[test]
fn two_writers_on_one_database_neither_lose_an_entry_nor_share_a_sequence() {
    let db = Db::new();
    let clock = TestClock::at(1_000);

    // Separate `Log`s, so separate SQLite connections to one file: the real
    // arrangement, since the log opens its own connection rather than
    // borrowing the store's. SQLite allows one writer at a time, and an
    // entry that loses the race must wait rather than be dropped.
    let mut writers = Vec::new();
    for w in 0..4 {
        let log = db.log(clock.clone());
        writers.push(std::thread::spawn(move || {
            for i in 0..50 {
                log.append(rec(Issuer::agent(format!("w{w}")), &format!("c{i}")))
                    .unwrap();
            }
        }));
    }
    for w in writers {
        w.join().unwrap();
    }

    let all = db.log(clock).after(EntryId::START, usize::MAX).unwrap();
    assert_eq!(all.len(), 200, "an entry was lost under concurrent writers");
    let mut seqs: Vec<i64> = all.iter().map(|e| e.seq.inner()).collect();
    seqs.sort_unstable();
    seqs.dedup();
    // A shared sequence number would make two different actions one row to
    // every reader downstream, the shipper included.
    assert_eq!(seqs.len(), 200, "two entries were given the same sequence");
}

// ---- bad data -----------------------------------------------------------

#[test]
fn an_unreadable_row_is_an_error_not_a_silently_dropped_entry() {
    // An entry that cannot be understood means an action happened and is no
    // longer attributed to anyone. Skipping the row would hide that.
    let unknown_kind = Db::new();
    unknown_kind
        .raw()
        .execute(
            "INSERT INTO events (at_ms, issuer_kind, command, target_node, params, outcome) \
             VALUES (1000, 'robot', 'files.delete', 'node-a', '', 'ok')",
            [],
        )
        .unwrap();
    let err = unknown_kind
        .log(TestClock::at(1_000))
        .timeline(&Query::default())
        .unwrap_err();
    assert!(
        matches!(
            err,
            EventsError::MalformedEntry {
                field: "issuer_kind",
                ..
            }
        ),
        "expected a malformed-entry error, got {err}"
    );

    // An agent entry with no session name is the same problem wearing a valid
    // kind: it says an agent did it and cannot say which.
    let nameless_agent = Db::new();
    nameless_agent
        .raw()
        .execute(
            "INSERT INTO events (at_ms, issuer_kind, command, target_node, params, outcome) \
             VALUES (1000, 'agent', 'files.delete', 'node-a', '', 'ok')",
            [],
        )
        .unwrap();
    let err = nameless_agent
        .log(TestClock::at(1_000))
        .timeline(&Query::default())
        .unwrap_err();
    assert!(
        matches!(
            err,
            EventsError::MalformedEntry {
                field: "issuer_name",
                ..
            }
        ),
        "expected a malformed-entry error, got {err}"
    );
}

#[test]
fn an_unparseable_client_address_does_not_lose_the_entry() {
    let db = Db::new();
    db.raw()
        .execute(
            "INSERT INTO events (at_ms, issuer_kind, command, target_node, params, outcome, from_ip) \
             VALUES (1000, 'operator', 'files.delete', 'node-a', '', 'ok', 'not-an-address')",
            [],
        )
        .unwrap();

    // The address is evidence; the attribution is the record. Losing the
    // whole entry over a bad address would be the wrong trade.
    let entry = db.log(TestClock::at(1_000)).newest().unwrap().unwrap();
    assert_eq!(entry.command, "files.delete");
    assert_eq!(entry.issuer, Issuer::Operator);
    assert!(entry.from.is_none());
}

#[test]
fn the_log_refuses_a_database_that_has_not_been_migrated() {
    let dir = tempfile::tempdir().unwrap();
    // A log that created its own table would hide a missing migration, and
    // the entries would then live in a file nothing else reads.
    let err = Log::sqlite(&dir.path().join("fresh.db")).unwrap_err();
    assert!(
        matches!(err, EventsError::NotMigrated { .. }),
        "expected a not-migrated error, got {err}"
    );
}

#[test]
fn a_file_action_on_a_path_that_is_not_utf8_is_still_recorded() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let db = Db::new();
    let log = db.log(TestClock::at(1_000));

    // Linux paths are bytes. A mangled path in the log beats no record that
    // the delete happened at all.
    let raw = PathBuf::from(OsStr::from_bytes(b"/home/me/bad-\xff-name.md"));
    log.append(rec(Issuer::Operator, "files.delete").path(raw))
        .unwrap();

    let entry = log.newest().unwrap().unwrap();
    assert_eq!(entry.command, "files.delete");
    let path = entry.path.expect("the path must survive, lossily or not");
    assert!(path.to_string_lossy().starts_with("/home/me/bad-"));
}

// ---- shape --------------------------------------------------------------

#[test]
fn the_log_can_be_held_across_an_await_by_every_handler() {
    // The bus spawns handlers, so a `Log` that was not `Send + Sync` would
    // make every command future non-`Send` — a whole-system compile failure
    // with a one-line cause. Asserting it here fails in this file instead.
    fn assert_send_sync<T: Send + Sync + 'static>() {}
    assert_send_sync::<Log>();
    assert_send_sync::<Entry>();
    assert_send_sync::<Record>();
}
