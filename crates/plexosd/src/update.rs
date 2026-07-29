//! Replacing `/usr` from the console, so a new build stops meaning a USB stick.
//!
//! The decision, the writing and the boot entry live in `plexos-update` and
//! [`crate::esp`]. This is the caller: it fetches a bundle, asks what should happen,
//! downloads the parts, hands them to the writer and installs the entry. It reports the
//! way [`crate::provision`] does, for the same reason — the work takes minutes and a
//! request cannot be held open for it.
//!
//! # What this trusts, and what makes it survivable
//!
//! Nothing signs the bundle. Whoever answers on the configured address chooses what
//! `/usr` this appliance will run, which is acceptable on a bench and nowhere else; the
//! page says so and [`plexos_update::Metadata::TRUSTED`] is a constant `false`.
//!
//! What makes that tolerable is not the transport. It is that the update goes to the slot
//! that is *not* running, and that `systemd-boot` sorts an entry with no tries left to the
//! end of its list — so a bundle that turns out to be rubbish costs three reboots and
//! lands back on the system that was working. The running slot is never written and the
//! working boot entry is never removed.
//!
//! # What has run
//!
//! **This has updated an appliance twice**, alternating slots, with no USB stick
//! involved. What it does not yet do is check a signature, and that has not changed.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use plexos_update::{Decision, Metadata};

