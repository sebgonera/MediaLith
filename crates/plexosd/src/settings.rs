//! Reading, writing and *applying* the configuration (ADR-0008).
//!
//! The schema has existed in [`plexos_types::config`] since early on, with tests and a
//! frozen wire format — and nothing read it. `paths::CONFIG_FILE` had no callers, no
//! hostname was ever set, and no timezone was ever applied. That is the third time this
//! project has found a complete, tested, uncalled design; the others were the `auth`
//! gate and `cgroup::delegation`.
//!
//! # Storing is not applying, and the difference is the whole module
//!
//! A settings page that writes a file and reports success is worse than no settings page.
//! It looks like it worked. Every field here therefore carries an [`Outcome`] saying what
//! actually happened to the machine, and "written to the file, and the machine has not
//! changed" is a distinct, reportable answer rather than a silent one.
//!
//! Some settings genuinely cannot take effect until a restart. That is fine and gets
//! said. What is not fine is letting the reader assume.
//!
//! # Where the file lives, and why that survives things
//!
//! `/etc/plexos/config.toml`, and `/etc` is an overlay whose upper layer is on `/var`
//! (`plexos-init::plan`). So a change survives a reboot, and it survives a *rollback*,
//! because rollback reverts `/usr` and never `/var` (ADR-0005). That is the desirable
//! direction: a machine that rolls back keeps its name.
//!
//! It also means ADR-0009's rule applies — a configuration written by a new release has
//! to stay readable by the old one, which is what `schema_version` is for.
//!
//! # What has run
//!
//! **Nothing on hardware yet.**

use std::io;
use std::path::{Path, PathBuf};

use plexos_types::config::Config;
use plexos_types::paths;

/// What happened to the machine when a setting was applied.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Outcome {
    /// In force now.
    Applied {
        /// What the machine is doing as a result.
        detail: String,
    },
    /// Stored, and it takes effect at the next restart.
    ///
    /// Distinct from [`Outcome::Applied`] on purpose: a reader who is told "saved" and
    /// finds nothing changed has been misled, and a reader told "at next restart" knows
    /// exactly what to do.
    Pending {
        /// Why it could not take effect now.
        detail: String,
    },
    /// Stored, and it could not be applied.
    Failed {
        /// What went wrong, and what to do about it.
        detail: String,
    },
    /// Nothing to do — the machine already matched.
    Unchanged,
}

/// The result of saving a configuration.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Applied {
    /// What happened to the hostname.
    pub hostname: Outcome,
    /// What happened to the timezone.
    pub timezone: Outcome,
}

/// Loads the configuration, falling back to defaults.
///
/// A missing file is not an error: an appliance that has never been configured is the
/// normal first state, and refusing to serve a page because nobody has set a hostname
/// would be absurd. A *malformed* file is an error, because silently replacing somebody's
/// settings with defaults is how configuration disappears.
///
/// # Errors
/// If the file exists and cannot be read or parsed.
pub fn load(path: &Path) -> Result<Config, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Config::parse(&text).map_err(|e| {
            format!(
                "{} is not a configuration this build understands: {e}. Remedy: fix or \
                 remove it. This deliberately refuses rather than falling back to \
                 defaults, because quietly replacing settings is how they vanish.",
                path.display()
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
        Err(error) => Err(format!("could not read {}: {error}", path.display())),
    }
}

/// Writes the configuration atomically.
///
/// Through a temporary file and a rename, because the alternative is a half-written TOML
/// on a machine that lost power mid-save — and [`load`] refuses to parse that, which
/// would leave the appliance unable to read its own configuration. A rename within one
/// filesystem is the closest thing to atomic available here, and it is the same reasoning
/// ADR-0005 uses for boot entries.
///
/// # Errors
/// Any I/O failure, naming the path.
pub fn save(config: &Config, path: &Path) -> Result<(), String> {
    let text = toml::to_string_pretty(config)
        .map_err(|e| format!("could not serialise the configuration: {e}"))?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }

    let temporary = path.with_extension("toml.new");
    std::fs::write(&temporary, text)
        .map_err(|e| format!("could not write {}: {e}", temporary.display()))?;

    std::fs::rename(&temporary, path).map_err(|e| {
        let _ = std::fs::remove_file(&temporary);
        format!("could not replace {}: {e}", path.display())
    })
}

/// Where the hostname is left for the next boot to find.
///
/// The kernel forgets its hostname at reboot, so the syscall alone gives a machine that
/// renames itself back overnight.
pub const HOSTNAME_FILE: &str = "/etc/hostname";

