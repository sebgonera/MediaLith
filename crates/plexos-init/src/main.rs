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
use plexos_init::supervise::{self, Service};
use plexos_init::{execute, plan, screen, state};
use plexos_types::paths;
use plexos_types::version::STATE_LAYOUT_VERSION;

/// The health gate. Nothing else may declare a boot good (ADR-0005).
const PLEXOSD: &str = "/usr/bin/plexosd";

/// The terminal the log and the shell share.
///
/// The second one, and the first is left to the dashboard (ADR-0019). Before that existed,
/// both of these inherited PID 1's `/dev/console` — the foreground virtual terminal — which
/// was right while everything this machine put on a screen was a line of text.
///
/// A dashboard is not a line of text, and the two cannot share: a daemon's log written over
/// a drawing wins, and the result is a designed screen with sentences through it. So they
/// move one terminal along, where **Alt+F2** finds them and **Alt+F1** comes back. Nothing
/// is hidden by this and nothing is harder to reach; the shell that was on the screen is
/// still there, still with no password, exactly as it has always been.
const LOG_AND_SHELL: &str = "/dev/tty2";

/// What PID 1 keeps running, in the order it starts them.
///
/// The console first, because it is what somebody reaches for when the shell below is not
/// enough — and on the reference laptop, reading a diagnostic off a 2160x1440 panel is
/// exactly the thing this project keeps trying not to have to do.
///
/// The shell is a service rather than the thing PID 1 turns into. Exiting it used to be a
/// kernel panic; it now gives another shell, like a getty.
static SERVICES: &[Service] = &[
    Service {
        name: "the status console",
        program: PLEXOSD,
        args: &["--serve"],
        // PID 1 gets the environment the kernel provides, which is empty, and everything
        // it spawns inherits that. glibc's execvp then falls back to `/bin:/usr/bin`
        // while busybox installs `ip` and `udhcpc` into `/sbin` and `/usr/sbin`, so a
        // program invoked by name fails with a bare ENOENT while the same name typed at
        // the shell works. plexosd resolves those two by absolute path and no longer
        // depends on this; anything added later would walk into it again.
        env: &[("PATH", "/sbin:/usr/sbin:/bin:/usr/bin")],
        // Its log goes here; the dashboard it runs draws on tty1 by opening that terminal
        // itself. One process, two screens, and the one somebody is looking at stays clean.
        tty: Some(LOG_AND_SHELL),
    },
    Service {
        name: "the console shell",
        program: "/bin/sh",
        args: &[],
        env: &[("PATH", "/sbin:/usr/sbin:/bin:/usr/bin"), ("HOME", "/root")],
        tty: Some(LOG_AND_SHELL),
    },
];

const USAGE: &str = "\
plexos-init — MediaLith PID 1

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

/// Reports a fatal error.
///
/// As PID 1 this holds the message on screen before returning, because returning
/// panics the kernel and the panic dump scrolls the explanation away in milliseconds.
/// That is not a theoretical concern: the first hardware boot with a working console
/// showed "Freeing unused kernel image" followed straight by a panic, because the
/// only failure path that held was the one at the very end.
///
/// Run as an ordinary command it returns immediately, so the development loop stays
/// quick. The distinction is made by asking whether this is PID 1 rather than by
/// remembering to call a different function at each of eight call sites.
fn fail(message: &str) -> ExitCode {
    eprintln!("\nplexos-init: BOOT FAILED");
    eprintln!("plexos-init: {message}");

    if execute::is_pid_one() {
        eprintln!(
            "\nplexos-init: holding for {}s so this can be read, then PID 1 exits and \
             the kernel panics.",
            FAILURE_HOLD.as_secs()
        );
        std::thread::sleep(FAILURE_HOLD);
    }
    ExitCode::FAILURE
}

/// How long a fatal boot error is left on screen before PID 1 exits.
///
/// Exiting panics the kernel immediately, and the panic output buries the line that
/// explains what actually went wrong. On a machine with a serial console that does
/// not matter, because the whole log is captured; on a laptop the only record is
/// whatever someone can read or photograph before it scrolls.
///
/// The cost is paid only on a failed boot, and it delays a rollback by this much per
/// attempt — not the rollback itself, which the bootloader's counter drives.
const FAILURE_HOLD: std::time::Duration = std::time::Duration::from_secs(60);

