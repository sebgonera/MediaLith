//! One authorised browser approving another (ADR-0019).
//!
//! [`crate::pairing`] answers "how does the *first* browser get in", and its answer is
//! physical presence: press P at the machine's own screen. This answers the question that
//! comes next, an hour later, at a desktop on the other side of the house — and the wrong
//! answer to it is the one that undoes the first: fetching the recovery device code out of
//! a drawer and typing sixteen characters, which is exactly the ceremony the QR replaced.
//!
//! So: the desktop asks, the phone approves, and MediaLith issues the desktop a session of
//! its own.
//!
//! # The phone is not a relay, and that is the whole design
//!
//! The obvious implementation hands the phone's session token to the desktop, and it is
//! wrong in a way that is easy to miss because it *works*. A credential that travels
//! between browsers is one that exists in two places, cannot be revoked in one of them,
//! and turns "sign this desktop out" into a question about which copy. There is no
//! mechanism here for moving a session, and the tests say so in as many words.
//!
//! What the phone sends is a sentence: *I approve request X.* Everything else — creating
//! the session, deciding its lifetime, handing it over — is this appliance's own work, and
//! it is [`crate::session`]'s existing work rather than a second implementation of it.
//!
//! # Two values, and neither is enough alone
//!
//! A request has an id and a secret. The **id** travels in the QR on the desktop's screen,
//! where anybody in the room can photograph it. The **secret** never leaves the desktop
//! that asked.
//!
//! That split is what makes a photograph of the screen worthless: redeeming needs both, and
//! an attacker who has watched the whole exchange still has only one. Collapsing them into
//! a single QR secret would be simpler, would look identical in every demonstration, and
//! would hand the session to whoever photographed the monitor first.
//!
//! # What is not here
//!
//! No persistence. A rebooted appliance has no pending requests and needs none. No second
//! session type — what a desktop receives is an ordinary [`crate::session`] token with the
//! ordinary deadlines. No trust database, no remembered devices, no certificates: a browser
//! that was approved yesterday and has since closed its tab is a browser that asks again.
//!
//! # What has run
//!
//! **Nothing on hardware at the time of writing.**

use std::time::{Duration, Instant};

/// How long a request stands before it is no use to anybody.
///
/// Five minutes, the same as a pairing code, and for the same reason: it is long enough to
/// walk to another room with a phone and short enough that a request nobody finished is
/// gone before it is forgotten about.
pub const LIFETIME: Duration = Duration::from_secs(5 * 60);

/// How many requests may be waiting at once.
///
/// Sixteen. This is a bound rather than a capacity: a request costs a few hundred bytes and
/// anybody on the network can create one, so what matters is that the number cannot grow
/// without limit. One household will never see two at a time.
pub const MAX_PENDING: usize = 16;

/// Where a request has got to.
///
/// `Expired` is deliberately **not** here. Expiry is a fact about the clock rather than a
/// state something transitions into, and giving it a variant means two representations of
/// one condition and a background task to keep them in step. Every method asks the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Created, and nobody has decided yet.
    Pending,
    /// An administrator said yes. The desktop may now redeem it.
    Approved,
    /// The desktop redeemed it. It will never be redeemable again.
    Consumed,
    /// An administrator said no.
    Denied,
}

/// What the desktop is told when it asks how its request is getting on.
///
/// A separate type from [`Status`] because it carries the one thing `Status` must not: the
/// session token, present in exactly one arm. A single enum with an `Option<String>` on it
/// would make "is there a token here" a question rather than a shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nobody has decided yet.
    Pending,
    /// Approved, redeemed now, and this is the desktop's own session.
    Approved(String),
    /// An administrator refused it.
    Denied,
    /// Its five minutes are up, or it never existed, or the secret was wrong.
    ///
    /// One answer for four causes, deliberately. Telling a caller that an id exists but its
    /// secret is wrong is telling an attacker that they have half of what they need.
    Refused,
}

