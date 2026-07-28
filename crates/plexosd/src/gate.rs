//! The boot health gate: deciding a slot is good, and recording it.
//!
//! ARCHITECTURE.md §2 step 7, and ADR-0005's teeth. Until this clears the try counter in
//! the UKI's filename, three failed boots hand the machine back to the previous slot.
//!
//! # Why this is a module and not just `main`
//!
//! Because it has to run *after* Plex, and Plex is started by the console. The order in
//! ARCHITECTURE.md is step 6 services, step 7 gate, and the implementation had it the
//! other way round: `plexos-init` ran the gate and only then started `plexosd --serve`,
//! which is what starts Plex. While the machine was unprovisioned nobody noticed —
//! `plex-http` answered `NotApplicable`. The moment Plex was installed the check became
//! applicable and failed on every boot, so the counter was never cleared and ADR-0005
//! stopped meaning what it says.
//!
//! So the gate lives here, the console calls it once Plex is answering, and `main` calls
//! it for a bare `plexosd` invocation, which is now a diagnostic rather than the boot
//! path.
//!
//! # It runs late and it must not delay the console
//!
//! Waiting for Plex takes seconds. The console is the only tool for finding out why a
//! machine is unwell, so it must not wait for the machine to be well: the caller starts
//! serving and runs this on a thread.

use std::path::Path;

use crate::health::Health;

/// How long Plex is given to open its listener before the gate gives up on it.
///
/// Plex reads a database and scans its plugins before it binds, which on this hardware
/// is comfortably under a minute from cold. Long enough not to fail a healthy machine,
/// short enough that a genuinely broken one is reported while somebody is still watching.
pub const PLEX_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// What the gate last decided, for anything that wants to report it.
///
/// The verdict is reached once, on a thread, seconds into a boot — and then the only
/// record of it is a line on a console nobody can read remotely. That is the gap this
/// closes: whether the try counter was actually cleared is exactly the kind of "should
/// have" this project keeps discovering was a "did not", and it belongs on the network
/// rather than on a screen.
static LAST: std::sync::OnceLock<std::sync::Mutex<Option<String>>> = std::sync::OnceLock::new();

/// The gate's last verdict, if it has run.
#[must_use]
pub fn last_verdict() -> Option<String> {
    LAST.get()?
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Records a verdict for [`last_verdict`].
fn remember(verdict: &Verdict) {
    let slot = LAST.get_or_init(|| std::sync::Mutex::new(None));
    *slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(verdict.to_string());
}

/// The ESP's GPT label, resolved through sysfs because this system has no `udev`.
const ESP_LABEL: &str = plexos_types::partition::LABEL_ESP;

/// What the gate decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Healthy, and the try counter was dealt with. Carries what happened to it.
    Good(String),
    /// Healthy, but the counter could not be cleared. The slot will still roll back.
    Unrecorded(String),
    /// Not healthy. The counter is deliberately left standing.
    Unhealthy(Vec<String>),
}

impl Verdict {
    /// Whether the boot was healthy, regardless of whether that could be recorded.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        !matches!(self, Self::Unhealthy(_))
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Good(outcome) => write!(f, "healthy; {outcome}"),
            Self::Unrecorded(error) => write!(
                f,
                "healthy, but could not clear the boot counter: {error}. Left unfixed \
                 this rolls the machine back in three reboots, long after the cause."
            ),
            Self::Unhealthy(failures) => write!(
                f,
                "NOT healthy, so the try counter stands and this slot rolls back \
                 (ADR-0005): {}",
                failures.join("; ")
            ),
        }
    }
}

/// Runs the checks and, if they pass, makes the slot permanent.
///
/// `esp_device` overrides the partition-label lookup, which is what `--esp` is for.
pub fn run(esp_device: Option<&str>, log: &mut dyn FnMut(&str)) -> Verdict {
    let health: Health = crate::health::run_all();
    log("boot health");
    for check in &health.checks {
        log(&format!("  {check}"));
    }

    if !health.is_healthy() {
        let verdict = Verdict::Unhealthy(
            health
                .failures()
                .iter()
                .map(|c| format!("{}: {}", c.name, c.detail))
                .collect(),
        );
        remember(&verdict);
        return verdict;
    }

    // Resolved late: an unhealthy boot must not fail for want of an ESP it was never
    // going to write to.
    let device = match esp_device.map(ToOwned::to_owned) {
        Some(explicit) => explicit,
        None => match plexos_sys::device::by_partlabel(ESP_LABEL) {
            Ok(found) => found,
            Err(error) => {
                let verdict = Verdict::Unrecorded(format!("the ESP was not found: {error}"));
                remember(&verdict);
                return verdict;
            }
        },
    };

    let verdict = match clear_counter(&device) {
        Ok(outcome) => Verdict::Good(outcome),
        Err(error) => Verdict::Unrecorded(error),
    };
    remember(&verdict);
    verdict
}

