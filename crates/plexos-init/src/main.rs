//! `plexos-init` — PID 1.
//!
//! Currently implements the planning half only: it reads the kernel command line and
//! the state layout version, computes the boot plan, and prints it. Executing the plan
//! is the next step, and separating the two means the hard part is already testable.
//!
//! Runs safely as an ordinary user on any Linux system, which is how the plan gets
//! reviewed before it is ever trusted to boot a machine:
//!
//! ```text
//! plexos-init --dry-run --cmdline "plexos.slot=a plexos.roothash=<64 hex chars>"
//! ```

use std::process::ExitCode;

use plexos_init::cmdline::BootArgs;
use plexos_init::{plan, state};
use plexos_types::paths;
use plexos_types::version::STATE_LAYOUT_VERSION;

const USAGE: &str = "\
plexos-init — PlexOS PID 1

USAGE:
    plexos-init [--dry-run] [--cmdline <string>] [--state-version <n>]

OPTIONS:
    --dry-run            Print the boot plan without performing it
    --cmdline <string>   Use this instead of reading /proc/cmdline
    --state-version <n>  Assume this /var layout version instead of reading it
    --help               Show this message

Executing the plan is not implemented yet, so --dry-run is currently required.
";

fn fail(message: &str) -> ExitCode {
    eprintln!("plexos-init: {message}");
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let cli: Vec<String> = std::env::args().skip(1).collect();

    let mut dry_run = false;
    let mut cmdline: Option<String> = None;
    let mut state_version: Option<Option<u32>> = None;

    let mut tokens = cli.iter();
    while let Some(token) = tokens.next() {
        match token.as_str() {
            "--dry-run" => dry_run = true,
            "--help" | "-h" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--cmdline" => match tokens.next() {
                Some(value) => cmdline = Some(value.clone()),
                None => return fail("--cmdline needs a value"),
            },
            "--state-version" => match tokens.next().map(|v| v.parse::<u32>()) {
                Some(Ok(value)) => state_version = Some(Some(value)),
                Some(Err(_)) => return fail("--state-version needs an integer"),
                None => return fail("--state-version needs a value"),
            },
            other => {
                eprintln!("plexos-init: unrecognised argument {other:?}\n");
                eprint!("{USAGE}");
                return ExitCode::from(64); // EX_USAGE
            }
        }
    }

    if !dry_run {
        eprint!("{USAGE}");
        return fail("executing the boot plan is not implemented yet; pass --dry-run");
    }

    let cmdline = match cmdline {
        Some(value) => value,
        None => match std::fs::read_to_string("/proc/cmdline") {
            Ok(value) => value,
            Err(error) => return fail(&format!("could not read /proc/cmdline: {error}")),
        },
    };

    let boot = match BootArgs::parse(&cmdline) {
        Ok(parsed) => parsed,
        Err(error) => return fail(&error.to_string()),
    };

    // Absent on a fresh /var, and unreadable when running as an ordinary user on a
    // machine that is not PlexOS. Both are treated as "no state yet", which is the
    // right answer in each case.
    let found = state_version.unwrap_or_else(|| {
        std::fs::read_to_string(paths::STATE_VERSION_FILE)
            .ok()
            .and_then(|text| state::parse_state_version(&text))
    });
    let action = state::decide(found, STATE_LAYOUT_VERSION);

    println!("slot          {}", boot.slot);
    println!("root hash     {}", boot.root_hash);
    println!("state         {action}");
    if boot.debug_shell {
        println!("debug shell   requested");
    }
    println!();
    print!("{}", plan::render(&plan::boot_plan(&boot, action)));

    ExitCode::SUCCESS
}
