//! The ticket table and the target text, through their public API.
//!
//! The socket-level behavior (a ticket runs a call once, on one socket, for
//! one set of parameters, within a minute) is asserted end to end in
//! `wiring.rs` against the assembled node. These cover what is cheaper and
//! sharper to say here: the parameter digest's notion of "the same call",
//! the bound on how many tickets one socket can park, and what the prompt's
//! target text keeps and drops.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use deckjack_core::auth::session::SessionId;
use deckjack_core::command::{CommandId, Issuer, name_paths, plain_text};
use deckjack_core::ticket::{self, Binding, Refusal, Tickets};
use deckjack_core::ws::{Clock, ConnectionId};
use serde_json::json;

struct Wound {
    ahead: Mutex<Duration>,
    base: Instant,
}

impl Wound {
    fn new() -> Arc<Wound> {
        Arc::new(Wound {
            ahead: Mutex::new(Duration::ZERO),
            base: Instant::now(),
        })
    }

    fn advance(&self, by: Duration) {
        *self.ahead.lock().unwrap() += by;
    }
}

impl Clock for Wound {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + *self.ahead.lock().unwrap()
    }

    fn mono(&self) -> Instant {
        self.base + *self.ahead.lock().unwrap()
    }
}

fn command(id: &str) -> CommandId {
    CommandId::new(id).expect("a valid command id")
}

fn binding(connection: u64, issuer: &Issuer, params: serde_json::Value) -> Binding {
    Binding::new(
        ConnectionId(connection),
        issuer.clone(),
        command("files.trash.empty"),
        ticket::params_digest(&params),
        None,
    )
}

#[test]
fn the_same_call_written_with_its_keys_in_another_order_is_the_same_call() {
    let tickets = Tickets::new(Wound::new());
    let me = Issuer::browser(SessionId::generate().unwrap());
    let ticket = tickets
        .issue(binding(
            1,
            &me,
            json!({ "a": 1, "b": [1, 2, { "y": 1, "x": 2 }] }),
        ))
        .unwrap();
    assert_eq!(
        tickets.redeem(
            ticket.reveal(),
            &binding(1, &me, json!({ "b": [1, 2, { "x": 2, "y": 1 }], "a": 1 })),
        ),
        Ok(()),
        "key order is not part of what was confirmed"
    );

    // Array order is: `[a, b]` and `[b, a]` can be different operations.
    let ticket = tickets
        .issue(binding(1, &me, json!({ "ids": ["a", "b"] })))
        .unwrap();
    assert_eq!(
        tickets.redeem(
            ticket.reveal(),
            &binding(1, &me, json!({ "ids": ["b", "a"] }))
        ),
        Err(Refusal::OtherParams)
    );
}

#[test]
fn a_ticket_is_128_random_bits_and_never_repeats() {
    let tickets = Tickets::new(Wound::new());
    let me = Issuer::browser(SessionId::generate().unwrap());
    let a = tickets.issue(binding(1, &me, json!({}))).unwrap();
    let b = tickets.issue(binding(1, &me, json!({}))).unwrap();
    for t in [&a, &b] {
        assert_eq!(t.reveal().len(), 32, "128 bits as hex");
        assert!(t.reveal().chars().all(|c| c.is_ascii_hexdigit()));
    }
    assert_ne!(a.reveal(), b.reveal());
    assert!(
        !format!("{a:?}").contains(a.reveal()),
        "a ticket's Debug must not print the ticket"
    );
}

#[test]
fn another_issuer_on_the_same_connection_is_refused() {
    // Stage 3 gives app frames their own issuer on the shell's socket. A
    // ticket the shell was given must not be redeemable as the app, or the
    // reverse, even though the connection matches.
    let tickets = Tickets::new(Wound::new());
    let shell = Issuer::browser(SessionId::generate().unwrap());
    let ticket = tickets.issue(binding(1, &shell, json!({}))).unwrap();
    assert_eq!(
        tickets.redeem(ticket.reveal(), &binding(1, &Issuer::operator(), json!({}))),
        Err(Refusal::OtherIssuer)
    );
    // And the presentation spent it.
    assert_eq!(
        tickets.redeem(ticket.reveal(), &binding(1, &shell, json!({}))),
        Err(Refusal::Unknown)
    );
}

