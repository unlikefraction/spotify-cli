# Backend API

The production API is `https://backend.spotify.unlikefraction.com/api/v1`. Every authenticated operation acts as the Carbon or Silicon account verified by Silicon Accounts. The app secret stays on the server.

| Route | Purpose |
| --- | --- |
| `GET /healthz` | Service health, outside `/api/v1` |
| `GET /api/v1/accounts` | App and account discovery |
| `POST /api/v1/auth/login` | Exchange `{slt}` for an app session |
| `POST /api/v1/auth/code` | Exchange `{code, redirect_uri, code_verifier}` from browser sign-in |
| `POST /api/v1/auth/refresh` | Rotate `{refresh_token}` |
| `POST /api/v1/auth/logout` | Revoke `{token}` |
| `GET /api/v1/auth/me` | Verify the Bearer session and return the current account |

Token mutations send `Idempotency-Key`. Reuse the same key after a lost response. The backend stores encrypted replay results so it does not exchange a one-use login or refresh credential twice.

Session responses contain `access_token`, `refresh_token`, `expires_in`, `refresh_token_expires_at`, and the account identity. Never log these token fields. Browser CORS is restricted to configured frontend origins. Authorization-code redirects must be registered in Silicon Accounts and allowed by the backend.

Playback stays local to the Mac. The backend handles identity, Ting delivery, bug reports, and telemetry. Ting calls use account-bound App verification and User verification; an incompatible or unavailable provider returns a clear dependency error and retains queued work.

Errors are structured objects with a code, message, and recovery hint. An outage is not a logout. See [Authentication](auth.md) for client session handling.
