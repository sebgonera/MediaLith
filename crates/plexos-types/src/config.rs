//! The user-facing configuration schema (ADR-0008).
//!
//! `/etc/plexos/config.toml` is declarative: `plexosd` reconciles system state to match
//! it and never writes it back, so user comments and formatting survive and there is
//! exactly one source of truth.
//!
//! Two properties here are the opposite of [`crate::manifest`], deliberately:
//!
//! **Unknown keys are rejected.** On an appliance, a typo that is silently ignored
//! produces a system that boots, reports itself healthy, and does not do what the user
//! asked — the worst available failure. `transcod_dir` must be a startup error.
//!
//! **Every value has a working default.** A file containing only `schema_version = 1`
//! must produce a functioning Plex server. Configuration expresses deviation from sane
//! behaviour, not the minimum required to boot.
//!
//! Both make a *newer* config unusable on an older release, which matters because
//! rollback reverts `/usr` but not `/var` (ADR-0005). Migrations therefore keep the
//! pre-migration file, and `plexos-init` restores it when the running release is older
//! than the file it finds.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::paths;
use crate::version::CONFIG_SCHEMA_VERSION;

/// Reads `schema_version` and nothing else.
///
/// Tolerates every other key, including ones this build rejects, so that a config from
/// a later release can be identified as such instead of reported as a syntax error.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct VersionProbe {
    /// Schema version of the configuration file.
    pub schema_version: u32,
}

/// Why a configuration file could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The file declares a schema version this build does not implement.
    UnsupportedVersion {
        /// Version declared by the file.
        found: u32,
        /// Version this build implements.
        supported: u32,
    },
    /// The file is not valid TOML, is missing `schema_version`, or has an unknown key.
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion { found, supported } if *found > *supported => write!(
                f,
                "config schema version {found} was written by a newer PlexOS release \
                 (this one supports {supported})"
            ),
            Self::UnsupportedVersion { found, supported } => write!(
                f,
                "config schema version {found} is obsolete and should have been \
                 migrated to {supported}"
            ),
            Self::Invalid(detail) => write!(f, "invalid configuration: {detail}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// The complete system configuration.
///
/// **Unknown top-level sections are ignored, and the sub-structures below are strict.**
/// That split is deliberate and was arrived at the hard way. With `deny_unknown_fields`
/// here as well, this format could not gain a field at all: a configuration written by a
/// newer release was refused outright by an older one, which on a machine with A/B
/// rollback means the slot you fall back to cannot read its own settings. ADR-0006 chose
/// tolerance for the update manifest and wrote down why; this had chosen the opposite,
/// for the same class of document.
///
/// Strictness stays where a person actually types. A misspelt key inside `[system]` is a
/// setting that silently does nothing, which is the failure this project keeps recording
/// — so that is still an error. A whole section this build has never heard of is a newer
/// release talking, and ignoring it is how a rollback survives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Schema version. Mandatory — there is no default, because guessing which schema
    /// a file was written against is exactly the ambiguity this field removes.
    pub schema_version: u32,
    /// Host identity and locale.
    #[serde(default)]
    pub system: System,
    /// Update policy.
    #[serde(default)]
    pub updates: Updates,
    /// Plex Media Server settings owned by the OS rather than by Plex.
    #[serde(default)]
    pub plex: Plex,
    /// Network file sharing.
    #[serde(default)]
    pub shares: Shares,
    /// How this machine gets its address.
    #[serde(default)]
    pub network: NetworkConfig,
}

impl Config {
    /// Reads `schema_version` from a TOML document without validating the rest.
    ///
    /// # Errors
    /// Fails if the document is not TOML or has no `schema_version`.
    pub fn probe_version(toml_text: &str) -> Result<u32, ConfigError> {
        toml::from_str::<VersionProbe>(toml_text)
            .map(|p| p.schema_version)
            .map_err(|e| ConfigError::Invalid(e.to_string()))
    }

    /// Parses a configuration file, checking `schema_version` before the body.
    ///
    /// # Errors
    /// Returns [`ConfigError::UnsupportedVersion`] for a schema this build does not
    /// implement, or [`ConfigError::Invalid`] for malformed TOML or an unknown key.
    pub fn parse(toml_text: &str) -> Result<Self, ConfigError> {
        let found = Self::probe_version(toml_text)?;
        if found != CONFIG_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedVersion {
                found,
                supported: CONFIG_SCHEMA_VERSION,
            });
        }
        toml::from_str(toml_text).map_err(|e| ConfigError::Invalid(e.to_string()))
    }
}

/// How the appliance obtains its address.
///
/// A media server is a thing other machines connect *to*, so its address wants to stay
/// still — and a DHCP reservation is not always somebody's to make. This is also the one
/// setting on the console that can cut off the console, which is why applying it is
/// wrapped in a confirmation rather than simply done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    /// `dhcp` or `static`.
    #[serde(default = "NetworkConfig::default_mode")]
    pub mode: String,
    /// The address, with its prefix length, e.g. `192.168.2.50/24`.
    ///
    /// Carried as written rather than parsed into an address type, because this crate is
    /// the wire format and a value that will not parse has to survive a round trip to be
    /// reportable. The consumer validates.
    #[serde(default)]
    pub address: String,
    /// The default gateway.
    #[serde(default)]
    pub gateway: String,
    /// Resolvers, in the order they should be tried.
    #[serde(default)]
    pub nameservers: Vec<String>,
}

