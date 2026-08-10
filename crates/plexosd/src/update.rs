//! Replacing `/usr` from the console, so a new build stops meaning a USB stick.
//!
//! The decision, the writing and the boot entry live in `plexos-update` and
//! [`crate::esp`]. This is the caller: it fetches a bundle, asks what should happen,
//! downloads the parts, hands them to the writer and installs the entry. It reports the
//! way [`crate::provision`] does, for the same reason — the work takes minutes and a
//! request cannot be held open for it.
//!
//! # What this trusts
//!
//! A signature over the manifest's exact bytes, chaining to a root key compiled into
//! `/usr` (ADR-0006). Whoever answers on the configured address no longer chooses what
//! this appliance runs; they choose only whether it gets an update at all, which is a
//! denial of service and not a compromise.
//!
//! Two things are worth being precise about. **The address is still unauthenticated**, so
//! a bundle can be served from anywhere and moved without re-signing — that is deliberate,
//! and it is why sources in a manifest are file names rather than URLs. And **the trust is
//! only as good as the custody of the root key**: while a development key is in force its
//! private half is on a build host rather than offline, which is reported through
//! [`Progress::signature`] rather than hidden behind the word "signed".
//!
//! What makes a bad-but-signed update survivable is unchanged and is not the signature: the
//! update goes to the slot that is *not* running, and `systemd-boot` sorts an entry with no
//! tries left to the end of its list. The running slot is never written and the working
//! boot entry is never removed.
//!
//! # What has run
//!
//! **This has updated an appliance twice**, alternating slots, with no USB stick involved.
//! Both of those were unsigned, through the improvised `update.json` that this module no
//! longer reads. **No appliance has yet installed a signed manifest.**

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use plexos_update::{Decision, Role, trust};

/// Where a bundle is fetched from when the request does not say.
///
/// Empty, and deliberately: there is no discovery, inventing one would be a protocol
/// nobody asked for, and baking one developer's build host into every image is worse
/// than asking. `tools/publish-update.sh` prints the address to paste, which is the
/// shortest honest path from "I built something" to "the appliance has it".
///
/// A request with no source is therefore refused rather than sent somewhere arbitrary.
pub const DEFAULT_SOURCE: &str = "";

/// The manifest, its detached signature, and the revocation list, beside each other.
///
/// Fixed names rather than anything discovered: the appliance asks for exactly these three
/// and a publisher chooses only where they are served from.
pub const MANIFEST: &str = "manifest.json";
/// The detached Ed25519 signature over [`MANIFEST`]'s exact bytes, base64.
pub const SIGNATURE: &str = "manifest.json.sig";
/// The root-signed revocation list, if the publisher serves one.
pub const REVOCATIONS: &str = "revocations.json";

/// Where downloads are staged before anything is written to a partition.
///
/// Under the state root so it survives nothing in particular — it is deliberately
/// disposable, and cleared at the start of every run rather than at the end, so an
/// interrupted update leaves evidence rather than tidying it away.
pub const STAGING: &str = "/var/lib/plexos/update/staging";

/// How long the whole fetch may take.
pub const FETCH_TIMEOUT_SECS: u64 = 1800;

/// Lines of progress kept, bounded for the same reason provisioning bounds its own.
pub const MAX_LOG_LINES: usize = 200;

/// Where a run has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Nothing has been asked for.
    Idle,
    /// Reading the bundle's metadata.
    Checking,
    /// Downloading the image, its hash tree and the boot entry.
    Downloading,
    /// Writing the inactive slot and reading it back.
    Writing,
    /// Installing the boot entry, on trial.
    Activating,
    /// Written. The machine has to be restarted to use it.
    Ready,
    /// Gave up, and `error` says why.
    Failed,
}

impl Phase {
    /// Whether work is in flight.
    #[must_use]
    pub fn is_running(self) -> bool {
        matches!(
            self,
            Self::Checking | Self::Downloading | Self::Writing | Self::Activating
        )
    }
}

