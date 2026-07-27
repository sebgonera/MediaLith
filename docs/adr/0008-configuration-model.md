# ADR-0008: Declarative, versioned configuration

**Status:** Accepted
**Date:** 2026-07-27

## Context

Configuration is a schema that ships to devices, and it ages exactly like the update
manifest does: a v0.1 device will meet a config file written for a later release, a
later release will meet config files written years earlier, and neither may
misinterpret the other.

On an immutable system there is a second question that traditional distributions never
have to answer: where does configuration live when `/etc` is an overlay over a
read-only image, and what happens to it on rollback?

## Decision

A single file, `/etc/plexos/config.toml`, on the persistent `/etc` overlay.

```toml
schema_version = 1

[system]
hostname = "plexos"
timezone = "Europe/Warsaw"

[updates]
channel = "stable"
automatic = true
window = "03:00-05:00"

[plex]
media = ["/var/media/movies", "/var/media/tv"]
transcode_dir = "/var/cache/plex-transcode"

[shares.smb]
enabled = true
```

**Declarative and reconciled.** `plexosd` reads the file and drives system state to
match. It never writes the file back — user comments and formatting survive, and
there is exactly one source of truth. Runtime changes made through the web UI are
written as a patch to this file and then reconciled through the same path, so the API
and the file cannot diverge.

**`schema_version` is mandatory and is parsed first.** A file with a higher version
than the running release is refused with a message naming both versions. A missing
`schema_version` is an error, not a default.

**Unknown keys within a known schema version are rejected.** On an appliance a typo
that is silently ignored produces a system that boots, reports itself healthy, and
does not do what the user asked — the worst possible failure. `transcod_dir` must be
an error at startup.

**Every value has a working default.** A config file containing only `schema_version`
must produce a functioning Plex server. Configuration expresses deviation from sane
behaviour, not the minimum required to boot.

**Migration is explicit and one-way.** A schema bump ships a migration that rewrites
the file, keeping a timestamped backup. Migrations are code, tested against fixture
files from every previously released version.

## Alternatives considered

**YAML.** Widely used for exactly this. Rejected on TOML's clearer failure modes:
significant whitespace, the Norway problem, and multiple ways to express the same
document are all liabilities in a file a non-expert edits over SSH at midnight.

**A database with the file generated from it.** What many appliances do. Rejected:
the file stops being the source of truth, and inspecting or version-controlling
configuration becomes impossible without the running system.

**Ignore unknown keys, log a warning.** The conventional choice, and it is what makes
config typos so painful to diagnose. Rejected deliberately. Additive evolution is
handled through `schema_version` and defaults, not through silent tolerance.

**Configuration only through the web UI, with no user-editable file.** Rejected:
no version control, no reproducible setup, no support workflow that starts with "send
me your config".

## Consequences

- Rejecting unknown keys means a config file from a *newer* release is unusable on an
  older one. Combined with rollback not reverting `/var` (ADR-0005, ADR-0009), a
  rollback after a schema migration must find a config it can still parse — so
  migrations keep the pre-migration file, and `plexos-init` restores it when the
  running release is older than the file's schema version.
- Third-party tooling that appends unrecognised keys will break the system. Acceptable:
  there is no third-party tooling, and the appliance is not an extension point.
- Every schema version ever released needs a fixture file kept in the repository
  forever. That is the cost of being able to migrate, and it is cheap.
- The reconciliation loop must be idempotent and safe to run at any time, since it runs
  at boot, on file change, and on every API write.