impl NetworkConfig {
    fn default_mode() -> String {
        "dhcp".into()
    }

    /// Whether this asks for a static address.
    #[must_use]
    pub fn is_static(&self) -> bool {
        self.mode == "static"
    }
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            mode: Self::default_mode(),
            address: String::new(),
            gateway: String::new(),
            nameservers: Vec::new(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            system: System::default(),
            updates: Updates::default(),
            plex: Plex::default(),
            shares: Shares::default(),
            network: NetworkConfig::default(),
        }
    }
}

/// Host identity and locale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct System {
    /// Hostname, also used as the mDNS name.
    #[serde(default = "System::default_hostname")]
    pub hostname: String,
    /// IANA timezone. Plex schedules library maintenance against this.
    #[serde(default = "System::default_timezone")]
    pub timezone: String,
}

impl System {
    fn default_hostname() -> String {
        "plexos".into()
    }
    fn default_timezone() -> String {
        "UTC".into()
    }
}

impl Default for System {
    fn default() -> Self {
        Self {
            hostname: Self::default_hostname(),
            timezone: Self::default_timezone(),
        }
    }
}

/// Update policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Updates {
    /// Channel to track. A manifest from any other channel is ignored.
    #[serde(default = "Updates::default_channel")]
    pub channel: String,
    /// Whether OS updates install without being asked.
    ///
    /// Defaults to on. Rollback (ADR-0005) is what makes that defensible: an appliance
    /// nobody logs into is far more likely to be harmed by unpatched software than by
    /// an update that undoes itself.
    #[serde(default = "Updates::default_automatic")]
    pub automatic: bool,
    /// Local-time window in which a reboot for an update may happen.
    #[serde(default)]
    pub window: MaintenanceWindow,
}

impl Updates {
    fn default_channel() -> String {
        "stable".into()
    }
    const fn default_automatic() -> bool {
        true
    }
}

impl Default for Updates {
    fn default() -> Self {
        Self {
            channel: Self::default_channel(),
            automatic: Self::default_automatic(),
            window: MaintenanceWindow::default(),
        }
    }
}

/// A local-time window, written `HH:MM-HH:MM`.
///
/// Validated at parse time rather than when the updater next runs, because a window
/// that turns out to be unparseable at 03:00 fails where nobody is watching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceWindow {
    /// Inclusive start, minutes since midnight.
    pub start_minute: u16,
    /// Exclusive end, minutes since midnight. May be less than the start, meaning the
    /// window crosses midnight.
    pub end_minute: u16,
}

impl Default for MaintenanceWindow {
    fn default() -> Self {
        Self {
            start_minute: 3 * 60,
            end_minute: 5 * 60,
        }
    }
}

impl fmt::Display for MaintenanceWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (sh, sm) = (self.start_minute / 60, self.start_minute % 60);
        let (eh, em) = (self.end_minute / 60, self.end_minute % 60);
        write!(f, "{sh:02}:{sm:02}-{eh:02}:{em:02}")
    }
}

impl FromStr for MaintenanceWindow {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        fn minutes(part: &str) -> Result<u16, String> {
            let (h, m) = part
                .split_once(':')
                .ok_or_else(|| format!("expected HH:MM, found {part:?}"))?;
            let h: u16 = h.parse().map_err(|_| format!("bad hour in {part:?}"))?;
            let m: u16 = m.parse().map_err(|_| format!("bad minute in {part:?}"))?;
            if h > 23 || m > 59 {
                return Err(format!("{part:?} is not a valid time of day"));
            }
            Ok(h * 60 + m)
        }
        let (start, end) = s
            .split_once('-')
            .ok_or_else(|| format!("expected HH:MM-HH:MM, found {s:?}"))?;
        let (start_minute, end_minute) = (minutes(start)?, minutes(end)?);
        if start_minute == end_minute {
            return Err("maintenance window is empty".into());
        }
        Ok(Self {
            start_minute,
            end_minute,
        })
    }
}

impl Serialize for MaintenanceWindow {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MaintenanceWindow {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

/// Plex settings the OS owns. Everything else belongs to Plex's own configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plex {
    /// Directories exposed to Plex as libraries, mounted read-only.
    #[serde(default)]
    pub media: Vec<String>,
    /// Scratch directory for transcoding.
    #[serde(default = "Plex::default_transcode_dir")]
    pub transcode_dir: String,
    /// Whether to require a verified hardware transcode before reporting healthy.
    ///
    /// On by default: silent fallback to software transcoding is the failure PlexOS
    /// exists to make visible.
    #[serde(default = "Plex::default_require_hw_transcode")]
    pub require_hardware_transcode: bool,
}

impl Plex {
    fn default_transcode_dir() -> String {
        paths::PLEX_TRANSCODE_DIR.into()
    }
    const fn default_require_hw_transcode() -> bool {
        true
    }
}

impl Default for Plex {
    fn default() -> Self {
        Self {
            media: Vec::new(),
            transcode_dir: Self::default_transcode_dir(),
            require_hardware_transcode: Self::default_require_hw_transcode(),
        }
    }
}

/// Network file sharing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shares {
    /// SMB export, served by in-kernel ksmbd.
    #[serde(default)]
    pub smb: ShareService,
    /// NFS export.
    #[serde(default)]
    pub nfs: ShareService,
}

