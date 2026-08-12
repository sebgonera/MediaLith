//! Finding out that a release exists, without installing it (ADR-0020).
//!
//! Every update this project has ever done was somebody pasting an address. That is a fine
//! way to move a build between two rooms and it is not an update story: it requires the
//! owner of the appliance to be the person who built it. This module is the other half —
//! the appliance asks, about once a day, whether there is a newer MediaLith release for the
//! channel it tracks, and says so on its own page.
//!
//! # What this is allowed to decide, which is almost nothing
//!
//! Discovery answers *what release is available*. Whether it may be installed is decided by
//! the code that already existed, unchanged: [`crate::update::evaluate`] runs the signature
//! chain, the certificate, the revocation list, the anti-rollback counter, the product, the
//! channel and the slot arithmetic. This module does not parse a manifest, does not verify
//! anything, and cannot make a release installable by liking the look of it.
//!
//! That division is what makes the feed file below safe to be unsigned. Whoever answers at
//! the configured address chooses which *signed* manifest this appliance evaluates, and
//! nothing else. They can withhold an update, and they can point at an older release that
//! the anti-rollback counter then refuses, and they can point at another channel's release
//! that the channel check then refuses. They cannot make the machine run their code.
//!
//! # The shape of an update service
//!
//! ```text
//! <base>/channels/stable.json          {"release": "…", "manifest": "releases/…/manifest-stable.json"}
//! <base>/releases/<release>/manifest-stable.json    and .sig
//! <base>/releases/<release>/usr.erofs  usr.hash  plexos-<release>-a.efi  -b.efi
//! ```
//!
//! Files, and no server. The artefacts exist once per release and every channel's manifest
//! names them by the bare file names ADR-0006 requires, which is why the manifest has to
//! live in the release's directory rather than in the channel's: a name is resolved
//! against wherever the manifest itself came from.
//!
//! # What has run
//!
//! **Nothing on hardware yet.**

use std::sync::Mutex;

use plexos_types::manifest::{Channel, Importance};

use crate::update::{Bundle, Evaluation, Event, Signature};

/// Directory holding one file per channel.
pub const CHANNELS: &str = "channels";

/// How long between automatic checks.
pub const INTERVAL_SECS: u64 = 24 * 60 * 60;

/// How much of the day the jitter may add.
///
/// A fleet that all checked at the same instant would be a fleet that arrives as a spike,
/// and the spike would be at whatever hour the release was published — because every
/// appliance's timer starts when it boots, and a release is what makes people reboot. An
/// hour of spread costs nothing and is the difference between a static host and a static
/// host being asked for the same file ten thousand times in a second.
pub const JITTER_SECS: u64 = 60 * 60;

/// How long after the daemon starts the first automatic check happens.
///
/// Late enough that the boot decision has been made and the network has had its chance, and
/// deliberately not part of either. An appliance whose update service is unreachable is a
/// healthy appliance; ADR-0005's gate must never learn that a web server exists.
pub const FIRST_CHECK_DELAY_SECS: u64 = 5 * 60;

/// Largest feed document this will read.
///
/// A channel file is about eighty bytes. The bound is what stops a hostile or broken source
/// from making a check cost the appliance a download it never asked for.
pub const MAX_FEED_BYTES: u64 = 64 * 1024;

/// Where a check has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Nothing has been asked yet.
    NeverChecked,
    /// A check is running.
    Checking,
    /// There is nowhere to look.
    NotConfigured,
    /// The service answered, and this appliance runs the newest release it offers.
    UpToDate,
    /// A newer release exists, and it passed everything that could have refused it.
    Available,
    /// The check did not produce an answer. `error` says why.
    ///
    /// Never merged into [`Status::UpToDate`]. "I could not authenticate what the server
    /// sent" and "there is nothing newer" are opposite states, and reporting the first as
    /// the second is how an appliance that is being attacked looks like an appliance that
    /// is fine.
    Failed,
}

