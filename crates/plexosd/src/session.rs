//! Administrator browser sessions (ADR-0019).
//!
//! A pairing code is spent once and produces one of these: an opaque 256-bit string the
//! browser holds in `sessionStorage` and sends as `Authorization: Bearer`, exactly the way
//! the device token is sent. Every route that already demanded the device token accepts
//! either, and no route knows which it got — that is the whole of the integration, and
//! [`crate::auth::authenticate`] is the one place that decides.
//!
//! # Why not a JWT
//!
//! There is nothing to put in it. A JWT exists so that a server can verify a claim without
//! looking anything up, which matters when the thing verifying is not the thing that
//! issued. Here they are the same process, on the same machine, with sixteen possible
//! sessions in a `Vec`. A signed token would add a signing key, a clock dependency and a
//! parser for attacker-supplied structure, in exchange for avoiding a linear scan of at
//! most sixteen strings.
//!
//! An opaque random value also has a property a JWT cannot have: it can be revoked. A
//! signed token is valid until it expires because that is what "self-describing" means, and
//! revoking one needs the lookup table it was supposed to remove.
//!
//! # Two deadlines, and they do different jobs
//!
//! [`IDLE_TIMEOUT`] ends a session nobody is using — the tab left open on a laptop that
//! went in a bag. [`ABSOLUTE_LIFETIME`] ends one that is being used, and is the one that
//! cannot be extended: without it, a browser polling the status page every three seconds
//! would hold an administrator session open for ever, which is precisely what the console
//! does. A session that renews itself indefinitely on the strength of the page's own
//! polling is not a session, it is a permanent credential in a browser's memory.
//!
//! Both are measured on [`Instant`], for the reason [`crate::pairing`] gives at greater
//! length: this appliance has no clock synchronisation, so a wall-clock deadline is one an
//! NTP correction can move.
//!
//! # Nothing here survives anything
//!
//! No file, no `/var`, no `/run`. A restarted `plexosd` has no sessions, a rebooted
//! appliance has no sessions, and a rollback to a release that has never heard of this
//! module has no sessions and does not need any — the device token is untouched and is
//! still the way in. That is the whole of this feature's rollback story, and it is short
//! because the state is deliberately not persistent.

use std::io::Read as _;
use std::time::{Duration, Instant};

/// Bytes of entropy in a session token.
///
/// 32, which is 256 bits. Larger than the device token's 80 bits for a reason that is
/// about people rather than cryptography: nobody transcribes this one, so the argument
/// that made the device token short — entropy nobody can type is an obstacle to the person
/// who owns the device — does not apply, and there is no reason to spend less.
pub const TOKEN_BYTES: usize = 32;

/// How long a session lasts however much it is used.
///
/// Twelve hours. The console polls several endpoints continuously while a tab is open, so
/// an idle timeout alone would never fire on a page somebody left up — this is the deadline
/// that makes "an administrator session" a session rather than a credential that happens to
/// live in a browser.
pub const ABSOLUTE_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);

/// How long a session lasts with nothing using it.
///
/// An hour. A closed laptop, a tab in a background window, a phone that locked: none of
/// them is a person administering an appliance, and all of them leave a credential sitting
/// in a browser.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// How many sessions may be live at once.
///
/// Sixteen. One appliance and one administrator, so this is not a capacity figure — it is
/// a bound, so that a map growing without limit is not a thing an attacker can arrange.
/// A phone, a laptop and a desktop is three; sixteen leaves room for a fortnight of tabs
/// nobody closed before any of it matters.
pub const MAX_SESSIONS: usize = 16;

/// One live session, as the server holds it.
///
/// The token itself is not here. Only its digest is kept, which is the same reasoning
/// ADR-0013 gives for the device token and applies with less force — this one is in memory
/// rather than on a disk somebody can pull — but costs nothing and means a core dump of
/// `plexosd` yields nothing presentable.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Session {
    digest: String,
    created_at: Instant,
    last_used_at: Instant,
}

impl Session {
    /// Whether this session is still good at `now`.
    fn is_live(&self, now: Instant) -> bool {
        now.duration_since(self.created_at) < ABSOLUTE_LIFETIME
            && now.duration_since(self.last_used_at) < IDLE_TIMEOUT
    }
}

/// Every live administrator session.
///
/// A struct with an injected clock rather than a bare global, for the reason
/// [`crate::pairing::Offers`] gives: Rust runs tests as threads in one process, and a suite
/// built on a global is a suite where one test revokes what another is about to validate.
#[derive(Debug, Default)]
pub struct Store {
    sessions: Vec<Session>,
}

