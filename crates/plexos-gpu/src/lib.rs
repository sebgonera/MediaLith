//! GPU detection, VA-API driver selection, and hardware transcode diagnosis.
//!
//! This crate exists to eliminate one specific failure, the one every Plex user has
//! met: transcoding silently falls back to software, the CPU pins at 100%, playback
//! stutters, and nothing anywhere says why. On PlexOS a GPU that cannot transcode is a
//! **reported failure with a named remedy**, not a silent degradation.
//!
//! ```no_run
//! use plexos_gpu::{env::System, report::Report};
//!
//! let report = Report::generate(&System);
//! println!("{report}");
//! ```
//!
//! # Design
//!
//! Two decisions shape everything here.
//!
//! **Selection is probe-driven, not table-driven.** Mapping PCI IDs to generations to
//! drivers is the usual approach and is why hardware newer than a distribution's
//! release so often falls back to software: the table does not know the device, so it
//! guesses wrong and says nothing. Instead this crate picks a likely driver, then
//! verifies it by running the same code path Plex will. See [`gpu`].
//!
//! **Everything is behind [`env::Environment`].** All conclusions derive from four
//! operations — list a directory, read a file, read a symlink, run a command — so the
//! entire decision path is testable against captures from real machines with no GPU
//! present. This crate diagnoses hardware nobody testing it has in front of them; the
//! logic has to be trustworthy on machines it has never run on.
//!
//! # Scope
//!
//! Intel (QuickSync) and AMD via VA-API. NVENC is out of scope for v1: the proprietary
//! driver has redistribution restrictions and an out-of-tree build. NVIDIA devices are
//! still discovered and reported, so a user is told their card is unsupported rather
//! than left wondering why nothing happens.

#![forbid(unsafe_code)]

pub mod env;
pub mod firmware;
pub mod gpu;
pub mod nvidia;
pub mod report;
pub mod vainfo;

pub use gpu::{Gpu, VaapiDriver, Vendor};
pub use report::{Finding, Health, Report, Severity};