/// Where the timezone is selected, by symlink, as every Linux program expects.
pub const LOCALTIME: &str = "/etc/localtime";

/// Where the zoneinfo database lives, when there is one.
pub const ZONEINFO: &str = "/usr/share/zoneinfo";

/// Applies a configuration to the running machine.
///
/// Each step reports what happened to it and none aborts the others: a timezone that
/// cannot be set is no reason to leave the hostname alone.
#[must_use]
pub fn apply(config: &Config) -> Applied {
    Applied {
        hostname: apply_hostname(&config.system.hostname),
        timezone: apply_timezone(&config.system.timezone),
    }
}

/// Sets the kernel hostname and records it for the next boot.
fn apply_hostname(wanted: &str) -> Outcome {
    if !plexos_sys::hostname::is_valid(wanted) {
        return Outcome::Failed {
            detail: format!(
                "{wanted:?} is not a usable hostname. Remedy: letters, digits and \
                 hyphens only, not starting or ending with a hyphen, at most {} bytes. \
                 This is narrower than the kernel accepts, because the name also goes to \
                 the DHCP server and into logs.",
                plexos_sys::hostname::MAX_LEN
            ),
        };
    }

    if plexos_sys::hostname::get().is_ok_and(|current| current == wanted) {
        return Outcome::Unchanged;
    }

    if let Err(error) = plexos_sys::hostname::set(wanted) {
        return Outcome::Failed {
            detail: format!(
                "the kernel refused the name: {error}. Remedy: this needs CAP_SYS_ADMIN, \
                 and plexosd is started by PID 1, so if this is reachable something is \
                 wrong beyond the hostname."
            ),
        };
    }

    // Reported rather than fatal. A running machine with the right name and a file that
    // did not save is a machine that renames itself at the next boot, which is worth
    // saying and is not worth undoing a working change over.
    let persisted = std::fs::write(HOSTNAME_FILE, format!("{wanted}\n"));

    match persisted {
        Ok(()) => Outcome::Applied {
            detail: format!("the machine is now {wanted}"),
        },
        Err(error) => Outcome::Applied {
            detail: format!(
                "the machine is now {wanted}, but {HOSTNAME_FILE} could not be written \
                 ({error}), so it will revert at the next restart. Remedy: check that \
                 the /etc overlay is writable."
            ),
        },
    }
}

/// Points `/etc/localtime` at the requested zone.
///
/// The check that the zone *exists* is the point. Without zoneinfo in the image — which
/// is where this appliance started — a symlink to a missing file is created happily and
/// every program silently falls back to UTC, so the setting appears to work and does
/// nothing. That is precisely the failure this module exists to refuse.
fn apply_timezone(wanted: &str) -> Outcome {
    // Refused before it is used as a path. A timezone arrives over HTTP, and `..` in it
    // would point the symlink anywhere on the filesystem.
    if wanted.is_empty()
        || wanted.starts_with('/')
        || wanted
            .split('/')
            .any(|part| part == ".." || part.is_empty())
    {
        return Outcome::Failed {
            detail: format!(
                "{wanted:?} is not an IANA timezone name. Remedy: use the form \
                 Europe/Warsaw. This refuses anything containing '..' or a leading \
                 slash, because the value becomes a path."
            ),
        };
    }

    let zone = Path::new(ZONEINFO).join(wanted);
    if !zone.exists() {
        return Outcome::Failed {
            detail: format!(
                "there is no {} on this appliance. Remedy: if the name is right, the \
                 image has no timezone database -- BR2_TARGET_TZ_INFO in the defconfig, \
                 which needs a rebuild. Without it a symlink would be created happily \
                 and every program would silently use UTC.",
                zone.display()
            ),
        };
    }

    if std::fs::read_link(LOCALTIME).is_ok_and(|t| t == zone) {
        return Outcome::Unchanged;
    }

    // Removed first: symlink() refuses to replace an existing path, and there is no
    // atomic swap for a symlink that does not go through a temporary name.
    let temporary = PathBuf::from(format!("{LOCALTIME}.new"));
    let _ = std::fs::remove_file(&temporary);

    let linked = std::os::unix::fs::symlink(&zone, &temporary)
        .and_then(|()| std::fs::rename(&temporary, LOCALTIME));

    match linked {
        Ok(()) => Outcome::Pending {
            detail: format!(
                "{LOCALTIME} now points at {wanted}. Programs already running keep the \
                 old zone until they restart -- glibc and musl read this once. Plex \
                 picks it up on its next start."
            ),
        },
        Err(error) => Outcome::Failed {
            detail: format!(
                "could not point {LOCALTIME} at {wanted}: {error}. Remedy: check that \
                 the /etc overlay is writable."
            ),
        },
    }
}