/// What the appliance knows about releases beyond the one it is running.
///
/// Separate from [`crate::update::Progress`] rather than folded into it, because they are
/// different questions about different things: one is a job that writes a partition, and
/// this is a fact about the world that is true whether or not anybody is installing
/// anything. Merging them is how "checking" and "installing" become one spinner.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Availability {
    /// Where the check got to.
    pub status: Status,
    /// One line for a person.
    pub detail: String,
    /// The channel this appliance tracks, as configured.
    pub channel: String,
    /// The update service, as configured. Empty when there is none.
    pub source: String,
    /// The release the service offers, once one has been verified.
    pub available: Option<String>,
    /// How long ago the last check finished.
    ///
    /// Elapsed rather than a timestamp, and that is not laziness: this appliance has no
    /// time synchronisation, so a wall-clock "last checked at 14:12" can be years wrong
    /// while "four minutes ago" is measured against a monotonic clock and cannot be.
    pub checked_seconds_ago: Option<u64>,
    /// What the release says it is, if it says.
    pub summary: Option<String>,
    /// What changed, if the publisher said.
    pub notes: Vec<String>,
    /// How much it matters. Only ever changes the wording.
    pub importance: Importance,
    /// Who vouched for the manifest that was read.
    pub signature: Option<Signature>,
    /// Why the check failed, if it did.
    pub error: Option<String>,
}

impl Default for Availability {
    fn default() -> Self {
        Self {
            status: Status::NeverChecked,
            detail: "no check for a MediaLith release has been made yet".to_owned(),
            channel: String::new(),
            source: String::new(),
            available: None,
            checked_seconds_ago: None,
            summary: None,
            notes: Vec::new(),
            importance: Importance::Normal,
            signature: None,
            error: None,
        }
    }
}

/// The one discovery state, and the lock that stops two checks racing.
#[derive(Debug, Default)]
pub struct Discovery {
    state: Mutex<Availability>,
    /// When the last check finished, on the monotonic clock.
    finished: Mutex<Option<std::time::Instant>>,
}

impl Discovery {
    /// A discovery that has never run.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// What is known now.
    #[must_use]
    pub fn snapshot(&self) -> Availability {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.checked_seconds_ago = self
            .finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map(|at| at.elapsed().as_secs());
        state.clone()
    }

    /// Claims the check, if one is not already running.
    ///
    /// Returns false rather than queueing. Ten browsers pressing the button is ten
    /// requests, and the honest answer to nine of them is the state the first one is
    /// already producing.
    pub fn begin(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.status == Status::Checking {
            return false;
        }
        state.status = Status::Checking;
        "asking the update service what it has".clone_into(&mut state.detail);
        state.error = None;
        true
    }

    /// Records the outcome of a check.
    pub fn finish(&self, outcome: Availability) {
        *self
            .finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(std::time::Instant::now());
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = outcome;
    }
}

/// How long this particular appliance waits beyond the interval, from something about it.
///
/// Derived rather than drawn, so a machine keeps its own offset across reboots instead of
/// picking a new one every time — which is what would happen with a random number here, and
/// would mean a fleet that reboots for a release re-converges on the hour it was published.
/// There is no random number generator in this image and this does not need one.
#[must_use]
pub fn jitter_secs(seed: &str) -> u64 {
    // FNV-1a. Thirty years old, four lines, and the only property needed of it is that two
    // appliances that differ at all land in different places.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash % JITTER_SECS
}

/// Something about this machine that no other machine has.
///
/// The stored device credential is a fingerprint of a secret rather than the secret, and it
/// is never sent anywhere by this — it is hashed to a number under an hour. Falling back to
/// the version means a machine with no credential yet shares an offset with its siblings,
/// which is the state a fresh appliance is in for its first few minutes.
fn seed() -> String {
    std::fs::read_to_string(crate::auth::CREDENTIAL_FILE)
        .unwrap_or_else(|_| crate::update::running_version())
}

