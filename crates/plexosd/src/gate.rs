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

/// Whether this boot is one the bootloader is still counting.
///
/// The distinction decides what an unhealthy boot should *do*, and getting it backwards
/// is expensive in both directions. An entry on trial has tries left to burn, so
/// restarting spends one and three of them hand the machine to the slot that worked.
/// An entry that is already permanent has no counter at all: restarting cannot roll
/// anything back, it just takes away the console — which on this appliance is the only
/// way to find out what is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trial {
    /// The booted entry still carries a try counter. Restarting spends one.
    ///
    /// `tries_left` is what remains *after* this boot: the bootloader decrements by
    /// renaming before it hands off, so the filename already accounts for the attempt
    /// currently running. Carried rather than recomputed because it is the number that
    /// makes a rollback legible while it is happening — "two left" says how much of the
    /// mechanism has run, and a log that only says "restarting" does not.
    Counting {
        /// Tries remaining after the boot that is running now.
        tries_left: u32,
    },
    /// No entry is on trial. This slot is permanent and nothing will roll back.
    Permanent,
    /// The entry is on trial at zero tries left, and we are running it anyway.
    ///
    /// The bootloader has already given up on this entry, and it booted it regardless —
    /// which means there was nothing else to boot. Restarting cannot reach a different
    /// slot, so it would loop forever on the one machine that most needs its console:
    /// ADR-0005 says two consecutive bad updates leave no known-good slot and need
    /// recovery media, and somebody has to be able to find that out.
    Exhausted,
    /// This entry's last try was spent booting it, and another entry can still boot.
    ///
    /// The case ADR-0005 promised and never delivered. The bootloader chooses an entry
    /// while it still has a try, decrements it, and boots it — so the third boot of a bad
    /// update *runs* an entry that is exhausted by the time anything here looks. Nothing
    /// restarts by itself after that, and `systemd-boot` sorts an exhausted entry below
    /// every other, so the good slot is exactly one restart away and nothing will ask for
    /// it. Watched happen: two restarts, then a machine sitting on a broken slot claiming
    /// its slot was permanent.
    Spent {
        /// The entry that will win the next boot.
        alternative: String,
    },
    /// The ESP could not be read, so which of the others this is cannot be known.
    Unknown(String),
}

/// What the gate decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Healthy, and the try counter was dealt with. Carries what happened to it.
    Good(String),
    /// Healthy, but the counter could not be cleared. The slot will still roll back.
    Unrecorded(String),
    /// Not healthy. The counter is deliberately left standing.
    Unhealthy {
        /// The checks that failed, each as `name: detail`.
        failures: Vec<String>,
        /// Whether the bootloader is still counting this entry's tries.
        trial: Trial,
    },
}