/// The configuration file this appliance uses.
#[must_use]
pub fn path() -> PathBuf {
    PathBuf::from(paths::CONFIG_FILE)
}

/// What `GET /api/config` reports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct View {
    /// The stored configuration.
    pub config: Config,
    /// The hostname the kernel is actually using.
    ///
    /// Reported beside the stored one rather than instead of it, because the two
    /// disagreeing is a real state — a name saved but never applied — and a page that
    /// showed only the file would present it as if it had taken effect.
    pub hostname_now: Option<String>,
    /// Timezone names this image can actually be set to.
    ///
    /// Empty on an image built without `BR2_TARGET_TZ_INFO`, which is the honest answer:
    /// the field should offer nothing rather than offer names that will be refused.
    pub timezones: Vec<String>,
    /// Why the stored configuration could not be read, if it could not.
    pub error: Option<String>,
}

/// Gathers the current configuration and what the machine is actually doing.
#[must_use]
pub fn view(path: &Path) -> View {
    let (config, error) = match load(path) {
        Ok(config) => (config, None),
        // Defaults *and* the error, not one or the other: the page has to render, and it
        // has to say that what it is rendering is not what is on disk.
        Err(error) => (Config::default(), Some(error)),
    };

    View {
        config,
        hostname_now: plexos_sys::hostname::get().ok(),
        timezones: available_timezones(Path::new(ZONEINFO)),
        error,
    }
}

/// Every zone name under a zoneinfo directory, sorted.
///
/// Walked rather than read from `zone.tab`, which lists only country zones and omits
/// `UTC` and the `Etc/` names — a list that silently lacks the default this appliance
/// ships with would be an odd thing to offer.
#[must_use]
pub fn available_timezones(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    collect_zones(root, root, &mut found);
    found.sort();
    found
}

/// Recurses, collecting zone names relative to the root.
fn collect_zones(root: &Path, dir: &Path, into: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();

        // `metadata` follows symlinks; `entry.file_type()` does not, and that distinction
        // is the whole bug. tzdata ships `Europe` as a symlink to `posix/Europe`, so
        // `file_type()` reported it as a symlink -- neither a file nor a directory --
        // whereupon it fell through to the leaf branch and "Europe" was offered as a
        // timezone. The build host's own tzdata lays those out as real directories, so
        // the unit tests and a local run both agreed with the bug; a shell on the
        // appliance answered it in one `ls -ld`.
        if std::fs::metadata(&path).is_ok_and(|m| m.is_dir()) {
            collect_zones(root, &path, into);
            continue;
        }

        // The database ships indexes and a source tarball alongside the zones. Offering
        // "zone.tab" as a timezone would be a small, silly, entirely avoidable bug.
        let name = path.strip_prefix(root).unwrap_or(&path).to_string_lossy();
        if !name.contains('.') && !name.starts_with("posix") && !name.starts_with("right") {
            into.push(name.into_owned());
        }
    }
}

/// Applies a configuration and stores it, in that order.
///
/// Applying first is deliberate. If the machine refuses a setting, the file still records
/// what was asked for — so the page can show the request beside the refusal, and a later
/// image that *can* honour it will do so at the next boot without anyone retyping. The
/// alternative, storing only what succeeded, quietly discards the user's intent.
///
/// # Errors
/// Only if the file cannot be written. A setting the machine refused is reported in the
/// [`Applied`] rather than as an error, because the save did happen.
pub fn store(config: &Config, path: &Path) -> Result<Applied, String> {
    let applied = apply(config);
    save(config, path)?;
    Ok(applied)
}

