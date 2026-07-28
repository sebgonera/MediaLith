//! Installing Plex from the console, with the work in the background.
//!
//! ADR-0010 says PlexOS never ships Plex: the appliance fetches the official Debian
//! package from Plex's own endpoints, checks its signature against a pinned key, and
//! turns the payload into an app image. Every one of those steps is built and tested in
//! `plexos-plex`. What was missing was a caller — and a caller that does not block an
//! HTTP request for the two minutes the work takes.
//!
//! # The shape this forces
//!
//! `POST /api/provision` starts a thread and returns immediately; `GET /api/provision`
//! reports where that thread has got to. The page polls the second one. A request that
//! waited for the download would time out in the browser long before the work finished,
//! and the administrator would be left refreshing a page with no way to tell a slow
//! install from a dead one.
//!
//! One job at a time, enforced here rather than by the page. Two concurrent runs would
//! unpack into the same staging directory and produce an image neither of them could
//! vouch for.
//!
//! # What is trusted
//!
//! The catalogue is fetched over HTTPS from Plex, and the package URL it names is
//! required to be HTTPS on a Plex host — a catalogue that pointed somewhere else would
//! otherwise redirect the download wherever it liked. That check is a guard rail, not
//! the security boundary: **the signature is**. `plexos_plex::verify` checks the
//! clear-signed manifest against the key in the image, and
//! [`plexos_plex::agrees_with`] ties that manifest to the bytes actually downloaded.
//! Nothing is unpacked until both pass.
//!
//! The SHA-1 in the catalogue is checked too, and is worth being precise about: it
//! detects a truncated or corrupted download, and it is not evidence of anything, since
//! whoever could serve a bad package could serve a matching catalogue. It runs because a
//! transfer failure and a hostile package deserve different messages.
//!
//! # What has run
//!
//! The pipeline below is the one `plexos-plex`'s `provision` example runs end to end on
//! a build host against real downloads. **This module has not run on the appliance.**
//! Delete this notice when it has.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use plexos_plex::{ar, build, execute, manifest, store, tools, verify};

/// Plex's published list of downloads.
///
/// Version 5 of the endpoint, which is what the current clients use. It is fetched
/// rather than hard-coded per release for the obvious reason: a URL pattern assembled
/// here would go stale the first time upstream changed its packaging, and ADR-0010
/// already names that as a release-blocking event to be noticed rather than guessed at.
pub const CATALOGUE_URL: &str = "https://plex.tv/api/downloads/5.json";

/// The build this appliance runs. From the catalogue's own vocabulary.
pub const BUILD: &str = "linux-x86_64";

/// The packaging we can read. `plexos_plex::ar` parses `.deb`, not `.rpm`.
pub const DISTRO: &str = "debian";

/// Hosts a package may be downloaded from.
///
/// Suffix-matched against the URL's host, so `downloads.plex.tv` passes and
/// `downloads.plex.tv.example.com` does not.
pub const ALLOWED_HOSTS: [&str; 2] = ["plex.tv", "plex.direct"];

/// How long the whole download may take.
///
/// Generous: 83 MB over a slow domestic uplink is minutes, and an install that fails
/// because someone's line is slow would be a poor first experience. Not unbounded,
/// because a stalled transfer must eventually become an error a person can read.
pub const DOWNLOAD_TIMEOUT_SECS: u64 = 1800;

/// How long fetching the catalogue may take. Small: it is 56 KB of JSON.
pub const CATALOGUE_TIMEOUT_SECS: u64 = 60;

/// How often the growing download is measured.
///
/// Half a second: often enough that the figure on the page moves, rare enough that the
/// `stat()` costs nothing next to the transfer.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Lines of progress kept for the page.
///
/// Bounded because this lives in memory for the life of the daemon, and an install that
/// retried in a loop would otherwise grow it without limit.
pub const MAX_LOG_LINES: usize = 200;

/// A package Plex publishes for this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Upstream's version string, as it appears in the catalogue.
    pub version: String,
    /// Where the package is.
    pub url: String,
    /// The catalogue's SHA-1 of the package. See the module documentation for what this
    /// does and does not prove.
    pub sha1: String,
}

/// Why a release could not be chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogueError {
    /// The document was not JSON, or not the shape this endpoint has always had.
    Unreadable(String),
    /// Plex publishes nothing matching [`BUILD`] and [`DISTRO`].
    NoMatch,
    /// The chosen release names a URL this appliance will not fetch.
    Untrusted(String),
}

