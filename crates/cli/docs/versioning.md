# Versioning and compatibility

spotify 0.4.0 uses Silicon Accounts and Silicon Apps. The CLI, client library, daemon, and backend share the workspace release version. Native package manifests match it; Peek's native app also carries an increasing bundle build number.

## Authentication

Every actor is a Carbon or Silicon account. The immutable Accounts UUID keys stored resources; its public handle is shown to people. Sessions from the previous identity system need a fresh login. Existing private data is preserved during migration rather than assigned to an unverified new account.

Browser and CLI sessions persist across restart until expiry, revocation, or explicit logout. Refresh rotates credentials safely, with durable replay handling after a lost response.

## Interfaces

The backend remains under `/api/v1`. Compatible additions keep existing fields and use new optional fields. Native clients identify themselves using `accounts --json`, and `login status --json` works when signed out. Breaking interface changes require a new version.

## Releases

Silicon Apps uses immutable development releases promoted into production. Production versions use `x.y.z`. Apps owns updates for store installations; native download users rerun the installer. See the deployment runbook in the repository for signing, packaging, and publication.
