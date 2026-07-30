//! The audited unsafe surface of PlexOS.
//!
//! Every other crate in the workspace sets `unsafe_code = "forbid"`. This one exists
//! so that they can: the work `plexos-init` does as PID 1 — creating a device-mapper
//! target, mounting filesystems, replacing the root — is syscalls, and syscalls are
//! unsafe. Confining them to one small crate makes the unsafe reviewable, which
//! forbidding it everywhere and then making an exception in the middle of the boot
//! sequence would not.
//!
//! # Why this is `plexos-sys` and not `plexos-dm`
//!
//! The decision that created this crate was about dm-verity specifically. It widened
//! on contact with the problem: `mount(2)`, `chroot(2)` and `execve(2)` are exactly
//! as unsafe as the device-mapper ioctls, and splitting them across two unsafe crates
//! would have doubled the audit surface to no benefit. One crate, one boundary.
//!
//! # Dependencies
//!
//! `libc`, and nothing else. It contributes declarations rather than code — the
//! symbols it names are in the libc that `std` already links against, so on a
//! `+crt-static` build it adds no dependency that was not there anyway.
//!
//! `rustix` and `nix` were both considered, and either would have cut the unsafe here
//! to just the ioctls by providing safe `mount`/`chroot` wrappers. They were rejected
//! because this code *is* the initrd: a single static binary that must keep working
//! with no loader, no modules and nothing to go stale (ARCHITECTURE.md §3). A
//! dependency tree on the boot path is precisely what that design avoids, and the
//! syscalls in question are thin enough that wrapping them ourselves costs less than
//! carrying a general-purpose crate through every future audit.
//!
//! # Rules for this crate
//!
//! - Every `unsafe` block carries a comment saying why it is sound. `clippy::
//!   undocumented_unsafe_blocks` is denied, so this is enforced rather than hoped for.
//! - One unsafe operation per block, so the justification and the operation cannot
//!   drift apart.
//! - Anything that can be written in safe Rust is written in safe Rust and tested as
//!   such — see [`verity`], which is pure byte parsing, and [`device`], which reads
//!   sysfs. Neither needs `unsafe`; both are here because they are how this system
//!   talks to the kernel, and because more than one binary needs them.
//!
//! That last point is why [`device`] lives here rather than in `plexos-init`, where
//! it started. `plexosd` has to find the ESP by label to clear the boot counter, and
//! it hits exactly the same absence of `udev` for exactly the same reason. Two copies
//! of that logic would be two things to get wrong.

pub mod device;
pub mod dm;
pub mod hostname;
pub mod landlock;
pub mod mount;
pub mod power;
pub mod privilege;
pub mod process;
pub mod pty;
pub mod verity;

pub use device::{Partition, by_partlabel, wait_for_partlabel};
pub use verity::{VerityError, VeritySuperblock};

/// Serialises the tests that create child processes.
///
/// [`process::reap`] calls `waitpid(-1)`, which is process-wide: it collects *any* child,
/// including one another test is waiting for. Rust runs tests as threads in one process,
/// so without this the reaping tests and the PTY tests steal each other's children and
/// fail in whichever order the scheduler happens to pick.
///
/// The hazard is not confined to tests, and this is the cheapest place to write it down:
/// **only PID 1 may call `reap`.** A library that reaps indiscriminately inside a process
/// that also uses `Command::status()` will make that call hang or report the wrong child.
#[cfg(test)]
pub(crate) static CHILD_PROCESS_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