/// Merges an incoming JSON document onto the stored configuration.
///
/// A patch rather than a replacement: the page edits two fields, and a body that had to
/// carry the whole document would silently revert anything a newer page had added. Only
/// the keys present are changed.
///
/// # Errors
/// If the body is not JSON, or names a field with the wrong type.
pub fn patch(config: &mut Config, body: &[u8]) -> Result<(), String> {
    let document: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("the request body is not JSON: {e}"))?;

    let Some(system) = document.get("system") else {
        return Ok(());
    };

    if let Some(hostname) = system.get("hostname") {
        let hostname = hostname
            .as_str()
            .ok_or_else(|| "system.hostname must be a string".to_owned())?;
        config.system.hostname.clear();
        config.system.hostname.push_str(hostname.trim());
    }

    if let Some(timezone) = system.get("timezone") {
        let timezone = timezone
            .as_str()
            .ok_or_else(|| "system.timezone must be a string".to_owned())?;
        config.system.timezone.clear();
        config.system.timezone.push_str(timezone.trim());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("plexos-settings-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_machine_that_has_never_been_configured_gets_defaults() {
        // The normal first state. Refusing to serve a page because nobody has set a
        // hostname would be absurd.
        let missing = scratch("absent").join("config.toml");
        assert_eq!(load(&missing), Ok(Config::default()));
    }

    #[test]
    fn a_malformed_file_is_refused_rather_than_replaced_with_defaults() {
        // The alternative is that a typo silently reverts every setting somebody made,
        // and the page then reports the defaults as if they were their choices.
        let path = scratch("malformed").join("config.toml");
        std::fs::write(&path, "this is not toml {{{").unwrap();

        let error = load(&path).expect_err("must refuse");
        assert!(error.contains("Remedy:"), "{error}");
        assert!(error.contains("how they vanish"), "{error}");
    }

    #[test]
    fn a_configuration_survives_the_round_trip() {
        let dir = scratch("roundtrip");
        let path = dir.join("config.toml");

        let mut config = Config::default();
        config.system.hostname = "cinema".to_owned();
        config.system.timezone = "Europe/Warsaw".to_owned();

        save(&config, &path).expect("saves");
        assert_eq!(load(&path), Ok(config));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_leaves_no_temporary_behind() {
        // The rename is what makes this atomic; a leftover .toml.new would mean the
        // rename did not happen and the next load reads the old file.
        let dir = scratch("atomic");
        let path = dir.join("config.toml");
        save(&Config::default(), &path).expect("saves");

        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "config.toml")
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hostname_the_dhcp_server_would_object_to_is_refused_before_the_syscall() {
        match apply_hostname("not a hostname") {
            Outcome::Failed { detail } => {
                assert!(detail.contains("Remedy:"), "{detail}");
                assert!(
                    detail.contains("DHCP"),
                    "and says why it is strict: {detail}"
                );
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_timezone_containing_a_traversal_is_refused_because_it_becomes_a_path() {
        // It arrives over HTTP and ends up as the target of a symlink in /etc.
        for attempt in ["../../etc/shadow", "/etc/shadow", "", "Europe//Warsaw"] {
            assert!(
                matches!(apply_timezone(attempt), Outcome::Failed { .. }),
                "{attempt:?} was not refused"
            );
        }
    }

    #[test]
    fn a_directory_reached_through_a_symlink_is_still_walked() {
        // tzdata ships `Europe` as a symlink to `posix/Europe`. DirEntry::file_type does
        // not follow symlinks, so the first version of this reported "Europe" as a
        // timezone and never found Europe/Warsaw -- on the appliance only, because the
        // build host's tzdata uses real directories and every test agreed with the bug.
        let root = std::env::temp_dir().join("plexos-zones-symlink");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("posix/Europe")).unwrap();
        std::fs::write(root.join("posix/Europe/Warsaw"), b"tz").unwrap();
        std::os::unix::fs::symlink("posix/Europe", root.join("Europe")).unwrap();

        let zones = available_timezones(&root);

        assert!(
            zones.contains(&"Europe/Warsaw".to_owned()),
            "a zone behind a symlinked directory must be found: {zones:?}"
        );
        assert!(
            !zones.contains(&"Europe".to_owned()),
            "and the directory itself must not be offered as a zone: {zones:?}"
        );
        assert!(
            !zones.iter().any(|z| z.starts_with("posix")),
            "the posix/ duplicates stay out: {zones:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_timezone_with_no_zoneinfo_behind_it_is_a_failure_not_a_success() {
        // The whole reason this module distinguishes storing from applying. On an image
        // with no timezone database the symlink would be created happily and every
        // program would silently use UTC -- a setting that appears to work and does not.
        let outcome = apply_timezone("Nowhere/Invented");
        match outcome {
            Outcome::Failed { detail } => {
                assert!(detail.contains("BR2_TARGET_TZ_INFO"), "{detail}");
                assert!(detail.contains("silently use UTC"), "{detail}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn pending_and_applied_are_different_answers() {
        // A reader told "saved" who finds nothing changed has been misled. The three
        // states have to be distinguishable in the JSON, not merged into a boolean.
        let applied = serde_json::to_string(&Outcome::Applied {
            detail: "x".to_owned(),
        })
        .unwrap();
        let pending = serde_json::to_string(&Outcome::Pending {
            detail: "x".to_owned(),
        })
        .unwrap();

        assert!(applied.contains("\"applied\""), "{applied}");
        assert!(pending.contains("\"pending\""), "{pending}");
        assert_ne!(applied, pending);
    }
}
