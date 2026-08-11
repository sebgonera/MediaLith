//! The boot health gate.
//!
//! ARCHITECTURE.md §2 calls step 7 "the load-bearing one", and this is it. Until this
//! passes, the boot try counter stands; three failures and the previous slot wins.
//!
//! # The two ways to get this wrong
//!
//! **Too weak**, and a broken update that still reaches PID 1 is marked good and
//! never rolls back. ADR-0005 rejects "services were spawned" for exactly this
//! reason: spawning is not working.
//!
//! **Too strict**, and healthy systems roll back on a transient failure — an
//! infuriating outcome, and much harder to diagnose than a system that simply did not
//! boot. This is why no check here may depend on the network. Ethernet can arrive
//! over USB, which enumerates seconds after PCI, and a gate that waited for an address
//! would roll back a perfectly good update because a dongle was slow. Plex is checked
//! **on loopback only**, and a machine with an unplugged cable is a machine with a
//! network problem, not one that needs its OS reverted.
//!
//! # Checks that do not apply yet
//!
//! Plex is not in the image (ADR-0010 has it provisioned separately and it is not
//! written). A check for something that is not installed can only be one of two
//! things: a failure that never clears, so nothing ever boots successfully, or a
//! silent pass, which is the "too weak" failure above wearing a disguise.
//!
//! So a check reports [`Status::NotApplicable`] when the thing it tests is not part of
//! this image, and that is treated as neither pass nor fail. The moment Plex is
//! provisioned it becomes required, and
//! `a_provisioned_plex_is_required_not_optional` fails if anyone weakens that.

use std::fmt;
use std::path::Path;

use serde::Serialize;

/// The outcome of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// The condition holds.
    Pass,
    /// The condition does not hold. The boot is not healthy.
    Fail,
    /// The component is not part of this image, so there is nothing to check.
    NotApplicable,
}

/// One condition, with enough detail to act on when it fails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    /// Short name, e.g. `var-writable`.
    pub name: &'static str,
    /// What happened.
    pub status: Status,
    /// Why, in terms someone reading a console can use.
    pub detail: String,
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let marker = match self.status {
            Status::Pass => "ok",
            Status::Fail => "FAIL",
            Status::NotApplicable => "n/a",
        };
        write!(f, "{marker:>4}  {:<16} {}", self.name, self.detail)
    }
}

/// The whole gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Health {
    /// Every check, in the order run.
    pub checks: Vec<Check>,
}

impl Health {
    /// Whether the boot may be declared good.
    ///
    /// Requires at least one check to have passed. An all-`NotApplicable` result
    /// means nothing was actually verified, and marking a slot permanent on that
    /// basis would be the too-weak failure in its purest form.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        let failed = self.checks.iter().any(|c| c.status == Status::Fail);
        let passed = self.checks.iter().any(|c| c.status == Status::Pass);
        !failed && passed
    }

    /// The checks that failed.
    #[must_use]
    pub fn failures(&self) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|c| c.status == Status::Fail)
            .collect()
    }
}

/// Runs the whole gate against the running system.
///
/// Lives here rather than in `main` because the status console reports the same
/// verdict the gate reached, and two lists of checks that drifted apart would be worse
/// than no console at all — the page would show a healthy machine that had in fact
/// rolled back.
#[must_use]
pub fn run_all() -> Health {
    use plexos_types::paths;

    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();

    Health {
        checks: vec![
            check_var_writable(Path::new(paths::VAR)),
            check_usr_verified(&mounts),
            // The probe was `|| false` for as long as Plex could not exist, which was
            // correct then and became a permanent false failure the moment one was
            // installed: the console reported "installed but not answering" about a
            // server that was answering perfectly well. It asks Plex now.
            check_plex(Path::new(paths::PLEX_APPS), &crate::plex::is_answering),
        ],
    }
}

/// Is `/var` mounted read-write?
///
/// Writing rather than reading the mount table: a filesystem can be mounted `rw` and
/// still refuse writes, having been remounted read-only after an I/O error, which is
/// exactly the state worth catching.
#[must_use]
pub fn check_var_writable(var: &Path) -> Check {
    let probe = var.join(".plexosd-health");
    let detail = match std::fs::write(&probe, b"health\n") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            return Check {
                name: "var-writable",
                status: Status::Pass,
                detail: format!("{} accepts writes", var.display()),
            };
        }
        Err(error) => format!(
            "{} is not writable: {error}. The slot will roll back; check the disk \
             and the filesystem before reinstalling.",
            var.display()
        ),
    };

    Check {
        name: "var-writable",
        status: Status::Fail,
        detail,
    }
}

/// Is `/usr` mounted, read-only, and coming from the verity device?
///
/// A writable `/usr` means the verity mapping was skipped or the wrong device was
/// mounted, and the whole trust chain of ADR-0004 is not in place. That is worth
/// failing the boot over even though the system appears to work.
#[must_use]
pub fn check_usr_verified(mounts: &str) -> Check {
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(source), Some(target), Some(_fstype), Some(options)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if target != "/usr" {
            continue;
        }

        let read_only = options.split(',').any(|o| o == "ro");
        let from_verity = source.contains("mapper");

        return if read_only && from_verity {
            Check {
                name: "usr-verified",
                status: Status::Pass,
                detail: format!("{source} mounted read-only"),
            }
        } else {
            Check {
                name: "usr-verified",
                status: Status::Fail,
                detail: format!(
                    "/usr is {source} with options {options}; expected a read-only \
                     device-mapper device. The verity chain is not established, so \
                     this image cannot be trusted even though it booted."
                ),
            }
        };
    }

    Check {
        name: "usr-verified",
        status: Status::Fail,
        detail: "/usr is not mounted at all".to_owned(),
    }
}

