# PlexOS → MediaLith: what the rebrand changed and what it deliberately did not

> Moved here from `rebrand.txt` at the repository root on 2026-08-14, unchanged. It is a
> record of one day's work, not a live document; the decision it describes is now
> [ADR-0022](../adr/0022-medialith-is-the-product-plexos-is-the-namespace.md).

================================================================================
PRODUCT REBRAND: PlexOS -> MediaLith
================================================================================

Date          2026-08-11
Commits       aaaf21b  Rebrand product from PlexOS to MediaLith
              db1bc9e  Say why the boot entry still carries the old name
Branch        feature/console-redesign (PR #11)
Deployed as   0.1.0.202608111733, on 192.168.2.102, slot b, gate permanent
Scope         72 files, +465 / -227

Product name    MediaLith   (exact capitalisation; not Medialith, not MEDIALITH)
Tagline         A purpose-built Linux appliance for Plex Media Server
Relationship    Independent community project. Not affiliated with, endorsed by
                or sponsored by Plex Inc. Plex and Plex Media Server are
                trademarks of Plex Inc. Stated once, in the README.


================================================================================
1. THE RULE THAT MADE THIS SAFE
================================================================================

No global search-and-replace was performed. The audit found a property of this
tree that did most of the work:

    Capitalised `PlexOS` is almost always the PRODUCT.
    Lower-case `plexos` is almost always an IDENTIFIER.

Replacing only the capitalised form therefore left every path, boot parameter,
crate name and kconfig symbol untouched by construction.

    254 capitalised occurrences   -> changed (each reviewed)
    ~1500 lower-case occurrences  -> deliberately untouched

Every scripted edit was verified afterwards by grepping for the new text and by
reading the diff. One replacement matched four of five intended sites (a printf
format string in tools/publish-update.sh differed); it was found by that check
and fixed by hand.


================================================================================
2. AUDIT: CLASSIFICATION BEFORE ANY EDIT
================================================================================

Every occurrence was traced from producer to consumer and placed in a class.

A. PUBLIC BRANDING ............................. renamed
B. DOCUMENTATION / USER-FACING TEXT ............ renamed
C. BUILD / IMAGE PRODUCT METADATA .............. renamed (artifact names, NAME,
                                                  PRETTY_NAME, FAT label)
D. INTERNAL IMPLEMENTATION IDENTIFIERS ......... retained (crates, daemon,
                                                  kconfig, build env vars)
E. ON-DISK / PROTOCOL / COMPATIBILITY .......... retained (see section 4)
F. HISTORICAL REFERENCES ....................... retained (ADRs, hardware
                                                  capture, one README note)


================================================================================
3. WHAT CHANGED
================================================================================

3.1 Web management console  (crates/plexosd/src/ui/console.html)
    - <title>MediaLith</title>
    - <h1 id="product">MediaLith</h1>   (header; normally overwritten by the
      machine's own PRETTY_NAME, see 3.3)
    - document.title per view: "MediaLith", "MediaLith - Network", ...
    - terminal window title: "MediaLith Terminal"
    - installer confirmation, disk-install warning, Plex-not-shipped note,
      the "PlexOS" tag on the disk this system runs from
    - Layout, polling, navigation, authentication and functionality: UNCHANGED.
      This was a rebrand of an approved design, not a redesign.

3.2 Operating system identity  (buildroot/board/plexos/x86_64/post-image.sh)

        NAME="MediaLith"
        ID=plexos                       <- unchanged, see 4.6
        VERSION_ID=0.1.0.202608111733
        PRETTY_NAME="MediaLith 0.1.0.202608111733"
        SORT_KEY=plexos                 <- unchanged, see 4.6

    NAME also reaches Plex: plexos-plex passes it through as
    PLEX_MEDIA_SERVER_INFO_VENDOR, so Plex clients now report the server's
    vendor as MediaLith. FriendlyName (the server's name in Plex) is the
    owner's and was not touched.

3.3 The console shows the machine's name, not its own
    The header renders PRETTY_NAME read from /etc/os-release, falling back to
    the built-in string only when there is none. This matters more after a
    rename than before it: a console must not tell somebody they are running
    MediaLith while the machine is running something older.

3.4 Build artefacts
    plexos.img       -> medialith.img
    plexos-update    -> medialith-update      (this is also the publish URL)

    Producers and all consumers moved together:
      buildroot/board/plexos/x86_64/post-image.sh        (produces both)
      buildroot/board/plexos/x86_64/post-image-test.sh
      tools/publish-update.sh, tools/sign-bundle.sh,
      tools/break-bundle.sh, tools/build-progress.sh
      docs/DEVELOPMENT.md

    NEW PUBLISH URL:  http://<build host>:8080/medialith-update

    NOTE: a stale `plexos-update` directory remains in output/images/ on the
    build host from earlier builds. Publishing that one serves an older
    release, which the appliance refuses on anti-rollback sequence - so the
    failure is safe, but the URL above is the correct one.

3.5 FAT volume label of the ESP:  PLEXOS_ESP -> MEDIALITH
    Written once, read by nothing: the ESP is always located by its GPT
    partition label (`esp`, ADR-0003). It exists for the case where the disk is
    plugged into another computer and a file manager names it. Reaches newly
    built images only - an installed machine's ESP is copied byte for byte.

3.6 Default hostname:  plexos -> medialith
    NEW INSTALLATIONS ONLY. It is a serde default, so a machine whose
    /etc/plexos/config.toml already carries a hostname never consults it.
    Verified on the appliance after the update: hostname "PlexAsus" unchanged.
    Safe to change because a hostname is not a contract here - nothing resolves
    anything by it, and the TLS certificate carries the machine's ADDRESS
    rather than its name (ADR-0014).

3.7 Documentation
    README.md               heading, opening, trademark/independence note,
                            "Previously developed under the working name
                            PlexOS", and a new section "Names that did not
                            change" listing every retained identifier
    docs/ARCHITECTURE.md    product references
    docs/DEVELOPMENT.md     product references and artifact paths
    docs/PRODUCTION-READINESS.md   the naming entry rewritten (see 6)
    CLAUDE.md               product references; open decision rewritten (see 6)
    docs/adr/README.md      one note explaining why ADRs keep the old name
    buildroot/README.md, package Config.in descriptions

3.8 Other user-facing strings
    plexos-init boot banner            "plexos-init - MediaLith PID 1"
    tools/make-secureboot-keys.sh      certificate subjects ("MediaLith
                                       Platform Key" etc.). No key has ever
                                       been enrolled, so nothing in firmware
                                       refers to the old subjects.
    crates/plexosd/src/tls.rs          certificate CommonName "MediaLith
                                       console". SAFE: the fingerprint reported
                                       at /api/status and printed on the
                                       attached screen is of the KEY, not the
                                       certificate, so no pinned fingerprint
                                       changed.
    crates/plexos-types/Cargo.toml     crate description


================================================================================
4. WHAT DID NOT CHANGE, AND WHAT EACH RENAME WOULD HAVE BROKEN
================================================================================

These are contracts with disks and with releases already in the field. Each was
traced from producer to consumer before the decision.

4.1 `PRODUCT = "plexos"`   (crates/plexos-types/src/version.rs)

    THE ONE THAT WOULD HAVE MADE THE REBRAND UNDELIVERABLE.

    plexos-update/src/plan.rs refuses any manifest whose `product` differs:
    "this update is for X and this appliance is Y". Every appliance in the
    world runs a build compiled with `plexos` here. A MediaLith bundle claiming
    `medialith` would be REFUSED BY EVERY INSTALLED MACHINE, leaving
    reinstallation as the only route - and a reinstall gives the machine a
    fresh /var: new device token, new TLS identity, Plex unprovisioned.

    The check is doing its job in that scenario; the mistake would be ours.
    This string identifies the product line whose updates are interchangeable,
    and a rebrand does not change which images are interchangeable.

    Now pinned by a test, `the_update_product_identifier_is_still_the_legacy_one`,
    carrying that reasoning - because a comment is what the next rename deletes.

4.2 `/var/lib/plexos/**`   (crates/plexos-types/src/paths.rs)

    /var is the one surface an update does not replace and a ROLLBACK DOES NOT
    REVERT. It holds the device token, the TLS key, the anti-rollback floor
    (accepted_sequence), the revocation list, the rollback record, the Plex app
    images and STATE_VERSION.

    ADR-0005 assumes a release can fail its health gate and hand the machine
    back to the release before it - including one published before the rename,
    which must still find its own state where it left it. Renaming this is not
    a rename but a state migration, and the migration would have to leave the
    PREVIOUS release able to read the result.

4.3 `/etc/plexos/config.toml`

    Persistent state wearing an /etc address: the /etc overlay's upper layer is
    /var/lib/plexos/etc. Renaming it would leave an installed machine's
    hostname, timezone and static addressing in a file nothing reads any more -
    the settings would silently revert to defaults, with nothing reporting it.

4.4 `plexos.slot`, `plexos.roothash`, `plexos.debug_shell`

    Kernel command-line keys. The command line lives INSIDE THE SIGNED UKI, so
    every release carries its own - including the previous release a rollback
    lands on, whose UKI was built when these were the names. A plexos-init that
    understood only `medialith.slot` would boot its own image and refuse the one
    behind it: precisely the image ADR-0005 exists to fall back to.

4.5 The `plexos-<version>.efi` boot entry prefix

    THE SUBTLE ONE. There is a producer/consumer asymmetry:

        the entry for a release is written by the release INSTALLING it
        (esp::install_entry), and read by the release BOOTING it
        (gate::decide_trial).

    So a MediaLith bundle installed by a machine still running PlexOS gets a
    `plexos-` name whatever the new build calls itself. Had the new build looked
    for `medialith-<version>`, decide_trial would have found nothing and
    returned Trial::Unknown - which correctly declines to roll back, but ALSO
    never clears the try counter. The entry then decays +2-1, +1-2, +0-3 and the
    bootloader falls back.

    Result: a completely healthy machine rolling itself back three reboots after
    a successful update, looking like a hardware fault.

    See section 7: this exact path was exercised on hardware.

4.6 `SORT_KEY=plexos` and `ID=plexos` in os-release

    SORT_KEY is what systemd-boot groups entries by before comparing versions.
    An ESP holding one UKI keyed `plexos` and another keyed `medialith` is two
    groups, and ADR-0005 depends on the newest version being chosen. How the
    bootloader orders BETWEEN groups was not established here, and the moment to
    find out is not on a machine that has just been updated.

    ID has no consumer anywhere in this tree (checked), so changing it buys
    nothing and risks the same class of surprise.

    post-image-test.sh now ASSERTS both still say `plexos`, so a later tidy-up
    cannot take them quietly.

4.7 Crate, package, daemon and build-variable names

    plexos-sys, plexos-init, plexos-plex, plexos-gpu, plexos-update,
    plexos-types, plexosd, plexos-sign, plexos-layout,
    BR2_PACKAGE_PLEXOS_*, BR2_EXTERNAL_PLEXOS_PATH, PLEXOS_VERSION,
    PLEXOS_SB_KEY, buildroot/board/plexos/, plexos_x86_64_defconfig

    A large diff, no user-visible benefit, and every build script, package
    definition and image assembly step is an opportunity to break something that
    works. Note the recorded trap: a Buildroot package's DIRECTORY NAME becomes
    its kconfig symbol, so renaming packages means renaming symbols and editing
    the defconfig.

    The daemon is "the MediaLith management daemon"; the executable is still
    `plexosd` and its log lines still say `plexosd:`, which is what a log prefix
    is for - the program identifying itself, like `sshd:`.

4.8 VERSION_ID format
    Untouched, and worth stating: the updater's anti-rollback floor is read from
    it (update::running_version_from), and systemd-boot orders entries by it.

4.9 GPT partition labels - NOT AFFECTED AT ALL
    The frozen layout uses `esp`, `usr_a`, `usr_b`, `var`. No brand name has
    ever been in them, so ADR-0003's frozen layout needed no thought.

4.10 mDNS
    Not implemented anywhere in this tree. `medialith.local` is therefore NOT
    advertised or documented, because documenting a feature that does not exist
    is worse than the missing feature.


================================================================================
5. TESTS
================================================================================

Added:
  the_page_calls_the_product_by_its_name
      asserts <title>, <h1> and the terminal window title, then that the string
      "PlexOS" appears NOWHERE in the served page - comments included, because a
      comment calling this PlexOS tells the next reader something untrue.
  the_page_says_what_it_is_told_rather_than_assuming_the_product_name
      asserts the header renders os-release's PRETTY_NAME and only falls back to
      the built-in name.
  the_update_product_identifier_is_still_the_legacy_one
      pins PRODUCT = "plexos" with the consequence spelled out (4.1).
  post-image-test.sh: four checks
      NAME is MediaLith; PRETTY_NAME is "MediaLith <version>";
      SORT_KEY is still plexos; ID is still plexos.

Changed:
  config::tests::a_minimal_config_yields_a_working_system
      expects the new default hostname. This is a product default that changed,
      not a compatibility contract that was broken.

Preserved untouched: A/B update, rollback, configuration, terminal,
authentication, installer, GPU diagnostics, Plex supervision.

Results:
  cargo fmt --check ............... clean
  cargo clippy --workspace -D warnings ... clean
  cargo test --workspace .......... 444 passed, 2 failed
      The two failures are the documented pre-existing ones:
      plex::tests::a_handle_that_has_started_nothing_reports_nothing_running
      plex::tests::an_unprovisioned_machine_is_told_where_plex_would_be_...
      Both probe 127.0.0.1:32400 and fail on any development host that is
      itself running Plex. Not related to this change.
  post-image-test.sh .............. 9 passed, 0 failed, 8 skipped
      (skipped stages need host tools not present; run on the build host too)


================================================================================
6. TWO STATEMENTS THE MECHANICAL PASS MADE FALSE
================================================================================

A blind replacement produced text that was no longer true, and both were fixed
rather than left:

  CLAUDE.md, "Open decisions":
      became  '"MediaLith" uses a third-party trademark and likely needs to
              change'
      which is false - that was the REASON for this change. Rewritten to record
      the name as decided, and to name what is still open: the internal
      namespace.

  docs/PRODUCTION-READINESS.md 1.5:
      same sentence, same fix. The entry now records that the rename happened in
      the cheapest possible form - everything a person reads, nothing on disk -
      so no machine needed migrating, and that what remains is inconsistency
      rather than a fault.

This is the general rule applied: comments and documentation must describe
reality. A comment about a literal identifier keeps that identifier.


================================================================================
7. ROLLBACK COMPATIBILITY: PROVEN, NOT ARGUED
================================================================================

The deployment was itself the compatibility test. The appliance was running
0.1.0.202608111644, whose os-release says PlexOS.

  1. It was offered the MediaLith bundle.
     -> ACCEPTED. The manifest's product field still says `plexos`, so the
        guard in plan.rs passed. (Had 4.1 been renamed, this step would have
        failed and the rebrand could not have been delivered at all.)

  2. Written to the inactive slot, boot entry installed on trial.
     -> The entry was named by the OLD release:
        plexos-0.1.0.202608111733+3.efi

  3. Restarted into MediaLith.
     -> Booted. /usr verified, health gate ran.

  4. The gate identified the entry this boot came from and cleared its counter:

        healthy; plexos-0.1.0.202608111733+2-1.efi
              -> plexos-0.1.0.202608111733.efi

     -> THE DECISIVE RESULT. The entry was written by a release calling itself
        PlexOS and recognised by a release calling itself MediaLith. Slot
        permanent. Had 4.5 been renamed, this step would have silently failed
        and the machine would have rolled itself back within three reboots.

  5. Persistent state read back:
        hostname   PlexAsus   (the machine's own, NOT overwritten by the new
                               default)
        timezone   Poland
        device token   still accepted
        TLS identity   unchanged (fingerprint is of the key)
        Plex           1.43.3.10861 installed and running

The reverse direction needs no experiment to reason about: nothing on disk
changed, so a rollback to the previous release finds byte-identical /var
contents, the same command-line keys inside its own UKI, and its own boot entry
under the same name. The only difference it would see is the string
NAME="MediaLith" in the OTHER slot's os-release, which it never reads.


================================================================================
8. REMAINING OCCURRENCES OF "PlexOS" - 54, ALL INTENTIONAL
================================================================================

The goal was zero INCORRECT occurrences, not zero occurrences.

Counted EXCLUDING this file, which necessarily says the old name throughout. To
reproduce:

    grep -ro "PlexOS" . --exclude-dir=.git --exclude-dir=target \
        --exclude=rebrand.txt | wc -l

That exclusion is not bookkeeping fussiness: this project has already been
bitten by a presence check that passed because grep found the string inside the
test asserting its absence. Grep the artefact, never the tree that also contains
everything written about it.

  47  docs/adr/*.md
      Dated records of decisions taken under that name. An ADR is never edited
      after acceptance. They also remain LITERALLY accurate, because the
      identifiers they describe (/var/lib/plexos, plexos.slot, the manifest's
      product field) were not renamed either. One note at the top of
      docs/adr/README.md explains this.

   1  tools/captures/huawei-wrt-wx9.txt
      A hardware capture taken from a real machine. Dated evidence; editing it
      would falsify a record.

   1  README.md
      "Previously developed under the working name PlexOS."

   3  crates/plexosd/src/console.rs
      The test that ASSERTS the old name is absent from the page, plus its
      explanation.

   1  crates/plexosd/src/esp.rs
      The comment explaining that an entry may have been written by a machine
      still running PlexOS - which is the whole reason the prefix was kept.

   1  CLAUDE.md
      The rewritten open decision, explaining what the name used to be.

Lower-case `plexos` remains widely, by design: it is the internal namespace
listed in section 4.


================================================================================
9. WHAT A USER SEES NOW
================================================================================

  Browser tab            MediaLith
  Console header         MediaLith 0.1.0.202608111733   (from os-release)
  Terminal window        MediaLith Terminal
  Boot banner            plexos-init - MediaLith PID 1
  Plex clients           vendor: MediaLith
  Install confirmation   "Erase /dev/nvme0n1 completely and install MediaLith
                         onto it?"
  "This boot" line       healthy; plexos-0.1.0.202608111733+2-1.efi
                                -> plexos-0.1.0.202608111733.efi

The last one is CORRECT and deliberately not rewritten: it is the literal
filename on the EFI partition, and it is on-disk evidence rather than prose.
Printing `medialith-` there would show a name that does not exist, and mounting
the partition to check would find nothing. Commit db1bc9e adds one sentence
under it explaining that the prefix is an internal name kept from before the
rename, so a reader is not left guessing whether it is a bug.


================================================================================
10. WHAT IS LEFT
================================================================================

Nothing is broken and nothing is pending for the rebrand itself. What remains is
the internal namespace (section 4), which is invisible to an owner and is a
migration to design on its own terms: a release would have to accept BOTH
spellings for long enough that no machine is left behind. It gets cheaper the
sooner it is done and more expensive the more machines exist.

Commit db1bc9e (the "This boot" explanation) is committed and pushed but is a
page change, so it reaches the appliance with the next build.
