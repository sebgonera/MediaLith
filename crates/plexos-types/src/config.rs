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
//! asked — the worst available failure. `hostnam` must be a startup error.
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
                "config schema version {found} was written by a newer MediaLith release \
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
    /// Where releases are looked for, and whether the appliance looks by itself.
    ///
    /// A section of its own rather than three more keys in `[updates]`, and the reason is
    /// rollback rather than taste. [`Updates`] is `deny_unknown_fields`, so a key added
    /// there makes the whole file unreadable to every release already in the field — and
    /// the release you fall back to is by definition an older one. A whole section it has
    /// never heard of is ignored, which is the property the struct above was given for
    /// exactly this case and had never used.
    #[serde(default)]
    pub update_service: UpdateService,
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
            update_service: UpdateService::default(),
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
    /// The name a machine takes when nobody has chosen one.
    ///
    /// Changed with the MediaLith rename, and it reaches **new installations only**: a
    /// machine whose `/etc/plexos/config.toml` already names it keeps that name, because
    /// this is a serde default and a file that has the field does not consult it. Nothing
    /// rewrites a hostname somebody chose.
    ///
    /// Safe to change where the identifiers above are not, because a hostname is not a
    /// contract: no update, boot or state path resolves anything by it, and the TLS
    /// certificate carries the machine's *address* rather than its name — recorded in
    /// ADR-0014 as the reason a DNS name is no use for a certificate here.
    fn default_hostname() -> String {
        "medialith".into()
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
    /// Channel to track. A manifest from any other channel is refused.
    ///
    /// A string rather than [`crate::manifest::Channel`], and it stays one: a word this
    /// build cannot name must not make the file unreadable, or a config written by a
    /// release that knows a fourth channel would strand the release you roll back to.
    /// [`Updates::tracked`] is where the word becomes a decision, and it is allowed to
    /// answer "I cannot name that".
    #[serde(default = "Updates::default_channel")]
    pub channel: String,
    /// Whether OS updates install without being asked.
    ///
    /// Defaults to on. Rollback (ADR-0005) is what makes that defensible: an appliance
    /// nobody logs into is far more likely to be harmed by unpatched software than by
    /// an update that undoes itself.
    ///
    /// **Nothing implements this yet, and this release does not install anything without
    /// being asked.** It is left exactly as written rather than quietly redefined: the
    /// meaning above is the one it will have, and ADR-0020 defers it to the phase after
    /// discovery has been shown to work. [`UpdateService::check`] is the switch that is
    /// live, and it governs looking rather than installing.
    #[serde(default = "Updates::default_automatic")]
    pub automatic: bool,
    /// Local-time window in which a reboot for an update may happen.
    #[serde(default)]
    pub window: MaintenanceWindow,
}

impl Updates {
    fn default_channel() -> String {
        crate::manifest::Channel::Stable.as_str().into()
    }
    const fn default_automatic() -> bool {
        true
    }

    /// The channel this appliance tracks, or `None` if the word is not one this build has.
    ///
    /// The refusal is the useful half. An appliance configured to a channel it cannot name
    /// must say so and check nothing, because every alternative is worse: guessing stable
    /// would take releases the owner did not ask for, and treating it as unknown would
    /// compare equal to no manifest ever published and look like a machine that is simply
    /// never updated again.
    #[must_use]
    pub fn tracked(&self) -> Option<crate::manifest::Channel> {
        crate::manifest::Channel::from_config(self.channel.trim())
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

/// Where this appliance looks for MediaLith releases.
///
/// One base address and one switch. What it points at is a static tree — a channel file
/// naming the current release, and release directories holding the signed manifests and
/// the artefacts — so an update service is a web server with files on it and nothing else
/// (ADR-0020). The trust is in the signature over the manifest, never in the address, which
/// is what makes it safe for this to be a field somebody can type.
///
/// Empty means the appliance does not look. That is the shipped state and it is honest:
/// there is no MediaLith update service yet, and an image that pointed at one would either
/// name a host that does not exist or bake one developer's build host into every appliance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateService {
    /// Base address of the update service, or empty for none.
    ///
    /// Carried as written rather than parsed, for the reason [`NetworkConfig::address`]
    /// gives: this crate is the wire format, and a value that will not parse has to survive
    /// a round trip to be reportable.
    #[serde(default)]
    pub url: String,
    /// Whether the appliance checks for a release by itself, about once a day.
    ///
    /// Checking, never installing. Installing without being asked is a later decision and
    /// deliberately not this one; see [`Updates::automatic`], which is the field that will
    /// mean it and which nothing implements yet.
    #[serde(default = "UpdateService::default_check")]
    pub check: bool,
}

impl UpdateService {
    const fn default_check() -> bool {
        true
    }