/// Checks now, and then about once a day for as long as this process lives.
///
/// Deliberately not a cron and deliberately not part of the boot path. The first check
/// waits for [`FIRST_CHECK_DELAY_SECS`] so that ADR-0005's gate has already decided about
/// this boot: an appliance must never learn whether it is healthy from a web server, and
/// the surest way to keep that true is for nothing about the update service to happen while
/// the question is open.
pub fn schedule(
    discovery: &std::sync::Arc<Discovery>,
    update: &std::sync::Arc<crate::update::Job>,
    log: impl Fn(&str) + Send + 'static,
) {
    let discovery = std::sync::Arc::clone(discovery);
    let update = std::sync::Arc::clone(update);
    std::thread::spawn(move || {
        let offset = jitter_secs(&seed());
        log(&format!(
            "the first check for a MediaLith release is in {} minutes, then every {} hours \
             plus {} minutes for this machine",
            FIRST_CHECK_DELAY_SECS / 60,
            INTERVAL_SECS / 3600,
            offset / 60
        ));
        std::thread::sleep(std::time::Duration::from_secs(FIRST_CHECK_DELAY_SECS));
        loop {
            // An install owns the machine's attention and the same lock the check would
            // want. Skipping this round is right rather than waiting: the next one is a day
            // away and by then the install has long finished or long failed.
            if update.snapshot().phase.is_running() {
                log("an update is in progress, so this round's check is skipped");
            } else if discovery.begin() {
                check(&discovery);
                log(&discovery.snapshot().detail);
            }
            std::thread::sleep(std::time::Duration::from_secs(INTERVAL_SECS + offset));
        }
    });
}

/// What a channel file says.
///
/// Two fields, both advisory. `release` exists so a person reading the tree can see what a
/// channel points at without opening a manifest; the manifest is what decides, and if the
/// two disagree the manifest wins and the disagreement is reported.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Feed {
    /// The release this channel currently offers.
    pub release: String,
    /// Where its manifest is, relative to the base address.
    pub manifest: String,
}

/// Where a channel's file is.
#[must_use]
pub fn feed_url(base: &str, channel: Channel) -> String {
    format!("{}/{CHANNELS}/{channel}.json", base.trim_end_matches('/'))
}

/// Turns a channel file into a place to look, or refuses it.
///
/// Pure, so the interesting half — everything a hostile feed could try — is a test rather
/// than an experiment against a real server. The rules are the ones
/// `plexos_update::location` already applies to artefact names, for the same reason: this
/// value is joined to a URL and reaches `curl`, so a feed that could put anything here
/// would choose what this appliance fetches.
///
/// # Errors
/// A message naming what was wrong with the document, addressed to whoever publishes it.
pub fn locate(base: &str, document: &[u8]) -> Result<(Feed, Bundle), String> {
    if document.len() as u64 > MAX_FEED_BYTES {
        return Err(format!(
            "the channel file is {} bytes and a channel file is about eighty. Remedy: this \
             is not an update service, or not the directory one was published to.",
            document.len()
        ));
    }

    let feed: Feed = serde_json::from_slice(document).map_err(|error| {
        format!(
            "the channel file is not one this release can read: {error}. Remedy: it should \
             be a JSON object with a release and a manifest path; tools/publish-release.sh \
             writes it."
        )
    })?;

    let path = feed.manifest.trim();
    let refuse = |why: &str| {
        Err(format!(
            "the channel file points at {path:?}, which {why}. Remedy: treat this service as \
             hostile. A manifest path is a plain relative path below the update service, and \
             the publisher's own tooling cannot produce anything else."
        ))
    };
    if path.is_empty() {
        return refuse("is empty");
    }
    if path.contains("://") || path.starts_with('/') || path.starts_with('\\') {
        return refuse("is not relative to the update service");
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == ".." || part == ".")
    {
        return refuse("climbs out of the update service or has an empty component");
    }
    // A URL path is case-sensitive by definition, so this compares as written. clippy
    // reads `.ends_with(".json")` as a filesystem extension check, where the caution about
    // case would be right and here would mean accepting a name the server does not have.
    #[expect(
        clippy::case_sensitive_file_extension_comparisons,
        reason = "this is a URL path, not a file name on a case-folding filesystem"
    )]
    if !path.ends_with(".json") {
        return refuse("is not a manifest");
    }

    let base = base.trim().trim_end_matches('/');
    let bundle = match path.rsplit_once('/') {
        Some((directory, name)) => Bundle {
            base: format!("{base}/{directory}"),
            manifest: name.to_owned(),
        },
        // A manifest at the root of the service. Legal, and what a one-release bench tree
        // looks like.
        None => Bundle {
            base: base.to_owned(),
            manifest: path.to_owned(),
        },
    };
    Ok((feed, bundle))
}