impl Verdict {
    /// Whether the boot was healthy, regardless of whether that could be recorded.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        !matches!(self, Self::Unhealthy { .. })
    }

    /// Whether the caller should restart the machine to spend a try.
    ///
    /// This is the half of ADR-0005 that never existed. The counter was left standing on
    /// an unhealthy boot and the verdict said the slot "rolls back" — but nothing
    /// restarted, so nothing consumed the counter, and a machine that booted into a
    /// broken Plex ran happily forever underneath a message claiming it was rolling back.
    /// Two failure shapes wore one sentence: an image that cannot boot at all, which the
    /// kernel's `panic=` now recycles, and this one, which reaches userspace and needs a
    /// decision made here.
    ///
    /// Only [`Trial::Counting`] restarts. [`Trial::Permanent`] must not: there is no
    /// counter to spend, so it would be an unbounded reboot loop on a machine whose
    /// console is the only thing that could diagnose it. [`Trial::Unknown`] must not
    /// either, and that default is deliberate — a machine that stays up with a broken
    /// Plex can be looked at, and one in a reboot loop cannot.
    #[must_use]
    pub fn demands_restart(&self) -> bool {
        matches!(
            self,
            Self::Unhealthy {
                trial: Trial::Counting { .. } | Trial::Spent { .. },
                ..
            }
        )
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
            Self::Unhealthy { failures, trial } => {
                let failures = failures.join("; ");
                match trial {
                    Trial::Counting { tries_left } => write!(
                        f,
                        "NOT healthy, and this entry is on trial with {tries_left} \
                         {} left, so the machine restarts to spend one (ADR-0005). \
                         When they run out the previous slot takes over: {failures}",
                        if *tries_left == 1 { "try" } else { "tries" }
                    ),
                    Trial::Permanent => write!(
                        f,
                        "NOT healthy, but this slot is already permanent, so there is no \
                         try counter to spend and nothing rolls back. Restarting would \
                         loop forever and take away the console, so the machine stays up \
                         and this needs fixing by hand: {failures}"
                    ),
                    Trial::Spent { alternative } => write!(
                        f,
                        "NOT healthy, and this entry's last try was spent booting it, so \
                         the machine restarts once more and {alternative} takes over -- \
                         the bootloader sorts an entry with no tries left below every \
                         other. That restart is the rollback: {failures}"
                    ),
                    Trial::Exhausted => write!(
                        f,
                        "NOT healthy, and this entry has no tries left but was booted \
                         anyway -- so there is no other slot to fall back to. Both slots \
                         are bad and the appliance needs recovery media (ADR-0005). \
                         Staying up so the console can be reached: {failures}"
                    ),
                    Trial::Unknown(error) => write!(
                        f,
                        "NOT healthy, and the ESP could not be read ({error}), so whether \
                         this boot is on trial is unknown. Staying up rather than \
                         restarting, because a machine in a reboot loop cannot be \
                         diagnosed and this one can: {failures}"
                    ),
                }
            }
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

    // Resolved before the branch, not after. It used to be resolved late, on the
    // reasoning that "an unhealthy boot must not fail for want of an ESP it was never
    // going to write to" -- which was true while an unhealthy boot did nothing. It now
    // has a decision to make that only the ESP can answer, so it needs the device too.
    // Failing to find it is no longer fatal to either path: it degrades the healthy one
    // to `Unrecorded` and the unhealthy one to `Trial::Unknown`.
    let device = match esp_device.map(ToOwned::to_owned) {
        Some(explicit) => Ok(explicit),
        // On the disk the running system is on, not merely the first with that label.
        // The counter this clears is what makes a slot permanent, and clearing it on
        // another disk's ESP leaves the running entry on trial -- so a machine that is
        // working perfectly rolls back three boots later, for no reason it can report.
        None => match crate::install::running_disk(&plexos_gpu::env::System) {
            Some(disk) => plexos_sys::device::by_partlabel_on(&disk, ESP_LABEL)
                .map_err(|error| format!("the ESP was not found on {disk}: {error}")),
            None => Err(
                "this machine's own disk could not be identified, so the ESP cannot be \
                 found. The boot counter is left alone, which means this slot rolls back \
                 rather than becoming permanent."
                    .to_owned(),
            ),
        },
    };

    if !health.is_healthy() {
        let verdict = Verdict::Unhealthy {
            failures: health
                .failures()
                .iter()
                .map(|c| format!("{}: {}", c.name, c.detail))
                .collect(),
            trial: match &device {
                Ok(device) => trial_state(device, &crate::update::running_version()),
                Err(error) => Trial::Unknown(error.clone()),
            },
        };
        remember(&verdict);
        return verdict;
    }

    let device = match device {
        Ok(found) => found,
        Err(error) => {
            let verdict = Verdict::Unrecorded(error);
            remember(&verdict);
            return verdict;
        }
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

/// Asks the ESP what the bootloader thinks of the entry that booted.
///
/// # It asks about *this* entry, by name
///
/// It used to infer which entry had booted from the shape of the set: one entry on trial
/// meant that one had booted, none on trial plus a permanent one meant the permanent one
/// had. The second half of that is wrong in the case it matters most. The bootloader picks
/// an entry while it still has a try, decrements the counter, and boots it — so on the
/// third boot of a bad update the running entry is *exhausted*, it is filtered out of the
/// counting set, a good permanent entry exists, and the inference concluded that the good
/// one was running. The machine then reported "this slot is already permanent" about a
/// slot on trial, and stayed there.
///
/// That was watched happening on the reference laptop: two restarts, then a broken slot
/// held for good with `plexos-<bad>+0-3.efi` and `plexos-<good>.efi` side by side on the
/// ESP.
///
/// The running version names the entry exactly, so nothing has to be inferred. `os-release`
/// is inside the `/usr` this boot mounted and dm-verity covers it, which makes it a
/// stronger answer to "what booted" than anything on a FAT partition anybody can write.
fn trial_state(device: &str, running: &str) -> Trial {
    let mut found = Trial::Unknown("the ESP was not read".to_owned());

    let result = crate::esp::with_esp_mounted(device, &mut |esp_path| {
        let entries = crate::esp::entries(esp_path)?;
        found = decide_trial(
            &entries.iter().map(|(_, e)| e.clone()).collect::<Vec<_>>(),
            running,
        );
        Ok(())
    });

    match result {
        Ok(()) => found,
        Err(error) => Trial::Unknown(error.to_string()),
    }
}

/// What the entries on the ESP say about the boot that is running `running`.
///
/// Separated from the mount so it can be tested. The decision it makes ends in a reboot or
/// in a machine left broken, and until this was pulled out the only way to exercise it was
/// to install a bad update on real hardware — which is how the defect above was found, and
/// is not a way to check the fix.
#[must_use]
pub fn decide_trial(entries: &[crate::bootcounter::BootEntry], running: &str) -> Trial {
    let running_stem = format!("plexos-{running}");

    let Some(booted) = entries.iter().find(|e| e.stem == running_stem) else {
        // Not a state anything here produces, and not one to guess about: without the
        // entry there is no counter to reason from, and staying up keeps the console.
        return Trial::Unknown(format!(
            "no boot entry on the ESP names {running}, which is the version running"
        ));
    };

    if !booted.is_on_trial() {
        return Trial::Permanent;
    }

    if !booted.is_exhausted() {
        // `is_on_trial` is exactly `tries_left.is_some()`, so this cannot be None here.
        // Defaulted rather than unwrapped anyway: the cost of being wrong is a restart on
        // a machine that had no counter to spend.
        return Trial::Counting {
            tries_left: booted.tries_left.unwrap_or(0),
        };
    }

    // Exhausted, and running. One restart reaches anything that still has a try or never
    // had one, because systemd-boot orders entries with no tries left last -- read out of
    // its source rather than assumed, in plexos_update's documentation.
    entries
        .iter()
        .find(|e| e.stem != running_stem && !e.is_exhausted())
        .map_or(
            // Nothing else can boot. ADR-0005's "two bad updates leave no known-good
            // slot": restarting would loop on the machine that most needs its console.
            Trial::Exhausted,
            |alternative| Trial::Spent {
                alternative: alternative.to_string(),
            },
        )
}

/// Clears the try counter by renaming the entry on the ESP.
fn clear_counter(device: &str) -> Result<String, String> {
    let mut outcome = String::new();

    let result = crate::esp::with_esp_mounted(device, &mut |esp_path| {
        let entries = crate::esp::entries(esp_path)?;

        // Exhausted entries are excluded before anything else looks at this set, and that
        // exclusion is what a real rollback taught. `is_on_trial` is true for `+0-3` --
        // the counter is still in the name -- so the wreckage of a failed update read as
        // "an entry on trial" forever after. `mark_good` rightly refused to resurrect it,
        // which turned every subsequent healthy boot into `Unrecorded`, under a message
        // ending "this rolls the machine back in three reboots". Nothing was going to roll
        // back: the running entry was permanent and the bootloader had already given up on
        // the other one. The remedy the message named was for a problem that did not
        // exist, which is worse than saying nothing.
        let wreckage: Vec<_> = entries.iter().filter(|(_, e)| e.is_exhausted()).collect();
        let on_trial: Vec<_> = entries
            .iter()
            .filter(|(_, e)| e.is_on_trial() && !e.is_exhausted())
            .collect();

        // Reported, not acted on. Removing it is the update path's job -- see
        // `esp::install_entry`, which knows which entry booted and so can tell wreckage
        // from the last thing standing.
        let note = if wreckage.is_empty() {
            String::new()
        } else {
            format!(
                ". A previous update failed and its entry is still here, skipped by the \
                 bootloader and cleared away by the next update: {}",
                wreckage
                    .iter()
                    .map(|(_, e)| e.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };

        match on_trial.as_slice() {
            [] => {
                outcome = format!("no entry on trial; this slot is already permanent{note}");
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
                outcome.push_str(&note);
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

    fn unhealthy(trial: Trial) -> Verdict {
        Verdict::Unhealthy {
            failures: vec!["plex-http: not answering".to_owned()],
            trial,
        }
    }

    #[test]
    fn an_unhealthy_boot_on_trial_restarts_to_spend_a_try() {
        // The defect this replaces: the verdict said the slot "rolls back" and nothing
        // restarted, so the counter it left standing was never spent by anybody. A
        // machine that booted into a broken Plex ran forever underneath a sentence
        // claiming it was rolling back.
        let verdict = unhealthy(Trial::Counting { tries_left: 2 });
        assert!(!verdict.is_healthy());
        assert!(verdict.demands_restart());

        let message = verdict.to_string();
        assert!(message.contains("restarts"), "{message}");
        assert!(message.contains("plex-http"), "{message}");
        assert!(message.contains("ADR-0005"), "{message}");
        assert!(
            message.contains("2 tries left"),
            "and says how much of the mechanism has run: {message}"
        );
    }

    #[test]
    fn the_last_try_is_reported_in_the_singular() {
        // Small, and the reason it is worth a test is that this line is read exactly
        // once per project -- while somebody watches a rollback happen and wants to know
        // whether the next restart is the one that changes slots.
        let message = unhealthy(Trial::Counting { tries_left: 1 }).to_string();
        assert!(message.contains("1 try left"), "{message}");
    }

    #[test]
    fn an_unhealthy_boot_on_a_permanent_slot_stays_up() {
        // There is no counter to spend, so restarting cannot roll anything back -- it is
        // an unbounded loop that takes away the console, which on this appliance is the
        // only way to find out what is wrong. Broken Plex on a slot that was already
        // good is a repair, not a rollback.
        let verdict = unhealthy(Trial::Permanent);
        assert!(!verdict.is_healthy());
        assert!(!verdict.demands_restart());

        let message = verdict.to_string();
        assert!(message.contains("nothing rolls back"), "{message}");
        assert!(
            message.contains("loop forever"),
            "and says why it is not restarting: {message}"
        );
    }

    #[test]
    fn an_exhausted_entry_that_booted_anyway_stays_up() {
        // Exhausted means the bootloader gave up on this entry, and it booted it
        // regardless -- so there was nothing else to boot, and another try lands in the
        // same place. ADR-0005 calls this the two-bad-updates case, which needs recovery
        // media and therefore needs somebody to be able to read the machine.
        let verdict = unhealthy(Trial::Exhausted);
        assert!(!verdict.demands_restart());

        let message = verdict.to_string();
        assert!(message.contains("recovery media"), "{message}");
    }

    #[test]
    fn an_unknown_trial_state_does_not_restart() {
        // Asymmetric on purpose. Guessing "on trial" restarts a machine that had no
        // reason to; guessing "permanent" leaves one up that a person can still reach.
        // Only one of those two mistakes is recoverable over the network.
        let verdict = unhealthy(Trial::Unknown("the ESP was not found".to_owned()));
        assert!(!verdict.demands_restart());

        let message = verdict.to_string();
        assert!(message.contains("unknown"), "{message}");
        assert!(message.contains("the ESP was not found"), "{message}");
    }

    #[test]
    fn an_exhausted_entry_is_not_an_entry_on_trial() {
        // The distinction a real rollback produced, and the reason both `clear_counter`
        // and `trial_state` filter on it. `is_on_trial` is true for `+0-3` -- the counter
        // is still in the name -- so the wreckage of a failed update looked exactly like
        // a boot awaiting judgement. That made every healthy boot afterwards report
        // "could not clear the boot counter ... this rolls the machine back in three
        // reboots", about a machine whose running entry was permanent and which was never
        // going to roll back anywhere.
        let wreckage = crate::bootcounter::BootEntry::parse("plexos-0.1.0.1+0-3.efi")
            .expect("the bootloader writes this name");
        assert!(
            wreckage.is_on_trial(),
            "the counter is still in the name, which is exactly the trap"
        );
        assert!(
            wreckage.is_exhausted(),
            "and this is the property that has to be consulted as well"
        );

        let live = crate::bootcounter::BootEntry::parse("plexos-0.1.0.2+2-1.efi").unwrap();
        assert!(live.is_on_trial() && !live.is_exhausted());
    }

    #[test]
    fn a_healthy_verdict_never_restarts() {
        // The restart is reached from one place, and this is the guard that it cannot
        // widen by accident into the path that runs on every successful boot.
        assert!(
            !Verdict::Good("plexos-0.1.0+3.efi -> plexos-0.1.0.efi".to_owned()).demands_restart()
        );
        assert!(!Verdict::Unrecorded("the ESP was not found".to_owned()).demands_restart());
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

    /// A boot entry as it appears on the ESP, from its filename.
    fn entry(name: &str) -> crate::bootcounter::BootEntry {
        crate::bootcounter::BootEntry::parse(name).expect("a well-formed entry name")
    }

    #[test]
    fn the_last_try_being_spent_is_a_rollback_and_not_a_permanent_slot() {
        // Watched on the reference laptop, and the reason this function exists. A bad
        // update was installed, the gate restarted twice, and the third boot -- running an
        // entry the bootloader had just spent the last try on -- reported "this slot is
        // already permanent" and stayed there. The good system was one restart away and
        // nothing was going to ask for it.
        //
        // The old inference: no entry is *counting* (the exhausted one is filtered out), a
        // permanent entry exists, therefore the permanent one is what booted. It is not:
        // the bootloader chose the bad entry while it still had a try, decremented it, and
        // booted it.
        let entries = [
            entry("plexos-0.1.0.202607301330.efi"),
            entry("plexos-0.1.0.202607301341+0-3.efi"),
        ];

        let trial = decide_trial(&entries, "0.1.0.202607301341");
        assert!(
            matches!(&trial, Trial::Spent { alternative } if alternative.contains("202607301330")),
            "expected a rollback to the other entry, got {trial:?}"
        );

        let verdict = Verdict::Unhealthy {
            failures: vec!["plex-http: installed but not answering".to_owned()],
            trial,
        };
        assert!(
            verdict.demands_restart(),
            "the third restart is the rollback; without it ADR-0005 stops one boot short"
        );
        assert!(verdict.to_string().contains("takes over"), "{verdict}");
    }

    #[test]
    fn a_boot_that_still_has_tries_counts_them_down() {
        let entries = [
            entry("plexos-0.1.0.202607301330.efi"),
            entry("plexos-0.1.0.202607301341+2-1.efi"),
        ];
        assert_eq!(
            decide_trial(&entries, "0.1.0.202607301341"),
            Trial::Counting { tries_left: 2 }
        );
    }

    #[test]
    fn a_permanent_entry_is_recognised_as_the_one_running() {
        // The healthy steady state, and the case the old inference got right.
        let entries = [
            entry("plexos-0.1.0.202607301330.efi"),
            entry("plexos-0.1.0.202607301341+0-3.efi"),
        ];
        assert_eq!(
            decide_trial(&entries, "0.1.0.202607301330"),
            Trial::Permanent
        );
    }

    #[test]
    fn two_bad_updates_leave_nothing_to_fall_back_to_and_must_not_loop() {
        // ADR-0005's genuine dead end. Both entries are spent, so a restart cannot reach
        // anywhere new and would take away the console on the one machine that needs it.
        let entries = [
            entry("plexos-0.1.0.1+0-3.efi"),
            entry("plexos-0.1.0.2+0-3.efi"),
        ];
        let trial = decide_trial(&entries, "0.1.0.2");
        assert_eq!(trial, Trial::Exhausted);
        assert!(
            !Verdict::Unhealthy {
                failures: vec!["x".to_owned()],
                trial,
            }
            .demands_restart()
        );
    }

    #[test]
    fn an_esp_that_does_not_name_the_running_version_is_not_guessed_at() {
        // Nothing here produces this, which is exactly why it must not be interpreted.
        // Every other branch decides whether to reboot the machine.
        let entries = [entry("plexos-0.1.0.202607301330.efi")];
        let trial = decide_trial(&entries, "0.1.0.999");
        assert!(
            matches!(&trial, Trial::Unknown(why) if why.contains("0.1.0.999")),
            "{trial:?}"
        );
        assert!(
            !Verdict::Unhealthy {
                failures: vec!["x".to_owned()],
                trial,
            }
            .demands_restart(),
            "an unknown state must stay up so it can be looked at"
        );
    }

    #[test]
    fn the_whole_three_boot_sequence_ends_back_on_the_working_slot() {
        // The promise ADR-0005 makes, walked end to end: a bad update costs three reboots
        // and lands on the system that was working. Each line is what the ESP holds when
        // that boot's gate runs.
        let good = "plexos-0.1.0.202607301330.efi";
        let bad = "0.1.0.202607301341";

        let restarts = [
            "plexos-0.1.0.202607301341+2-1.efi",
            "plexos-0.1.0.202607301341+1-2.efi",
            "plexos-0.1.0.202607301341+0-3.efi",
        ]
        .into_iter()
        .filter(|name| {
            Verdict::Unhealthy {
                failures: vec!["plex-http".to_owned()],
                trial: decide_trial(&[entry(good), entry(name)], bad),
            }
            .demands_restart()
        })
        .count();

        assert_eq!(
            restarts, 3,
            "every boot of a bad slot must end in a restart, including the last -- it is \
             the last one that reaches the working system"
        );
    }
}