impl std::fmt::Display for CatalogueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(detail) => write!(
                f,
                "Plex's download catalogue could not be read: {detail}. Either the \
                 endpoint changed shape or something on the network answered in its \
                 place. Check {CATALOGUE_URL} from another machine; if it looks \
                 different from what this expects, PlexOS needs updating rather than \
                 retrying."
            ),
            Self::NoMatch => write!(
                f,
                "Plex's catalogue lists no {DISTRO} package for {BUILD}. Upstream has \
                 changed what it publishes, which ADR-0010 treats as release-blocking \
                 for new installs. Existing installations are unaffected. Supplying the \
                 package by hand is the way round it."
            ),
            Self::Untrusted(url) => write!(
                f,
                "the catalogue points the download at {url}, which is not HTTPS on a \
                 Plex host, so nothing was fetched. This is not a transient failure: \
                 either the catalogue was tampered with in transit or upstream now \
                 serves from somewhere this build does not know about."
            ),
        }
    }
}

impl std::error::Error for CatalogueError {}

/// Chooses the package for this appliance out of Plex's catalogue.
///
/// Pure, so the whole selection — including the host check — is testable against a
/// recorded document rather than against the live endpoint.
///
/// # Errors
/// See [`CatalogueError`].
pub fn pick(catalogue: &str) -> Result<Release, CatalogueError> {
    let document: serde_json::Value = serde_json::from_str(catalogue)
        .map_err(|error| CatalogueError::Unreadable(error.to_string()))?;

    let linux = document
        .get("computer")
        .and_then(|c| c.get("Linux"))
        .ok_or_else(|| CatalogueError::Unreadable("no computer.Linux section".to_owned()))?;

    let version = linux
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| CatalogueError::Unreadable("no computer.Linux.version".to_owned()))?;

    let releases = linux
        .get("releases")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| CatalogueError::Unreadable("no computer.Linux.releases".to_owned()))?;

    let chosen = releases
        .iter()
        .find(|release| {
            let field = |name: &str| release.get(name).and_then(serde_json::Value::as_str);
            field("build") == Some(BUILD) && field("distro") == Some(DISTRO)
        })
        .ok_or(CatalogueError::NoMatch)?;

    let url = chosen
        .get("url")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| CatalogueError::Unreadable("the matching release has no url".to_owned()))?;

    if !is_trusted(url) {
        return Err(CatalogueError::Untrusted(url.to_owned()));
    }

    Ok(Release {
        version: version.to_owned(),
        url: url.to_owned(),
        sha1: chosen
            .get("checksum")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

/// Whether a URL is one this appliance will download a package from.
///
/// HTTPS, and a host that is or ends in one of [`ALLOWED_HOSTS`]. Parsed by hand because
/// nothing here needs a URL crate: the shape being accepted is `https://host/path`, and
/// anything else is refused rather than interpreted.
#[must_use]
pub fn is_trusted(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    // Everything before the first '/' is the authority. Any '@' in it is userinfo, which
    // is the classic way to make a URL look like it points somewhere it does not, so a
    // URL carrying one is refused rather than having its host extracted.
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.contains('@') || authority.is_empty() {
        return false;
    }
    let host = authority.split(':').next().unwrap_or_default();

    ALLOWED_HOSTS
        .iter()
        .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
}

/// Where a provisioning run has got to.
///
/// Ordered as the work happens, so the page can render a sequence rather than a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Nothing has been asked for.
    Idle,
    /// Reading Plex's catalogue.
    Catalogue,
    /// Downloading the package.
    Downloading,
    /// Checking the signature and tying it to the bytes.
    Verifying,
    /// Unpacking and building the app image.
    Building,
    /// Mounting the new image and starting Plex.
    Starting,
    /// Installed.
    Done,
    /// Gave up, and `error` says why.
    Failed,
}

impl Phase {
    /// Whether work is in flight.
    #[must_use]
    pub fn is_running(self) -> bool {
        matches!(
            self,
            Self::Catalogue | Self::Downloading | Self::Verifying | Self::Building | Self::Starting
        )
    }
}

/// What `GET /api/provision` reports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Progress {
    /// Where the run is.
    pub phase: Phase,
    /// One line describing what is happening now.
    pub detail: String,
    /// The version installed, once one has been.
    pub version: Option<String>,
    /// Why the run failed, if it did. Carries its own remedy.
    pub error: Option<String>,
    /// Everything logged so far, bounded by [`MAX_LOG_LINES`].
    pub log: Vec<String>,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            detail: "no installation has been started".to_owned(),
            version: None,
            error: None,
            log: Vec::new(),
        }
    }
}

