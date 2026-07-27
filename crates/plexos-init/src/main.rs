//! `plexos-init` — PID 1.
//!
//! Reads the kernel command line and the state layout version, computes the boot plan,
//! and — when it is actually PID 1 — performs it.
//!
//! `--dry-run` runs safely as an ordinary user on any Linux system, which is how the
//! plan gets reviewed before it is ever trusted to boot a machine:
//!
//! ```text
//! plexos-init --dry-run --cmdline "plexos.slot=a plexos.roothash=<64 hex chars>"
//! ```
//!
//! Executing requires being PID 1, and that is a guard rather than a formality. Every
//! step of the plan is destructive to the mount namespace it runs in: it moves `/dev`,
//! `/proc` and `/sys` elsewhere and then replaces the root. Running it by accident on
//! the machine it is being developed on would take that machine down. `--force` exists
//! for a deliberate test under a container or a private namespace, and says so.

use std::process::ExitCode;

use plexos_init::cmdline::BootArgs;
use plexos_init::execute::Log as _;
use plexos_init::{execute, plan, state};
use plexos_types::paths;
use plexos_types::version::STATE_LAYOUT_VERSION;

const USAGE: &str = "\
plexos-init — PlexOS PID 1

USAGE:
    plexos-init [--dry-run] [--cmdline <string>] [--state-version <n>] [--force]

OPTIONS:
    --dry-run            Print the boot plan without performing it
    --cmdline <string>   Use this instead of reading /proc/cmdline
    --state-version <n>  Assume this /var layout version instead of reading it
    --force              Execute even when not PID 1. Destroys the mount namespace
                         it runs in; only meaningful inside a container or a
                         private namespace.
    --help               Show this message

With no options, executes the plan. Refuses unless it is PID 1.
";

fn fail(message: &str) -> ExitCode {
    eprintln!("plexos-init: {message}");
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let cli: Vec<String> = std::env::args().skip(1).collect();

    let mut dry_run = false;
    let mut force = false;
    let mut cmdline: Option<String> = None;
    let mut state_version: Option<Option<u32>> = None;

    let mut tokens = cli.iter();
    while let Some(token) = tokens.next() {
        match token.as_str() {
            "--dry-run" => dry_run = true,
            "--force" => force = true,
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

    let steps = plan::boot_plan(&boot, action);

    if dry_run {
        println!("slot          {}", boot.slot);
        println!("root hash     {}", boot.root_hash);
        println!("state         {action}");
        if boot.debug_shell {
            println!("debug shell   requested");
        }
        println!();
        print!("{}", plan::render(&steps));
        return ExitCode::SUCCESS;
    }

    if !execute::is_pid_one() && !force {
        return fail(
            "refusing to execute: this is not PID 1, and the plan moves /dev, /proc \
             and /sys and then replaces the root. Pass --dry-run to see what it would \
             do, or --force inside a container or private mount namespace.",
        );
    }

    // From here the console is the only thing anyone will see if this goes wrong, so
    // announce the decisions before acting on them.
    let mut log = execute::StderrLog;
    log.line(&format!("slot {}, state: {action}", boot.slot));

    // execute() returns Result<Infallible, _>: a successful plan ends in switch_root,
    // which replaces this process image, so the Ok arm is uninhabited.
    match execute::execute(&steps, &mut log) {
        Ok(never) => match never {},
        Err(error) => fail(&error.to_string()),
    }
}