/// Where a bundle is fetched from when the request does not say.
///
/// Empty, and deliberately: there is no discovery, inventing one would be a protocol
/// nobody asked for, and baking one developer's build host into every image is worse
/// than asking. `tools/publish-update.sh` prints the address to paste, which is the
/// shortest honest path from "I built something" to "the appliance has it".
///
/// A request with no source is therefore refused rather than sent somewhere arbitrary.
pub const DEFAULT_SOURCE: &str = "";

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
    /// Whether anything vouched for the bundle. Always false today, and reported so the
    /// page can say it rather than the reader having to know.
    pub trusted: bool,
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
    /// History rather than status: it is not cleared, and it carries the version it is
    /// about so a reader can tell "you were rolled back off this" from "an older boot of
    /// what you are running now failed once".
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
            trusted: Metadata::TRUSTED,
            gate: crate::gate::last_verdict(),
            rollback: crate::rollback::last(),
            log: Vec::new(),
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
        state.rollback = crate::rollback::last();
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
    pub fn finish(&self, outcome: Result<Option<String>, String>) {
        self.with(|state| match outcome {
            Ok(Some(version)) => {
                state.phase = Phase::Ready;
                state.detail = format!("{version} is written and will be tried on restart");
                push(&mut state.log, state.detail.clone());
                state.staged = Some(version);
            }
            Ok(None) => {
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

/// Runs an update in a new thread, reporting into `job`.
pub fn spawn(job: &Arc<Job>, source: String, install: bool) {
    let job = Arc::clone(job);
    std::thread::spawn(move || {
        let outcome = run(&job, &source, install);
        job.finish(outcome.map_err(|error| error.to_string()));
    });
}

/// Checks a bundle and, if `install`, writes it.
///
/// # Errors
/// Anything that stops the update. Nothing here can damage the running system: every
/// write goes to the slot that is not running, and the boot entry that works is never
/// touched.
pub fn run(
    job: &Job,
    source: &str,
    install: bool,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
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

    let document = fetch(&curl, &format!("{source}/update.json"))?;
    let bundle = Metadata::parse(&document)?;
    job.with(|state| state.available = Some(bundle.version.clone()));
    job.note(&format!(
        "bundle offers {} (root hash {})",
        bundle.version, bundle.root_hash
    ));

    let slot = running_slot();
    let running = running_version();
    let decision = plexos_update::plan(slot, &running, &bundle)?;

    let (target, version) = match decision {
        Decision::UpToDate { running } => {
            job.note(&format!("this appliance already runs {running}"));
            return Ok(None);
        }
        Decision::Install { target, version } => (target, version),
    };

    job.note(&format!(
        "running {running} on slot {slot}; {version} would go to slot {target}"
    ));
    if !install {
        return Ok(None);
    }

    // Cleared at the start rather than the end: an interrupted update should leave what
    // it had, not tidy the evidence away.
    let staging = Path::new(STAGING);
    let _ = std::fs::remove_dir_all(staging);
    std::fs::create_dir_all(staging)?;

    job.step(Phase::Downloading, &format!("downloading {version}"));
    let usr = stage(job, &curl, source, staging, &bundle.usr)?;
    let verity = stage(job, &curl, source, staging, &bundle.verity)?;
    let uki = stage(job, &curl, source, staging, bundle.uki_for(target))?;

    job.step(
        Phase::Writing,
        &format!("writing slot {target} and reading it back"),
    );
    write_partition(job, target.usr_label(), &usr, &bundle.usr)?;
    write_partition(job, target.verity_label(), &verity, &bundle.verity)?;

    job.step(Phase::Activating, "installing the boot entry, on trial");
    let device = plexos_sys::device::by_partlabel(plexos_types::partition::LABEL_ESP)?;
    let mut cleared = Vec::new();
    let installed = crate::esp::with_esp_mounted(&device, &mut |esp| {
        // Before the install, and inside the same mount. A failed update leaves an
        // exhausted 18 MB entry on an ESP sized for three, and nothing else ever removes
        // it -- so without this the partition the machine cannot boot without fills up
        // one bad update at a time. Found by causing a rollback rather than by reading.
        cleared = crate::esp::remove_wreckage(esp, &running);
        crate::esp::install_entry(esp, &uki, &version)
    })?;
    if !cleared.is_empty() {
        job.note(&format!(
            "removed the boot {} of a previous failed update: {}",
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

    let _ = std::fs::remove_dir_all(staging);
    Ok(Some(version))
}

/// Downloads one artifact and checks it against the digest the bundle declared.
fn stage(
    job: &Job,
    curl: &Path,
    source: &str,
    staging: &Path,
    artifact: &plexos_update::Artifact,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let destination = staging.join(&artifact.name);
    let url = format!("{source}/{}", artifact.name);

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
            "{} does not match the digest the bundle declared: got {found}, expected {}. \
             The download was corrupted; nothing has been written to a partition. \
             Retrying is the remedy.",
            artifact.name, artifact.sha256
        )
        .into());
    }
    job.note(&format!(
        "{} downloaded and matches its digest ({} MB)",
        artifact.name,
        artifact.size / 1_000_000
    ));
    Ok(destination)
}

/// Writes one staged file to its partition.
fn write_partition(
    job: &Job,
    label: &str,
    staged: &Path,
    artifact: &plexos_update::Artifact,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = std::fs::File::open(staged)?;
    let mut last = 0_u64;
    plexos_update::write::to_partition(
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

/// Fetches a small document as text.
fn fetch(curl: &Path, url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let output = std::process::Command::new(curl)
        .args(["--fail", "--silent", "--show-error", "--location"])
        .arg("--max-time")
        .arg("60")
        .arg(url)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "could not read {url}: {}. Nothing is serving an update bundle there.",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    let _ = std::io::stdout().flush();
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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
        assert!(!progress.trusted, "nothing signs a bundle yet");
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
        job.finish(Ok(Some("0.1.0.2".to_owned())));
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
        job.finish(Ok(None));
        let progress = job.snapshot();
        assert_eq!(progress.phase, Phase::Idle);
        assert!(progress.error.is_none());
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
    fn progress_serialises_to_what_the_page_reads() {
        let job = Job::new();
        job.begin();
        job.step(Phase::Writing, "usr_b: 12 of 74 MB");
        let json = serde_json::to_string(&job.snapshot()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["phase"], "writing");
        assert_eq!(parsed["trusted"], false);
    }
}