/// The service manager role. Not yet a supervisor: it reports that the boot
/// succeeded and hands over to a shell, which is what docs/DEVELOPMENT.md promises a
/// first image does. `plexosd`, the health gate, and Plex itself come later.
fn supervise_system() -> ExitCode {
    let mut log = execute::StderrLog;
    log.line("root assembled, /usr verified, running as the service manager");

    // debugfs, purely so plexos-gpu can read the GuC/HuC load state. That state lives
    // nowhere else: i915 publishes it under /sys/kernel/debug/dri/0/, and HuC is what
    // gives QuickSync its quality at low bitrates, so "unknown" is the one answer this
    // appliance must not settle for on the question it exists to answer.
    //
    // Here rather than in the boot plan: the plan runs in the initrd, and its /sys is
    // moved into the new root near the end. Mounting a filesystem *underneath* one that
    // is about to be moved works, but it makes the move's behaviour part of what has to
    // be right, and there is nothing to gain from it being early.
    //
    // Failure is logged and ignored. A machine with no debugfs is a machine whose GPU
    // report says "unknown" — worse, but not worth refusing to boot over.
    match plexos_sys::mount::mount(
        "debugfs",
        "/sys/kernel/debug",
        "debugfs",
        "nosuid,nodev,noexec",
    ) {
        Ok(()) => log.line("debugfs mounted; the GPU report can read GuC/HuC state"),
        Err(error) => log.line(&format!(
            "could not mount debugfs: {error}. The GPU report will say its firmware \
             state is unknown, which hides whether HuC is loaded. Everything else is \
             unaffected."
        )),
    }

    // NVIDIA, if there is one. Here rather than in the boot plan because the modules
    // live in /usr and this is the first place /usr is certainly mounted, and because
    // nothing about it should be able to stop a boot.
    //
    // There is no udev and no modprobe, so this is the thing ADR-0015 step 2 said would
    // have to exist: the driver takes its majors with register_chrdev_region and never
    // calls class_create, so devtmpfs makes no nodes and something has to. The uvm major
    // is read from /proc/devices rather than assumed, because the kernel allocates it at
    // load time -- proven on the RTX 5060, where it came back 241 while nvidia was 195.
    plexos_init::nvidia::bring_up(&plexos_gpu::env::System, &mut |line| log.line(line));

    // ARCHITECTURE.md §2 step 6: services before the gate. Mounting Plex has to happen
    // first because step 7's verdict includes `plex-http`, and a gate that runs before
    // the thing it checks can only ever report NotApplicable — which is what it has
    // been doing.
    //
    // Failure here is reported and the boot continues. An appliance with no Plex, or
    // with an app image that failed its integrity check, is a machine someone needs to
    // reach the console of; refusing to boot would take away the console too. The gate
    // below is what decides whether the slot was good, and an unmountable Plex will
    // show up there rather than being papered over here.
    match std::process::Command::new(PLEXOSD)
        .arg("--mount-plex")
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(_) => log.line("the Plex app image was not mounted; see the message above"),
        Err(error) => log.line(&format!(
            "could not run {PLEXOSD} --mount-plex: {error}. Plex will not start, and \
             the health gate will report it."
        )),
    }

    // ARCHITECTURE.md §2 step 7. Run before the shell, and its result is reported
    // The health gate is deliberately NOT run here any more. ARCHITECTURE.md puts
    // services in step 6 and the gate in step 7, and Plex is a service: running the gate
    // before `plexosd --serve` -- which is what starts Plex -- meant `plex-http` could
    // never pass on a provisioned machine, so the try counter was never cleared and the
    // slot never became permanent. `plexosd --serve` now runs the gate itself, on a
    // thread, once Plex is answering. A bare `plexosd` remains available as a diagnostic.

    // health.rs forbids the gate from depending on it: Ethernet arrives over USB,
    // which enumerates seconds after PCI, and a gate that waited for an address would
    // roll back a perfectly good update because a dongle was slow. Spawned rather
    // than run, for the same reason — it waits up to 30 s for a link, and the shell
    // must not wait behind it.
    //
    // Its failure is not this function's business. A machine with no cable still
    // boots, and still has a console on the screen saying so.
    log.line("supervising: the status console, then a shell on this screen");

    // And then let that screen go dark. Last, because everything above may have had
    // something to say on it and the timer is reset by output anyway; and here rather than
    // inside the supervisor because it is one write that persists for the life of the
    // terminal, not something to keep doing. Never fatal — see `screen::arrange`.
    screen::arrange(&mut |line| log.line(line));

    // Never returns. PID 1 exiting is a kernel panic, so the only two acceptable ends for
    // this function are looping forever and `fail`.
    //
    // What changed here is worth stating: this used to `exec` a shell, which made the
    // shell PID 1 and left nothing that could restart anything or reap an orphan. Both
    // gaps were invisible -- a crashed console had the power button for a remedy, and
    // leaked zombies surface weeks later as `fork` failing somewhere unrelated.
    supervise::run(SERVICES, &mut |line| log.line(line))
}

fn main() -> ExitCode {
    let cli: Vec<String> = std::env::args().skip(1).collect();

    // Printed before anything can fail. Without it, an early exit is indistinguishable
    // from the kernel never having executed the binary at all -- both look like
    // "Freeing unused kernel image" followed by a panic.
    if execute::is_pid_one() {
        eprintln!("plexos-init: starting as PID 1");
    }

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

    // The processor, before anything is asked of it that it might not be able to do.
    //
    // This is placed here rather than anywhere later for one reason: every program
    // this image runs after this point is a *Buildroot* binary, compiled by a
    // different toolchain against a CPU baseline chosen in the defconfig. If that
    // baseline is ever above what the machine has, the first such program dies of
    // SIGILL — and the kernel then reports `Attempted to kill init` about a program
    // that was perfectly fine, which is a sentence that sends whoever reads it in
    // entirely the wrong direction. Nothing external has run yet, so this is the last
    // moment at which a diagnostic can still be printed by something that is known to
    // execute.
    //
    // It refuses nothing today: `plexos_sys::cpu::REQUIRED` is empty, because the
    // image genuinely needs nothing above the x86-64 baseline. The summary is printed
    // anyway, so a machine that fails for some *other* reason is still a machine whose
    // processor is named in the log.
    let cpu = plexos_sys::cpu::detect();
    if let Some(refusal) = cpu.refusal() {
        return fail(&refusal);
    }
    if execute::is_pid_one() || dry_run {
        eprintln!("plexos-init: cpu: {}", cpu.summary());
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
    // machine that is not MediaLith. Both are treated as "no state yet", which is the
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