/// What an approver is shown about the browser asking.
///
/// Descriptive and not authoritative: every field here comes from a header the requesting
/// browser sent, so it says what that browser *claims* to be. It is on the approval screen
/// to help somebody recognise the machine in front of them, and the thing that actually
/// decides they are approving the right one is the verification code, which is derived from
/// the request rather than reported by it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Describes {
    /// A short reading of the `User-Agent`, e.g. "Chrome on Windows".
    pub browser: String,
    /// Whole seconds since the request was made.
    pub age_seconds: u64,
    /// Four digits shown on both screens, so a person can see that they match.
    pub verification: String,
}

/// A browser waiting to be let in.
#[derive(Debug, Clone)]
struct Request {
    id: String,
    /// SHA-256 of the desktop's secret. The secret itself is returned once, to the desktop
    /// that asked, and never held here — the same reasoning [`crate::session`] gives for
    /// storing a digest rather than a token.
    secret_digest: String,
    status: Status,
    created_at: Instant,
    expires_at: Instant,
    browser: String,
}

/// Every request waiting on this appliance.
///
/// A struct with an injected clock rather than a bare global, for the reason
/// [`crate::pairing::Offers`] gives at length: Rust runs tests as threads in one process, so
/// a suite built on a global is one test approving what another is about to redeem.
#[derive(Debug, Default)]
pub struct Requests {
    waiting: Vec<Request>,
}

impl Requests {
    /// Nothing waiting, which is what a started daemon has.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a request, returning `false` when there is no room.
    ///
    /// Full means refused rather than something evicted, and this is the one place in this
    /// crate where that is the right way round. [`crate::session`] evicts, because the
    /// alternative there is sixteen abandoned tabs locking an owner out of their own
    /// appliance. Here the entries belong to *strangers* — anybody on the network can make
    /// one — so evicting would let a stranger cancel an approval somebody was in the middle
    /// of giving. Sixteen fill up, five minutes pass, and the owner tries again.
    pub fn open(
        &mut self,
        id: String,
        secret_digest: String,
        browser: String,
        now: Instant,
    ) -> bool {
        self.prune(now);
        if self.waiting.len() >= MAX_PENDING {
            return false;
        }
        self.waiting.push(Request {
            id,
            secret_digest,
            status: Status::Pending,
            created_at: now,
            expires_at: now + LIFETIME,
            browser,
        });
        true
    }

    /// What an approver should be shown about a request, if it is still live.
    ///
    /// `None` covers "no such request" and "expired" alike. The caller is an authenticated
    /// administrator, so nothing is being protected by the vagueness — there is simply
    /// nothing useful to distinguish.
    #[must_use]
    pub fn describe(&self, id: &str, now: Instant) -> Option<Describes> {
        let request = self.live(id, now)?;
        Some(Describes {
            browser: request.browser.clone(),
            age_seconds: now.duration_since(request.created_at).as_secs(),
            verification: verification_code(&request.id),
        })
    }

    /// Every request still waiting for somebody to decide it, newest last.
    ///
    /// This exists because scanning the desktop's QR code cannot work, and the reason is
    /// not a bug anywhere — it is what `sessionStorage` *is*. A session belongs to a tab,
    /// and a tab opened by a phone's camera is a brand-new one with nothing in it. So the
    /// browser that arrives by scanning is never the browser that holds the session, in any
    /// browser, on any phone.
    ///
    /// Turning the flow around fixes it completely: the browser that *is* signed in asks
    /// the appliance whether anybody is waiting, and offers to approve them. Nothing is
    /// scanned, nothing is pasted, and the desktop's code becomes something to compare
    /// rather than something to carry.
    #[must_use]
    pub fn waiting(&self, now: Instant) -> Vec<(String, Describes)> {
        self.waiting
            .iter()
            .filter(|request| now < request.expires_at && request.status == Status::Pending)
            .map(|request| {
                (
                    request.id.clone(),
                    Describes {
                        browser: request.browser.clone(),
                        age_seconds: now.duration_since(request.created_at).as_secs(),
                        verification: verification_code(&request.id),
                    },
                )
            })
            .collect()
    }