/// What `GET /api/provision` actually answers: the machine's state, and any run in it.
///
/// The progress alone is not enough for the page. After a reboot the job is idle on a
/// machine with Plex installed and running, and a page that read only the job would
/// offer to install it again.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Report {
    /// The installation in progress, or the last one, or none.
    #[serde(flatten)]
    pub progress: Progress,
    /// Whether an app image is mounted and holds a Plex to run.
    pub installed: bool,
    /// Whether this daemon has a Plex running right now.
    pub running: bool,
    /// Where Plex's own interface is, once there is one to visit.
    ///
    /// A port and a path rather than a whole URL: the page knows the host it was served
    /// from, and this appliance does not reliably know how it was reached.
    pub web: &'static str,
}

/// Plex's own web interface, relative to the appliance's address.
pub const PLEX_WEB: &str = ":32400/web";

/// The one provisioning job this daemon will run, and its progress.
///
/// A mutex rather than a channel: the page asks "where are you now", which is a question
/// about current state, and a channel would make every reader responsible for replaying
/// a history to answer it.
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
    ///
    /// # Panics
    /// If a previous holder of the lock panicked. The state is a plain struct that no
    /// operation can leave half-written, so recovering the poisoned lock is correct
    /// rather than merely convenient.
    #[must_use]
    pub fn snapshot(&self) -> Progress {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether a run is in flight.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.snapshot().phase.is_running()
    }

    fn with<R>(&self, f: impl FnOnce(&mut Progress) -> R) -> R {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut state)
    }

    /// Claims the job for a new run, if no other run holds it.
    ///
    /// Returns `false` when one is already in flight. Checked and set under one lock, so
    /// two requests arriving together cannot both be told they may proceed.
    pub fn begin(&self) -> bool {
        self.with(|state| {
            if state.phase.is_running() {
                return false;
            }
            *state = Progress {
                phase: Phase::Catalogue,
                detail: "asking Plex what it publishes".to_owned(),
                version: None,
                error: None,
                log: vec!["starting".to_owned()],
            };
            true
        })
    }

    /// Moves to a phase, and records the line describing it.
    pub fn step(&self, phase: Phase, detail: &str) {
        self.with(|state| {
            state.phase = phase;
            detail.clone_into(&mut state.detail);
            push_line(&mut state.log, detail.to_owned());
        });
    }

    /// Records a line without changing the phase.
    ///
    /// Used for the executor's own output, which is a running commentary rather than a
    /// change of state.
    pub fn note(&self, line: &str) {
        self.with(|state| push_line(&mut state.log, line.to_owned()));
    }

    /// Records the outcome.
    pub fn finish(&self, outcome: Result<String, String>) {
        self.with(|state| match outcome {
            Ok(version) => {
                state.phase = Phase::Done;
                state.detail = format!("Plex {version} is installed");
                push_line(&mut state.log, state.detail.clone());
                state.version = Some(version);
            }
            Err(error) => {
                state.phase = Phase::Failed;
                "installation failed".clone_into(&mut state.detail);
                push_line(&mut state.log, error.clone());
                state.error = Some(error);
            }
        });
    }
}

/// Appends a line, dropping the oldest once [`MAX_LOG_LINES`] is reached.
fn push_line(log: &mut Vec<String>, line: String) {
    if log.len() >= MAX_LOG_LINES {
        log.remove(0);
    }
    log.push(line);
}

/// Runs a whole provisioning cycle in a new thread, reporting into `job`.
///
/// Returns immediately. The caller has already claimed the job with [`Job::begin`];
/// doing it here would leave a window in which a second request saw an idle job.
pub fn spawn(job: &Arc<Job>, plex: &Arc<crate::plex::Handle>, apps: PathBuf, keyring: PathBuf) {
    let job = Arc::clone(job);
    let plex = Arc::clone(plex);
    std::thread::spawn(move || {
        // Armed for the whole run. Without it a panic anywhere in the pipeline would
        // unwind past finish(), leaving the job in a running phase that nothing ever
        // clears -- so the console would report an installation in progress for ever and
        // refuse to start another. A stuck appliance is a worse outcome than a failure
        // with an ugly message.
        let mut guard = Unfinished::arm(&job);
        let outcome = run(&job, &apps, &keyring);

        // Only after a successful build. Mounting and starting from an image that was
        // never published would run whatever the previous attempt left behind.
        if outcome.is_ok() {
            job.step(Phase::Starting, "mounting the app image and starting Plex");
            crate::plex::mount_and_start(&plex, &mut |line| job.note(line));
        }

        guard.disarm();
        job.finish(outcome.map_err(|error| error.to_string()));
    });
}

