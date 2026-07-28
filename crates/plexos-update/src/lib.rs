//! Replacing `/usr` over the network, into the slot that is not running.
//!
//! ADR-0001 makes `/usr` the unit of update and gives the disk two interchangeable slots;
//! ADR-0005 makes a new slot prove itself over three boots before it becomes permanent.
//! This crate is what finally uses either of them. Until now the second slot has been
//! 1 GiB of zeroes and rollback has never been exercised.
//!
//! # What makes this safe enough to build unsigned
//!
//! Not confidence. `systemd-boot`'s own entry ordering, read out of its source rather
//! than assumed:
//!
//! ```c
//! /* Order entries that have no tries left to the end of the list */
//! r = CMP(a->tries_left == 0, b->tries_left == 0);
//! ...
//! r = -strverscmp_improved(a->version, b->version);   /* newest first */
//! ```
//!
//! An entry whose three tries are exhausted sorts last, so the previously-good entry wins
//! and the machine comes back on the slot it was working on. That is the whole safety
//! argument, and it is why an update that turns out to be rubbish costs three reboots
//! rather than a reflash.
//!
//! Two consequences follow, and both are constraints on what is written rather than
//! preferences:
//!
//! - **A new bundle must carry a higher version than the running one**, or the bootloader
//!   will keep choosing the old entry and the update will appear to do nothing.
//! - **The running entry is never touched.** It is the rollback target, not litter.
//!
//! # What is trusted, today and later
//!
//! Today: nothing. The bundle is fetched over plain HTTP from a machine on the same LAN,
//! and whoever can answer that request chooses what `/usr` this appliance runs. That is
//! acceptable on a bench and nowhere else, it is stated in the console, and
//! [`Metadata::TRUSTED`] is `false` so nothing can quietly forget it.
//!
//! Later: ADR-0006's signed manifest. The layers below this one — choosing the slot,
//! writing the partitions, verifying what was written, installing the boot entry — do not
//! change when that arrives. Only where the bytes come from and what vouches for them.
//!
//! # What has run
//!
//! **Nothing here has updated an appliance.** Delete this notice when one has been
//! updated and has booted the result.

#![forbid(unsafe_code)]

pub mod bundle;
pub mod plan;
pub mod write;

pub use bundle::{Artifact, Metadata, MetadataError};
pub use plan::{Decision, Refusal, plan};
