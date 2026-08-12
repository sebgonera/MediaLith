//! What the processor can do, and whether this build can run on it.
//!
//! # Why this exists at all
//!
//! For most of this project's life the Buildroot userspace was compiled
//! `-march=corei7`, which permits the compiler to emit SSE4.2, POPCNT and
//! `CMPXCHG16B` anywhere it likes. Nothing chose that floor: it was in the defconfig
//! from the first day, it matched the reference laptop, and no component ever asked
//! for it. The consequence is the failure this module exists to replace — a machine
//! below the floor boots the kernel, runs `plexos-init` perfectly well, and then dies
//! on the *first Buildroot binary it executes* with `SIGILL`. No message, no log line,
//! and a kernel that reports `Attempted to kill init` about a program that was fine.
//!
//! The floor is gone (the userspace is built `-march=x86-64` now), so nothing in the
//! image requires anything above the architectural baseline. That is the finding, not
//! an oversight, and [`REQUIRED`] is empty because of it.
//!
//! # Then what is this for
//!
//! Two things, neither of which is "reject old hardware".
//!
//! 1. **It says what the processor is.** A machine that fails for some unrelated
//!    reason should not also be a machine nobody can identify. Vendor, family, model
//!    and brand string are diagnostic and are *never* a reason to refuse: a CPU is
//!    judged by what it can do, and a list of model numbers is a list that is wrong
//!    the moment somebody buys something newer.
//! 2. **It makes a future raised requirement fail deliberately.** If a later release
//!    genuinely needs something — a package that will not build below x86-64-v2, a
//!    kernel that stops supporting original K8 — adding it to [`REQUIRED`] turns an
//!    unexplained `SIGILL` in the middle of the boot into a sentence naming the
//!    missing extension. That is the whole architectural point, and it has to be
//!    wired up *before* it is needed, because the state it protects against is one
//!    where nothing can print anything.
//!
//! # What has run
//!
//! The pure half — [`evaluate`], [`Compatibility::missing`] and the diagnostic — is
//! covered by tests that build their inputs by hand and so give the same answer on
//! any machine. [`detect`] has run on the build host and under QEMU system emulation
//! on `Opteron_G1`, Conroe, Nehalem and Haswell CPU models. It has **not** run on a
//! physical CPU older than the reference laptop, because there is not one here.

use std::fmt;

/// An x86-64 instruction-set extension above the architectural baseline.
///
/// The baseline itself — MMX, SSE and SSE2 — is deliberately absent. Those are not
/// optional on x86-64: a processor without SSE2 cannot execute the 64-bit ABI at all,
/// so it could not have run the kernel, let alone reached this code. A check for them
/// could only ever answer "yes", and a check that cannot fail is worse than no check
/// because it gets counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Feature {
    /// SSE3, Prescott and K8 revision E onwards.
    Sse3,
    /// SSSE3, Core 2 onwards.
    Ssse3,
    /// SSE4.1, Penryn onwards.
    Sse41,
    /// SSE4.2, Nehalem onwards. Half of what `-march=corei7` used to assume.
    Sse42,
    /// `POPCNT`. Enumerated separately from SSE4.2 by CPUID and separately here,
    /// because AMD shipped it in K10 without the rest of SSE4.
    Popcnt,
    /// `CMPXCHG16B`. Absent on the earliest AMD64 parts and on some early Intel
    /// 64-bit Pentium 4s, which is why `-march=x86-64` still does not assume it.
    Cmpxchg16b,
    /// `LAHF`/`SAHF` in 64-bit mode. Removed in the original AMD64 and restored
    /// later, so it is a real hole rather than a formality.
    LahfSahf,
    /// AVX, Sandy Bridge onwards.
    Avx,
    /// AVX2, Haswell onwards.
    Avx2,
    /// BMI1, Haswell onwards.
    Bmi1,
    /// BMI2, Haswell onwards.
    Bmi2,
    /// FMA, Haswell onwards.
    Fma,
}

