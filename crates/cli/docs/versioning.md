# Versioning

Which contracts spotify-cli versions, how each one changes, and how the CLI, the daemon and the
backend find out whether they still fit together.

## Contracts and versions

| Contract | Version | Where |
| --- | --- | --- |
| Backend HTTP API | `v1` (path prefix `/api/v1`) | `GET /api/v1/version` lists supported majors |
| Daemon protocol | `1` (field `v`) | mismatches fail with `protocol_mismatch` |
| CLI JSON output | follows the CLI's semver | fields are only added within a major |
| Ting payloads | `spotify.trigger.*` types | fields are only added; types never change meaning |
| Rust crates | semver | `silicon-spotify-client`, `silicon-spotify-cli` |

Rules:
- A **breaking change** bumps the major version: `/api/v2`, protocol `2`, crate `1.0.0 → 2.0.0`.
- Within a major, changes are additive: new fields, new optional arguments, new error codes. Do
  not branch on the absence of a field.
- The backend keeps serving the previous API major for at least 6 months after a new one ships,
  and `GET /api/v1/version` announces `deprecated`/`sunset` dates before removal.
- Deprecated CLI flags keep working for one minor release with a warning on stderr.

## Negotiation

- CLI ↔ daemon: every request carries `v` and `client_version`. A daemon older than the CLI is
  restarted automatically; an unknown op returns `unknown_op` naming the fix.
- CLI ↔ backend: `spotify doctor` reads `GET /api/v1/version` (shown in the `backend_reachable`
  check's `detail.version`). When the backend names a `min_cli`, the required check
  `cli_version_supported` fails for an older CLI, with the fix `spotify update`. Offline, or with
  a backend that sends no `min_cli`, the check is left out.

## Compatibility matrix

| CLI / daemon | Backend API | Ting types |
| --- | --- | --- |
| 0.1.x | v1 | `spotify.trigger.fired`, `spotify.trigger.expired` |

## Updates

Script installs: the daemon checks GitHub releases hourly and installs verified updates
(`auto_update`, default on). Honeycomb installs: Honeycomb's worker updates them
(`honeycomb update spotify`). `spotify update --check` reports; for a script install that
includes your `auto_update` setting (`update` in `spotify daemon status --json` shows the same).
`spotify update` installs now.