    /// The status of a live request.
    #[must_use]
    pub fn status(&self, id: &str, now: Instant) -> Option<Status> {
        self.live(id, now).map(|request| request.status)
    }

    /// Records an administrator's decision.
    ///
    /// Only a `Pending` request may be decided. An approval that arrived twice, or after the
    /// desktop already redeemed, changes nothing — which is what makes the approve route
    /// safe to retry from a phone on a flaky network.
    pub fn decide(&mut self, id: &str, approve: bool, now: Instant) -> bool {
        self.prune(now);
        let Some(request) = self
            .waiting
            .iter_mut()
            .find(|request| request.id == id && now < request.expires_at)
        else {
            return false;
        };
        if request.status != Status::Pending {
            return false;
        }
        request.status = if approve {
            Status::Approved
        } else {
            Status::Denied
        };
        true
    }

    /// Spends an approved request, if `secret` is the one that opened it.
    ///
    /// **Atomic by construction.** The check and the state change are one `&mut self` call,
    /// so under the lock the caller holds there is no window between "this is approved" and
    /// "this is consumed". Two redemptions arriving together are two calls, and the second
    /// one finds `Consumed`.
    ///
    /// `mint` produces the session, and is a closure rather than a call into
    /// [`crate::session`] so that this can be tested without the daemon's global store —
    /// and so that the ordering above is visible: the request is marked spent *before* the
    /// token is handed out, because the failure worth avoiding is the one where a session
    /// exists and the request that authorised it still looks fresh.
    pub fn redeem(
        &mut self,
        id: &str,
        secret: &str,
        now: Instant,
        mint: impl FnOnce() -> Option<String>,
    ) -> Outcome {
        self.prune(now);

        let Some(request) = self
            .waiting
            .iter_mut()
            .find(|request| request.id == id && now < request.expires_at)
        else {
            return Outcome::Refused;
        };

        // Constant time, and before the status is consulted, so that neither the answer nor
        // how long it took distinguishes a wrong secret from a request in the wrong state.
        if !crate::auth::constant_time_eq(&crate::auth::digest(secret), &request.secret_digest) {
            return Outcome::Refused;
        }

        match request.status {
            Status::Pending => Outcome::Pending,
            Status::Denied => Outcome::Denied,
            Status::Consumed => Outcome::Refused,
            Status::Approved => {
                request.status = Status::Consumed;
                match mint() {
                    Some(token) => Outcome::Approved(token),
                    // The request stays consumed. A minting failure is /dev/urandom being
                    // unreadable, which is not a state to hand a second chance to -- and
                    // the desktop's remedy is one button.
                    None => Outcome::Refused,
                }
            }
        }
    }

    /// Withdraws a request, if `secret` is the one that opened it.
    ///
    /// Possession of the secret is the whole of the authority here, which is why this needs
    /// no administrator: only the browser that asked can have it. Without that check an id
    /// read off somebody's screen would let a passer-by cancel their pairing.
    pub fn cancel(&mut self, id: &str, secret: &str, now: Instant) -> bool {
        self.prune(now);
        let digest = crate::auth::digest(secret);
        let before = self.waiting.len();
        self.waiting.retain(|request| {
            request.id != id || !crate::auth::constant_time_eq(&digest, &request.secret_digest)
        });
        self.waiting.len() != before
    }

    /// Drops everything past its five minutes, returning how many went.
    ///
    /// Opportunistic, called from every method that touches the list. A timer thread would
    /// be machinery in place of a measurement for sixteen entries.
    pub fn prune(&mut self, now: Instant) -> usize {
        let before = self.waiting.len();
        self.waiting.retain(|request| now < request.expires_at);
        before - self.waiting.len()
    }

    /// How many requests are live at `now`.
    #[must_use]
    pub fn live_count(&self, now: Instant) -> usize {
        self.waiting
            .iter()
            .filter(|request| now < request.expires_at)
            .count()
    }