impl Feature {
    /// Every extension this module knows how to look for.
    ///
    /// Detection covers all of them whatever [`REQUIRED`] contains, so the diagnostic
    /// describes the processor rather than only the part of it somebody once cared
    /// about.
    pub const ALL: &'static [Self] = &[
        Self::Sse3,
        Self::Ssse3,
        Self::Sse41,
        Self::Sse42,
        Self::Popcnt,
        Self::Cmpxchg16b,
        Self::LahfSahf,
        Self::Avx,
        Self::Avx2,
        Self::Bmi1,
        Self::Bmi2,
        Self::Fma,
    ];

    /// What to call it in a message a person reads.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Sse3 => "SSE3",
            Self::Ssse3 => "SSSE3",
            Self::Sse41 => "SSE4.1",
            Self::Sse42 => "SSE4.2",
            Self::Popcnt => "POPCNT",
            Self::Cmpxchg16b => "CMPXCHG16B",
            Self::LahfSahf => "LAHF/SAHF",
            Self::Avx => "AVX",
            Self::Avx2 => "AVX2",
            Self::Bmi1 => "BMI1",
            Self::Bmi2 => "BMI2",
            Self::Fma => "FMA",
        }
    }

    /// The earliest widely-known part that has it, for a diagnostic.
    ///
    /// Named as an *example* and never used to decide anything. "Your processor is
    /// older than Nehalem" is a sentence somebody can act on; it is not the test, and
    /// the test is [`Compatibility::missing`].
    #[must_use]
    pub const fn first_seen_in(self) -> &'static str {
        match self {
            Self::Sse3 => "Prescott / Athlon 64 revision E",
            Self::Ssse3 => "Core 2",
            Self::Sse41 => "Penryn",
            Self::Sse42 | Self::Popcnt => "Nehalem",
            // One string, two extensions, and they really did arrive together: both
            // were absent from the original AMD64 and added in the same revision.
            Self::Cmpxchg16b | Self::LahfSahf => "Athlon 64 revision D / Core 2",
            Self::Avx => "Sandy Bridge",
            Self::Avx2 | Self::Bmi1 | Self::Bmi2 | Self::Fma => "Haswell",
        }
    }
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The extensions MediaLith requires beyond the x86-64 architectural baseline.
///
/// **Empty. That is a measured result, not a stub.**
///
/// Every component was checked against the question separately, because they are built
/// by three different toolchains and only one of them was ever the problem:
///
/// * the **Buildroot userspace** is compiled `-march=x86-64`, which permits MMX, SSE
///   and SSE2 and nothing else — this is the half that used to be `corei7`;
/// * the **workspace's own binaries** (`plexos-init`, `plexosd`, `plexos-gpu`) are
///   built for `x86_64-unknown-linux-gnu` with no `-C target-cpu`, whose enabled
///   features are exactly `fxsr,sse,sse2`;
/// * **Plex Media Server** carries its own musl runtime and dispatches on CPUID at
///   run time, and was observed serving on an `Opteron_G1` CPU model — a part with no
///   SSSE3, no SSE4, no POPCNT and no `CMPXCHG16B`.
///
/// So nothing here has anything to add, and inventing a requirement so that this list
/// is not empty would be inventing a reason to refuse a machine that works. The list
/// is what the guard is *for*; if a later release genuinely needs an extension, adding
/// it here is the whole change.
pub const REQUIRED: &[Feature] = &[];

/// Who the processor says it is. Diagnostic only.
///
/// Every field here is reported and none of it is judged. Refusing on a model number
/// is how a distribution ends up rejecting hardware that would have worked perfectly
/// and accepting hardware that does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The twelve-byte CPUID vendor string, `GenuineIntel` or `AuthenticAMD` on
    /// anything this is likely to meet.
    pub vendor: String,
    /// Display family, with the extended family already folded in.
    pub family: u32,
    /// Display model, with the extended model already folded in.
    pub model: u32,
    /// Stepping.
    pub stepping: u32,
    /// The marketing brand string, or empty if the processor does not offer one.
    pub brand: String,
}

impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.brand.is_empty() {
            write!(
                f,
                "{} family {} model {} stepping {}",
                self.vendor, self.family, self.model, self.stepping
            )
        } else {
            write!(
                f,
                "{} ({}, family {} model {} stepping {})",
                self.brand, self.vendor, self.family, self.model, self.stepping
            )
        }
    }
}

/// A processor, what it can do, and what this build needs of it.
///
/// Deliberately holds the requirement as data rather than reaching for [`REQUIRED`]
/// inside its own methods. That is what makes the whole model testable without a
/// machine that has the property under test: a test states both halves and gets the
/// same answer on any host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compatibility {
    /// Who the processor says it is.
    pub identity: Identity,
    /// What it turned out to have, of everything in [`Feature::ALL`].
    pub detected: Vec<Feature>,
    /// What this build needs. Normally a copy of [`REQUIRED`].
    pub required: Vec<Feature>,
}