    /// Whether this appliance has somewhere to look.
    ///
    /// Trimmed, because a field somebody pasted into and then cleared leaves a space, and
    /// "a source consisting of one space" is not a state worth having.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.url.trim().is_empty()
    }
}

impl Default for UpdateService {
    fn default() -> Self {
        Self {
            url: String::new(),
            check: Self::default_check(),
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

// The `[plex]` section was removed on 2026-08-14. It had three fields, no reader
// anywhere outside this file's own tests, and each had been superseded by something
// that does the job better:
//
//   * `media` — a bare list of paths. `plexosd::shares` and `plexosd::disks` own this
//     now, and both track what a list of strings cannot: whether the thing is actually
//     mounted, and an identity that survives the kernel renumbering the drives.
//   * `transcode_dir` — `/var/cache/plex-transcode` is not a preference. It is granted
//     in Plex's Landlock policy and exported as `TMPDIR` by `plexos_plex::run`, so a
//     configurable path is only correct if the grant follows it. A setting whose value
//     the sandbox does not know about is the deny-by-default trap this project has
//     already paid for four times.
//   * `require_hardware_transcode` — the capability is real and lives elsewhere.
//     `/api/gpu` reports the verdict unconditionally and ADR-0018's activity card flags
//     a software transcode while it is happening. The only thing the field would have
//     added is failing the *boot* gate, and that gate decides rollback: a machine with
//     no working GPU would have handed back every good update for ever.
//
// Removing the whole section rather than the three fields is the only shape that is
// compatible in both directions. `Config` is not `deny_unknown_fields`, so a file that
// still carries `[plex]` is ignored by this release; `Plex` was, so deleting a key from
// inside it would have made such a file unreadable. See ADR-0008, amended.

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

    /// The configuration shape of a release that shipped before update discovery existed.
    ///
    /// Written out rather than referred to, because the thing being tested is that a file
    /// this build writes stays readable by a parser that is *not* this one. A test that
    /// asked the current structs would be comparing this crate to itself, which is the
    /// failure mode already recorded about the partition GUIDs.
    #[derive(Debug, Deserialize)]
    struct ConfigAsShippedBeforeDiscovery {
        #[allow(dead_code)]
        schema_version: u32,
        #[serde(default)]
        updates: UpdatesAsShippedBeforeDiscovery,
    }

    #[derive(Debug, Default, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct UpdatesAsShippedBeforeDiscovery {
        #[serde(default)]
        channel: String,
        #[serde(default)]
        automatic: bool,
        #[serde(default)]
        window: Option<String>,
    }

    #[test]
    fn the_release_you_roll_back_to_can_still_read_what_this_one_writes() {
        // The whole reason [update_service] is a section and not two more keys in
        // [updates]. Rollback reverts /usr and never /var, so the file stays and the parser
        // goes backwards -- and the release it goes back to is one that has never heard of
        // any of this.
        let mut config = Config::default();
        config.update_service.url = "https://updates.example/medialith".to_owned();
        config.update_service.check = false;
        config.updates.channel = "dev".to_owned();
        let written = toml::to_string(&config).expect("serialises");

        let old: ConfigAsShippedBeforeDiscovery =
            toml::from_str(&written).expect("an older release must still read this file");
        assert_eq!(old.updates.channel, "dev");
        // Not just "it parsed": the settings the older release actually acts on have to
        // arrive intact, which is the thing a tolerated section could have quietly cost.
        assert!(old.updates.automatic);
        assert_eq!(old.updates.window.as_deref(), Some("03:00-05:00"));
        assert!(
            !written.contains("[update_service]\n") || written.contains("url ="),
            "the section has to carry the address, or this proves nothing"
        );

        // And the shape that was rejected, failing the way it would have failed in the
        // field: one key in the wrong table, and the older release cannot read its own
        // hostname either.
        let inside_updates = "schema_version = 1\n\n[updates]\nchannel = \"dev\"\n\
                              url = \"https://updates.example\"\n";
        assert!(
            toml::from_str::<ConfigAsShippedBeforeDiscovery>(inside_updates).is_err(),
            "if this ever passes, the section could have gone in [updates] after all"
        );
    }

    #[test]
    fn an_appliance_with_no_update_service_is_configured_not_to_look() {
        // The shipped state, and the one every machine in the field is in. There is no
        // MediaLith update service, so the honest default is a field nobody has filled in.
        let config = Config::parse("schema_version = 1").unwrap();
        assert!(!config.update_service.is_configured());
        assert_eq!(config.update_service.url, "");
        assert!(
            config.update_service.check,
            "checking is on; it simply has nowhere to look, which is what the page says"
        );

        // A field somebody pasted into and then cleared.
        let cleared =
            Config::parse("schema_version = 1\n[update_service]\nurl = \"  \"\n").unwrap();
        assert!(!cleared.update_service.is_configured());
    }

    #[test]
    fn the_update_service_round_trips_and_refuses_a_misspelt_key() {
        let text = "schema_version = 1\n\n[updates]\nchannel = \"dev\"\n\n\
                    [update_service]\nurl = \"http://192.168.2.165:8080/medialith\"\ncheck = false\n";
        let config = Config::parse(text).unwrap();
        assert_eq!(
            config.updates.tracked(),
            Some(crate::manifest::Channel::Dev)
        );
        assert!(config.update_service.is_configured());
        assert!(!config.update_service.check);
        assert_eq!(
            Config::parse(&toml::to_string(&config).unwrap()).unwrap(),
            config
        );

        // Strictness stays where a person types, exactly as it does in every other section.
        assert!(Config::parse("schema_version = 1\n[update_service]\nurl_ = \"x\"\n").is_err());
    }

    #[test]
    fn a_channel_this_build_cannot_name_is_readable_and_not_trackable() {
        // Both halves matter. The file must parse, or a release that knows a fourth channel
        // strands the one you roll back to. And it must not resolve to something, or the
        // appliance quietly tracks a feed nobody chose.
        let config = Config::parse("schema_version = 1\n[updates]\nchannel = \"lts\"\n").unwrap();
        assert_eq!(config.updates.channel, "lts");
        assert_eq!(config.updates.tracked(), None);

        for known in crate::manifest::Channel::ALL {
            let text = format!("schema_version = 1\n[updates]\nchannel = \"{known}\"\n");
            assert_eq!(Config::parse(&text).unwrap().updates.tracked(), Some(known));
        }
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
        // The product default, which changed with the MediaLith rename. It reaches new
        // installations only: a machine whose config already names it never asks for this.
        assert_eq!(config.system.hostname, "medialith");
        assert!(config.updates.automatic);
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

            [shares.smb]
            enabled = true
        "#,
        )
        .unwrap();

        assert_eq!(config.system.hostname, "kino");
        assert!(config.shares.smb.enabled);
        assert!(!config.shares.nfs.enabled);
        assert_eq!(config.updates.window.start_minute, 180);
    }

