# Authentication

Spotify uses Silicon Accounts for Carbon and Silicon identity. Spotify's own music account is a separate sign-in handled by spotify_player.

## Sign in to Silicon Accounts

Install Silicon Accounts from [Silicon Apps](https://apps.teamofsilicons.com). Sign in there as your Carbon (`c:id`) or Silicon (`si:id`), then request a one-use token for Spotify:

```sh
spotify accounts --json
silicon-accounts login --app spotify --json | jq -r .slt | spotify login --token-file -
spotify login status --json
```

The SLT is valid for two minutes and only for this app. The backend exchanges it using the app secret; the CLI never receives that secret or your Silicon's STK. Every session belongs to one account's immutable UUID. Public `c:id` and `si:id` handles are labels and may change.

Sessions are saved privately in the selected Silicon home and reused across CLI invocations. Access tokens rotate using the saved refresh token. Refresh is serialized with the daemon and retried with the same idempotency key after a lost response. A network outage keeps the session saved. Expiry, revocation, or explicit logout ends sign-in.

```sh
spotify login contexts
spotify login use si:planner
spotify logout
```

Changing a backend requires a session for that backend. Changing the selected account does not move existing triggers to another account.

## Browser sign-in

Use **Sign in** on [spotify.unlikefraction.com](https://spotify.unlikefraction.com). The browser opens Silicon Accounts and returns using an authorization code bound to this browser with state and PKCE. The backend exchanges the code; no application secret enters the browser.

The browser saves its own session across reloads and restarts, refreshes while it remains valid, and keeps it through temporary network failures. **Sign out** revokes that browser session. Browser and CLI sessions are independent, so signing into one does not overwrite the other.

## Spotify music sign-in

```sh
spotify auth login
spotify auth status
```

This opens Spotify's own authorization flow for spotify_player. It enables Web API search, playlists, lyrics, devices, and playback while keeping Spotify.app in the background. Its cache belongs to the Mac user. AppleScript playback needs Spotify.app but not a Silicon Accounts login.

## Ting notifications

Ting requests use Silicon Accounts App verification and User verification, bound to the signed-in account. See [Ting integration](ting.md). Local triggers can be used without remote delivery.
