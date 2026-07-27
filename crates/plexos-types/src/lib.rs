//! Definitions of every PlexOS format that outlives a single release.
//!
//! This crate exists because of an asymmetry: code can be rewritten in an afternoon,
//! but a partition GUID, an update manifest schema, or a configuration key that has
//! already reached a user's disk cannot be changed at all. Those definitions are
//! collected here, versioned explicitly, and covered by fixture tests that fail if the
//! wire format shifts.
//!
//! The rules that follow from that, and that every module here upholds:
//!
//! 1. **Version fields are parsed before anything else.** [`manifest::VersionProbe`]
//!    and [`config::VersionProbe`] exist so that an unsupported document can be
//!    rejected with a clear message rather than misparsed.
//! 2. **Forward compatibility is deliberate, per format.** The manifest tolerates
//!    unknown fields and unknown source kinds, because servers must be able to add
//!    capabilities without stranding deployed devices. Configuration does the
//!    opposite and rejects unknown keys, because a silently ignored typo produces an
//!    appliance that reports itself healthy while not doing what was asked.
//! 3. **Signatures cover bytes, never structures.** Nothing here re-serialises a
//!    document for verification. See [`manifest`].
//!
//! Design rationale lives in `docs/adr/`; each module points at the ADR it implements.

#![forbid(unsafe_code)]

pub mod config;
pub mod manifest;
pub mod partition;
pub mod paths;
pub mod version;

pub use config::Config;
pub use manifest::Manifest;
pub use partition::Slot;
pub use version::OsVersion;