/// Marks a run failed if the thread ends without saying how it went.
///
/// Only a panic can do that, and only a bug can panic — but the cost of not handling it
/// is an appliance that reports an installation in progress until it is rebooted.
struct Unfinished {
    job: Arc<Job>,
    armed: bool,
}

impl Unfinished {
    fn arm(job: &Arc<Job>) -> Self {
        Self {
            job: Arc::clone(job),
            armed: true,
        }
    }

    /// The run reached its own ending, and will record its own outcome.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for Unfinished {
    fn drop(&mut self) {
        if self.armed {
            self.job.finish(Err(UNEXPECTED_END.to_owned()));
        }
    }
}

/// What a run that ended without an outcome reports.
const UNEXPECTED_END: &str = "the installation stopped unexpectedly, without saying why. Nothing was published, \
     so the machine is as it was before, and starting the installation again is safe. If \
     it happens a second time the fault is in PlexOS rather than in the download.";

/// The whole pipeline, start to finish.
///
/// # Errors
/// Any step. Every message names what to do about it, because this is the one operation
/// an ordinary user performs and the only place they will read an error from.
pub fn run(job: &Job, apps: &Path, keyring: &Path) -> Result<String, Box<dyn std::error::Error>> {
    // Resolved before anything is fetched. Reporting a missing mkfs.erofs after an 83 MB
    // download is a poor way to discover the image is incomplete.
    let tools = tools::Tools::on_this_system()?;
    let curl = tools::resolve("curl", &|p: &Path| p.exists()).ok_or_else(|| {
        format!(
            "curl is in none of {}, so nothing can be downloaded. This is an image \
             fault: BR2_PACKAGE_LIBCURL_CURL and a CA store are supposed to provide it.",
            tools::PROGRAM_DIRS.join(", ")
        )
    })?;

    std::fs::create_dir_all(apps)?;

    let catalogue = fetch_text(&curl, CATALOGUE_URL, CATALOGUE_TIMEOUT_SECS)?;
    let release = pick(&catalogue)?;
    job.step(
        Phase::Downloading,
        &format!("Plex {} — downloading", release.version),
    );

    let package = apps.join(".package.deb");
    download(job, &curl, &tools, &release, &package)?;

    job.step(Phase::Verifying, "checking Plex's signature");
    let mut file = std::fs::File::open(&package)?;
    let members = ar::directory(&mut file)?;
    let signed = verified_manifest(&mut file, &members, apps, keyring)?;
    job.note(&format!("signature good, signer {}", signed.signer));

    tie_to_payload(&mut file, &members, &tools, apps, &signed)?;
    job.note(&format!(
        "all {} members match the signed manifest",
        members.len()
    ));

    // Only now. Everything above answers "are these the bytes Plex signed"; nothing
    // below can be undone as cheaply.
    let version = version_of(&tools, &mut file, &members, apps)?;
    job.step(
        Phase::Building,
        &format!("building the app image for {}", version.raw),
    );

    let layout = build::Layout {
        apps: apps.to_path_buf(),
    };
    let data = members
        .iter()
        .find(|member| member.name == "data.tar.xz")
        .ok_or("the package has no data.tar.xz member")?;

    let listing: Vec<String> = std::fs::read_dir(apps)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    let current = std::fs::read_link(layout.current())
        .ok()
        .map(|target| target.to_string_lossy().into_owned());
    let superseded = store::Store::from_listing(&listing, current.as_deref()).superseded(&version);

    let steps = build::install_plan(&layout, &version, &package, data, &superseded);
    execute::plan(&steps, &tools, &mut |line| job.note(line))?;

    // The package is three times the size of the image it produced and has served its
    // purpose. Left behind it would sit in /var until someone noticed.
    let _ = std::fs::remove_file(&package);

    Ok(version.raw)
}