impl Compatibility {
    /// What is required and absent, in [`Feature::ALL`] order.
    ///
    /// Ordered rather than in whatever order the requirement was written, so two
    /// machines missing the same things produce the same sentence.
    #[must_use]
    pub fn missing(&self) -> Vec<Feature> {
        Feature::ALL
            .iter()
            .copied()
            .filter(|f| self.required.contains(f) && !self.detected.contains(f))
            .collect()
    }

    /// Whether this build can run here.
    #[must_use]
    pub fn is_compatible(&self) -> bool {
        self.missing().is_empty()
    }

    /// One line describing the processor, for a log that is read when nothing is wrong.
    #[must_use]
    pub fn summary(&self) -> String {
        let have: Vec<&str> = self.detected.iter().map(|f| f.name()).collect();
        let extensions = if have.is_empty() {
            "x86-64 baseline only".to_owned()
        } else {
            have.join(" ")
        };
        format!("{}; {extensions}", self.identity)
    }

    /// The refusal, or `None` when the processor is fine.
    ///
    /// Names the remedy, because a diagnostic that stops at "unsupported" has
    /// reproduced the problem it was written to prevent. There are only two honest
    /// remedies for a missing instruction and this gives both: different hardware, or
    /// a build that targets this hardware.
    #[must_use]
    pub fn refusal(&self) -> Option<String> {
        let missing = self.missing();
        if missing.is_empty() {
            return None;
        }
        let mut message = format!(
            "this processor cannot run this build of MediaLith\n  processor: {}\n",
            self.identity
        );
        for feature in &missing {
            use fmt::Write as _;
            let _ = writeln!(
                message,
                "  missing:   {} (first seen in {})",
                feature.name(),
                feature.first_seen_in()
            );
        }
        message.push_str(
            "  remedy:    run MediaLith on a processor that has the extensions listed \
             above, or rebuild the image for this one — buildroot/configs/\
             plexos_x86_64_defconfig selects the CPU baseline, and \
             plexos_sys::cpu::REQUIRED is what declares it here\n",
        );
        Some(message)
    }
}

/// Decides compatibility from stated inputs. Pure, and the whole of the policy.
///
/// Every test in this module goes through here, which is why none of them depend on
/// what the machine running the tests happens to support.
#[must_use]
pub fn evaluate(identity: Identity, detected: &[Feature], required: &[Feature]) -> Compatibility {
    Compatibility {
        identity,
        detected: detected.to_vec(),
        required: required.to_vec(),
    }
}

/// Asks this processor what it is and what it has, and judges it against [`REQUIRED`].
///
/// Safe to call arbitrarily early: it executes no program, opens no file and mounts
/// nothing. That matters more than it sounds — the whole point is to answer before the
/// first external binary runs, and on a machine that cannot run that binary there is
/// no second chance to say why.
#[must_use]
pub fn detect() -> Compatibility {
    evaluate(identity(), &detected_features(), REQUIRED)
}

/// Everything in [`Feature::ALL`] that this processor actually has.
///
/// `is_x86_feature_detected!` is `std`'s own CPUID wrapper and needs no `unsafe`, so
/// all but one of these are safe. `LAHF`/`SAHF` is the exception: this compiler does
/// not accept it as a feature name, so it is read out of CPUID by hand below.
#[must_use]
fn detected_features() -> Vec<Feature> {
    let mut found = Vec::new();
    let mut note = |yes: bool, feature: Feature| {
        if yes {
            found.push(feature);
        }
    };
    note(is_x86_feature_detected!("sse3"), Feature::Sse3);
    note(is_x86_feature_detected!("ssse3"), Feature::Ssse3);
    note(is_x86_feature_detected!("sse4.1"), Feature::Sse41);
    note(is_x86_feature_detected!("sse4.2"), Feature::Sse42);
    note(is_x86_feature_detected!("popcnt"), Feature::Popcnt);
    note(is_x86_feature_detected!("cmpxchg16b"), Feature::Cmpxchg16b);
    note(lahf_sahf(), Feature::LahfSahf);
    note(is_x86_feature_detected!("avx"), Feature::Avx);
    note(is_x86_feature_detected!("avx2"), Feature::Avx2);
    note(is_x86_feature_detected!("bmi1"), Feature::Bmi1);
    note(is_x86_feature_detected!("bmi2"), Feature::Bmi2);
    note(is_x86_feature_detected!("fma"), Feature::Fma);
    found.sort_unstable();
    found
}

