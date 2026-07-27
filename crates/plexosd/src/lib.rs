//! The management daemon, and the boot health gate it exists to run.
//!
//! ARCHITECTURE.md §2 step 7 is the reason this crate exists before any management
//! API does: **`plexosd` declares the boot good, and nothing else may.** Until it
//! does, the try counter in the UKI's filename stands, and three failed boots hand
//! the machine back to the previous slot.
//!
//! ADR-0005 rejects the obvious alternative — letting `plexos-init` declare success
//! once services are spawned — and the rejection is the important part of that ADR.
//! Spawning is not working. An update that breaks Plex while leaving PID 1 perfectly
//! healthy would be marked good and never rolled back, which is precisely the failure
//! the mechanism exists to catch.
//!
//! The three pieces are deliberately separate:
//!
//! - [`health`] decides whether the boot is good. Policy, and the part with teeth.
//! - [`bootcounter`] understands the filename convention. String handling, no policy.
//! - [`esp`] performs the rename. Filesystem work, no policy.

#![forbid(unsafe_code)]

pub mod bootcounter;
pub mod esp;
pub mod health;

pub use bootcounter::BootEntry;
pub use health::{Check, Health, Status};
