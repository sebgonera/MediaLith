# Development

## The Rust workspace

Builds anywhere with a Rust toolchain, including the hosted Claude Code environment.
No special setup.

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Two binaries run standalone on any Linux system, which is deliberate — both had to be
usable before an image existed to run them on:

```
cargo run -p plexos-gpu                # will hardware transcoding work here?
cargo run -p plexos-init -- --dry-run  # what would PID 1 do?
```

## Building an image

This is the part with a real host requirement.

**Buildroot cannot be built in the hosted Claude Code environment.** Its proxy reaches
GitHub and nothing else, while Buildroot fetches roughly a hundred tarballs from
`ftpmirror.gnu.org`, `cdn.kernel.org`, `sources.buildroot.net` and others. The build
fails on the first package. This was tested, not assumed.

So image builds happen either on a Linux host you control, or in CI.

### On a Linux host

Requirements: a real Linux system (WSL2 counts), around 30 GB of free disk **on the
filesystem holding the output directory**, and the usual build prerequisites. On
Debian or Ubuntu:

```
sudo apt install build-essential git wget cpio rsync bc unzip file \
                 libncurses-dev flex bison python3
```

Buildroot builds its own toolchain and most host tools, so the list is short. Add
`qemu-system-x86 ovmf` to try the resulting image without hardware.