/// Fetches a small document and returns it as text.
fn fetch_text(curl: &Path, url: &str, timeout: u64) -> Result<String, Box<dyn std::error::Error>> {
    let output = std::process::Command::new(curl)
        .args(curl_arguments(timeout))
        .arg(url)
        .output()?;

    if !output.status.success() {
        return Err(curl_failure(url, &output).into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The flags every fetch uses.
///
/// `--proto =https` and `--tlsv1.2` are not decoration. Without the first, a redirect to
/// `http://` would be followed silently and the transport would prove nothing; `-L` is
/// needed because `downloads.plex.tv` does redirect.
fn curl_arguments(timeout: u64) -> Vec<String> {
    vec![
        "--fail".to_owned(),
        "--silent".to_owned(),
        "--show-error".to_owned(),
        "--location".to_owned(),
        "--proto".to_owned(),
        "=https".to_owned(),
        "--proto-redir".to_owned(),
        "=https".to_owned(),
        "--tlsv1.2".to_owned(),
        "--max-time".to_owned(),
        timeout.to_string(),
    ]
}

/// Turns a curl failure into something with a remedy in it.
fn curl_failure(url: &str, output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    let remedy = if detail.contains("certificate") || detail.contains("SSL") {
        "The transport could not be verified. The appliance's CA store or its clock is \
         wrong -- a certificate that is not yet valid looks exactly like this."
    } else if detail.contains("Could not resolve") {
        "DNS did not answer. The status page shows whether this machine has an address \
         and a nameserver at all."
    } else {
        "Check that this machine has a working route to the internet; the status page \
         reports its address and link state."
    };
    format!("could not fetch {url}: {detail}. {remedy}")
}

/// Downloads the package, reporting bytes as they arrive.
///
/// curl is spawned rather than run to completion so the file can be watched while it
/// grows. An 83 MB download over a domestic line is minutes, and a page that said only
/// "downloading" for that long is indistinguishable from one that has hung.
fn download(
    job: &Job,
    curl: &Path,
    tools: &tools::Tools,
    release: &Release,
    into: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut child = std::process::Command::new(curl)
        .args(curl_arguments(DOWNLOAD_TIMEOUT_SECS))
        .arg("--output")
        .arg(into)
        .arg(&release.url)
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    loop {
        let Some(status) = child.try_wait()? else {
            // Still running. The file is watched rather than curl's own progress meter
            // parsed: an 83 MB download over a domestic line is minutes, and a page that
            // said only "downloading" for that long is indistinguishable from one that
            // has hung.
            if let Ok(meta) = std::fs::metadata(into) {
                job.step(
                    Phase::Downloading,
                    &format!(
                        "Plex {} — downloaded {} MB",
                        release.version,
                        meta.len() / 1_000_000
                    ),
                );
            }
            std::thread::sleep(POLL_INTERVAL);
            continue;
        };

        if status.success() {
            break;
        }

        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        let _ = std::fs::remove_file(into);
        return Err(curl_failure(
            &release.url,
            &std::process::Output {
                status,
                stdout: Vec::new(),
                stderr: stderr.into_bytes(),
            },
        )
        .into());
    }

    // A transfer check, not a security one -- see the module documentation. It runs
    // because "the download was corrupted" and "this is not Plex's package" need
    // opposite responses, and the signature check alone cannot tell them apart: a
    // truncated file fails the signature too, with a message about tampering that would
    // send someone looking for an attacker instead of a flaky connection.
    if release.sha1.is_empty() {
        job.note("the catalogue gave no checksum; the signature is the only check");
        return Ok(());
    }

    job.note("checking the download against the catalogue's checksum");
    let measured = sha1_of(tools, into)?;
    if measured.eq_ignore_ascii_case(&release.sha1) {
        return Ok(());
    }

    let size = std::fs::metadata(into).map(|m| m.len()).unwrap_or_default();
    let _ = std::fs::remove_file(into);
    Err(format!(
        "the download does not match the checksum Plex published for it: got {measured}, \
         expected {}. The transfer was corrupted or cut short at {size} bytes; the file \
         has been deleted. Starting the installation again is the remedy.",
        release.sha1
    )
    .into())
}

/// Verifies the package's signature and returns the manifest it covers.
fn verified_manifest(
    file: &mut std::fs::File,
    members: &[ar::Member],
    scratch: &Path,
    keyring: &Path,
) -> Result<manifest::Manifest, Box<dyn std::error::Error>> {
    let signature = members
        .iter()
        .find(|member| member.name == plexos_plex::SIGNATURE_MEMBER)
        .ok_or(
            "the package carries no signature member, so it cannot be checked and will \
             not be installed. A package downloaded from anywhere but Plex is the usual \
             reason.",
        )?;

    let path = scratch.join(".signature");
    std::fs::File::create(&path)?.write_all(&member_bytes(file, signature)?)?;
    let body = verify::clearsigned(&path, keyring);
    let _ = std::fs::remove_file(&path);

    Ok(manifest::parse(&body?)?)
}

/// Checks the signed manifest against the bytes actually downloaded.
///
/// The step that makes the signature mean something. Without it the manifest could be a
/// genuine, correctly signed description of a *different* package.
fn tie_to_payload(
    file: &mut std::fs::File,
    members: &[ar::Member],
    tools: &tools::Tools,
    scratch: &Path,
    signed: &manifest::Manifest,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut measured = Vec::new();
    for member in members {
        let path = scratch.join(format!(".measure-{}", member.name));
        std::fs::File::create(&path)?.write_all(&member_bytes(file, member)?)?;
        let sha1 = sha1_of(tools, &path);
        let _ = std::fs::remove_file(&path);
        measured.push(plexos_plex::Measured {
            name: member.name.clone(),
            size: member.size,
            sha1: sha1?,
        });
    }

    let problems = plexos_plex::agrees_with(&measured, signed);
    if problems.is_empty() {
        return Ok(());
    }
    Err(format!(
        "the package does not match the manifest Plex signed, so it will not be \
         installed: {}. The download was altered between Plex and this machine, or it \
         is not the package it claims to be. Retrying is worth one attempt; a second \
         failure is not a network problem.",
        problems
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    )
    .into())
}

/// Reads the upstream version out of `control.tar.xz`.
fn version_of(
    tools: &tools::Tools,
    file: &mut std::fs::File,
    members: &[ar::Member],
    scratch: &Path,
) -> Result<store::Version, Box<dyn std::error::Error>> {
    let control = members
        .iter()
        .find(|member| member.name == "control.tar.xz")
        .ok_or("the package has no control.tar.xz member")?;

    let path = scratch.join(".control.tar.xz");
    std::fs::File::create(&path)?.write_all(&member_bytes(file, control)?)?;
    let output = std::process::Command::new(&tools.tar)
        .arg("-xJO")
        .arg("-f")
        .arg(&path)
        .arg("./control")
        .output();
    let _ = std::fs::remove_file(&path);

    let output = output?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix("Version:"))
        .ok_or("the package's control file has no Version field")?;

    store::Version::parse(line.trim())
        .ok_or_else(|| format!("cannot read {:?} as a version", line.trim()).into())
}

/// Reads one `ar` member out of the package.
fn member_bytes(file: &mut std::fs::File, member: &ar::Member) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(member.offset))?;
    let mut raw = vec![0_u8; usize::try_from(member.size).unwrap_or(usize::MAX)];
    file.read_exact(&mut raw)?;
    Ok(raw)
}