impl Store {
    /// A store with nothing in it, which is what a started daemon has.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `token` as a live session.
    ///
    /// When the store is full the least recently used session is dropped rather than the
    /// new one being refused. Refusing is the tempting reading and it is the wrong one: it
    /// would let sixteen abandoned tabs — or sixteen deliberate pairings by somebody who
    /// reached the screen once — lock the owner out of their own appliance, which is
    /// ADR-0013's argument against a lockout wearing different clothes. Sessions are cheap
    /// and the device token is always the way back regardless.
    pub fn issue(&mut self, token: &str, now: Instant) {
        self.prune(now);

        if self.sessions.len() >= MAX_SESSIONS {
            // Oldest use first. `min_by_key` on an Instant is fine here because every
            // instant in the store was taken from the same monotonic clock.
            if let Some(index) = self
                .sessions
                .iter()
                .enumerate()
                .min_by_key(|(_, session)| session.last_used_at)
                .map(|(index, _)| index)
            {
                self.sessions.remove(index);
            }
        }

        self.sessions.push(Session {
            digest: crate::auth::digest(token),
            created_at: now,
            last_used_at: now,
        });
    }

    /// Whether `presented` is a live session, and marks it used if it is.
    ///
    /// The idle deadline is pushed forward here and only here, and only on success. A
    /// failed attempt must not extend anything: otherwise anyone who can reach the port
    /// keeps every abandoned session alive for ever by presenting rubbish to it.
    ///
    /// The absolute deadline is never touched, which is what stops the console's own
    /// polling from renewing a session indefinitely.
    pub fn validate(&mut self, presented: &str, now: Instant) -> bool {
        let digest = crate::auth::digest(presented);

        // Every session is compared, and the comparison is constant-time, so neither how
        // many sessions exist nor which one matched is measurable from outside.
        let mut found = None;
        for (index, session) in self.sessions.iter().enumerate() {
            if crate::auth::constant_time_eq(&digest, &session.digest) && session.is_live(now) {
                found = Some(index);
            }
        }

        match found {
            Some(index) => {
                self.sessions[index].last_used_at = now;
                true
            }
            None => false,
        }
    }

    /// Ends the session `presented` names, if it names one.
    ///
    /// Returns whether anything was revoked. Signing out of a session that has already
    /// expired is not an error — the browser asking has no way to know, and the outcome it
    /// wanted is the outcome it gets.
    pub fn revoke(&mut self, presented: &str) -> bool {
        let digest = crate::auth::digest(presented);
        let before = self.sessions.len();
        self.sessions
            .retain(|session| !crate::auth::constant_time_eq(&digest, &session.digest));
        self.sessions.len() != before
    }

    /// Ends every session, returning how many there were.
    ///
    /// Called when the device token is rotated. Rotating is what somebody does when a
    /// credential has leaked, and leaving browsers that were admitted under the old one
    /// still administering would make rotation a change of password that logs nobody out.
    pub fn revoke_all(&mut self) -> usize {
        let count = self.sessions.len();
        self.sessions.clear();
        count
    }

    /// Drops sessions past either deadline, returning how many went.
    ///
    /// Opportunistic: called from [`Self::issue`] and from the status count, never from a
    /// timer. A session nobody is asking about costs a few dozen bytes, and a thread whose
    /// only job is to remove them would be machinery in place of a measurement.
    pub fn prune(&mut self, now: Instant) -> usize {
        let before = self.sessions.len();
        self.sessions.retain(|session| session.is_live(now));
        before - self.sessions.len()
    }

    /// How many sessions are live at `now`.
    #[must_use]
    pub fn live(&self, now: Instant) -> usize {
        self.sessions
            .iter()
            .filter(|session| session.is_live(now))
            .count()
    }
}

/// Reads a fresh session token from the kernel.
///
/// Hex rather than the token alphabet: this is never read by a person, and hex is what
/// every other machine-only identifier in this crate uses.
///
/// # Errors
/// Only if `/dev/urandom` cannot be read.
pub fn generate() -> std::io::Result<String> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(crate::auth::hex(&bytes))
}

/// The appliance's sessions.
static CURRENT: std::sync::Mutex<Store> = std::sync::Mutex::new(Store {
    sessions: Vec::new(),
});

fn store() -> std::sync::MutexGuard<'static, Store> {
    CURRENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Issues a session token and records it.
///
/// # Errors
/// If randomness cannot be read.
pub fn issue() -> std::io::Result<String> {
    let token = generate()?;
    store().issue(&token, Instant::now());
    Ok(token)
}

/// Whether `presented` is a live session, marking it used if so.
#[must_use]
pub fn validate(presented: &str) -> bool {
    store().validate(presented, Instant::now())
}