/// What `GET /api/update` reports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Progress {
    /// Where the run is.
    pub phase: Phase,
    /// One line describing what is happening now.
    pub detail: String,
    /// What this appliance is running.
    pub running: String,
    /// The slot it is running from.
    pub slot: String,
    /// The version last offered by a bundle, once one has been read.
    pub available: Option<String>,
    /// The version written and waiting for a restart.
    pub staged: Option<String>,
    /// Why the run failed, if it did.
    pub error: Option<String>,
    /// What vouched for the last manifest that was read, if anything did.
    ///
    /// `None` means nothing has been verified — either nothing has been checked yet, or the
    /// check failed, in which case `error` says how. It is deliberately not a `bool`: the
    /// question a reader has is not "is this signed" but "signed by what", and the answer
    /// includes whether the root of the chain is a development key whose private half sits
    /// on a build host. An appliance that said "signed" about that would be telling the
    /// reader something false.
    pub signature: Option<Signature>,
    /// What the boot health gate decided about this boot, once it has decided.
    ///
    /// Here because "did this slot become permanent" is a question about the system's
    /// relationship with its slots, which is what this endpoint is about — and because
    /// the answer was previously only ever printed to a console.
    pub gate: Option<String>,
    /// The last boot that was handed back to the other slot, if there has ever been one.
    ///
    /// The one field here that describes a system other than this one. A rollback reverts
    /// `/usr`, so the image that failed is gone and every explanation it held went with
    /// it; what a person otherwise sees is an appliance that quietly went backwards a
    /// version. This is read off `/var`, which rollback leaves alone (ADR-0005).
    ///
    /// Present only while it still describes this machine. The file behind it is never
    /// cleared — it is history, and worth keeping — but serving it unconditionally is how
    /// a nine-day-old rollback stayed on the page, in the future tense, on an appliance
    /// that had been healthy and permanent for a week. `rollback::last_for` makes the
    /// comparison; `rollback::last` is still there for anything that wants the history.
    pub rollback: Option<crate::rollback::Record>,
    /// Everything logged so far.
    pub log: Vec<String>,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            detail: "no update has been checked for".to_owned(),
            running: running_version(),
            slot: running_slot().to_string(),
            available: None,
            staged: None,
            error: None,
            signature: None,
            gate: crate::gate::last_verdict(),
            rollback: crate::rollback::last_for(&running_version()),
            log: Vec::new(),
        }
    }
}

/// Who vouched for a manifest, once the chain has been checked.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Signature {
    /// The signing key that signed the manifest.
    pub key_id: String,
    /// The root key that certified it.
    pub root_key_id: String,
    /// Whether that root is a development stand-in rather than an offline key.
    pub development: bool,
}

impl From<&trust::Verified> for Signature {
    fn from(verified: &trust::Verified) -> Self {
        Self {
            key_id: verified.key_id.clone(),
            root_key_id: verified.root_key_id.clone(),
            development: verified.development,
        }
    }
}

/// The slot this system booted from.
///
/// Read from the kernel command line, which is the only thing that knows: the partition
/// is mounted through a device-mapper name that is the same either way.
#[must_use]
pub fn running_slot() -> plexos_types::Slot {
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    match crate::status::cmdline_value(&cmdline, "plexos.slot").as_deref() {
        Some("b") => plexos_types::Slot::B,
        // A, and A for anything unreadable. Getting this wrong the other way would write
        // the running slot, so the safe default is the one the first image ships with.
        _ => plexos_types::Slot::A,
    }
}

/// The version this system is running, from `/etc/os-release`.
#[must_use]
pub fn running_version() -> String {
    let contents = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    running_version_from(&contents)
}

