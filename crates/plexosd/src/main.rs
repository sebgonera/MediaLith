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

OPTIONS:
    --check          Report the health gate without clearing the boot counter
    --esp <device>   ESP to clear the counter on (default: found by partition label)
    --serve          Bring the network up and serve the status console, staying in
                     the foreground. Does not run the gate: by the time this starts,
                     the gate has already returned its verdict.
    --port <n>       Port for --serve (default: 80)
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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut check_only = false;
    let mut serve = false;
    let mut port = console::DEFAULT_PORT;
    let mut esp_device: Option<String> = None;

    let mut tokens = args.iter();
    while let Some(token) = tokens.next() {
        match token.as_str() {
            "--check" => check_only = true,
            "--serve" => serve = true,
            "--port" => {
                let Some(value) = tokens.next() else {
                    eprintln!("plexosd: --port needs a value");
                    return ExitCode::from(64);
                };
                match value.parse() {
                    Ok(parsed) => port = parsed,
                    Err(error) => {
                        eprintln!("plexosd: --port {value:?} is not a port number: {error}");
                        return ExitCode::from(64);
                    }
                }
            }
            "--help" | "-h" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--esp" => {
                let Some(value) = tokens.next() else {
                    eprintln!("plexosd: --esp needs a value");
                    return ExitCode::from(64);
                };
                esp_device = Some(value.clone());
            }
            other => {
                eprintln!("plexosd: unrecognised argument {other:?}\n");
                eprint!("{USAGE}");
                return ExitCode::from(64);
            }
        }
    }

    // Deliberately before the gate runs, and mutually exclusive with it. --serve is
    // the daemon that comes after the boot decision; running the gate here as well
    // would write the health probe to /var a second time and print a verdict nobody
    // asked this invocation for.
    if serve {
        let mut log = |line: &str| println!("plexosd: {line}");
        return match console::run(port, &mut log) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("plexosd: {error}");
                ExitCode::FAILURE
            }
        };
    }

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