/// Ends the session `presented` names.
///
/// Returns whether anything was ended, which every caller uses: the sign-out route
/// reports it and the tests assert their own cleanup worked.
#[must_use]
pub fn revoke(presented: &str) -> bool {
    store().revoke(presented)
}

/// Ends every session. Returns how many there were.
#[must_use]
pub fn revoke_all() -> usize {
    store().revoke_all()
}

/// How many sessions are live right now.
#[must_use]
pub fn live() -> usize {
    let now = Instant::now();
    let mut store = store();
    store.prune(now);
    store.live(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A base to build a timeline forwards from. See `pairing`'s note on why forwards.
    fn timeline() -> Instant {
        Instant::now()
    }

    const A: &str = "aaaa1111";
    const B: &str = "bbbb2222";

    #[test]
    fn a_generated_token_carries_the_entropy_the_adr_claims() {
        let token = generate().expect("/dev/urandom");
        assert_eq!(token.len(), TOKEN_BYTES * 2, "256 bits as hex");
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()), "{token}");
    }

    #[test]
    fn two_generated_tokens_differ() {
        assert_ne!(generate().unwrap(), generate().unwrap());
    }

    #[test]
    fn an_issued_session_authorises_and_an_unknown_one_does_not() {
        let t0 = timeline();
        let mut store = Store::new();
        store.issue(A, t0);

        assert!(store.validate(A, t0));
        assert!(!store.validate(B, t0), "a token nobody issued is nobody's");
    }

    #[test]
    fn a_new_store_knows_nothing_which_is_what_a_reboot_leaves() {
        // The whole of this feature's persistence story. A restarted daemon, a rebooted
        // appliance and a rollback all arrive here.
        let store = Store::new();
        assert_eq!(store.live(timeline()), 0);
    }

    #[test]
    fn a_session_expires_after_an_hour_with_nothing_using_it() {
        let t0 = timeline();
        let mut store = Store::new();
        store.issue(A, t0);

        let nearly = t0 + Duration::from_secs(IDLE_TIMEOUT.as_secs() - 1);
        assert!(store.validate(A, nearly), "still inside the hour");

        // And from that use, another hour -- measured from the use rather than from the
        // issue, which is what "idle" means.
        let mut store = Store::new();
        store.issue(A, t0);
        assert!(
            !store.validate(A, t0 + IDLE_TIMEOUT),
            "an hour idle is over"
        );
    }

    #[test]
    fn using_a_session_pushes_its_idle_deadline_forward() {
        let t0 = timeline();
        let mut store = Store::new();
        store.issue(A, t0);

        // Used every half hour for four hours: never idle, so never expired.
        let mut when = t0;
        for _ in 0..8 {
            when += Duration::from_secs(30 * 60);
            assert!(store.validate(A, when), "used, so not idle");
        }
        assert!(when.duration_since(t0) > IDLE_TIMEOUT * 3);
    }

    #[test]
    fn a_failed_attempt_extends_nothing() {
        // Otherwise anyone who can reach the port keeps every abandoned session alive for
        // ever by presenting rubbish at it, which turns the idle timeout off for exactly
        // the sessions it exists to end.
        let t0 = timeline();
        let mut store = Store::new();
        store.issue(A, t0);

        let half = t0 + Duration::from_secs(IDLE_TIMEOUT.as_secs() / 2);
        assert!(!store.validate("not-a-session", half));

        assert!(
            !store.validate(A, t0 + IDLE_TIMEOUT),
            "the real session's hour ran from when it was last used, not from the noise"
        );
    }

    #[test]
    fn no_amount_of_use_gets_a_session_past_twelve_hours() {
        // The deadline that makes this a session. The console polls several endpoints
        // continuously, so an idle timeout alone would never fire on a page left open --
        // and a credential that renews itself on the strength of its own page's polling is
        // not a session.
        let t0 = timeline();
        let mut store = Store::new();
        store.issue(A, t0);

        let mut when = t0;
        for _ in 0..23 {
            when += Duration::from_secs(30 * 60);
            let expected = when.duration_since(t0) < ABSOLUTE_LIFETIME;
            assert_eq!(
                store.validate(A, when),
                expected,
                "at {:?} after issue",
                when.duration_since(t0)
            );
        }
        assert!(!store.validate(A, t0 + ABSOLUTE_LIFETIME));
    }

    #[test]
    fn the_absolute_deadline_is_measured_from_issue_and_not_from_last_use() {
        let t0 = timeline();
        let mut store = Store::new();
        store.issue(A, t0);

        // Kept in use throughout, because a session left alone for twelve hours dies on
        // the idle deadline first and would prove nothing about this one. That is the
        // mistake this test was written with: it asserted the absolute deadline against a
        // session the idle timeout had already ended eleven hours earlier.
        let mut when = t0;
        for _ in 0..23 {
            when += Duration::from_secs(30 * 60);
            assert!(store.validate(A, when), "in use, so not idle");
        }
        assert_eq!(
            when.duration_since(t0),
            Duration::from_secs(11 * 3600 + 1800)
        );

        let late = t0 + Duration::from_secs(ABSOLUTE_LIFETIME.as_secs() - 60);
        assert!(store.validate(A, late), "a minute left");
        assert!(
            !store.validate(A, late + Duration::from_secs(120)),
            "and using it a minute before the end did not buy another twelve hours"
        );
    }

    #[test]
    fn signing_out_ends_that_session_and_leaves_the_others() {
        let t0 = timeline();
        let mut store = Store::new();
        store.issue(A, t0);
        store.issue(B, t0);

        assert!(store.revoke(A));
        assert!(!store.validate(A, t0), "signed out");
        assert!(store.validate(B, t0), "the other browser is untouched");
    }

    #[test]
    fn signing_out_of_an_already_dead_session_is_not_an_error_worth_making() {
        let mut store = Store::new();
        assert!(!store.revoke(A), "nothing was revoked");
        // And the caller's desired state -- not authenticated -- is the state it is in.
        assert!(!store.validate(A, timeline()));
    }

    #[test]
    fn rotating_the_recovery_code_ends_every_session() {
        // Rotation is what somebody does when a credential has leaked. Leaving browsers
        // admitted under the old one still administering would make it a password change
        // that logs nobody out.
        let t0 = timeline();
        let mut store = Store::new();
        store.issue(A, t0);
        store.issue(B, t0);

        assert_eq!(store.revoke_all(), 2);
        assert!(!store.validate(A, t0));
        assert!(!store.validate(B, t0));
    }

    #[test]
    fn the_store_is_bounded_and_drops_the_least_recently_used() {
        // Not refused: sixteen abandoned tabs must not be able to lock the owner out of
        // their own appliance, which is ADR-0013's argument against lockouts in different
        // clothes.
        let t0 = timeline();
        let mut store = Store::new();

        for n in 0..MAX_SESSIONS {
            store.issue(&format!("token-{n}"), t0 + Duration::from_secs(n as u64));
        }
        assert_eq!(store.live(t0), MAX_SESSIONS);

        // token-0 is the least recently used, so it is the one that goes.
        let later = t0 + Duration::from_secs(1_000);
        store.issue("newcomer", later);

        assert_eq!(store.live(later), MAX_SESSIONS, "still bounded");
        assert!(store.validate("newcomer", later), "and the newcomer is in");
        assert!(!store.validate("token-0", later), "the oldest use went");
        assert!(store.validate("token-1", later), "and nothing else did");
    }

    #[test]
    fn expired_sessions_are_reclaimed_before_anything_is_evicted() {
        // A store full of yesterday's sessions must not evict a live one to make room.
        let t0 = timeline();
        let mut store = Store::new();
        for n in 0..MAX_SESSIONS {
            store.issue(&format!("old-{n}"), t0);
        }

        let tomorrow = t0 + ABSOLUTE_LIFETIME + Duration::from_secs(1);
        store.issue("fresh", tomorrow);

        assert_eq!(
            store.live(tomorrow),
            1,
            "the old ones were dead, not evicted"
        );
        assert!(store.validate("fresh", tomorrow));
    }

    #[test]
    fn a_dead_session_is_not_counted_as_live() {
        let t0 = timeline();
        let mut store = Store::new();
        store.issue(A, t0);

        let after = t0 + ABSOLUTE_LIFETIME;
        assert_eq!(store.live(after), 0);
        assert_eq!(store.prune(after), 1);
        assert_eq!(store.live(after), 0, "and pruning is idempotent");
    }

    #[test]
    fn the_token_itself_is_never_held() {
        // A core dump of plexosd should yield nothing presentable. Cheap, and the same
        // reasoning ADR-0013 gives for the device token's file.
        let t0 = timeline();
        let mut store = Store::new();
        store.issue("the-secret-value", t0);

        let held = format!("{store:?}");
        assert!(
            !held.contains("the-secret-value"),
            "the store must hold a digest and not a token: {held}"
        );
        assert!(held.contains(&crate::auth::digest("the-secret-value")));
    }

    #[test]
    fn the_deadlines_are_the_ones_the_adr_states() {
        assert_eq!(ABSOLUTE_LIFETIME, Duration::from_secs(43_200));
        assert_eq!(IDLE_TIMEOUT, Duration::from_secs(3_600));
        assert_eq!(TOKEN_BYTES * 8, 256);
    }
}