/// The running version, out of an `os-release` that has already been read.
///
/// Split out so the rollback record can pin that it reads the same field of the same
/// file. The console compares the two strings to decide whether a recorded rollback is
/// about the version running now, and that comparison is only meaningful while both
/// sides come from one source.
#[must_use]
pub fn running_version_from(os_release: &str) -> String {
    crate::status::os_release_value(os_release, "VERSION_ID")
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The one update job, and its progress.
#[derive(Debug, Default)]
pub struct Job {
    state: Mutex<Progress>,
}

impl Job {
    /// A job that has never run.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current state.
    #[must_use]
    pub fn snapshot(&self) -> Progress {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Refreshed on every read: these describe the machine rather than the run, and a
        // stale answer to "what am I running" is worse than none.
        state.running = running_version();
        state.slot = running_slot().to_string();
        state.gate = crate::gate::last_verdict();
        // Against the version just read, not a captured one: the whole point of the
        // comparison is that it is about the system answering this request.
        state.rollback = crate::rollback::last_for(&state.running);
        state.clone()
    }

    fn with<R>(&self, f: impl FnOnce(&mut Progress) -> R) -> R {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut state)
    }

    /// Claims the job, if no run holds it.
    pub fn begin(&self) -> bool {
        self.with(|state| {
            if state.phase.is_running() {
                return false;
            }
            let staged = state.staged.clone();
            *state = Progress {
                phase: Phase::Checking,
                detail: "reading the update bundle".to_owned(),
                staged,
                ..Progress::default()
            };
            state.log.push("starting".to_owned());
            true
        })
    }

    /// Moves to a phase.
    pub fn step(&self, phase: Phase, detail: &str) {
        self.with(|state| {
            state.phase = phase;
            detail.clone_into(&mut state.detail);
            push(&mut state.log, detail.to_owned());
        });
    }

    /// Records a line without changing the phase.
    pub fn note(&self, line: &str) {
        self.with(|state| push(&mut state.log, line.to_owned()));
    }

    /// Records the outcome.
    pub fn finish(&self, outcome: Result<Outcome, String>) {
        self.with(|state| match outcome {
            Ok(Outcome::Written(version)) => {
                state.phase = Phase::Ready;
                state.detail = format!("{version} is written and will be tried on restart");
                push(&mut state.log, state.detail.clone());
                state.staged = Some(version);
            }
            Ok(Outcome::Available(version)) => {
                state.phase = Phase::Idle;
                state.detail = format!("{version} is available, and nothing has been written");
                push(&mut state.log, state.detail.clone());
            }
            Ok(Outcome::UpToDate) => {
                state.phase = Phase::Idle;
                "already up to date".clone_into(&mut state.detail);
                push(&mut state.log, state.detail.clone());
            }
            Err(error) => {
                state.phase = Phase::Failed;
                "the update failed".clone_into(&mut state.detail);
                push(&mut state.log, error.clone());
                state.error = Some(error);
            }
        });
    }
}

fn push(log: &mut Vec<String>, line: String) {
    if log.len() >= MAX_LOG_LINES {
        log.remove(0);
    }
    log.push(line);
}

/// Whether the request asked to install rather than only to look.
///
/// Two verbs would have been one too many: checking and installing differ by one field,
/// and a caller that can reach one can reach the other. Absent means check only, because
/// the safe reading of an ambiguous request is the one that changes nothing.
#[must_use]
pub fn wants_install(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("install").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

/// Where the request said to fetch from, or [`DEFAULT_SOURCE`].
#[must_use]
pub fn source_in(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("source")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        })
        .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
        .unwrap_or_else(|| DEFAULT_SOURCE.to_owned())
}

/// How a run ended, when it ended well.
///
/// Three outcomes and not two. A check that finds a newer release and a check that finds
/// nothing both do the same thing — nothing — and reporting them the same way produced a
/// page that said "already up to date" directly underneath a line naming the version it
/// had just found. Seen on the appliance the first time this was driven end to end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Written to the inactive slot; the version is waiting for a restart.
    Written(String),
    /// A newer version exists and was not asked for.
    Available(String),
    /// This appliance runs the newest release offered.
    UpToDate,
}

/// Runs an update in a new thread, reporting into `job`.
pub fn spawn(job: &Arc<Job>, source: String, install: bool) {
    let job = Arc::clone(job);
    std::thread::spawn(move || {
        let outcome = run(&job, &source, install);
        job.finish(outcome.map_err(|error| error.to_string()));
    });
}

