//! `plexosd` — runs the boot health gate and clears the boot try counter.
//!
//! ```text
//! plexosd              run the gate, and mark the boot good if it passes
//! plexosd --check      run the gate and report, changing nothing
//! ```
//!
//! Exit status is the gate's verdict: success only when the boot is healthy. That
//! makes it usable as a plain command as well as from `plexos-init`.

use std::process::ExitCode;

use plexosd::console;
use plexosd::esp;
use plexosd::health::Health;

const USAGE: &str = "\
plexosd — PlexOS management daemon

USAGE:
    plexosd [--check] [--esp <device>]
    plexosd --serve [--port <n>]
    plexosd --mount-plex
    plexosd --plex-child

OPTIONS:
    --check          Report the health gate without clearing the boot counter
    --esp <device>   ESP to clear the counter on (default: found by partition label)
    --serve          Bring the network up and serve the status console, staying in
                     the foreground. Does not run the gate: by the time this starts,
                     the gate has already returned its verdict.
    --port <n>       Port for --serve (default: 80)
    --mount-plex     Verify the provisioned Plex app image against its integrity
                     record and mount it (ADR-0007). Runs before the health gate,
                     because ARCHITECTURE.md step 6 starts services and step 7 is
                     where the gate reports on them. Exits 0 when there is nothing
                     to mount: an unprovisioned machine is not a broken one.
    --plex-child     Confine this process and become Plex: join the cgroup, apply
                     Landlock, drop to the plex account, exec. Started by --serve
                     rather than by hand; it exists as a flag because the
                     confinement must apply to Plex and to nothing else, and doing
                     it in a pre_exec closure would need unsafe (ADR-0011).
    --help           Show this message

Exit status is the verdict: 0 only when the boot is healthy.
";

/// The ESP's GPT label, from the frozen layout. Resolved through sysfs rather than
/// through `/dev/disk/by-partlabel/`, because that directory is made by `udev` and
/// this system has none — the same absence `plexos-init` deals with at boot.
const ESP_LABEL: &str = plexos_types::partition::LABEL_ESP;

