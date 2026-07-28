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
//!
//! # The management API, so far
//!
//! The rest of this crate is the beginning of the one the module documentation above
//! has been promising: [`net`] brings the network up, [`http`] serves it, [`status`]
//! assembles what there is to say, and [`console`] is the page a person reads.
//!
//! All of it runs **after** the gate has returned its verdict, and none of it can
//! reach [`Health::is_healthy`](health::Health::is_healthy). That ordering is the
//! whole design: `health`'s own documentation forbids any check from depending on the
//! network, because Ethernet arrives over USB and a gate that waited for an address
//! would roll back good updates. Putting the network in the same binary is safe only
//! because it cannot run before the decision it must not influence.
//!
//! # It now does, as well as shows
//!
//! [`provision`] installs Plex, which is the first route that changes the machine, and
//! [`auth`] is why it may: ADR-0013's device token is generated on first start, stored
//! as a fingerprint, and demanded by [`http`]'s gate before any non-`GET` request
//! reaches a handler. Reads still need nothing at all -- a console that asked for a
//! credential before it would say why a boot failed would defeat the reason it exists.

#![forbid(unsafe_code)]

pub mod appmount;
pub mod auth;
pub mod bootcounter;
pub mod console;
pub mod esp;
pub mod gate;
pub mod health;
pub mod http;
pub mod net;
pub mod plex;
pub mod power;
pub mod provision;
pub mod status;
pub mod update;

pub use bootcounter::BootEntry;
pub use health::{Check, Health, Status};
