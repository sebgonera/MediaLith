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
//! # What is trusted
//!
//! ADR-0006's signed manifest, and nothing else. Until this crate carried [`trust`] the
//! bundle was believed because of where it came from, which is acceptable on a bench and
//! nowhere else — and since ADR-0014 that same LAN also offers a root shell, so the two
//! openings were the same opening.
//!
//! Four independent things now have to be true before a byte is written, and they fail in
//! four different directions on purpose:
//!
//! - **[`trust`]**: a root key certified the signing key, and that key signed these exact
//!   manifest bytes. Refuses a document from anyone else.
//! - **[`sequence`]**: the manifest is not older than one already accepted. Refuses a
//!   *genuine* document being replayed, which every signature check would pass.
//! - **[`mod@plan`]**: it is for this product, in a format this release can mount, and it fits
//!   the slot. Refuses an update that is honest and inapplicable.
//! - **[`uki`]** and [`mod@write`]: the bytes that arrived are the bytes the manifest named,
//!   and the boot entry belongs to the slot being written. Refuses a bundle assembled
//!   wrongly, which no signature can notice because the signature is over the mistake.
//!
//! The transport is not one of them, which is what makes it acceptable for the transport
//! to be plain HTTP from a laptop on the LAN.
//!
//! The layers below all of this — choosing the slot, writing the partitions, verifying what
//! was written, installing the boot entry — did not change when signing arrived. Only where
//! the bytes come from and what vouches for them, exactly as this file predicted.
//!
//! # What has run
//!
//! **This has updated an appliance, twice, and it booted the result both times.** Slot A
//! to B and back again, over the LAN, from a browser's request: written, read back, entry
//! installed on trial, restarted, and the boot then marked good — `+2-1` renamed to no
//! counter at all, which is ADR-0005 completing a cycle for the first time.

#![forbid(unsafe_code)]

pub mod atomic;
pub mod clock;
pub mod location;
pub mod plan;
pub mod sequence;
pub mod trust;
pub mod uki;
pub mod write;

pub use location::{LocationError, Role};
pub use plan::{Decision, Refusal, plan};
pub use sequence::SequenceError;
pub use trust::{Policy, TrustError, Verified};