fn clear_counter(device: &str) -> Result<String, String> {
    let mut outcome = String::new();

    let result = esp::with_esp_mounted(device, &mut |esp_path| {
        let entries = esp::entries(esp_path)?;
        let on_trial: Vec<_> = entries.iter().filter(|(_, e)| e.is_on_trial()).collect();

        match on_trial.as_slice() {
            [] => {
                "no entry on trial; this slot is already permanent".clone_into(&mut outcome);
                Ok(())
            }
            [(path, entry)] => {
                match esp::mark_good(path, entry)? {
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

/// Verifies and mounts the provisioned Plex app image.
///
/// Before the gate, and exiting rather than falling through to it: mounting is one job
/// and reporting health is another. ARCHITECTURE.md puts them in that order for a
/// reason — the gate's `plex-http` check is meaningless until Plex is mounted and
/// running.
fn mount_plex_image() -> ExitCode {
    let outcome = plexosd::appmount::mount_current(
        std::path::Path::new(plexos_types::paths::PLEX_APPS),
        std::path::Path::new(plexos_types::paths::PLEX_MOUNT),
        &mut |line| println!("plexosd: {line}"),
    );
    println!("plexosd: {outcome}");

    // An unprovisioned machine exits 0. That is the normal state of a fresh install,
    // and a non-zero exit would have plexos-init report a fault on a system with
    // nothing wrong with it.
    match outcome {
        plexosd::appmount::Outcome::Refused(_) => ExitCode::FAILURE,
        _ => ExitCode::SUCCESS,
    }
}

/// What this invocation is for.
///
/// An enum rather than a set of flags because these are modes, not options: each one
/// takes a different path through the daemon and none of them combines with another.
/// `--serve --check` was never meaningful, and a struct of booleans made it look as
/// though it might be.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// Run the health gate and, unless `check_only`, clear the boot try counter.
    Gate {
        /// Report the verdict and leave the counter alone.
        check_only: bool,
        /// The ESP to write to, if not the one found by partition label.
        esp_device: Option<String>,
    },
    /// Bring the network up and serve the console.
    Serve {
        /// Port to bind.
        port: u16,
    },
    /// Verify and mount the provisioned app image.
    MountPlex,
    /// Confine this process and become Plex.
    PlexChild,
}

/// Parses the arguments into a mode, or returns the status to exit with.
///
/// Separated from `main` so the dispatch below reads as the list of things this daemon
/// does rather than as a list interrupted by argument handling.
fn parse(args: &[String]) -> Result<Mode, ExitCode> {
    let mut check_only = false;
    let mut mount_plex = false;
    let mut plex_child = false;
    let mut serve = false;
    let mut port = console::DEFAULT_PORT;
    let mut esp_device = None;

    let mut tokens = args.iter();
    while let Some(token) = tokens.next() {
        match token.as_str() {
            "--check" => check_only = true,
            plexosd::plex::CHILD_FLAG => plex_child = true,
            "--mount-plex" => mount_plex = true,
            "--serve" => serve = true,
            "--port" => {
                let Some(value) = tokens.next() else {
                    eprintln!("plexosd: --port needs a value");
                    return Err(ExitCode::from(64));
                };
                match value.parse() {
                    Ok(parsed) => port = parsed,
                    Err(error) => {
                        eprintln!("plexosd: --port {value:?} is not a port number: {error}");
                        return Err(ExitCode::from(64));
                    }
                }
            }
            "--esp" => {
                let Some(value) = tokens.next() else {
                    eprintln!("plexosd: --esp needs a value");
                    return Err(ExitCode::from(64));
                };
                esp_device = Some(value.clone());
            }
            "--help" | "-h" => {
                print!("{USAGE}");
                return Err(ExitCode::SUCCESS);
            }
            other => {
                eprintln!("plexosd: unrecognised argument {other:?}\n");
                eprint!("{USAGE}");
                return Err(ExitCode::from(64));
            }
        }
    }

    // Ordered by how far each one is from the ordinary boot path, so that a stray
    // --plex-child can never be swallowed by a mode that also happens to be requested.
    if plex_child {
        return Ok(Mode::PlexChild);
    }
    if mount_plex {
        return Ok(Mode::MountPlex);
    }
    if serve {
        return Ok(Mode::Serve { port });
    }
    Ok(Mode::Gate {
        check_only,
        esp_device,
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = match parse(&args) {
        Ok(mode) => mode,
        Err(code) => return code,
    };

    let (check_only, esp_device) = match mode {
        Mode::PlexChild => {
            // Returning rather than falling through: on success this process is replaced
            // by Plex and nothing below ever runs.
            let mut log = |line: &str| println!("plexosd: plex: {line}");
            let error = plexosd::plex::become_plex(&mut log)
                .expect_err("confine_and_exec returns only on failure");
            eprintln!("plexosd: could not start Plex: {error}");
            return ExitCode::FAILURE;
        }
        Mode::MountPlex => return mount_plex_image(),
        // Deliberately before the gate runs, and mutually exclusive with it. --serve is
        // the daemon that comes after the boot decision; running the gate here as well
        // would write the health probe to /var a second time and print a verdict nobody
        // asked this invocation for.
        Mode::Serve { port } => {
            let mut log = |line: &str| println!("plexosd: {line}");
            return match console::run(port, &mut log) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("plexosd: {error}");
                    ExitCode::FAILURE
                }
            };
        }
        Mode::Gate {
            check_only,
            esp_device,
        } => (check_only, esp_device),
    };

    let health: Health = plexosd::health::run_all();
    println!("plexosd: boot health");
    for check in &health.checks {
        println!("  {check}");
    }

    if !health.is_healthy() {
        eprintln!(
            "plexosd: boot is NOT healthy; leaving the try counter alone so this slot \
             rolls back (ADR-0005)"
        );
        for failure in health.failures() {
            eprintln!("  {}: {}", failure.name, failure.detail);
        }
        return ExitCode::FAILURE;
    }

    if check_only {
        println!("plexosd: healthy (--check, counter untouched)");
        return ExitCode::SUCCESS;
    }

    // Resolved late: --check never needs it, and an unhealthy boot must not fail for
    // want of an ESP it was never going to write to.
    let device = match esp_device {
        Some(explicit) => explicit,
        None => match plexos_sys::device::by_partlabel(ESP_LABEL) {
            Ok(found) => found,
            Err(error) => {
                eprintln!(
                    "plexosd: healthy, but the ESP could not be found: {error}\n  \
                     the boot counter cannot be cleared, so this slot will roll back"
                );
                return ExitCode::FAILURE;
            }
        },
    };

    match clear_counter(&device) {
        Ok(outcome) => {
            println!("plexosd: healthy; {outcome}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            // The boot is good but could not be recorded as such. Not fatal to this
            // boot, and deliberately loud: left unfixed it rolls the machine back to
            // the previous slot in three reboots, long after the cause.
            eprintln!("plexosd: healthy, but could not clear the boot counter: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_of(args: &[&str]) -> Result<Mode, ExitCode> {
        let owned: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        parse(&owned)
    }

    #[test]
    fn no_arguments_runs_the_boot_gate() {
        // The invocation plexos-init makes, and the only one that can mark a slot good.
        assert_eq!(
            parse_of(&[]).unwrap(),
            Mode::Gate {
                check_only: false,
                esp_device: None
            }
        );
    }

    #[test]
    fn the_child_flag_wins_over_every_other_mode() {
        // It ends in execve and never returns, so it must never be reachable by
        // accident -- and equally must never be swallowed by a mode requested alongside
        // it. Ordering it first is what makes both true.
        assert_eq!(
            parse_of(&["--serve", plexosd::plex::CHILD_FLAG]).unwrap(),
            Mode::PlexChild
        );
        assert_eq!(
            parse_of(&[plexosd::plex::CHILD_FLAG, "--check"]).unwrap(),
            Mode::PlexChild
        );
    }

    #[test]
    fn serving_takes_a_port_and_defaults_to_eighty() {
        assert_eq!(
            parse_of(&["--serve"]).unwrap(),
            Mode::Serve {
                port: console::DEFAULT_PORT
            }
        );
        assert_eq!(
            parse_of(&["--serve", "--port", "8080"]).unwrap(),
            Mode::Serve { port: 8080 }
        );
    }

    #[test]
    fn an_unusable_port_is_refused_rather_than_defaulted() {
        // Falling back to 80 would bind a port the caller did not ask for, and the
        // mistake would surface as a console reachable at an address nobody expected.
        assert!(parse_of(&["--serve", "--port", "not-a-number"]).is_err());
        assert!(parse_of(&["--serve", "--port"]).is_err());
    }

    #[test]
    fn an_unrecognised_argument_is_an_error_and_not_ignored() {
        assert!(parse_of(&["--sevre"]).is_err());
    }
}
