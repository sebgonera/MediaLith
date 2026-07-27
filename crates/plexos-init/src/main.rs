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

/// Started by [`supervise_system`] until there is a real supervisor.
const DEBUG_SHELL: &str = "/bin/sh";

/// The health gate. Nothing else may declare a boot good (ADR-0005).
const PLEXOSD: &str = "/usr/bin/plexosd";

const USAGE: &str = "\
plexos-init — PlexOS PID 1

USAGE:
    plexos-init [--dry-run] [--cmdline <string>] [--state-version <n>] [--force]

OPTIONS:
    --dry-run            Print the boot plan without performing it
    --cmdline <string>   Use this instead of reading /proc/cmdline
    --state-version <n>  Assume this /var layout version instead of reading it
    --supervise          Run as the service manager, assuming the root is already
                         assembled. switch_root passes this; it is not for humans.
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

/// The service manager role. Not yet a supervisor: it reports that the boot
/// succeeded and hands over to a shell, which is what docs/DEVELOPMENT.md promises a
/// first image does. `plexosd`, the health gate, and Plex itself come later.
fn supervise_system() -> ExitCode {
    let mut log = execute::StderrLog;
    log.line("root assembled, /usr verified, running as the service manager");

    // ARCHITECTURE.md §2 step 7. Run before the shell, and its result is reported
    // rather than acted on: a failed gate must leave the try counter standing so the
    // slot rolls back, which is precisely what plexosd does by not clearing it.
    //
    // Deliberately not fatal here. Dropping to a shell on an unhealthy boot is what
    // makes the failure diagnosable; killing PID 1 instead would panic the kernel and
    // throw away the console output explaining why.
    match std::process::Command::new(PLEXOSD).status() {
        Ok(status) if status.success() => log.line("health gate passed; boot marked good"),
        Ok(_) => log.line(
            "health gate FAILED — the boot counter stands, and this slot will roll \
             back after three attempts (ADR-0005)",
        ),
        Err(error) => log.line(&format!(
            "could not run {PLEXOSD}: {error}. The boot cannot be marked good, so this \
             slot will roll back."
        )),
    }

    log.line("no supervisor yet: starting a shell (ARCHITECTURE.md section 2, step 6)");

    match plexos_sys::process::exec(DEBUG_SHELL, &[]) {
        Ok(never) => match never {},
        Err(error) => fail(&format!(
            "could not start {DEBUG_SHELL}: {error}. The system booted and /usr is \
             mounted; only the shell is missing."
        )),
    }
}

fn main() -> ExitCode {
    let cli: Vec<String> = std::env::args().skip(1).collect();

    let mut dry_run = false;
    let mut force = false;
    let mut supervise = false;
    let mut cmdline: Option<String> = None;
    let mut state_version: Option<Option<u32>> = None;

    let mut tokens = cli.iter();
    while let Some(token) = tokens.next() {
        match token.as_str() {
            "--dry-run" => dry_run = true,
            "--force" => force = true,
            "--supervise" => supervise = true,
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

    // /proc has to exist before the command line can be read, and mounting it is a
    // step in the plan the command line produces. The first real boot panicked here
    // with "could not read /proc/cmdline: No such file or directory". So bootstrap it
    // first -- but only when actually booting: --dry-run must stay safe to run as an
    // ordinary user, and must not try to mount anything.
    if cmdline.is_none()
        && !dry_run
        && let Err(error) = execute::bootstrap_proc()
    {
        return fail(&format!(
            "could not mount /proc, so the kernel command line cannot be read: {error}"
        ));
    }

    let cmdline = match cmdline {
        Some(value) => value,
        None => match std::fs::read_to_string("/proc/cmdline") {
            Ok(value) => value,
            Err(error) => {
                return fail(&format!(
                    "could not read /proc/cmdline: {error}; /proc is mounted but the \
                 kernel did not provide a command line"
                ));
            }
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

    // The second of the two roles in ARCHITECTURE.md §3. The root is already
    // assembled and /usr is already verified; re-running the boot plan here would
    // try to create a device-mapper target that exists, which is how the first
    // booting image ended.
    if supervise {
        return supervise_system();
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