**Recent Ubuntu ships uutils coreutils**, whose `install(1)` is affected by
[uutils#12166](https://github.com/uutils/coreutils/issues/12166), and Buildroot
refuses to build against it. Its own dependency check catches this and names the fix:

```
sudo update-alternatives --install /usr/bin/install install /usr/bin/gnuinstall 100
```

Then the build itself. The upstream version is **pinned deliberately** — it carries
package versions and security patches, so tracking master is not an option:

```
git clone https://github.com/buildroot/buildroot.git ../buildroot-upstream
git -C ../buildroot-upstream checkout 2026.02.3
make -C ../buildroot-upstream \
     BR2_EXTERNAL=$(pwd)/buildroot \
     O=$(pwd)/output \
     plexos_x86_64_defconfig
make -C ../buildroot-upstream O=$(pwd)/output
```

`2026.02.3` is a `YYYY.02` release, which is the series upstream maintains long-term.
`--depth 1` is deliberately absent: a shallow clone of master gives whatever master
happens to be today, which is the opposite of a pin.

**Put `TMPDIR` on the same roomy filesystem as the output.** GCC writes temporary
assembly there, and on a machine where `/tmp` is a small tmpfs the build dies partway
through `host-gcc-initial` with `Disk quota exceeded` — a message that points at the
compiler rather than at `/tmp`:

```
export TMPDIR=$(pwd)/output/tmp && mkdir -p "$TMPDIR"
```

The output directory can live anywhere, which is the answer when the system disk is
too small. An external disk formatted ext4 works well; a network filesystem does not.
NFS was measured at 16 file creations per second against roughly 17,000 locally, and
Buildroot creates hundreds of thousands of files.

First build: two to four hours on four cores, and closer to six on two. **Every build
after that is incremental and takes minutes** — which is the whole reason to build
locally during bring-up rather than in CI.

Buildroot prints a wall of compiler invocations and nothing that answers "how much is
left", so:

```
tools/build-progress.sh /path/to/output          # one-shot
tools/build-progress.sh /path/to/output --watch  # refreshes until the build stops
```

It weights packages by rough build cost, because counting them evenly gives a bar that
reaches 80% and then sits still for two hours: `host-gcc-final` alone outweighs thirty
of the small ones. The percentage is honest about direction, not about minutes.

### Checking the defconfig without building

Worth doing after every edit. It takes seconds and catches the failure mode that
matters most:

```
make -C ../buildroot-upstream BR2_EXTERNAL=$(pwd)/buildroot O=/tmp/check \
     plexos_x86_64_defconfig
grep -c '^BR2_PACKAGE_INTEL_MEDIADRIVER=y' /tmp/check/.config
```

kconfig **silently drops** options whose dependencies are unmet. It is not an error;
you simply end up with something else. One early version of the defconfig produced a
uClibc toolchain, on which Plex Media Server cannot run at all, and nothing in the
output said so. Always check that the options you set actually survived.

## Working on this with Claude Code locally

Claude Code runs on your own machine as well as in the browser, and a local session
can do what a hosted one cannot: run Buildroot, drive QEMU, and talk to real hardware.
For the bring-up phase that is a large difference — incremental rebuilds turn a
fix-and-test cycle from hours into minutes.

On Windows, install it inside WSL2 rather than natively: Buildroot needs Linux
regardless, so the whole loop is simpler in one place.

```
# in WSL2 (Ubuntu)
npm install -g @anthropic-ai/claude-code
git clone <this repo>
cd OS
claude
```

Installation details: https://code.claude.com/docs

A fresh session picks the project up from `CLAUDE.md`, `docs/ARCHITECTURE.md`, and
`docs/adr/`. That is a large part of why the decision records were written before any
code — the design survives outside any one conversation.

Nothing about the workflow changes: same repository, same branch, same conventions.

## Trying an image

A successful build ends by running `post-image.sh`, which produces
`output/images/plexos.img`: a raw disk image with the full six-partition layout and
slot A populated.

### Under QEMU first

PlexOS boots UEFI only, so QEMU needs OVMF firmware. Give it a writable copy of the
variable store, or the boot order cannot be recorded:

```
cp /usr/share/OVMF/OVMF_VARS_4M.fd /tmp/plexos-vars.fd
qemu-system-x86_64 \
    -machine q35,accel=kvm -cpu host -m 2048 \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,file=/tmp/plexos-vars.fd \
    -drive if=virtio,format=raw,file=output/images/plexos.img \
    -nographic
```

Use `OVMF_CODE_4M.fd`, not the `.secboot.` variant: development images are unsigned
and Secure Boot firmware will refuse them. `-nographic` puts the console on the
terminal, which is where `plexos-init` reports each boot step — and on a boot that
hangs, the last line printed is the step that hung.

`accel=kvm` needs membership of the `kvm` group, and fails with `Could not access KVM
kernel module: Permission denied` without it:

```
sudo usermod -aG kvm "$USER"   # then log out and back in
```

Without KVM, substitute `accel=tcg` **and name a CPU**: `-cpu Nehalem` or better.
This is not optional. The defconfig sets `BR2_x86_corei7`, so everything Buildroot
compiles targets that instruction set, while QEMU's default `qemu64` model does not
implement it. The kernel and `plexos-init` boot fine — the former is built for generic
x86-64 and the latter by the workspace's own cargo — and then the first Buildroot-built
binary to run dies with `SIGILL`, which the console reports as
`Attempted to kill init! exitcode=0x00000004`.

`accel=tcg` is otherwise just slow: full software emulation of a boot that takes
seconds under KVM. Fine for proving the boot path, painful for anything iterative.

QEMU proves the kernel, dm-verity, the partition layout, and the mount sequence. It
proves **nothing** about QuickSync: virtio-gpu has no VA-API, so hardware transcoding
can only be tested on real hardware.

### On the reference machine

```
sudo dd if=output/images/plexos.img of=/dev/sdX bs=4M status=progress conv=fsync
```

No installer is involved, and nothing on the machine's internal disk is touched.

**Turn Secure Boot off in firmware first.** Development images are self-signed, and
enrolling a key is a decision that has not been made yet (ADR-0004).

Expect a shell, not a media server. Plex needs `plexosd`, app image mounting, and
provisioning, all of which come later.

A first image reaching that shell looks like this, and the last four lines are the
ones worth checking:

```
plexos-init: 24/24 switch_root /sysroot /usr/bin/plexos-init
plexos-init: root assembled, /usr verified, running as the service manager
plexos-init: no supervisor yet: starting a shell
~ #
```

From that shell, `/proc/mounts` should show a tmpfs `/`, `/usr` as erofs on
`/dev/mapper/plexos-usr` mounted `ro`, `/var` as XFS mounted `rw`, and `/etc` as an
overlay. `touch /usr/anything` must fail with "Read-only file system": that is
dm-verity and the read-only mount doing their job, and it is the cheapest check that
the trust chain is actually assembled rather than merely configured.

## Publishing an update

An appliance installs only what a root key it carries has vouched for (ADR-0006), so a
freshly built bundle is not yet something any machine will take. Signing is a separate
step from the build on purpose: the key must not have to be on every host with a Buildroot
tree, and a manifest has to be written exactly once, because the signature covers its
exact bytes.

### Once, per developer

```
cargo run -p plexos-update --bin plexos-sign -- root-key    ~/.plexos-keys/root-dev
cargo run -p plexos-update --bin plexos-sign -- signing-key ~/.plexos-keys/signing-dev
cargo run -p plexos-update --bin plexos-sign -- certify \
    ~/.plexos-keys/root-dev ~/.plexos-keys/signing-dev \
    plexos-signing-dev 2028-01-01T00:00:00Z > ~/.plexos-keys/signing-dev.cert
cargo run -p plexos-update --bin plexos-sign -- trust ~/.plexos-keys/root-dev plexos-root-dev
```

The last command prints a `RootKey` to paste into `ROOT_KEYS` in
`crates/plexos-update/src/trust.rs`. **An image trusts the keys it was built with**, so a
machine already in the field will not believe a root key added after it was flashed: it has
to take one more update signed by the key it already has, carrying the new one. That is
what makes rotation a thing to plan rather than a thing to do.

There is one such key today, `plexos-root-dev`, and it is marked `development: true`
because its private half is a file on a build host rather than an offline secret. The
console says so wherever it reports a signature. What it proves is "this came from that
build host", which is a large improvement on "this came from whoever answered at that
address" and is not the same as a root of trust.

### Every publish

```
tools/sign-bundle.sh output/images/plexos-update \
    ~/.plexos-keys/signing-dev ~/.plexos-keys/signing-dev.cert
tools/publish-update.sh
```

`sign-bundle.sh` reads the `update.json` the build wrote, produces `manifest.json` and
`manifest.json.sig`, and then verifies them with the same code the appliance runs — so
"will the machine take this" is answered on the build host rather than after a 74 MB
download onto a machine in another room. `publish-update.sh` refuses to serve a bundle with
no signed manifest, for the same reason.

The build stamp is load-bearing. `PLEXOS_VERSION` must end in `YYYYMMDDHHMM`, because that
number is the manifest's anti-rollback `sequence` as well as the string `systemd-boot`
orders boot entries by. `post-image.sh` defaults it from the clock, or from
`SOURCE_DATE_EPOCH` when that is set.

### Revoking a signing key

```
cargo run -p plexos-update --bin plexos-sign -- revoke \
    ~/.plexos-keys/root-dev 1 plexos-signing-dev > output/images/plexos-update/revocations.json
```

Served beside the manifest, it is picked up on the next check and stored. The counter must
increase with every list published: an appliance keeps the highest it has seen, so an older
list — genuinely root-signed, from before the revocation — un-revokes nothing.