/// A single sharing protocol. Off unless asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareService {
    /// Whether the service runs.
    #[serde(default)]
    pub enabled: bool,
    /// Paths exported. Empty means the configured media directories.
    #[serde(default)]
    pub paths: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_section_from_a_newer_release_is_ignored_rather_than_fatal() {
        // The property that lets an A/B machine roll back and still read its own
        // settings. Without it, a configuration written by a newer image is refused
        // outright by the older one, which is the slot you fall back *to*.
        let newer = "schema_version = 1\n\n[system]\nhostname = \"cinema\"\n\n\
                     [telemetry]\nenabled = true\n";
        let config = Config::parse(newer).expect("an unknown section must not be fatal");
        assert_eq!(config.system.hostname, "cinema");
    }

    #[test]
    fn a_misspelt_key_inside_a_known_section_is_still_an_error() {
        // Strictness stays where a person types. A setting that silently does nothing is
        // the failure this project keeps recording, and inside [system] the reader knows
        // every legal key.
        let typo = "schema_version = 1\n\n[system]\nhostnam = \"cinema\"\n";
        assert!(
            Config::parse(typo).is_err(),
            "a typo in a known section must not be swallowed"
        );
    }

    #[test]
    fn network_defaults_to_dhcp_and_survives_a_round_trip() {
        let config = Config::parse("schema_version = 1\n").expect("a bare file is valid");
        assert!(!config.network.is_static());
        assert_eq!(config.network.mode, "dhcp");

        let text = toml::to_string(&config).expect("serialises");
        assert_eq!(Config::parse(&text).expect("re-parses"), config);
    }

    #[test]
    fn a_minimal_config_yields_a_working_system() {
        let config = Config::parse("schema_version = 1").unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(config.system.hostname, "plexos");
        assert!(config.updates.automatic);
        assert!(config.plex.require_hardware_transcode);
        assert!(!config.shares.smb.enabled);
    }

    #[test]
    fn parses_the_documented_example() {
        let config = Config::parse(
            r#"
            schema_version = 1

            [system]
            hostname = "kino"
            timezone = "Europe/Warsaw"

            [updates]
            channel = "stable"
            automatic = true
            window = "03:00-05:00"

            [plex]
            media = ["/var/media/movies", "/var/media/tv"]

            [shares.smb]
            enabled = true
        "#,
        )
        .unwrap();

        assert_eq!(config.system.hostname, "kino");
        assert_eq!(config.plex.media.len(), 2);
        assert!(config.shares.smb.enabled);
        assert!(!config.shares.nfs.enabled);
        assert_eq!(config.updates.window.start_minute, 180);
    }

    #[test]
    fn rejects_a_misspelled_key_rather_than_ignoring_it() {
        let err = Config::parse("schema_version = 1\n[plex]\ntranscod_dir = \"/tmp\"").unwrap_err();
        assert!(
            matches!(&err, ConfigError::Invalid(d) if d.contains("transcod_dir")),
            "error should name the offending key, got: {err}"
        );
    }

    #[test]
    fn requires_an_explicit_schema_version() {
        assert!(matches!(
            Config::parse("[system]\nhostname = \"x\""),
            Err(ConfigError::Invalid(_))
        ));
    }

    #[test]
    fn identifies_a_config_from_a_newer_release() {
        let text = "schema_version = 7\n[section_that_does_not_exist_yet]\nkey = 1";
        assert_eq!(Config::probe_version(text).unwrap(), 7);
        let err = Config::parse(text).unwrap_err();
        assert!(
            err.to_string().contains("newer PlexOS release"),
            "got: {err}"
        );
    }

    #[test]
    fn maintenance_windows_round_trip_and_validate() {
        let w: MaintenanceWindow = "03:00-05:00".parse().unwrap();
        assert_eq!(w.to_string(), "03:00-05:00");

        let crossing: MaintenanceWindow = "23:30-01:15".parse().unwrap();
        assert!(crossing.end_minute < crossing.start_minute);

        for bad in ["03:00", "25:00-26:00", "03:60-04:00", "3-4", "04:00-04:00"] {
            assert!(
                bad.parse::<MaintenanceWindow>().is_err(),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn an_invalid_window_fails_at_parse_time_not_at_0300() {
        assert!(matches!(
            Config::parse("schema_version = 1\n[updates]\nwindow = \"tonight\""),
            Err(ConfigError::Invalid(_))
        ));
    }

    #[test]
    fn serialised_config_parses_back_unchanged() {
        let config = Config::default();
        let text = toml::to_string(&config).unwrap();
        assert_eq!(Config::parse(&text).unwrap(), config);
    }
}
