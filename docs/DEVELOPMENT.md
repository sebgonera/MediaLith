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

Requirements: a real Linux system (WSL2 counts), around 30 GB of free disk, and the
usual build prerequisites. On Debian or Ubuntu:

```
sudo apt install build-essential git wget cpio rsync bc unzip file \
                 libncurses-dev flex bison python3
```

Buildroot builds its own toolchain and most host tools, so the list is short.

```
git clone --depth 1 https://github.com/buildroot/buildroot.git ../buildroot-upstream
make -C ../buildroot-upstream \
     BR2_EXTERNAL=$(pwd)/buildroot \
     O=$(pwd)/output \
     plexos_x86_64_defconfig
make -C ../buildroot-upstream O=$(pwd)/output
```

First build: two to four hours on four cores. **Every build after that is incremental
and takes minutes** — which is the whole reason to build locally during bring-up
rather than in CI.

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

Once `post-image.sh` exists and a build succeeds, the output is a raw disk image
containing the full six-partition layout with slot A populated. Write it to a USB
stick and boot from it — no installer is involved, and nothing on the machine's
internal disk is touched.

```
sudo dd if=output/images/plexos.img of=/dev/sdX bs=4M status=progress conv=fsync
```

**Turn Secure Boot off in firmware first.** Development images are self-signed, and
enrolling a key is a decision that has not been made yet.

Expect a shell, not a media server. First boot proves the kernel, dm-verity, the
partition layout, and the mount sequence. Plex needs `plexosd`, app image mounting,
and provisioning, all of which come later.
