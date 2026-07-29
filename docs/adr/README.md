# Architecture Decision Records

Each ADR records one decision, the alternatives considered, and the consequences we
accept. An ADR is never edited after acceptance — it is superseded by a new one.

These cover the decisions that are expensive or impossible to reverse once an image
has shipped to a device: on-disk layout, the trust chain, the update protocol, and the
schemas that outlive any single release. ADR-0011, ADR-0012 and ADR-0013 are the
exceptions — all three are entirely revisable. ADR-0011 is recorded because it
deliberately suspends a workspace-wide rule; ADR-0012 because it puts an unauthenticated
network service on the appliance, and the reasoning that makes that acceptable has an
expiry date; ADR-0013 because that expiry arrived, and how a device is claimed is the
kind of decision that is embarrassing to get wrong quietly.

| ADR | Decision | Status |
| --- | --- | --- |
| [0001](0001-image-based-ab-os.md) | Immutable, image-based OS with A/B slots | Accepted |
| [0002](0002-buildroot-base.md) | Buildroot as the base build system | Accepted |
| [0003](0003-partition-layout.md) | Partition layout and on-disk contract | Accepted |
| [0004](0004-verified-boot.md) | Verified boot via dm-verity and signed UKIs | Accepted |
| [0005](0005-bootloader-and-rollback.md) | systemd-boot with boot counting for rollback | Accepted |
| [0006](0006-update-manifest.md) | Update manifest format, signing, and key rotation | Accepted |
| [0007](0007-plex-app-image.md) | Plex as an independently versioned app image | Accepted |
| [0008](0008-configuration-model.md) | Declarative, versioned configuration | Accepted |
| [0009](0009-persistent-state.md) | Persistent state layout and migrations | Accepted |
| [0010](0010-plex-provisioning.md) | Plex binary provisioning and redistribution | Accepted |
| [0011](0011-syscall-boundary.md) | One crate for unsafe, and no libraries on the boot path | Accepted |
| [0012](0012-management-console.md) | A read-only status console, served by `plexosd` | Accepted |
| [0013](0013-console-authentication.md) | A device token for routes that change the machine | Accepted |
| [0014](0014-console-terminal.md) | A terminal in the console, on a trusted network | Accepted |
| [0015](0015-discrete-gpu-support.md) | Discrete GPUs, and what NVIDIA would cost | Accepted |

## Template

```markdown
# ADR-NNNN: Title

**Status:** Proposed | Accepted | Superseded by ADR-NNNN
**Date:** YYYY-MM-DD

## Context
## Decision
## Alternatives considered
## Consequences
```
