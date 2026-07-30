//! PID 1 for PlexOS: verified `/usr`, assembled root, persistent `/var`.
//!
//! ```
//! use plexos_init::{cmdline::BootArgs, plan, state};
//!
//! let args = BootArgs::parse(
//!     "plexos.slot=a \
//!      plexos.roothash=9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08 \
//!      quiet",
//! )
//! .unwrap();
//! let action = state::decide(Some(1), 1);
//! print!("{}", plan::render(&plan::boot_plan(&args, action)));
//! ```
//!
//! # Why the plan is computed before it is executed
//!
//! Everything here is a pure function of two inputs: the kernel command line, and the
//! state layout version found on `/var`. [`plan::boot_plan`] produces the complete
//! sequence of operations, and only then does anything touch the machine.
//!
//! This is not architectural neatness. PID 1 is the one component where a bug means a
//! machine that does not come back, and debugging it otherwise means reading kernel
//! panics on a device in a cupboard. A pure plan is exhaustively testable without root,
//! a filesystem, or a device, and it yields a `--dry-run` that prints exactly what
//! would happen.
//!
//! # The two refusals that matter
//!
//! **A command line without a verity root hash is fatal** ([`cmdline`]). Mounting `/usr`
//! unverified would defeat the whole trust chain, so the boot fails and the try counter
//! in ADR-0005 hands the next attempt to the other slot.
//!
//! **State newer than this release understands is never fatal** ([`state`]). That means
//! we have rolled back, and refusing to boot would turn the safety mechanism into the
//! thing that bricks the machine.

#![forbid(unsafe_code)]

pub mod cmdline;
pub mod execute;
pub mod plan;
pub mod state;
pub mod supervise;

pub use cmdline::BootArgs;
pub use plan::{BootStep, boot_plan};
pub use state::StateAction;
pub use supervise::{Service, Supervisor};
