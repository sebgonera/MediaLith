# CPU baseline: what was measured, and how

The evidence behind moving MediaLith's Buildroot userspace from `BR2_x86_corei7` to
`BR2_x86_x86_64`. Kept because the conclusion — "MediaLith needs nothing above the
x86-64 baseline" — is the kind of claim that decays silently, and because most of the
work here was distinguishing what a *compiler is permitted to emit* from what a *binary
contains* from what a *machine actually executes*. Those are three different questions
and only the third one decides whether an appliance boots.

Evidence is labelled: STATIC (read from a source or artefact), QEMU USER (a component
run under emulation), QEMU SYSTEM (the real image booted under QEMU + OVMF). Nothing
here is physical-hardware evidence; no processor older than the reference laptop's
Core i5-8265U has run this image in silicon.

Reproduce with `tools/cpu-component-matrix.sh`, `tools/cpu-boot-matrix.sh` and
`tools/cpu-baseline-bench.sh`. `post-image-test.sh` stage 8 is the standing check.


Host: Intel Core i7-14700, 28 threads, 30 GiB RAM, Ubuntu, /dev/kvm present,
QEMU 10.2.1 (system) + OVMF, qemu-x86_64-static 7.2.22 (Debian deb12 static build,
extracted from a .deb without root; qemu-user is not installable here).

Start state: branch feature/hw-compat-phase1, HEAD c53ab8c, clean tree (only untracked
output/). Buildroot 2026.02.3 (tag confirmed in ~/buildroot-upstream, BR2_VERSION).

## 1. Old contract (control sample: output-corei7)

Effective config (output-corei7/.config), generated from the unmodified defconfig:

    BR2_x86_64=y
    BR2_x86_corei7=y
    BR2_GCC_TARGET_ARCH="corei7"
    BR2_X86_CPU_HAS_{MMX,SSE,SSE2,SSE3,SSSE3,SSE4,SSE42}=y

Actual cross compiler built by that tree:

    x86_64-buildroot-linux-gnu-gcc (Buildroot 2026.02.3) 14.3.0
    --with-arch=corei7
    default -march=corei7, -mtune=generic     (Buildroot sets --with-arch, not --with-tune)

ISA the compiler is PERMITTED to emit, from that compiler's own -Q --help=target:

    enabled : MMX SSE SSE2 SSE3 SSSE3 SSE4 SSE4.1 SSE4.2 POPCNT CX16 SAHF
    disabled: AVX AVX2 BMI1 BMI2 FMA ABM LZCNT

ABI: unchanged. Same x86-64 System V LP64 ABI, SSE2 for floating point, in both
targets. -march/-mtune do not move the ABI.

corei7 == nehalem in permitted ISA (identical macro sets). Buildroot's own
arch/Config.in.x86 marks BR2_x86_corei7 deprecated and points at BR2_x86_nehalem.

### "Permitted" is not "contains"

A small test program compiled -march=corei7 emitted nothing above baseline in main()
and ran on Opteron_G1. Static glibc in the same binary DID contain pshufb, but reached
it only through IFUNC/CPUID dispatch. So the floor is only real for binaries that
actually contain such instructions -- which is why the artefacts below matter.

**Corrected later, in section 5**: the first conclusion drawn from this was that a
static scan of busybox would be sound, because busybox "has no dispatch". That is
false -- busybox ships CPUID-dispatched SHA assembly of its own -- and the generic
build is what proved it. A static scan cannot distinguish a floor from a guarded fast
path in *any* binary. Only execution can.

### What the corei7 artefacts actually contain  [STATIC]

    busybox : palignr x12 (SSSE3), pshufb x8 (SSSE3), ptest x5 (SSE4.1), popcnt x3
    gpgv    : ptest x8 (SSE4.1), pshufb x8 (SSSE3), pcmpgtq x8 (SSE4.2)

### And what they do when executed  [QEMU USER]

    binary    Opteron_G1  Conroe   Penryn   Nehalem  Haswell
    busybox   SIGILL      SIGILL   SIGILL   runs     runs
    gpgv      SIGILL      SIGILL   SIGILL   runs     runs