/// Where the configured service says this appliance's next release is.
///
/// The address resolution on its own, so that installing what an owner was shown goes
/// through exactly the same feed the check read. Returning a [`Bundle`] rather than
/// installing anything keeps this module free of any opinion about what happens next.
///
/// # Errors
/// No service configured, a channel this release cannot name, or a feed that could not be
/// read or trusted to name a path.
pub fn locate_configured() -> Result<Bundle, String> {
    let config = crate::settings::load(&crate::settings::path())?;
    if !config.update_service.is_configured() {
        return Err(
            "this appliance has no update service configured, so there is nothing \
                    to install from. Remedy: set one in Settings, or paste the address of \
                    a bundle under the development source."
                .to_owned(),
        );
    }
    let channel = config.updates.tracked().ok_or_else(|| {
        format!(
            "this appliance tracks the {:?} channel, which this release does not know. \
             Remedy: set it to one of {} in Settings.",
            config.updates.channel,
            Channel::ALL.map(Channel::as_str).join(", ")
        )
    })?;

    let base = config.update_service.url.trim();
    let curl = crate::update::curl_for(&Bundle::at(base)).map_err(|error| error.to_string())?;
    let document = crate::update::fetch_document(&curl, &feed_url(base, channel))
        .map_err(|error| format!("could not reach the update service: {error}"))?;
    locate(base, &document).map(|(_, bundle)| bundle)
}

/// Asks the configured service what it has, and judges the answer.
///
/// Best effort in one direction only: a failure here is a failure to *learn* something, and
/// the appliance is unaffected by it. Nothing this returns can make a machine unhealthy,
/// restart it, or write anything to a partition.
pub fn check(discovery: &Discovery) {
    let mut availability = Availability {
        status: Status::Checking,
        ..Availability::default()
    };

    let config = match crate::settings::load(&crate::settings::path()) {
        Ok(config) => config,
        Err(error) => {
            availability.status = Status::Failed;
            "the configuration could not be read, so there is no channel to check"
                .clone_into(&mut availability.detail);
            availability.error = Some(error);
            discovery.finish(availability);
            return;
        }
    };
    availability.channel.clone_from(&config.updates.channel);
    config
        .update_service
        .url
        .trim()
        .clone_into(&mut availability.source);

    if !config.update_service.is_configured() {
        availability.status = Status::NotConfigured;
        "system updates are not configured: this appliance has no update service to ask"
            .clone_into(&mut availability.detail);
        discovery.finish(availability);
        return;
    }

    let Some(channel) = config.updates.tracked() else {
        availability.status = Status::Failed;
        availability.detail = format!(
            "this appliance tracks the {:?} channel, which this release does not know",
            config.updates.channel
        );
        availability.error = Some(format!(
            "Remedy: set the update channel to one of {} in Settings.",
            Channel::ALL.map(Channel::as_str).join(", ")
        ));
        discovery.finish(availability);
        return;
    };

    let source = availability.source.clone();
    match ask(&source, channel, &mut availability) {
        Ok(evaluation) => describe(&evaluation, &mut availability),
        Err(error) => {
            availability.status = Status::Failed;
            "the check for a MediaLith release did not complete"
                .clone_into(&mut availability.detail);
            availability.error = Some(error);
        }
    }
    discovery.finish(availability);
}