/// Checks an update and, if `install`, writes it.
///
/// # Errors
/// Anything that stops the update. Nothing here can damage the running system: every
/// write goes to the slot that is not running, and the boot entry that works is never
/// touched.
pub fn run(job: &Job, source: &str, install: bool) -> Result<Outcome, Box<dyn std::error::Error>> {
    if source.is_empty() {
        return Err(
            "no update source was given, and there is no default: paste the \
                    address that tools/publish-update.sh prints on the build host. \
                    Guessing where to fetch a whole operating system from is not \
                    something this appliance will do."
                .into(),
        );
    }

    let curl = plexos_plex::tools::resolve("curl", &|p: &Path| p.exists())
        .ok_or("curl is not in this image, so no bundle can be fetched")?;

    let running = running_version();
    let manifest = believe(job, &curl, source, &running)?;

    let slot = running_slot();
    let (target, version) = match plexos_update::plan(slot, &running, &manifest)? {
        Decision::UpToDate { running } => {
            job.note(&format!("this appliance already runs {running}"));
            return Ok(Outcome::UpToDate);
        }
        Decision::Install { target, version } => (target, version),
    };

    job.note(&format!(
        "running {running} on slot {slot}; {version} would go to slot {target}"
    ));
    if !install {
        return Ok(Outcome::Available(version));
    }

    write_slot(job, &curl, source, &manifest, target, &version, &running)?;
    Ok(Outcome::Written(version))
}

/// Fetches the manifest and returns it only if this appliance may act on it.
///
/// Everything that can refuse an update before a byte of it is downloaded: the signature
/// chain, the revocation list, and the anti-rollback floor.
fn believe(
    job: &Job,
    curl: &Path,
    source: &str,
    running: &str,
) -> Result<plexos_types::manifest::Manifest, Box<dyn std::error::Error>> {
    // The manifest is held as the bytes that arrived and is never re-encoded on the way to
    // the signature check. `fetch_text` would be enough to parse it and would quietly
    // replace anything invalid with U+FFFD, which is a manifest that parses, verifies
    // against nothing, and reports a signature failure about a document nobody mistyped.
    let raw = plexos_types::manifest::RawManifest::new(fetch_bytes(
        curl,
        &format!("{source}/{MANIFEST}"),
    )?);
    let signature = trust::decode_signature(&fetch_text(curl, &format!("{source}/{SIGNATURE}"))?)?;

    let revoked = revocations_in_force(job, curl, source);
    let now = expiry_clock(running);
    let policy = trust::Policy::of_this_build(now.as_deref()).revoking(&revoked);

    let verified = trust::verify(&policy, &raw, &signature)?;
    job.with(|state| {
        state.available = Some(verified.manifest.release.clone());
        state.signature = Some(Signature::from(&verified));
    });
    job.note(&format!(
        "{} is signed by {}, certified by root key {}{}",
        verified.manifest.release,
        verified.key_id,
        verified.root_key_id,
        if verified.development {
            " -- a development key, whose private half is on a build host rather than \
             offline"
        } else {
            ""
        }
    ));

    // Before the plan, because being offered a downgrade is a different thing from being
    // offered something that does not fit, and the first one is the one worth saying out
    // loud: every signature on it was valid.
    let floor = plexos_update::sequence::floor(
        plexos_update::sequence::recorded(Path::new(plexos_types::paths::ACCEPTED_SEQUENCE_FILE)),
        running,
    );
    plexos_update::sequence::check(
        verified.manifest.sequence,
        verified.manifest.min_sequence,
        floor,
    )?;
    job.note(&format!(
        "sequence {} is at or above the {floor} this appliance has accepted",
        verified.manifest.sequence
    ));

    Ok(verified.manifest)
}