/// The name of the check that asks whether Plex is answering.
///
/// A constant because three places write it and a fourth now reads it: the appliance
/// dashboard picks this check out of the list to decide what to say about Plex. Two
/// spellings of one name is the shape of defect this repository has recorded twice --
/// once as a duplicate `id` on the console page, once as a route the gate and the router
/// disagreed about -- and it fails silently in both directions.
pub const PLEX_HTTP: &str = "plex-http";

/// Is Plex answering on loopback?
///
/// `plex_root` is where the app image would be. When it is absent Plex is not part of
/// this image, and the check does not apply — see the module documentation for why
/// that is a distinct outcome rather than a pass.
#[must_use]
pub fn check_plex(plex_root: &Path, probe: &dyn Fn() -> bool) -> Check {
    if !plex_root.exists() {
        return Check {
            name: PLEX_HTTP,
            status: Status::NotApplicable,
            detail: format!(
                "no Plex app image at {} — not provisioned yet (ADR-0010)",
                plex_root.display()
            ),
        };
    }

    if probe() {
        Check {
            name: PLEX_HTTP,
            status: Status::Pass,
            detail: "answering on loopback".to_owned(),
        }
    } else {
        Check {
            name: PLEX_HTTP,
            status: Status::Fail,
            detail: "installed but not answering on loopback; the slot will roll back".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &'static str, status: Status) -> Check {
        Check {
            name,
            status,
            detail: String::new(),
        }
    }

    #[test]
    fn a_writable_var_passes_and_leaves_nothing_behind() {
        let dir = std::env::temp_dir().join("plexosd-health-test");
        std::fs::create_dir_all(&dir).unwrap();
        let result = check_var_writable(&dir);
        assert_eq!(result.status, Status::Pass);
        assert!(
            !dir.join(".plexosd-health").exists(),
            "the probe file must be cleaned up"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unwritable_var_fails_with_a_remedy() {
        let result = check_var_writable(Path::new("/proc/nonexistent-dir"));
        assert_eq!(result.status, Status::Fail);
        assert!(result.detail.contains("roll back"), "{}", result.detail);
    }

    #[test]
    fn a_read_only_verity_usr_passes() {
        let mounts = "/dev/mapper/plexos-usr /usr erofs ro,nosuid,nodev 0 0\n";
        assert_eq!(check_usr_verified(mounts).status, Status::Pass);
    }

    #[test]
    fn a_writable_usr_fails_even_though_the_system_works() {
        // This is the case worth catching: everything runs, and nothing is verified.
        let mounts = "/dev/vda2 /usr erofs rw,nosuid,nodev 0 0\n";
        let result = check_usr_verified(mounts);
        assert_eq!(result.status, Status::Fail);
        assert!(
            result.detail.contains("cannot be trusted"),
            "{}",
            result.detail
        );
    }

    #[test]
    fn a_usr_not_on_the_mapper_fails() {
        // Mounted straight from the partition: read-only, and completely unverified.
        let mounts = "/dev/vda2 /usr erofs ro,nosuid,nodev 0 0\n";
        assert_eq!(check_usr_verified(mounts).status, Status::Fail);
    }

    #[test]
    fn an_absent_usr_fails_rather_than_being_skipped() {
        assert_eq!(check_usr_verified("").status, Status::Fail);
    }

    #[test]
    fn an_option_that_merely_contains_ro_is_not_read_only() {
        // "errors=remount-ro" contains "ro". Substring matching would call a
        // writable /usr verified.
        let mounts = "/dev/mapper/plexos-usr /usr erofs rw,errors=remount-ro 0 0\n";
        assert_eq!(check_usr_verified(mounts).status, Status::Fail);
    }

    #[test]
    fn plex_is_not_applicable_until_it_is_provisioned() {
        let result = check_plex(Path::new("/nonexistent-plex"), &|| false);
        assert_eq!(result.status, Status::NotApplicable);
        assert!(result.detail.contains("ADR-0010"), "{}", result.detail);
    }

    #[test]
    fn a_provisioned_plex_is_required_not_optional() {
        // The check that stops "not applicable" from quietly becoming "optional"
        // once Plex actually ships. If this ever passes with a dead Plex, the gate
        // has become the too-weak kind ADR-0005 warns about.
        let existing = std::env::temp_dir();
        assert_eq!(check_plex(&existing, &|| true).status, Status::Pass);
        assert_eq!(check_plex(&existing, &|| false).status, Status::Fail);
    }

    #[test]
    fn one_failure_makes_the_whole_boot_unhealthy() {
        let health = Health {
            checks: vec![
                check("a", Status::Pass),
                check("b", Status::Fail),
                check("c", Status::Pass),
            ],
        };
        assert!(!health.is_healthy());
        assert_eq!(health.failures().len(), 1);
    }

    #[test]
    fn not_applicable_checks_do_not_block_a_healthy_boot() {
        let health = Health {
            checks: vec![
                check("a", Status::Pass),
                check("plex", Status::NotApplicable),
            ],
        };
        assert!(health.is_healthy());
    }

    #[test]
    fn a_boot_that_verified_nothing_is_not_healthy() {
        // All-not-applicable means the gate did no work. Marking a slot permanent on
        // that basis is the too-weak failure in its purest form.
        let health = Health {
            checks: vec![check("a", Status::NotApplicable)],
        };
        assert!(!health.is_healthy());
        assert!(
            !Health { checks: vec![] }.is_healthy(),
            "an empty gate verified nothing"
        );
    }
}