/// The network half: fetch the channel file, then hand what it names to the updater.
fn ask(
    base: &str,
    channel: Channel,
    availability: &mut Availability,
) -> Result<Evaluation, String> {
    let bundle_at_base = Bundle::at(base);
    let curl = crate::update::curl_for(&bundle_at_base).map_err(|error| error.to_string())?;

    let url = feed_url(base, channel);
    // Optional, because "this service publishes no such channel" is an answer and not a
    // failure of the network. curl exits 22 for an HTTP error status, which is the only
    // thing that separates a 404 from a server that is not there -- and the two need
    // opposite words. Without the distinction, a stable appliance pointed at a tree with
    // only a dev feed was told to check the address and re-read the signing instructions.
    let document = crate::update::fetch_optional(&curl, &url)
        .map_err(|error| {
            format!(
                "could not reach the update service: {error}. The system is otherwise \
                 unaffected — nothing about this changes what the appliance is running."
            )
        })?
        .ok_or_else(|| {
            format!(
                "this update service publishes nothing on the {channel} channel: there is no \
                 {url}. The service is answering, so this is not a network fault. Remedy: \
                 track a channel it does publish, or ask whoever runs it to promote a \
                 release to {channel}."
            )
        })?;

    let (feed, bundle) = locate(base, &document)?;

    crate::update::evaluate(&bundle, &curl, &mut |event| {
        if let Event::Verified { release, signature } = event {
            availability.available = Some(release.to_owned());
            availability.signature = Some(signature.clone());
            if release != feed.release {
                // Worth saying and not worth refusing over. The manifest is what decides;
                // a channel file naming something else is a publisher mid-publish or a
                // stale copy, and both are the reader's business.
                availability.notes.push(format!(
                    "the {channel} channel file names {}, and the manifest it points at is \
                     {release}",
                    feed.release
                ));
            }
        }
    })
    .map_err(|error| error.to_string())
}