/// Downloads the update and writes it to the slot that is not running.
fn write_slot(
    job: &Job,
    curl: &Path,
    source: &str,
    manifest: &plexos_types::manifest::Manifest,
    target: plexos_types::Slot,
    version: &str,
    running: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Cleared at the start rather than the end: an interrupted update should leave what
    // it had, not tidy the evidence away.
    // Every partition this writes is on the disk the running system is on, and nothing
    // else. Without it, an update on a machine that has installed itself lands wherever
    // the kernel happened to enumerate a duplicate label first.
    let disk = &running_disk_or_refuse()?;

    let staging = Path::new(STAGING);
    let _ = std::fs::remove_dir_all(staging);
    std::fs::create_dir_all(staging)?;

    job.step(Phase::Downloading, &format!("downloading {version}"));
    let usr = stage(job, curl, source, staging, Role::Usr, &manifest.usr.image)?;
    let verity = stage(
        job,
        curl,
        source,
        staging,
        Role::Verity,
        &manifest.usr.verity.hashes,
    )?;
    let uki = stage(
        job,
        curl,
        source,
        staging,
        Role::Uki,
        manifest.uki.for_slot(target),
    )?;

    // The digest proved this is the file the manifest named. This proves the manifest
    // named the right one, which is a mistake a publishing script makes and a signature
    // cannot see, because the signature is over the mistake.
    let root_hash = &manifest.usr.verity.root_hash;
    plexos_update::uki::check(&std::fs::read(&uki)?, target, root_hash)?;
    job.note(&format!(
        "the boot entry carries plexos.slot={target} and root hash {root_hash}"
    ));

    job.step(
        Phase::Writing,
        &format!("writing slot {target} and reading it back"),
    );
    write_partition(job, disk, target.usr_label(), &usr, &manifest.usr.image)?;
    write_partition(
        job,
        disk,
        target.verity_label(),
        &verity,
        &manifest.usr.verity.hashes,
    )?;

    job.step(Phase::Activating, "installing the boot entry, on trial");
    let device = plexos_sys::device::by_partlabel_on(disk, plexos_types::partition::LABEL_ESP)?;
    let mut cleared = Vec::new();
    let installed = crate::esp::with_esp_mounted(&device, &mut |esp| {
        // Before the install, and inside the same mount. Both partitions have been
        // written by now, so the disk holds exactly two versions of /usr -- the running
        // one and the one just written -- and every entry naming any other version points
        // at a filesystem that has been overwritten. Leaving them cost a 511 MB ESP: 25
        // entries, 100% full, and an install that ran out of room halfway through copying
        // a kernel the bootloader would then have tried first.
        cleared = crate::esp::remove_superseded(esp, running);
        crate::esp::install_entry(esp, &uki, version)
    })?;
    if !cleared.is_empty() {
        job.note(&format!(
            "removed {} superseded boot {}: {}",
            cleared.len(),
            if cleared.len() == 1 {
                "entry"
            } else {
                "entries"
            },
            cleared.join(", ")
        ));
    }
    job.note(&format!(
        "{} installed with {} tries; the entry that works now is untouched and is the \
         way back",
        installed.file_name().unwrap_or_default().to_string_lossy(),
        crate::esp::INITIAL_TRIES
    ));

    // Last, and deliberately. Recording the sequence raises the floor permanently, so
    // doing it before the entry is installed would mean a failed download had refused
    // this release forever -- an appliance that will not take the update it just failed to
    // finish, and no way from the network to lower the number again.
    if let Err(error) = plexos_update::sequence::record(
        Path::new(plexos_types::paths::ACCEPTED_SEQUENCE_FILE),
        manifest.sequence,
    ) {
        job.note(&format!(
            "the update is installed, but sequence {} could not be recorded ({error}), so \
             this appliance would accept an older release again. /var may be full or \
             read-only.",
            manifest.sequence
        ));
    }

    let _ = std::fs::remove_dir_all(staging);
    Ok(())
}

/// The disk the running system is on, or a refusal to write anything.
///
/// `None` from [`crate::install::running_disk`] means the question could not be answered,
/// and that is not the same as "any disk will do": it is the one state in which writing
/// could land on a disk nobody chose.
fn running_disk_or_refuse() -> Result<String, Box<dyn std::error::Error>> {
    crate::install::running_disk(&plexos_gpu::env::System).ok_or_else(|| {
        Box::<dyn std::error::Error>::from(
            "this machine's own disk could not be identified, so nothing will be written. \
             PlexOS finds it behind the verified /usr; not finding it means this is not a \
             booted PlexOS system.",
        )
    })
}

