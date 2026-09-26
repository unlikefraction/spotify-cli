# Backend API (v1)

Origin: `https://backend.spotify.unlikefraction.com`. JSON in and out. Errors:
`{"error":{"code","message","hint","retryable","details"?}}` with an `X-Request-Id` header.
Credential-bearing responses are `Cache-Control: no-store`.

Common headers: `Idempotency-Key` (16–255 visible ASCII; required on auth mutations and reports),
`Authorization: Bearer oat_…` + `X-Org-ID: <org>` on authenticated routes,
`X-Testing-Environment-Key: <test ask_ secret>` to select an IAM testing plane,
`X-Spotify-Telemetry: on|off`, `X-Spotify-Source: cli|daemon|web`.

| Method & path | Auth | Body → response |
| --- | --- | --- |
| `GET /healthz` | — | `{"status":"ok","service":"silicon-spotify","version"}` |
| `GET /readyz` | — | `{"status":"ready"}` or 503 |
| `GET /api/v1/iam` | — | `{"app_id":"spotify","org_id":"unlikefraction","api_version":"v1","iam_url","ting_url","testing_environment_id","ting_types",…}` |
| `GET /api/v1/version` | — | `{"version","api_versions":["v1"],"min_cli"}` |
| `POST /api/v1/auth/login` | key | `{"slt"}` → session (below) + `"ting":{"subscribed","subscription_id"?,"error"?}` |
| `POST /api/v1/auth/refresh` | key | `{"refresh_token"}` → session |
| `POST /api/v1/auth/logout` | key | `{"token"}` → 204 (revokes the refresh family) |
| `GET /api/v1/auth/me` | bearer | `{"authenticated":true,"actor","org_id","membership_id","session_id","scopes","ting_ready","testing_environment_id"}` |
| `POST /api/v1/ting/subscription` | bearer | `{}` → `{"id":"sub_…","app_id","for","active":true}` |
| `POST /api/v1/tings` | bearer | `{"type","key","data","metadata"?}` → `{"id":"msg_…","created_at","key","silent","replayed"}` |
| `POST /api/v1/reports` | key, bearer optional | `{"message","pr"?,"attachments"?,"context"?}` → `{"id":"rep_…","status":"stored|filed","issue_url"?}` |
| `POST /api/v1/telemetry` | — | `{"table","events":[…]}` (1–40, ≤64 KiB) → 204 |
| `POST /webhook/` | IAM signature | IAM webhook deliveries → `{"received":true}` |

Session:

```json
{"access_token":"oat_…","refresh_token":"ort_…","token_type":"Bearer","expires_in":1800,
 "scope":"obo:ting:subscriptions.register obo:ting:tings.send self.identity.read self.profile.read",
 "actor":{"type":"silicon","public_id":"si:you"},"org_id":"unlikefraction","org_ids":["unlikefraction"],"testing_environment_id":null}
```

`POST /api/v1/tings` rules: `type` must be a registered spotify-cli type; `key` 1–200 bytes
starting with `<your public id>/`; `data` and `metadata` objects; the backend adds `org_id` and
`for` from the verified session (unknown fields such as `for` are rejected). Ting refusals pass
through with their code (`recipient_not_registered`, `not_found` for an unregistered type, …).

Refresh rotates the refresh token; retry an uncertain refresh with the *same* idempotency key
within 10 minutes (IAM replays it); a consumed token under a new key revokes the family.

## Configuration (operators)

See `.env.example`: `SPOTIFY_IAM_APP_SECRET`, `SPOTIFY_IAM_WEBHOOK_SECRET`, `SPOTIFY_IAM_URL`,
`SPOTIFY_TING_URL`, `SPOTIFY_DATABASE_PATH`, Space Station table keys, optional
`SPOTIFY_GITHUB_TOKEN` to file reports as GitHub issues. Deployment: `deploy/README.md`.