Full component matrix over the corei7 tree  [QEMU USER]:

    binary          Opt_G1  Opt_G2  Opt_G3  Conroe  Penryn  Nehalem     SandyB      Haswell
    busybox         SIGILL  SIGILL  SIGILL  SIGILL  SIGILL  runs        runs        runs
    curl            SIGILL  SIGILL  SIGILL  SIGILL  SIGILL  runs        runs        runs
    gpgv            SIGILL  SIGILL  SIGILL  SIGILL  SIGILL  runs        runs        runs
    veritysetup     SIGILL  SIGILL  SIGILL  SIGILL  SIGILL  runs        runs        runs
    ip              SIGILL  SIGILL  SIGILL  SIGILL  SIGILL  runs(rc=1)  runs(rc=1)  runs(rc=1)
    wpa_supplicant  SIGILL  SIGILL  SIGILL  SIGILL  SIGILL  runs        runs        runs

(`ip -V` exits 1 by design; that is not a CPU result and is reported as itself.)

Old effective ISA floor: Nehalem / x86-64-v2 class, uniformly across the userspace.
Failure mode below it: SIGILL in the first Buildroot binary the boot executes, after
kernel and PID 1 have succeeded.

## 2. QEMU CPU model ISA map  [QEMU USER, measured by executing one instruction each]

    MODEL        sse2 sse3 ssse3 sse41 sse42 popcnt cx16 sahf avx avx2 bmi1 bmi2 fma
    Opteron_G1   yes  yes  no    no    no    no     no   no   no  no   no   no   no
    Opteron_G2   yes  yes  no    no    no    no     yes  yes  no  no   no   no   no
    Opteron_G3   yes  yes  no    no    no    yes    yes  yes  no  no   no   no   no
    Conroe       yes  yes  yes   no    no    no     no   yes  no  no   no   no   no
    Penryn       yes  yes  yes   yes   no    no     yes  yes  no  no   no   no   no
    Nehalem      yes  yes  yes   yes   yes   yes    yes  yes  no  no   no   no   no
    SandyBridge  yes  yes  yes   yes   yes   yes    yes  yes  yes no   no   no   no
    Haswell      yes  yes  yes   yes   yes   yes    yes  yes  yes yes  yes  yes  yes

Note QEMU's Conroe model reports no cx16 (a real Core 2 has it). Emulator detail,
recorded so it is not read as a MediaLith fact.

## 3. Component requirements

### Kernel  [STATIC, from the source being built]

linux 6.19.14 arch/x86/Kconfig.cpufeatures -- X86_REQUIRED_FEATURE_* for X86_64:
ALWAYS, NOPL, CX8, CMOV, CPUID, FPU, PAE, PSE, PGE, MSR, FXSR, XMM, XMM2, LM.
MOVBE is required only when MATOM is selected. Nothing above the x86-64 baseline.

### plexos-init / plexosd / plexos-gpu  [STATIC]

All three: PLEXOS_*_RUST_TARGET = x86_64-unknown-linux-gnu, built by host cargo with
RUSTFLAGS="-C target-feature=+crt-static" and no -C target-cpu anywhere in the repo
(grepped). rustc 1.94.1 --print cfg for that triple: target_feature fxsr, sse, sse2.
That is exactly x86-64 v1.

### Plex Media Server 1.43.3.10861-07dfddaeb  [QEMU USER + STATIC]

Obtained through MediaLith's own mechanism: https://plex.tv/api/downloads/5.json ->
plexmediaserver_1.43.3.10861-07dfddaeb_amd64.deb. sha1 02011c32...b9fc9 matches the
checksum the catalogue publishes.

Structure: no PT_INTERP on "Plex Media Server"; RUNPATH $ORIGIN/lib; 61 bundled
libraries including ld-musl-x86_64.so.1, libc.so (same size as the musl loader),
libgcompat.so.0, its own libc++, OpenSSL, curl, ICU, Boost, FFmpeg, libdrm.
strace -e trace=%file during startup: the only paths opened outside its own tree are
/proc/self/exe and the executable. Nothing from the host, no glibc.