/// The current time, if this appliance's clock is plausible enough to judge expiry.
///
/// A wrong clock that is believed refuses every future update, which from outside is
/// indistinguishable from a broken update path. There is no time synchronisation here, so
/// the only reference is the running image's own build stamp: an image cannot predate
/// itself.
fn expiry_clock(running: &str) -> Option<String> {
    let now = plexos_update::clock::now()?;
    let built = plexos_update::clock::built_at(running)?;
    trust::expiry_is_checkable(&now, &built).then_some(now)
}

/// The signing keys this appliance will not believe, refreshed from the source if it
/// offers a newer list.
///
/// Failure here is never fatal and never permissive: whatever list is already in force
/// stays in force. The list an appliance holds can only be replaced by a root-signed one
/// with a higher counter, so somebody who can answer at this address can withhold a
/// revocation but cannot withdraw one.
fn revocations_in_force(job: &Job, curl: &Path, source: &str) -> Vec<String> {
    let path = Path::new(plexos_types::paths::REVOCATION_FILE);
    let held = std::fs::read_to_string(path)
        .ok()
        .and_then(|document| trust::verify_revocations(trust::ROOT_KEYS, &document).ok())
        .unwrap_or_else(trust::Revocations::none);

    let offered = match fetch_optional(curl, &format!("{source}/{REVOCATIONS}")) {
        Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
        // No list published here is the ordinary case and says nothing about the one held.
        Ok(None) => return held.revoked,
        Err(error) => {
            job.note(&format!("could not ask for a revocation list: {error}"));
            return held.revoked;
        }
    };

    match trust::verify_revocations(trust::ROOT_KEYS, &offered) {
        Ok(list) if list.supersedes(&held) => {
            if let Err(error) = plexos_update::atomic::write(path, offered.as_bytes()) {
                job.note(&format!(
                    "a newer revocation list was accepted but could not be stored \
                     ({error}), so it applies to this update and will have to be fetched \
                     again for the next one"
                ));
            }
            job.note(&format!(
                "revocation list updated to counter {}; {} signing {} no longer believed",
                list.counter,
                list.revoked.len(),
                if list.revoked.len() == 1 {
                    "key is"
                } else {
                    "keys are"
                }
            ));
            list.revoked
        }
        // Two silences, for two reasons. A list that is not newer is what every ordinary
        // update looks like once one exists, and a line saying so on every run would push
        // the useful ones out of a bounded log. And a build with no root keys is about to
        // refuse the manifest, which says the same thing and says it better.
        Ok(_) | Err(trust::TrustError::NoRootKeys) => held.revoked,
        Err(error) => {
            job.note(&format!(
                "the revocation list published here was not used: {error}"
            ));
            held.revoked
        }
    }
}

/// Downloads one artifact and checks it against the digest the manifest declared.
fn stage(
    job: &Job,
    curl: &Path,
    source: &str,
    staging: &Path,
    role: Role,
    artifact: &plexos_types::manifest::Artifact,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let url = plexos_update::location::resolve(source, role, artifact)?;
    // Named by role, so nothing a publisher writes chooses a path on this appliance.
    let destination = staging.join(role.staging_name());

    let output = std::process::Command::new(curl)
        .args(["--fail", "--silent", "--show-error", "--location"])
        .arg("--max-time")
        .arg(FETCH_TIMEOUT_SECS.to_string())
        .arg("--output")
        .arg(&destination)
        .arg(&url)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "could not fetch {url}: {}. Check that the publisher is still serving it.",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }

    // Before anything reaches a partition. A truncated download and a wrong image need
    // opposite responses, and this is the only place they can still be told apart.
    let found = plexos_update::write::digest_of_file(&destination)?;
    if found != artifact.sha256 {
        return Err(format!(
            "{url} does not match the digest the manifest declared: got {found}, expected \
             {}. Nothing has been written to a partition. If the manifest verified, this \
             is a corrupted download and retrying is the remedy.",
            artifact.sha256
        )
        .into());
    }
    job.note(&format!(
        "{role} downloaded and matches its digest ({} MB)",
        artifact.size / 1_000_000
    ));
    Ok(destination)
}

