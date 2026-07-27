//! `plexosd` — runs the boot health gate and clears the boot try counter.
//!
//! ```text
//! plexosd              run the gate, and mark the boot good if it passes
//! plexosd --check      run the gate and report, changing nothing
//! ```
//!
//! Exit status is the gate's verdict: success only when the boot is healthy. That
//! makes it usable as a plain command as well as from `plexos-init`.

use std::path::Path;
use std::process::ExitCode;

use plexos_types::paths;
use plexosd::esp;
use plexosd::health::{self, Health};

const USAGE: &str = "\
plexosd — PlexOS management daemon

USAGE:
    plexosd [--check] [--esp <device>]

OPTIONS:
    --check          Report the health gate without clearing the boot counter
    --esp <device>   ESP to clear the counter on (default: found by partition label)
    --help           Show this message

Exit status is the verdict: 0 only when the boot is healthy.
";

/// Where the ESP is, when nothing says otherwise. Resolved the same way every other
/// partition is: by label, because that is where slot identity lives (ADR-0003).
const ESP_BY_LABEL: &str = "/dev/disk/by-partlabel/esp";

fn run_checks() -> Health {
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();

    Health {
        checks: vec![
            health::check_var_writable(Path::new(paths::VAR)),
            health::check_usr_verified(&mounts),
            // Plex is not in the image yet. check_plex reports NotApplicable rather
            // than passing, so an absent Plex cannot be mistaken for a working one.
            health::check_plex(Path::new(paths::PLEX_APPS), &|| false),
        ],
    }
}

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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut check_only = false;
    let mut esp_device = ESP_BY_LABEL.to_owned();

    let mut tokens = args.iter();
    while let Some(token) = tokens.next() {
        match token.as_str() {
            "--check" => check_only = true,
            "--help" | "-h" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--esp" => {
                let Some(value) = tokens.next() else {
                    eprintln!("plexosd: --esp needs a value");
                    return ExitCode::from(64);
                };
                value.clone_into(&mut esp_device);
            }
            other => {
                eprintln!("plexosd: unrecognised argument {other:?}\n");
                eprint!("{USAGE}");
                return ExitCode::from(64);
            }
        }
    }

    let health = run_checks();
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

    match clear_counter(&esp_device) {
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