Server startup (real server, MediaLith's own env vars, probed on 127.0.0.1:32400):

    Opteron_G1  SERVED  no SIGILL
    Conroe      SERVED  no SIGILL
    Nehalem     SERVED  no SIGILL
    Haswell     SERVED  no SIGILL   (TCG warnings for tsc-deadline/hle/invpcid/rtm,
                                     which are emulator limits, not MediaLith)

### Plex Transcoder  [QEMU USER]

Software encoders available are mjpeg/png/rawvideo only (h264/hevc are nvenc/vaapi).
rawvideo -> mjpeg encode and mjpeg decode, all four models: rc=0, no SIGILL, and
byte-identical output (831088 bytes) on every model.

## 4. Build anomalies on this host (recorded, not hidden)

Two transient corruptions during the corei7 build, both of which FAILED LOUDLY rather
than producing bad output:

1. host-util-linux: cc1 aborted with "*** stack smashing detected ***" compiling
   la-nilfs.lo. Recompiling the same object succeeded immediately.
2. host-libopenssl: crypto/aes/aesni-xts-avx512.s contained "cmovc107431620712928"
   where "cmovcq" belonged. Deleting and regenerating the .s produced correct output.

Parallelism was reduced from -j28 to -j16 afterwards. Worth stating in the report
because two independent corruptions in one build is not normal; both were compile/
assemble errors rather than silent miscompilation.

## 5. Generic tree (output-generic) — the experiment

Configured from scratch after the defconfig change; full clean build, 1 attempt.

    BR2_x86_64=y  BR2_x86_x86_64=y  BR2_GCC_TARGET_ARCH="x86-64"
    BR2_X86_CPU_HAS_{MMX,SSE,SSE2}=y   and nothing else

Cross compiler actually built: x86_64-buildroot-linux-gnu-gcc 14.3.0,
--with-arch=x86-64, default -march=x86-64 -mtune=generic.

### Component matrix  [QEMU USER] — every binary, every model

    binary          Opt_G1 Opt_G2 Opt_G3 Conroe Penryn Nehalem SandyB Haswell
    busybox         runs   runs   runs   runs   runs   runs    runs   runs
    curl            runs   runs   runs   runs   runs   runs    runs   runs
    gpgv            runs   runs   runs   runs   runs   runs    runs   runs
    veritysetup     runs   runs   runs   runs   runs   runs    runs   runs
    ip              rc=1   rc=1   rc=1   rc=1   rc=1   rc=1    rc=1   rc=1   (by design)
    wpa_supplicant  runs   runs   runs   runs   runs   runs    runs   runs

### Instructions still present, and why that is not a floor  [STATIC + QEMU USER]

    binary          corei7                        generic
    busybox         palignr popcnt pshufb         palignr pshufb
    curl            (none)                        (none)
    gpgv            pcmpgtq pshufb ptest          (none)
    veritysetup     (none)                        (none)
    mkfs.erofs      ptest                         (none)
    ip              palignr popcnt pshufb         palignr pshufb
    wpa_supplicant  palignr popcnt pshufb ptest   (none)

busybox and ip still carry SSSE3 after the change. That is NOT a residual floor: they
run on Opteron_G1. busybox ships hand-written SHA-1/SHA-256 assembly
(libbb/hash_sha*_hwaccel_x86-64.S, plus sha256rnds2 = SHA-NI) reached only through
get_shaNI(), which asks CPUID first -- the same pattern as glibc's IFUNC.

**This invalidated the first version of post-image-test stage 8**, which grepped busybox
for post-baseline mnemonics. It would have failed on a correct build. Replaced with
execution under qemu-user on Opteron_G1.

### post-image-test.sh

    generic : 130 passed, 0 failed, 1 skipped
    corei7  : 118 passed, 12 failed, 1 skipped   (all 12 are stage 8, correctly)

The single skip in both is "signatures on the ESP -- PLEXOS_SB_KEY is unset, so this
build is deliberately unsigned". Classification: deliberate configuration, not a test
design issue and not missing coverage.

## 6. Full image boot matrix  [QEMU SYSTEM + OVMF, TCG]

TCG and not KVM, deliberately: KVM masks CPUID but does not remove the instruction from
the host silicon, so a corei7 binary runs fine on a "Conroe" KVM guest. Only TCG traps.

    image     CPU          OVMF kernel PID1/verity console  panic sigill
    corei7    Opteron_G1   yes  yes    yes         NO       no    62 (ip, sh)
    corei7    Conroe       yes  yes    yes         NO       no    62 (ip, sh)
    corei7    Nehalem      yes  yes    yes         yes      no    0
    corei7    Haswell      yes  yes    yes         yes      no    0
    generic   Opteron_G1   yes  yes    yes         yes      no    0
    generic   Conroe       yes  yes    yes         yes      no    0
    generic   Nehalem      yes  yes    yes         yes      no    0
    generic   Haswell      yes  yes    yes         yes      no    0

"console served" means an HTTPS answer from /api/status reached the host over the guest's
own DHCP lease -- which requires plexosd to have run `ip` and `udhcpc`, both Buildroot
binaries. It is the strongest single signal in the table.

Health on generic/Opteron_G1: var-writable pass, usr-verified pass
(/dev/mapper/plexos-usr mounted read-only), plex-http not-applicable (not provisioned).

### What the corei7 failure actually looks like

Serial (Conroe): `traps: ip[102] trap invalid opcode ... in ld-linux-x86-64.so.2`,
repeating for `ip` (56) and `sh` (6). The fault is inside the dynamic loader, so the
program never starts. No panic, no "Attempted to kill init", QEMU alive throughout.

The screen shows a fully rendered MediaLith welcome screen with a recovery device code
to write down, stuck on "Waiting for a network address...", and underneath:

    plexos-init: the console shell started as pid 196
    plexos-init: the console shell: pid 196 was killed by signal 4; starting another in 30s

Signal 4 is SIGILL. The appliance looks like a healthy new machine awaiting setup.

Note the limit this exposes in the CPU guard: plexos-init is generic, so it runs
happily on that Conroe and cannot detect that the Buildroot binaries around it are too
new. Only a populated REQUIRED would turn it into a sentence naming SSE4.2.

## 7. Performance  [native, both trees through their own loaders, interleaved, median of 7]

    case                        corei7      generic     delta
    sha256sum 512 MiB           288 ms      287 ms      -0.3%
    mkfs.erofs 200 MiB          44015 ms    44031 ms    +0.0%
    busybox spawn x400          953 ms      963 ms      +1.0%
    gpgv startup                11 ms       12 ms       +9.1%   (1 ms, timer granularity)
    veritysetup startup         16 ms       15 ms       -6.2%   (1 ms, opposite direction)

No material appliance-level regression. The two startup cases differ by one millisecond
and disagree in sign.

Three defects in the first version of this benchmark, all of which flattered the result
and all now fixed: it timed mkfs.erofs at a path that does not exist (exit 127 in 12 ms,
reported as a timing); it measured tree A to completion before tree B, so A paid every
cold-cache cost (busybox appeared "55% faster"); and it was started while the boot
matrix was still using the CPU.

## 8. Plex in the real image  [QEMU SYSTEM + OVMF, TCG]

Provisioned once on the oldest model and the resulting disk then booted on the others,
so /var carries an app image built on an Opteron_G1-class CPU.

On Opteron_G1, driven from the host through the appliance's own console API, with the
device token read off the appliance's own screen via a QMP screendump
(`PD1K-1C5X-ZF2D-6096`):

    downloading -> downloaded 73 MB
    checking the download against the catalogue's checksum
    checking Plex's signature -> signature good, signer Plex Inc.
    all 4 members match the signed manifest
    building the app image for 1.43.3.10861-07dfddaeb
    joined /sys/fs/cgroup/plex -> Landlock applied -> running as 900:900

    plex-http    pass    answering on loopback
    installed: True   running: True   version 1.43.3.10861-07dfddaeb

Same disk booted on the rest:

    cpu=Conroe   console=yes plex_http=yes sigill=0
    cpu=Nehalem  console=yes plex_http=yes sigill=0
    cpu=Haswell  console=yes plex_http=yes sigill=0

### A false alarm caused by the operator, recorded because it nearly became a finding

The first check on the Opteron_G1 guest showed Plex crash-looping: 163 log lines of
"joined cgroup / Landlock applied / running as 900:900 / Critical: libusb_init failed",
repeating. That was not the CPU. `POST /api/provision` returns `{"started":true}` rather
than status, so polling it with POST *re-triggers provisioning* -- it was posted seven
times while the build was in progress, which is exactly the documented trap about Plex's
files not surviving being interrupted mid-write. One clean reboot on the same disk
brought Plex up first try with a four-line log. **The status route is GET.**

"Critical: libusb_init failed" is benign -- Plex saying there is no USB tuner -- and
appears on the successful boots too.

## 9. Workspace tests

cargo fmt --all --check: clean. cargo clippy --workspace --all-targets -D warnings: clean.

cargo test --workspace: one run failed with the project's known unreproduced flake,
`plexos-plex execute::tests::a_mkfs_without_the_compressor_...`, and six consecutive
runs afterwards passed. Two new data points for the trap list:

* it happened under heavy load (a TCG guest was saturating the machine), which fits the
  existing note that it failed on a twelve-core host and passed on a two-core one;
* the test's scratch path is `std::env::temp_dir().join("plexos-mkfs-capability")` --
  fixed, with no test name in it, against the rule this repository already states -- and
  it *executes* the file it has just written, which is the ETXTBSY shape the trap list
  suspects.

Deliberately not fixed here: the repository's own rule is that a speculative fix to a
test that cannot be made to fail on demand is a change nobody can check, and six clean
runs did not reproduce it. Recorded instead.