/// Writes one staged file to its partition.
fn write_partition(
    job: &Job,
    disk: &str,
    label: &str,
    staged: &Path,
    artifact: &plexos_types::manifest::Artifact,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = std::fs::File::open(staged)?;
    let mut last = 0_u64;
    plexos_update::write::to_partition(
        disk,
        label,
        &mut file,
        artifact.size,
        &artifact.sha256,
        &mut |done, total| {
            // Every 16 MiB rather than every megabyte: the log is bounded and a hundred
            // near-identical lines would push the useful ones out of it.
            if done - last >= 16 * 1024 * 1024 || done == total {
                last = done;
                job.step(
                    Phase::Writing,
                    &format!("{label}: {} of {} MB", done / 1_000_000, total / 1_000_000),
                );
            }
        },
    )?;
    job.note(&format!("{label} written and verified"));
    Ok(())
}

/// Fetches a small document as the bytes that arrived.
fn fetch_bytes(curl: &Path, url: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let output = std::process::Command::new(curl)
        .args(["--fail", "--silent", "--show-error", "--location"])
        .arg("--max-time")
        .arg("60")
        .arg(url)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "could not read {url}: {}. Nothing is serving a signed update there. Remedy: \
             check the address, and check that the bundle was published by \
             tools/sign-bundle.sh -- an unsigned bundle has no {MANIFEST} and this release \
             will not install one.",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    let _ = std::io::stdout().flush();
    Ok(output.stdout)
}

/// [`fetch_bytes`], where the document not being there is an answer rather than a failure.
///
/// curl exits 22 for an HTTP error status when `--fail` is given, which separates "the
/// publisher does not serve this" from "the network is broken" — and those have opposite
/// meanings for an optional document.
fn fetch_optional(curl: &Path, url: &str) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error>> {
    const HTTP_ERROR: i32 = 22;

    let output = std::process::Command::new(curl)
        .args(["--fail", "--silent", "--show-error", "--location"])
        .arg("--max-time")
        .arg("60")
        .arg(url)
        .output()?;

    if output.status.code() == Some(HTTP_ERROR) {
        return Ok(None);
    }
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr)
            .trim()
            .to_owned()
            .into());
    }
    Ok(Some(output.stdout))
}