#[test]
fn expiry_is_measured_on_the_injected_clock() {
    let clock = Wound::new();
    let tickets = Tickets::new(Arc::clone(&clock) as Arc<dyn Clock>);
    let me = Issuer::browser(SessionId::generate().unwrap());

    let on_time = tickets.issue(binding(1, &me, json!({}))).unwrap();
    clock.advance(ticket::TTL);
    assert_eq!(
        tickets.redeem(on_time.reveal(), &binding(1, &me, json!({}))),
        Ok(()),
        "a ticket is good for its whole minute"
    );

    let late = tickets.issue(binding(1, &me, json!({}))).unwrap();
    clock.advance(ticket::TTL + Duration::from_secs(1));
    assert_eq!(
        tickets.redeem(late.reveal(), &binding(1, &me, json!({}))),
        Err(Refusal::Expired)
    );
}

#[test]
fn one_socket_cannot_park_unbounded_tickets_and_the_newest_survive() {
    let clock = Wound::new();
    let tickets = Tickets::new(Arc::clone(&clock) as Arc<dyn Clock>);
    let me = Issuer::browser(SessionId::generate().unwrap());

    let mut issued = Vec::new();
    for _ in 0..ticket::MAX_PER_CONNECTION + 4 {
        // Distinct issue times, so "oldest" is well defined.
        clock.advance(Duration::from_millis(1));
        issued.push(tickets.issue(binding(7, &me, json!({}))).unwrap());
    }
    // Another socket's ticket is not evicted by this one's flood.
    let theirs = tickets.issue(binding(8, &me, json!({}))).unwrap();

    assert_eq!(tickets.outstanding(), ticket::MAX_PER_CONNECTION + 1);
    assert_eq!(
        tickets.redeem(issued[0].reveal(), &binding(7, &me, json!({}))),
        Err(Refusal::Unknown),
        "the oldest ticket made room"
    );
    assert_eq!(
        tickets.redeem(issued.last().unwrap().reveal(), &binding(7, &me, json!({}))),
        Ok(()),
        "the ticket the operator is looking at right now still works"
    );
    assert_eq!(
        tickets.redeem(theirs.reveal(), &binding(8, &me, json!({}))),
        Ok(())
    );
}

#[test]
fn target_text_drops_bidi_controls_and_invisible_characters() {
    assert_eq!(
        plain_text("photo\u{202E}gpj.exe", 160),
        "photogpj.exe",
        "an override would render this as photoexe.jpg"
    );
    assert_eq!(
        plain_text("a\u{2066}b\u{2069}c\u{200F}d\u{061C}e", 160),
        "abcde"
    );
    assert_eq!(plain_text("same\u{200B}name\u{FEFF}", 160), "samename");
    assert_eq!(plain_text("tag\u{E0041}s", 160), "tags");
    assert_eq!(plain_text("bell\u{7}\u{1b}[31m", 160), "bell[31m");
    // A newline would push the rest of the target out of a one-line view;
    // it becomes a space rather than gluing two words together.
    assert_eq!(plain_text("  first\n\n\tsecond  ", 160), "first second");
    // Ordinary non-ASCII is kept: a name in Greek is not an attack.
    assert_eq!(
        plain_text("/σημειώσεις/αρχείο.txt", 160),
        "/σημειώσεις/αρχείο.txt"
    );
}

#[test]
fn long_target_text_loses_its_middle_and_keeps_both_ends() {
    let long = format!("/start/{}/the-file.txt", "middle/".repeat(40));
    let cut = plain_text(&long, 40);
    assert_eq!(cut.chars().count(), 40, "got {cut}");
    assert!(cut.starts_with("/start/"), "got {cut}");
    assert!(cut.ends_with("/the-file.txt"), "got {cut}");
    assert!(cut.contains('\u{2026}'), "the cut is marked: {cut}");
    // Short text is left alone.
    assert_eq!(plain_text("/a.txt", 40), "/a.txt");
}

#[test]
fn several_paths_show_three_and_say_how_many_more() {
    assert_eq!(name_paths(Vec::<String>::new()), None);
    assert_eq!(name_paths(["/a"]).as_deref(), Some("/a"));
    assert_eq!(
        name_paths(["/a", "/b", "/c", "/d", "/e"]).as_deref(),
        Some("/a, /b, /c and 2 more")
    );
}