/// Waits for Plex, then runs the gate.
///
/// Used by the console once it has started serving. An unprovisioned machine does not
/// wait at all: `check_plex` reports `NotApplicable` for a Plex that was never installed,
/// so there is nothing to wait for and nothing to fail.
pub fn run_after_plex(
    plex_root: &Path,
    esp_device: Option<&str>,
    log: &mut dyn FnMut(&str),
) -> Verdict {
    if plex_root.exists() && !crate::plex::wait_until_answering(PLEX_TIMEOUT, log) {
        log(&format!(
            "Plex did not answer within {}s. Running the gate anyway so its verdict is \
             the truth about this boot rather than an optimistic guess.",
            PLEX_TIMEOUT.as_secs()
        ));
    }
    run(esp_device, log)
}

/// Clears the try counter by renaming the entry on the ESP.
fn clear_counter(device: &str) -> Result<String, String> {
    let mut outcome = String::new();

    let result = crate::esp::with_esp_mounted(device, &mut |esp_path| {
        let entries = crate::esp::entries(esp_path)?;
        let on_trial: Vec<_> = entries.iter().filter(|(_, e)| e.is_on_trial()).collect();

        match on_trial.as_slice() {
            [] => {
                "no entry on trial; this slot is already permanent".clone_into(&mut outcome);
                Ok(())
            }
            [(path, entry)] => {
                match crate::esp::mark_good(path, entry)? {
                    Some(renamed) => {
                        outcome = format!(
                            "{} -> {}",
                            entry,
                            renamed.file_name().unwrap_or_default().to_string_lossy()
                        );
                    }
                    None => outcome = format!("{entry} was already good"),
                }
                Ok(())
            }
            many => {
                // Renaming the wrong one would mark a slot permanent that was never
                // booted. Leaving the counter alone costs at most a rollback.
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "{} entries are on trial and there is no way to tell which one \
                         booted; leaving the counter alone. Entries: {}",
                        many.len(),
                        many.iter()
                            .map(|(_, e)| e.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ))
            }
        }
    });

    match result {
        Ok(()) => Ok(outcome),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verdict_is_remembered_so_it_can_be_read_over_the_network() {
        // Whether the try counter was cleared is the kind of "should have" this project
        // keeps finding was a "did not", and until now the only record was a line on a
        // console nobody can read remotely.
        remember(&Verdict::Good(
            "plexos-0.1.0+3.efi -> plexos-0.1.0.efi".to_owned(),
        ));
        let seen = last_verdict().expect("a verdict was recorded");
        assert!(seen.contains("plexos-0.1.0.efi"), "{seen}");
        assert!(seen.starts_with("healthy"), "{seen}");
    }

    #[test]
    fn an_unhealthy_verdict_says_the_slot_rolls_back_and_names_what_failed() {
        let verdict = Verdict::Unhealthy(vec!["plex-http: not answering".to_owned()]);
        let message = verdict.to_string();
        assert!(!verdict.is_healthy());
        assert!(message.contains("rolls back"), "{message}");
        assert!(message.contains("plex-http"), "{message}");
        assert!(message.contains("ADR-0005"), "{message}");
    }

    #[test]
    fn a_healthy_boot_whose_counter_could_not_be_cleared_is_still_healthy() {
        // The distinction matters: the boot worked, and what failed is the recording of
        // that fact. Reporting it as unhealthy would send somebody looking at Plex.
        let verdict = Verdict::Unrecorded("the ESP was not found".to_owned());
        assert!(verdict.is_healthy());
        let message = verdict.to_string();
        assert!(
            message.contains("three reboots"),
            "and says the cost: {message}"
        );
    }

    #[test]
    fn the_plex_timeout_is_longer_than_a_cold_start_and_shorter_than_patience() {
        assert!(
            PLEX_TIMEOUT.as_secs() >= 60,
            "Plex reads a database before it binds; failing a healthy machine is worse \
             than waiting"
        );
        assert!(PLEX_TIMEOUT.as_secs() <= 300);
        assert!(
            PLEX_TIMEOUT > crate::plex::PROBE_TIMEOUT,
            "the wait must outlast a single probe"
        );
    }

    #[test]
    fn an_unprovisioned_machine_does_not_wait_for_a_plex_it_has_never_had() {
        // check_plex reports NotApplicable for a Plex that was never installed, so there
        // is nothing to wait for. Waiting anyway would add ninety seconds to every boot
        // of a fresh appliance -- the one a person is most likely to be watching.
        let nowhere = std::env::temp_dir().join("plexos-gate-unprovisioned");
        let _ = std::fs::remove_dir_all(&nowhere);

        let started = std::time::Instant::now();
        let mut lines = Vec::new();
        let _ = run_after_plex(&nowhere, Some("/dev/null"), &mut |line| {
            lines.push(line.to_owned());
        });
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "an unprovisioned machine must not wait: took {:?}",
            started.elapsed()
        );
    }
}