    #[test]
    fn rejects_a_misspelled_key_rather_than_ignoring_it() {
        // The key was `transcod_dir` in `[plex]` until that section was removed, at which
        // point this passed for the wrong reason for one commit: an unknown *section* is
        // ignored by design, so the test was asserting the opposite of its own name. The
        // property belongs to a section that still exists, and there is nothing special
        // about which one.
        let err = Config::parse("schema_version = 1\n[system]\nhostnam = \"kino\"").unwrap_err();
        assert!(
            matches!(&err, ConfigError::Invalid(d) if d.contains("hostnam")),
            "error should name the offending key, got: {err}"
        );
    }

    #[test]
    fn a_section_this_build_has_never_heard_of_is_ignored_rather_than_refused() {
        // The other half, and the half that makes removing `[plex]` safe: a file written
        // by a release that still had it stays readable here. Without this, deleting a
        // section would make every configuration carrying it unreadable — which is the
        // rollback hazard the strictness split was designed around.
        let config = Config::parse(
            "schema_version = 1\n[plex]\nmedia = [\"/var/media/films\"]\n[system]\nhostname = \"kino\"",
        )
        .expect("an unknown section is a newer release talking, not an error");
        assert_eq!(config.system.hostname, "kino");
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
            err.to_string().contains("newer MediaLith release"),
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
