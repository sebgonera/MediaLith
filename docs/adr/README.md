# Architecture Decision Records

Each ADR records one decision, the alternatives considered, and the consequences we
accept. An ADR is never edited after acceptance — it is superseded by a new one.

These ten cover the decisions that are expensive or impossible to reverse once an
image has shipped to a device: on-disk layout, the trust chain, the update protocol,
and the schemas that outlive any single release.

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