    fn live(&self, id: &str, now: Instant) -> Option<&Request> {
        self.waiting
            .iter()
            .find(|request| request.id == id && now < request.expires_at)
    }
}

/// The four digits shown on both screens.
///
/// Derived from the request id rather than generated separately, so there is nothing extra
/// to keep in step and no second value to leak. It is **not a secret**: the id is in a QR
/// code on a monitor, so anybody who can compute this could already have read it.
///
/// What it is for is the mistake a person actually makes — approving the wrong request,
/// because two are in flight or because the phone is showing one thing and the desk another.
/// Four digits is the length somebody will genuinely compare; sixteen characters is the
/// length they will glance at and assume.
#[must_use]
pub fn verification_code(request_id: &str) -> String {
    let digest = crate::auth::digest(request_id);
    // The first four bytes of the digest, as a number, in the last four decimal digits.
    // Any bits would do; taking them from a hash rather than from the id itself means the
    // code does not reveal a prefix of the id.
    let value: u32 = digest.as_bytes().iter().take(8).fold(0_u32, |acc, byte| {
        acc.wrapping_mul(31).wrapping_add(u32::from(*byte))
    });
    format!("{:04}", value % 10_000)
}

/// A short, honest reading of a `User-Agent`.
///
/// Honest in that it never claims more than it can see: an unrecognised agent becomes
/// "a browser" rather than a guess, and nothing here is used to decide anything. Browsers
/// have spent thirty years lying to each other in this header and the approval screen says
/// so in its own words.
#[must_use]
pub fn describe_browser(user_agent: &str) -> String {
    // Order matters: every Chromium browser also says "Chrome", and Chrome says "Safari".
    let browser = [
        ("Edg/", "Edge"),
        ("OPR/", "Opera"),
        ("Firefox/", "Firefox"),
        ("Chrome/", "Chrome"),
        ("Safari/", "Safari"),
    ]
    .into_iter()
    .find(|(needle, _)| user_agent.contains(needle))
    .map(|(_, name)| name);

    // Android before Linux, because every Android agent also says Linux.
    let system = [
        ("Android", "Android"),
        ("iPhone", "iPhone"),
        ("iPad", "iPad"),
        ("Windows", "Windows"),
        ("Macintosh", "macOS"),
        ("Linux", "Linux"),
    ]
    .into_iter()
    .find(|(needle, _)| user_agent.contains(needle))
    .map(|(_, name)| name);

    match (browser, system) {
        (Some(browser), Some(system)) => format!("{browser} on {system}"),
        (Some(browser), None) => browser.to_owned(),
        (None, Some(system)) => format!("a browser on {system}"),
        (None, None) => "a browser".to_owned(),
    }
}

/// The appliance's pending requests.
static CURRENT: std::sync::Mutex<Requests> = std::sync::Mutex::new(Requests {
    waiting: Vec::new(),
});

fn requests() -> std::sync::MutexGuard<'static, Requests> {
    CURRENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What a desktop is given when it asks to be let in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    /// Travels in the QR code. Not a secret.
    pub id: String,
    /// Never leaves the desktop that asked. 256 bits.
    pub secret: String,
    /// The four digits both screens show.
    pub verification: String,
}

/// Opens a request, or says why not.
///
/// # Errors
/// When randomness cannot be read, or when too many requests are already waiting.
pub fn open(user_agent: &str) -> Result<Opened, String> {
    // Two independent values from the kernel. The id is public and the secret is not, so
    // deriving one from the other -- which would be tidier -- would put the secret one hash
    // away from something printed on a screen.
    let id = crate::session::generate().map_err(|error| format!("no randomness: {error}"))?;
    let secret = crate::session::generate().map_err(|error| format!("no randomness: {error}"))?;

    let opened = Opened {
        verification: verification_code(&id),
        id: id.clone(),
        secret: secret.clone(),
    };

    if requests().open(
        id,
        crate::auth::digest(&secret),
        describe_browser(user_agent),
        Instant::now(),
    ) {
        Ok(opened)
    } else {
        Err(format!(
            "{MAX_PENDING} browsers are already waiting to be approved, which is more than \
             this appliance expects to see at once. Remedy: wait for them to expire — none \
             lasts longer than five minutes — or approve or dismiss the one you are \
             expecting."
        ))
    }
}