/// The SHA-1 of a file, using the tool the manifest's format implies.
fn sha1_of(tools: &tools::Tools, path: &Path) -> std::io::Result<String> {
    let output = std::process::Command::new(&tools.sha1sum)
        .arg(path)
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A catalogue with the shape the live endpoint returns, trimmed to two releases.
    /// Captured from `https://plex.tv/api/downloads/5.json`, not invented — a fixture
    /// written from memory would agree with whatever this parser believed.
    const CATALOGUE: &str = r#"{
      "computer": {
        "Linux": {
          "version": "1.43.3.10828-00f62d37d",
          "releases": [
            {
              "label": "Ubuntu (16.04+) / Debian (8+) - Intel 32-bit",
              "build": "linux-x86",
              "distro": "debian",
              "url": "https://downloads.plex.tv/plex-media-server-new/1.43.3.10828-00f62d37d/debian/plexmediaserver_1.43.3.10828-00f62d37d_i386.deb",
              "checksum": "637ca94795c0f0a6e832968d08088593399b3785"
            },
            {
              "label": "Ubuntu (16.04+) / Debian (8+) - Intel/AMD 64-bit",
              "build": "linux-x86_64",
              "distro": "debian",
              "url": "https://downloads.plex.tv/plex-media-server-new/1.43.3.10828-00f62d37d/debian/plexmediaserver_1.43.3.10828-00f62d37d_amd64.deb",
              "checksum": "d73eba8f297785e1b611ed6e1628b2432eaa617e"
            },
            {
              "label": "Fedora - Intel/AMD 64-bit",
              "build": "linux-x86_64",
              "distro": "redhat",
              "url": "https://downloads.plex.tv/plex-media-server-new/1.43.3.10828-00f62d37d/redhat/plexmediaserver-1.43.3.10828-00f62d37d.x86_64.rpm",
              "checksum": "610af0f9ef853f3839e7774f647047ef91dcd03d"
            }
          ]
        }
      }
    }"#;

    #[test]
    fn the_amd64_debian_package_is_the_one_chosen() {
        // Both the wrong architecture and the wrong packaging are present in the real
        // catalogue, and taking the first entry would install a 32-bit package.
        let release = pick(CATALOGUE).unwrap();
        assert_eq!(release.version, "1.43.3.10828-00f62d37d");
        assert!(release.url.ends_with("_amd64.deb"), "{}", release.url);
        assert_eq!(release.sha1, "d73eba8f297785e1b611ed6e1628b2432eaa617e");
    }

    #[test]
    fn a_catalogue_with_no_matching_package_says_upstream_changed() {
        // ADR-0010 calls a packaging change release-blocking for new installs. The
        // message has to say that rather than suggest retrying.
        let only_rpm = CATALOGUE.replace("\"distro\": \"debian\"", "\"distro\": \"redhat\"");
        let error = pick(&only_rpm).unwrap_err();
        assert_eq!(error, CatalogueError::NoMatch);
        assert!(error.to_string().contains("by hand"), "{error}");
    }

    #[test]
    fn a_document_that_is_not_the_catalogue_is_refused_rather_than_guessed_at() {
        assert!(matches!(
            pick("not json at all"),
            Err(CatalogueError::Unreadable(_))
        ));
        assert!(matches!(
            pick(r#"{"computer":{}}"#),
            Err(CatalogueError::Unreadable(_))
        ));
    }

    #[test]
    fn a_package_url_off_plexs_hosts_is_refused() {
        // The guard rail: whoever can alter the catalogue in transit must not thereby
        // choose where the package comes from. The signature is still the boundary, but
        // an appliance that fetches arbitrary URLs on request is a tool for someone else.
        let elsewhere = CATALOGUE.replace(
            "https://downloads.plex.tv/plex-media-server-new/1.43.3.10828-00f62d37d/debian/plexmediaserver_1.43.3.10828-00f62d37d_amd64.deb",
            "https://downloads.example.com/evil.deb",
        );
        assert!(matches!(
            pick(&elsewhere),
            Err(CatalogueError::Untrusted(_))
        ));
    }

    #[test]
    fn plain_http_is_refused_even_on_a_plex_host() {
        assert!(!is_trusted("http://downloads.plex.tv/x.deb"));
        assert!(is_trusted("https://downloads.plex.tv/x.deb"));
    }

    #[test]
    fn a_host_that_merely_ends_in_the_allowed_name_is_not_allowed() {
        // The mistake a naive `contains` makes, and the reason the check is on a dotted
        // suffix rather than a substring.
        assert!(!is_trusted("https://downloads.plex.tv.example.com/x.deb"));
        assert!(!is_trusted("https://notplex.tv/x.deb"));
        assert!(is_trusted("https://plex.tv/x.deb"));
    }

    #[test]
    fn a_url_with_userinfo_is_refused_rather_than_parsed() {
        // https://downloads.plex.tv@evil.example.com/ reads as a Plex host and is not
        // one. Refusing the whole shape is safer than extracting the host correctly.
        assert!(!is_trusted(
            "https://downloads.plex.tv@evil.example.com/x.deb"
        ));
    }

    #[test]
    fn a_fresh_job_reports_that_nothing_has_been_asked_for() {
        let job = Job::new();
        let progress = job.snapshot();
        assert_eq!(progress.phase, Phase::Idle);
        assert!(!progress.phase.is_running());
        assert!(progress.version.is_none());
    }

    #[test]
    fn only_one_run_may_hold_the_job() {
        // Two concurrent runs would unpack into the same staging directory and produce
        // an image neither could vouch for.
        let job = Job::new();
        assert!(job.begin(), "the first caller gets it");
        assert!(!job.begin(), "the second does not");
        assert!(job.is_running());
    }

    #[test]
    fn a_finished_run_frees_the_job_for_another() {
        let job = Job::new();
        assert!(job.begin());
        job.finish(Ok("1.2.3".to_owned()));
        assert_eq!(job.snapshot().phase, Phase::Done);
        assert_eq!(job.snapshot().version.as_deref(), Some("1.2.3"));
        assert!(job.begin(), "a second installation may be started");
    }

    #[test]
    fn a_failed_run_keeps_its_error_and_frees_the_job() {
        // Failure must not wedge the appliance: the usual cause is a network that was
        // down and now is not, and the remedy is to press the button again.
        let job = Job::new();
        assert!(job.begin());
        job.finish(Err("the network was unreachable".to_owned()));
        let progress = job.snapshot();
        assert_eq!(progress.phase, Phase::Failed);
        assert_eq!(
            progress.error.as_deref(),
            Some("the network was unreachable")
        );
        assert!(job.begin());
    }

    #[test]
    fn the_log_is_bounded_so_a_retrying_install_cannot_grow_it_forever() {
        let job = Job::new();
        job.begin();
        for i in 0..(MAX_LOG_LINES * 2) {
            job.note(&format!("line {i}"));
        }
        let log = job.snapshot().log;
        assert_eq!(log.len(), MAX_LOG_LINES);
        assert_eq!(
            log.last().map(String::as_str),
            Some(format!("line {}", MAX_LOG_LINES * 2 - 1).as_str()),
            "and it is the newest lines that are kept"
        );
    }

    #[test]
    fn a_run_that_ends_without_an_outcome_does_not_wedge_the_console() {
        // The guard, exercised directly. A panic in the pipeline would otherwise unwind
        // past finish(), and the console would report an installation in progress until
        // someone rebooted the appliance -- refusing to start another the whole time.
        let job = Arc::new(Job::new());
        assert!(job.begin());
        drop(Unfinished::arm(&job));

        let progress = job.snapshot();
        assert_eq!(progress.phase, Phase::Failed);
        assert!(
            progress
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("safe"),
            "and says whether retrying is safe: {progress:?}"
        );
        assert!(job.begin(), "another installation may be started");
    }

    #[test]
    fn a_run_that_finishes_normally_is_not_also_marked_failed() {
        // The other half: disarming has to actually work, or every successful install
        // would be overwritten by the guard's message on the way out.
        let job = Arc::new(Job::new());
        assert!(job.begin());
        let mut guard = Unfinished::arm(&job);
        guard.disarm();
        drop(guard);
        job.finish(Ok("1.2.3".to_owned()));

        assert_eq!(job.snapshot().phase, Phase::Done);
        assert!(job.snapshot().error.is_none());
    }

    #[test]
    fn progress_serialises_to_what_the_page_reads() {
        let job = Job::new();
        job.begin();
        job.step(Phase::Downloading, "downloaded 12 MB");
        let json = serde_json::to_string(&job.snapshot()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["phase"], "downloading");
        assert_eq!(parsed["detail"], "downloaded 12 MB");
    }

    #[test]
    fn every_download_forbids_a_downgrade_to_plain_http() {
        // curl follows redirects, and downloads.plex.tv does redirect. Without
        // --proto-redir a redirect to http:// would be followed silently and the
        // transport would prove nothing, while looking exactly like a success.
        let arguments = curl_arguments(60);
        assert!(arguments.contains(&"--proto-redir".to_owned()));
        assert_eq!(
            arguments.iter().filter(|a| *a == "=https").count(),
            2,
            "both the initial protocol and the redirect protocol are pinned: {arguments:?}"
        );
        assert!(
            arguments.contains(&"--fail".to_owned()),
            "an HTTP error is an error"
        );
    }

    #[test]
    fn a_certificate_failure_is_not_reported_as_a_missing_route() {
        // A wrong remedy is worse than none. A clock that is not yet at the certificate's
        // start date produces this, and "check your network" would send someone the
        // wrong way entirely.
        use std::os::unix::process::ExitStatusExt as _;

        let message = curl_failure(
            "https://plex.tv/x",
            &std::process::Output {
                status: std::process::ExitStatus::from_raw(60 << 8),
                stdout: Vec::new(),
                stderr: b"curl: (60) SSL certificate problem".to_vec(),
            },
        );
        assert!(message.contains("clock"), "{message}");

        let unreachable = curl_failure(
            "https://plex.tv/x",
            &std::process::Output {
                status: std::process::ExitStatus::from_raw(6 << 8),
                stdout: Vec::new(),
                stderr: b"curl: (6) Could not resolve host: plex.tv".to_vec(),
            },
        );
        assert!(unreachable.contains("nameserver"), "{unreachable}");
        assert!(
            !unreachable.contains("clock"),
            "the two failures must not share a remedy: {unreachable}"
        );
    }
}