/// Turns a decision into the words a page shows.
fn describe(evaluation: &Evaluation, availability: &mut Availability) {
    let manifest = &evaluation.manifest;
    availability.summary.clone_from(&manifest.summary);
    availability.importance = manifest.importance;
    availability.notes.extend(manifest.notes.iter().cloned());

    match &evaluation.decision {
        plexos_update::Decision::UpToDate { running } => {
            availability.status = Status::UpToDate;
            availability.detail = format!("MediaLith {running} is the newest release offered");
        }
        plexos_update::Decision::Install { version, .. } => {
            availability.status = Status::Available;
            availability.detail = match manifest.importance {
                Importance::Security => format!("MediaLith {version} is a security update"),
                _ => format!("MediaLith {version} is available"),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_document(manifest: &str) -> Vec<u8> {
        format!(r#"{{"release":"0.1.1.202608250900","manifest":"{manifest}"}}"#).into_bytes()
    }

    #[test]
    fn a_channel_is_one_file_at_a_predictable_address() {
        assert_eq!(
            feed_url("https://updates.example/medialith", Channel::Stable),
            "https://updates.example/medialith/channels/stable.json"
        );
        // A trailing slash is what somebody pastes, and it must not produce a double one:
        // some static hosts serve that as a different path and some refuse it.
        assert_eq!(
            feed_url("http://192.168.2.165:8080/tree/", Channel::Dev),
            "http://192.168.2.165:8080/tree/channels/dev.json"
        );
    }

    #[test]
    fn the_manifest_is_found_beside_the_artefacts_it_names() {
        // The property the whole layout exists for. Artefact sources are bare file names
        // resolved against the manifest's own directory, so a channel's manifest has to sit
        // in the release directory -- and the base handed to the updater is that directory.
        let (feed, bundle) = locate(
            "https://updates.example/medialith",
            &feed_document("releases/0.1.1.202608250900/manifest-stable.json"),
        )
        .unwrap();
        assert_eq!(feed.release, "0.1.1.202608250900");
        assert_eq!(
            bundle.base,
            "https://updates.example/medialith/releases/0.1.1.202608250900"
        );
        assert_eq!(bundle.manifest, "manifest-stable.json");
        assert_eq!(
            bundle.manifest_url(),
            "https://updates.example/medialith/releases/0.1.1.202608250900/manifest-stable.json"
        );
        assert_eq!(
            bundle.signature_url(),
            "https://updates.example/medialith/releases/0.1.1.202608250900/manifest-stable.json.sig"
        );
    }

    #[test]
    fn the_channel_files_the_publishing_tools_actually_write_are_read() {
        // Captured from a real run of tools/publish-release.sh and tools/promote-release.sh
        // rather than typed here. A fixture somebody imagined agrees with the code and not
        // with the machine -- already recorded about resolv.conf, where a parser and its
        // test agreed while the appliance saw something else.
        for (document, expected) in [
            (
                include_bytes!("../tests/fixtures/channel-dev.json").as_slice(),
                "manifest-dev.json",
            ),
            (
                include_bytes!("../tests/fixtures/channel-stable.json").as_slice(),
                "manifest-stable.json",
            ),
        ] {
            let (feed, bundle) = locate("https://updates.example/medialith", document)
                .expect("the tools' own output must be readable");
            assert_eq!(feed.release, "0.1.1.202608250900");
            assert_eq!(bundle.manifest, expected);
            assert_eq!(
                bundle.base, "https://updates.example/medialith/releases/0.1.1.202608250900",
                "the manifest has to be fetched from the directory its artefacts are in"
            );
        }
    }

    #[test]
    fn a_manifest_at_the_root_of_the_service_is_allowed() {
        let (_, bundle) = locate("http://host:8080/tree", &feed_document("manifest.json")).unwrap();
        assert_eq!(bundle.base, "http://host:8080/tree");
        assert_eq!(bundle.manifest, "manifest.json");
    }

    #[test]
    fn a_feed_cannot_choose_what_this_appliance_fetches() {
        // Everything a hostile or broken channel file could try. The feed is unsigned by
        // design -- it can only choose which signed manifest is evaluated -- and that is
        // only true while it cannot name something outside the service.
        for hostile in [
            "../../../etc/shadow.json",
            "/etc/passwd.json",
            "https://elsewhere.invalid/manifest.json",
            "releases//manifest.json",
            "releases/./manifest.json",
            "releases/../../manifest.json",
            "\\\\host\\share\\manifest.json",
            "",
            "releases/0.1.1/usr.erofs",
        ] {
            let error = locate("https://updates.example", &feed_document(hostile))
                .expect_err("{hostile} must be refused");
            assert!(error.contains("Remedy:"), "{hostile}: {error}");
        }
    }

    #[test]
    fn a_document_that_is_not_a_channel_file_is_refused_by_name() {
        let error = locate("https://updates.example", b"<html>404</html>").unwrap_err();
        assert!(error.contains("not one this release can read"), "{error}");
        assert!(error.contains("Remedy:"), "{error}");

        let error = locate("https://updates.example", &vec![b'x'; 70 * 1024]).unwrap_err();
        assert!(error.contains("about eighty"), "{error}");
    }

    #[test]
    fn two_appliances_do_not_ask_at_the_same_instant_and_each_keeps_its_own_offset() {
        // The property is spread, and the second half of it is stability: an offset drawn
        // fresh at every boot would put a whole fleet back on the same minute the moment a
        // release gives them all a reason to reboot.
        let one = jitter_secs("sha256:11111111");
        let two = jitter_secs("sha256:22222222");
        assert_ne!(one, two);
        assert_eq!(one, jitter_secs("sha256:11111111"));
        for seed in ["", "a", "sha256:deadbeef", "0.1.0.202608121430"] {
            assert!(
                jitter_secs(seed) < JITTER_SECS,
                "{seed} landed outside the hour"
            );
        }
    }

    #[test]
    fn a_fresh_appliance_has_never_checked_and_does_not_say_it_is_up_to_date() {
        // The difference that matters on a page: "nothing has been checked" and "there is
        // nothing newer" are both quiet states and only one of them is a claim.
        let discovery = Discovery::new();
        let state = discovery.snapshot();
        assert_eq!(state.status, Status::NeverChecked);
        assert!(state.available.is_none());
        assert!(state.checked_seconds_ago.is_none());
        assert!(state.error.is_none());
    }

    #[test]
    fn only_one_check_runs_at_a_time() {
        let discovery = Discovery::new();
        assert!(discovery.begin());
        assert!(!discovery.begin(), "a second caller sees the running one");

        discovery.finish(Availability {
            status: Status::UpToDate,
            ..Availability::default()
        });
        assert!(discovery.begin(), "and the next check may start");
    }

    #[test]
    fn a_finished_check_is_dated_by_a_clock_that_cannot_be_wrong() {
        // Not the RTC. This appliance has no time synchronisation, so an elapsed time is
        // the only honest answer to "when did you last look".
        let discovery = Discovery::new();
        discovery.finish(Availability::default());
        assert_eq!(discovery.snapshot().checked_seconds_ago, Some(0));
    }
}