/// Everything waiting for a decision right now.
#[must_use]
pub fn waiting() -> Vec<(String, Describes)> {
    requests().waiting(Instant::now())
}

/// What an approver should be shown, if the request is live.
#[must_use]
pub fn describe(id: &str) -> Option<Describes> {
    requests().describe(id, Instant::now())
}

/// Records an administrator's decision.
///
/// Returns whether anything changed, which the route reports: an approval that arrived
/// twice, or after the desktop was already let in, is not an error and is not a success.
#[must_use]
pub fn decide(id: &str, approve: bool) -> bool {
    requests().decide(id, approve, Instant::now())
}

/// Spends an approved request, issuing the desktop a session of its own.
#[must_use]
pub fn redeem(id: &str, secret: &str) -> Outcome {
    // `session::issue` inside the lock this call holds. That is deliberate: it is what makes
    // "mark consumed, then mint" one indivisible step, so two redemptions racing cannot
    // produce two sessions.
    requests().redeem(id, secret, Instant::now(), || crate::session::issue().ok())
}

/// Withdraws a request. Needs the secret, so only the browser that asked can do it.
#[must_use]
pub fn cancel(id: &str, secret: &str) -> bool {
    requests().cancel(id, secret, Instant::now())
}

/// Serialises the tests that drive the appliance's own request list.
///
/// The same reasoning as [`crate::pairing::test_lock`]: a suite built on a global is one
/// test approving what another is about to redeem, and the failures depend on the scheduler.
#[cfg(test)]
pub fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    requests().waiting.clear();
    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timeline() -> Instant {
        Instant::now()
    }

    /// A request in a store, with its secret, ready to be driven.
    fn opened(store: &mut Requests, now: Instant) -> (String, String) {
        let id = "request-id".to_owned();
        let secret = "desktop-secret".to_owned();
        assert!(store.open(
            id.clone(),
            crate::auth::digest(&secret),
            "Chrome on Windows".to_owned(),
            now
        ));
        (id, secret)
    }

    /// A minting closure that always succeeds. `Option` because the real one is
    /// `session::issue().ok()`, and a helper that could not fail would let the failing case
    /// go untested by accident.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the shape has to match session::issue().ok(), which can fail"
    )]
    fn a_session() -> Option<String> {
        Some("a-brand-new-session".to_owned())
    }

    #[test]
    fn a_pending_request_yields_nothing_however_correct_the_secret_is() {
        // The state an attacker most wants to shortcut: the desktop has asked and nobody
        // has said yes. Possession of the secret is ownership, not authority.
        let t0 = timeline();
        let mut store = Requests::new();
        let (id, secret) = opened(&mut store, t0);

        assert_eq!(store.redeem(&id, &secret, t0, a_session), Outcome::Pending);
    }

    #[test]
    fn approval_then_the_right_secret_issues_exactly_one_session() {
        let t0 = timeline();
        let mut store = Requests::new();
        let (id, secret) = opened(&mut store, t0);

        assert!(store.decide(&id, true, t0));
        assert_eq!(
            store.redeem(&id, &secret, t0, a_session),
            Outcome::Approved("a-brand-new-session".to_owned())
        );

        // And never again. This is the property the whole two-value design exists to make
        // enforceable: a photograph of the QR is worthless, and so is a replay of the
        // redemption that followed it.
        assert_eq!(store.redeem(&id, &secret, t0, a_session), Outcome::Refused);
        assert_eq!(store.status(&id, t0), Some(Status::Consumed));
    }

    #[test]
    fn the_wrong_secret_is_refused_and_says_nothing_about_which_half_was_wrong() {
        // An answer that distinguished "no such request" from "wrong secret" would tell
        // somebody who photographed a screen that they have half of what they need.
        let t0 = timeline();
        let mut store = Requests::new();
        let (id, _secret) = opened(&mut store, t0);
        assert!(store.decide(&id, true, t0));

        assert_eq!(
            store.redeem(&id, "not-the-secret", t0, a_session),
            Outcome::Refused
        );
        assert_eq!(
            store.redeem("no-such-request", "anything", t0, a_session),
            Outcome::Refused
        );
        // And the real one still works, so a wrong guess cost the owner nothing.
        assert!(matches!(
            store.redeem(&id, "desktop-secret", t0, a_session),
            Outcome::Approved(_)
        ));
    }

    #[test]
    fn a_denied_request_says_so_rather_than_timing_out() {
        // Somebody tapped Deny on a phone. The desktop should say so within a poll, not sit
        // on "waiting for approval" for the remaining four minutes.
        let t0 = timeline();
        let mut store = Requests::new();
        let (id, secret) = opened(&mut store, t0);

        assert!(store.decide(&id, false, t0));
        assert_eq!(store.redeem(&id, &secret, t0, a_session), Outcome::Denied);
        assert_eq!(store.status(&id, t0), Some(Status::Denied));
    }

    #[test]
    fn a_denied_request_cannot_be_approved_afterwards() {
        let t0 = timeline();
        let mut store = Requests::new();
        let (id, _) = opened(&mut store, t0);

        assert!(store.decide(&id, false, t0));
        assert!(
            !store.decide(&id, true, t0),
            "no changing your mind into yes"
        );
        assert_eq!(store.status(&id, t0), Some(Status::Denied));
    }

    #[test]
    fn approving_twice_is_harmless_and_approving_a_consumed_request_is_not_possible() {
        // The first half matters because a phone on a flaky network retries; the second
        // because an approval arriving after the desktop has already been let in must not
        // make the request live again.
        let t0 = timeline();
        let mut store = Requests::new();
        let (id, secret) = opened(&mut store, t0);

        assert!(store.decide(&id, true, t0));
        assert!(!store.decide(&id, true, t0), "the second changes nothing");

        assert!(matches!(
            store.redeem(&id, &secret, t0, a_session),
            Outcome::Approved(_)
        ));
        assert!(!store.decide(&id, true, t0));
        assert_eq!(store.redeem(&id, &secret, t0, a_session), Outcome::Refused);
    }

    #[test]
    fn everything_about_a_request_stops_at_five_minutes() {
        let t0 = timeline();
        let mut store = Requests::new();
        let (id, secret) = opened(&mut store, t0);
        assert!(store.decide(&id, true, t0));

        let after = t0 + LIFETIME;
        assert_eq!(store.status(&id, after), None);
        assert_eq!(store.describe(&id, after), None);
        assert!(!store.decide(&id, true, after));
        assert_eq!(
            store.redeem(&id, &secret, after, a_session),
            Outcome::Refused
        );

        // Just inside it, everything still works, so the deadline is the deadline and not
        // an off-by-one somewhere earlier.
        let mut store = Requests::new();
        let (id, secret) = opened(&mut store, t0);
        assert!(store.decide(&id, true, t0));
        let just_inside = t0 + Duration::from_secs(LIFETIME.as_secs() - 1);
        assert!(matches!(
            store.redeem(&id, &secret, just_inside, a_session),
            Outcome::Approved(_)
        ));
    }

    #[test]
    fn cancelling_needs_the_secret_so_a_passer_by_cannot_do_it() {
        // The id is on a monitor. If cancelling took only the id, anybody who walked past a
        // desk could stop somebody pairing, over and over, from anywhere on the network.
        let t0 = timeline();
        let mut store = Requests::new();
        let (id, secret) = opened(&mut store, t0);

        assert!(
            !store.cancel(&id, "read-the-screen", t0),
            "id alone is not authority"
        );
        assert_eq!(store.status(&id, t0), Some(Status::Pending), "still there");

        assert!(store.cancel(&id, &secret, t0));
        assert_eq!(store.status(&id, t0), None);
    }

    #[test]
    fn the_number_waiting_is_bounded_and_a_stranger_cannot_push_somebody_out() {
        // The opposite of `session`'s rule, deliberately. Sessions evict the least recently
        // used because the alternative is abandoned tabs locking an owner out. These belong
        // to strangers -- anybody on the network can make one -- so evicting would let a
        // stranger displace an approval somebody was in the middle of giving.
        let t0 = timeline();
        let mut store = Requests::new();

        for n in 0..MAX_PENDING {
            assert!(store.open(
                format!("id-{n}"),
                crate::auth::digest("s"),
                "a browser".to_owned(),
                t0
            ));
        }
        assert!(!store.open(
            "one-too-many".to_owned(),
            crate::auth::digest("s"),
            "a browser".to_owned(),
            t0
        ));
        assert_eq!(store.live_count(t0), MAX_PENDING);
        assert!(store.status("id-0", t0).is_some(), "nobody was pushed out");

        // And five minutes later there is room again without anybody doing anything.
        let later = t0 + LIFETIME;
        assert!(store.open(
            "after-they-expired".to_owned(),
            crate::auth::digest("s"),
            "a browser".to_owned(),
            later
        ));
        assert_eq!(store.live_count(later), 1);
    }

    #[test]
    fn an_administrator_can_see_what_is_waiting_without_scanning_anything() {
        // The route that makes this feature work at all on a phone. A session belongs to a
        // tab, and the tab a camera opens is a new one with nothing in it -- so the browser
        // that arrives by scanning is never the browser holding the session. The signed-in
        // browser has to be able to ask.
        let t0 = timeline();
        let mut store = Requests::new();
        let (id, secret) = opened(&mut store, t0);

        let waiting = store.waiting(t0);
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].0, id);
        assert_eq!(waiting[0].1.browser, "Chrome on Windows");
        assert_eq!(waiting[0].1.verification, verification_code(&id));

        // Only things still to be decided. An approved request is the desktop's business
        // now, and a denied or spent one is nobody's.
        assert!(store.decide(&id, true, t0));
        assert!(
            store.waiting(t0).is_empty(),
            "approved is no longer waiting"
        );

        assert!(matches!(
            store.redeem(&id, &secret, t0, a_session),
            Outcome::Approved(_)
        ));
        assert!(store.waiting(t0).is_empty());

        // And nothing past its five minutes.
        let mut store = Requests::new();
        opened(&mut store, t0);
        assert!(store.waiting(t0 + LIFETIME).is_empty());
    }

    #[test]
    fn nothing_waiting_carries_a_secret_an_approver_has_no_business_with() {
        // This list goes to every signed-in browser on a poll. It must describe, and it
        // must not hand over anything that could redeem a request -- the desktop's secret
        // is the whole reason a photograph of the QR is not enough.
        let t0 = timeline();
        let mut store = Requests::new();
        opened(&mut store, t0);

        let said = format!("{:?}", store.waiting(t0));
        assert!(!said.contains("desktop-secret"), "{said}");
        assert!(
            !said.contains(&crate::auth::digest("desktop-secret")),
            "{said}"
        );
    }

    #[test]
    fn a_new_store_knows_nothing_which_is_what_a_reboot_leaves() {
        let store = Requests::new();
        assert_eq!(store.live_count(timeline()), 0);
        assert_eq!(store.status("anything", timeline()), None);
    }

    #[test]
    fn the_secret_itself_is_never_held() {
        // The same property `session` has, and for a better reason: this one is held while
        // a stranger's request is pending, and the store is a list anybody on the network
        // can cause entries in.
        let t0 = timeline();
        let mut store = Requests::new();
        opened(&mut store, t0);

        let held = format!("{store:?}");
        assert!(!held.contains("desktop-secret"), "{held}");
        assert!(held.contains(&crate::auth::digest("desktop-secret")));
    }

    #[test]
    fn two_redemptions_arriving_together_produce_exactly_one_session() {
        // The concurrency this design is most exposed to, driven through the daemon's own
        // global rather than a local store -- because the property being tested is that the
        // lock makes "check approved, mark consumed, mint" indivisible, and a local store
        // has no lock to test.
        let _serialised = test_lock();

        let opened = open("Mozilla/5.0 (Windows NT 10.0) Chrome/120").expect("/dev/urandom");
        assert!(decide(&opened.id, true));

        let issued = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let id = opened.id.clone();
            let secret = opened.secret.clone();
            let issued = std::sync::Arc::clone(&issued);
            threads.push(std::thread::spawn(move || {
                if let Outcome::Approved(token) = redeem(&id, &secret) {
                    issued.lock().unwrap().push(token);
                }
            }));
        }
        for thread in threads {
            thread.join().expect("no thread panicked");
        }

        let issued = issued.lock().unwrap();
        assert_eq!(
            issued.len(),
            1,
            "eight redemptions of one approval produced {} sessions",
            issued.len()
        );
        assert!(crate::session::revoke(&issued[0]), "cleanup: it was live");
    }

    #[test]
    fn the_two_values_are_different_and_both_are_full_length() {
        // The id is public and the secret is not, so deriving one from the other -- which
        // would be tidier -- would put the secret one hash away from something printed on a
        // screen.
        let _serialised = test_lock();
        let opened = open("Chrome/120").expect("/dev/urandom");

        assert_ne!(opened.id, opened.secret);
        assert_eq!(opened.id.len(), crate::session::TOKEN_BYTES * 2);
        assert_eq!(opened.secret.len(), crate::session::TOKEN_BYTES * 2);
        assert!(opened.id.bytes().all(|b| b.is_ascii_hexdigit()));

        let second = open("Chrome/120").expect("/dev/urandom");
        assert_ne!(opened.id, second.id, "not a counter");
        assert_ne!(opened.secret, second.secret);
    }

    #[test]
    fn the_verification_code_is_four_digits_and_the_same_on_both_screens() {
        // Both sides derive it from the id rather than being told it, so there is nothing
        // to keep in step and nothing extra to leak.
        let code = verification_code("some-request-id");
        assert_eq!(code.len(), 4);
        assert!(code.bytes().all(|b| b.is_ascii_digit()));
        assert_eq!(code, verification_code("some-request-id"));
        assert_ne!(code, verification_code("another-request-id"));
    }

    #[test]
    fn the_verification_code_reveals_nothing_about_the_id_it_came_from() {
        // It is not a secret -- the id is on a monitor -- but it should not be a *prefix*
        // of one either, because a value that looks like part of a credential gets treated
        // like one.
        let id = "0123456789abcdef0123456789abcdef";
        let code = verification_code(id);
        assert!(!id.contains(&code), "{code} appears in the id it describes");
    }

    #[test]
    fn a_browser_is_described_without_pretending_to_know_more_than_it_does() {
        // Every field on the approval screen comes from a header the requesting browser
        // sent, so it says what that browser claims. Chromium says "Chrome" and Chrome says
        // "Safari", which is why the order these are tried in is part of the answer.
        for (agent, expected) in [
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64) Chrome/120.0 Safari/537.36",
                "Chrome on Windows",
            ),
            (
                "Mozilla/5.0 (Windows NT 10.0) Chrome/120 Safari/537 Edg/120",
                "Edge on Windows",
            ),
            (
                "Mozilla/5.0 (Macintosh; Intel Mac OS X) Firefox/121.0",
                "Firefox on macOS",
            ),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0) Version/17.0 Safari/604.1",
                "Safari on iPhone",
            ),
            (
                "Mozilla/5.0 (Linux; Android 14) Chrome/120 Mobile Safari/537",
                "Chrome on Android",
            ),
            ("curl/8.5.0", "a browser"),
            ("", "a browser"),
        ] {
            assert_eq!(describe_browser(agent), expected, "for {agent:?}");
        }
    }
}