/// One CPUID leaf.
///
/// No `unsafe`, in the one crate that is allowed it. `__cpuid` is a safe function on
/// `x86_64`: the instruction is unprivileged, present on every processor that can
/// execute this code — it predates x86-64 by a decade — reads no memory and has no
/// effect beyond the four registers it returns by value. An out-of-range leaf is
/// defined behaviour rather than undefined, which is why the signature does not need
/// to promise anything; the callers below still check the maximum leaf first, because
/// what such a leaf *returns* is unspecified and would be read as data.
fn cpuid(leaf: u32) -> std::arch::x86_64::CpuidResult {
    std::arch::x86_64::__cpuid(leaf)
}

/// Whether `LAHF`/`SAHF` work in 64-bit mode: CPUID leaf `0x8000_0001`, ECX bit 0.
fn lahf_sahf() -> bool {
    if cpuid(0x8000_0000).eax < 0x8000_0001 {
        return false;
    }
    cpuid(0x8000_0001).ecx & 1 != 0
}

/// Reads vendor, family, model, stepping and brand string out of CPUID.
#[must_use]
fn identity() -> Identity {
    let zero = cpuid(0);
    let mut vendor = Vec::with_capacity(12);
    for word in [zero.ebx, zero.edx, zero.ecx] {
        vendor.extend_from_slice(&word.to_le_bytes());
    }
    let vendor = String::from_utf8_lossy(&vendor).trim().to_owned();

    let one = cpuid(1);
    let base_family = (one.eax >> 8) & 0xf;
    let base_model = (one.eax >> 4) & 0xf;
    // The extended fields are not spare bits: Intel and AMD both moved past family 6
    // by adding to them rather than by widening the base field, so a reader that
    // ignores them reports every modern processor as one of a handful of old ones.
    let family = if base_family == 0xf {
        base_family + ((one.eax >> 20) & 0xff)
    } else {
        base_family
    };
    let model = if base_family == 0x6 || base_family == 0xf {
        base_model + (((one.eax >> 16) & 0xf) << 4)
    } else {
        base_model
    };

    let mut brand = Vec::new();
    if cpuid(0x8000_0000).eax >= 0x8000_0004 {
        for leaf in [0x8000_0002_u32, 0x8000_0003, 0x8000_0004] {
            let part = cpuid(leaf);
            for word in [part.eax, part.ebx, part.ecx, part.edx] {
                brand.extend_from_slice(&word.to_le_bytes());
            }
        }
    }
    let brand = String::from_utf8_lossy(&brand)
        .trim_matches(char::from(0))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    Identity {
        vendor,
        family,
        model,
        stepping: one.eax & 0xf,
        brand,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A processor to test against, so no test depends on the one running it.
    fn a_processor() -> Identity {
        Identity {
            vendor: "GenuineIntel".to_owned(),
            family: 6,
            model: 15,
            stepping: 11,
            brand: "Intel(R) Core(TM)2 CPU".to_owned(),
        }
    }

    /// Everything an Opteron_G1-class part has, of the set this module knows.
    ///
    /// Taken from running the extensions one instruction at a time under that CPU
    /// model rather than from a datasheet, which is the same rule the rest of this
    /// repository applies to `CONFIG_*` symbols and PCI IDs.
    const EARLY_AMD64: &[Feature] = &[Feature::Sse3];

    /// And a Nehalem, which is exactly the old accidental floor.
    const NEHALEM: &[Feature] = &[
        Feature::Sse3,
        Feature::Ssse3,
        Feature::Sse41,
        Feature::Sse42,
        Feature::Popcnt,
        Feature::Cmpxchg16b,
        Feature::LahfSahf,
    ];

    #[test]
    fn medialith_requires_nothing_above_the_x86_64_baseline() {
        // The assertion is the finding. If somebody adds a requirement, this test
        // fails and the failure is the review: it should only pass again once
        // something in the image genuinely needs the extension on a machine, and the
        // documented CPU contract has been changed to say so.
        assert!(
            REQUIRED.is_empty(),
            "REQUIRED gained {REQUIRED:?}. MediaLith's CPU contract is the x86-64 \
             baseline; raising it makes every machine below the new floor refuse to \
             boot, so it needs a measurement and a documentation change, not an edit \
             here."
        );
    }

    #[test]
    fn the_earliest_amd64_part_is_accepted_by_the_shipped_requirement() {
        let cpu = evaluate(a_processor(), EARLY_AMD64, REQUIRED);
        assert!(cpu.is_compatible());
        assert_eq!(cpu.missing(), Vec::new());
        assert!(cpu.refusal().is_none());
    }

    #[test]
    fn a_raised_requirement_refuses_and_says_what_is_missing() {
        // The guard has nothing to reject today, so the only way to know it works is
        // to state a requirement in the test. This is what a future release raising
        // the floor would do for real.
        let cpu = evaluate(a_processor(), EARLY_AMD64, &[Feature::Sse42]);
        assert!(!cpu.is_compatible());
        assert_eq!(cpu.missing(), vec![Feature::Sse42]);

        let refusal = cpu.refusal().expect("an incompatible CPU must say so");
        assert!(refusal.contains("SSE4.2"));
        assert!(refusal.contains("Nehalem"));
        // Every diagnostic in this tree names a remedy.
        assert!(refusal.contains("remedy:"));
    }

    #[test]
    fn a_processor_that_meets_a_raised_requirement_is_accepted() {
        let cpu = evaluate(a_processor(), NEHALEM, &[Feature::Sse42, Feature::Popcnt]);
        assert!(cpu.is_compatible());
        assert!(cpu.refusal().is_none());
    }

    #[test]
    fn missing_features_are_reported_in_a_stable_order() {
        // Written back to front, so a reader of two reports from two machines is
        // comparing the same sentence rather than the order somebody typed.
        let cpu = evaluate(
            a_processor(),
            &[],
            &[Feature::Fma, Feature::Sse42, Feature::Ssse3],
        );
        assert_eq!(
            cpu.missing(),
            vec![Feature::Ssse3, Feature::Sse42, Feature::Fma]
        );
    }

    #[test]
    fn the_model_number_is_never_a_reason_to_refuse() {
        // Same features, wildly different identities. A CPU is judged by what it can
        // do; the identity exists so a person can tell which machine the report is
        // about.
        let ancient = Identity {
            vendor: "AuthenticAMD".to_owned(),
            family: 15,
            model: 5,
            stepping: 10,
            brand: "AMD Opteron(tm) Processor 240".to_owned(),
        };
        let modern = Identity {
            vendor: "GenuineIntel".to_owned(),
            family: 6,
            model: 183,
            stepping: 1,
            brand: "Intel(R) Core(TM) i7-14700".to_owned(),
        };
        assert_eq!(
            evaluate(ancient, EARLY_AMD64, REQUIRED).is_compatible(),
            evaluate(modern, EARLY_AMD64, REQUIRED).is_compatible()
        );
    }

    #[test]
    fn a_summary_says_what_the_processor_is_and_what_it_has() {
        let cpu = evaluate(a_processor(), NEHALEM, REQUIRED);
        let summary = cpu.summary();
        assert!(summary.contains("Intel(R) Core(TM)2 CPU"));
        assert!(summary.contains("SSE4.2"));
    }

    #[test]
    fn a_baseline_processor_is_described_rather_than_left_blank() {
        // An empty extension list is a real answer about an early AMD64 part, and
        // rendering it as nothing at all reads as a failed probe.
        let cpu = evaluate(a_processor(), &[], REQUIRED);
        assert!(cpu.summary().contains("x86-64 baseline only"));
    }

    #[test]
    fn every_feature_has_a_name_and_an_example_part() {
        // Catches the half-finished addition: a variant added to the enum and to ALL
        // but left out of one of the two match arms would otherwise only show up in a
        // diagnostic somebody reads once, on a machine that will not boot.
        for feature in Feature::ALL {
            assert!(!feature.name().is_empty());
            assert!(!feature.first_seen_in().is_empty());
        }
        assert_eq!(Feature::ALL.len(), 12);
    }

    #[test]
    fn detection_agrees_with_itself() {
        // The one test that touches the host CPU, and it deliberately asserts nothing
        // about what that CPU is — only that detection is a function rather than a
        // coin, and that whatever it found is judged by the shipped requirement.
        let first = detect();
        let second = detect();
        assert_eq!(first, second);
        assert_eq!(first.required, REQUIRED.to_vec());
        assert!(first.detected.is_sorted());
    }

    #[test]
    fn the_host_processor_can_be_identified() {
        // Any x86-64 processor answers CPUID leaf 0 with a vendor string, so an empty
        // one means the reader is wrong rather than the machine.
        let cpu = detect();
        assert!(
            !cpu.identity.vendor.is_empty(),
            "CPUID leaf 0 returned no vendor string"
        );
    }
}