/// Fetches a small document as text, for things that are text by definition.
fn fetch_text(curl: &Path, url: &str) -> Result<String, Box<dyn std::error::Error>> {
    Ok(String::from_utf8_lossy(&fetch_bytes(curl, url)?).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_job_says_what_the_machine_is_running() {
        // The page shows this before anything has been checked, so it cannot be empty.
        let progress = Job::new().snapshot();
        assert_eq!(progress.phase, Phase::Idle);
        assert!(!progress.running.is_empty());
        assert!(matches!(progress.slot.as_str(), "a" | "b"));
        assert!(
            progress.signature.is_none(),
            "nothing has been verified yet, and that must not read as signed"
        );
    }

    #[test]
    fn an_unreadable_command_line_reports_slot_a() {
        // Guessing B would mean writing the slot that is running. A is what the first
        // image ships with, so it is the guess that cannot destroy anything.
        assert_eq!(
            crate::status::cmdline_value("nothing here", "plexos.slot"),
            None
        );
        assert_eq!(running_slot(), plexos_types::Slot::A);
    }

    #[test]
    fn only_one_update_may_hold_the_job() {
        let job = Job::new();
        assert!(job.begin());
        assert!(!job.begin());
    }

    #[test]
    fn a_staged_version_survives_a_later_check() {
        // Somebody who has written an update and not yet restarted must still be told
        // there is one waiting, even after asking whether there is a newer one.
        let job = Job::new();
        job.begin();
        job.finish(Ok(Outcome::Written("0.1.0.2".to_owned())));
        assert_eq!(job.snapshot().staged.as_deref(), Some("0.1.0.2"));

        job.begin();
        assert_eq!(
            job.snapshot().staged.as_deref(),
            Some("0.1.0.2"),
            "a new check must not forget what is already written"
        );
    }

    #[test]
    fn being_up_to_date_is_not_a_failure() {
        let job = Job::new();
        job.begin();
        job.finish(Ok(Outcome::UpToDate));
        let progress = job.snapshot();
        assert_eq!(progress.phase, Phase::Idle);
        assert!(progress.error.is_none());

        // And the other way a run ends without writing anything. These were one outcome
        // until the appliance reported "already up to date" under a line naming the
        // version it had just found.
        job.begin();
        job.finish(Ok(Outcome::Available("0.1.0.9".to_owned())));
        let progress = job.snapshot();
        assert_eq!(progress.phase, Phase::Idle);
        assert!(progress.error.is_none());
        assert!(
            progress.detail.contains("0.1.0.9")
                && progress.detail.contains("nothing has been written"),
            "a check that found an update must not read as one that found none: {}",
            progress.detail
        );
    }

    #[test]
    fn an_ambiguous_request_checks_rather_than_installs() {
        // The safe reading of "I could not tell what you meant" is the one that changes
        // nothing. Installing on a malformed body would write a partition because a
        // field was misspelled.
        assert!(!wants_install(b"{}"));
        assert!(!wants_install(b""));
        assert!(!wants_install(br#"{"install":"yes"}"#));
        assert!(wants_install(br#"{"install":true}"#));
    }

    #[test]
    fn a_source_that_is_not_a_url_falls_back_rather_than_being_used() {
        // The source is joined to file names and handed to curl. A body that could put
        // anything here would choose what this appliance fetches and how.
        // And the fallback is nothing at all: an appliance that guessed where to fetch
        // an operating system from would be a worse idea than one that asks.
        assert_eq!(source_in(b"{}"), "");
        assert_eq!(source_in(br#"{"source":"file:///etc"}"#), "");
        assert_eq!(source_in(br#"{"source":"/etc"}"#), "");
        assert_eq!(
            source_in(br#"{"source":"http://192.168.2.9:8080/b"}"#),
            "http://192.168.2.9:8080/b"
        );
    }

    #[test]
    fn a_clock_that_cannot_be_judged_is_not_used_to_expire_certificates() {
        // There is no time synchronisation here. A wrong clock that is believed refuses
        // every future update, which from outside is indistinguishable from a bricked
        // update path -- and unlike an expired certificate, nobody would know where to
        // look.
        assert_eq!(
            expiry_clock("unknown"),
            None,
            "a version with no build stamp gives nothing to judge the clock against"
        );
        assert_eq!(
            expiry_clock("0.1.0"),
            None,
            "and neither does one that was never stamped"
        );

        // A build host's clock is after the stamp of any image it could be running, so
        // this is the branch that does check expiry.
        assert!(expiry_clock("0.1.0.202607281844").is_some());
    }

    #[test]
    fn the_documents_this_asks_for_are_the_ones_the_publisher_writes() {
        // Three fixed names. tools/sign-bundle.sh writes the first two and they are what
        // an appliance refuses an update for the absence of, so a rename here is a rename
        // in two places or an update path that stops working.
        assert_eq!(MANIFEST, "manifest.json");
        assert_eq!(SIGNATURE, "manifest.json.sig");
        assert_eq!(REVOCATIONS, "revocations.json");
    }

    #[test]
    fn progress_serialises_to_what_the_page_reads() {
        let job = Job::new();
        job.begin();
        job.step(Phase::Writing, "usr_b: 12 of 74 MB");
        let json = serde_json::to_string(&job.snapshot()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["phase"], "writing");
        assert!(
            parsed["signature"].is_null(),
            "the page distinguishes unverified from signed, so this may not be absent"
        );
    }
}
