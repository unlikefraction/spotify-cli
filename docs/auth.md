# Authentication

Two separate sign-ins, for two different things.

| | IAM login (`spotify login`) | Spotify sign-in (`spotify auth login`) |
| --- | --- | --- |
| Who you are in IAM | ✓ | |
| Needed for | triggers (Ting notifications) | Web API features: search, lyrics, playlists, library, devices, queue reads, repeat-one, likes, and every start that keeps Spotify.app in the background |
| Stored in | `$SILICON_HOME/.spotify/session.json` | spotify_player's cache (`~/.cache/spotify-player`) |
| Per | Silicon home | Mac user |

Playback control through AppleScript needs neither (without the Spotify sign-in, every start goes
through AppleScript, which brings Spotify.app forward for a moment).

## IAM login (Silicons and Carbons)

The CLI never asks for passwords, OTPs, SID or STK. It takes one short-lived token (SLT) minted by
`iam` or IAM's consent screen (https://auth.iam.teamofsilicons.com). `iam` is Silicon IAM's own
CLI, a separate program from spotify-cli: get it with `cargo install silicon-iam-cli` (crate
`silicon-iam-cli`, binary `iam`). It talks to IAM at https://backend.iam.teamofsilicons.com, and
`iam iam --json` shows where everything is. `spotify login --help`, `spotify iam` and the
Authentication lines of `spotify --help` say the same, and `spotify iam --json` names it under
`iam_cli` (`{name, description, install}`).

```sh
cargo install silicon-iam-cli           # once: the iam CLI (Silicon IAM's, separate)
spotify iam --json                      # {"app_id": "spotify", "org_id": "unlikefraction", …} (offline)
# Silicon (in a fresh SILICON_HOME, first once: iam silicon-login --sid si:<handle>)
iam silicon-login --app-id spotify --grant-org "$SILICON_ORG" --approve-scopes
# Carbon
iam login --app-id spotify --grant-org <org>
spotify login '<SLT>'
spotify login status --json
spotify logout
```

Or pipe the SLT straight in, so it never lands in shell history:

```sh
iam -o json silicon-login --app-id spotify --grant-org "$SILICON_ORG" --approve-scopes \
  | jq -r .slt | spotify login --token-file -
```

An SLT lasts about 2 minutes and works once; if it expired, was used, or was minted for another
app, `spotify login` fails with `slt_rejected`: mint a fresh one and log in right away.

**Which `iam` session mints it.** The SLT logs in whoever the minting `iam` session belongs to.
`iam` honours `SILICON_HOME` too: it keeps its own session in `$SILICON_HOME/.silicon-iam`. So
mint a Silicon's SLT with that Silicon's `SILICON_HOME`, where the Silicon has signed in to IAM
once with its own credential (`iam silicon-login --sid si:<handle>`, which asks for the Silicon's
STK); in a fresh home `iam` has no session and refuses to mint. `spotify --help`, `spotify iam`
and `spotify login --help` say the same. A Carbon operator minting from their own `iam` session
unsets `SILICON_HOME` for `iam` only; the session saved in the Silicon home is then the Carbon's,
and Tings go to the Carbon:

```sh
env -u SILICON_HOME iam -o json login --app-id spotify --grant-org <org> \
  | jq -r .slt | spotify login --token-file -
```

What happens:
1. The CLI sends the SLT to the backend (`POST /api/v1/auth/login`) with an idempotency key derived
   from the SLT, so a retry after a network error replays instead of burning the single-use token.
2. The backend exchanges it with IAM using the application secret (which never leaves the server),
   validates the session against IAM's authorization snapshot and returns the app session.
3. The backend registers you as a **Ting recipient** for this app (`subscriptions.register`), so
   triggers can notify you. `spotify ting register` repeats that step explicitly.
4. The CLI saves the session (0600) under `$SILICON_HOME/.spotify/`.

Scopes requested: `self.identity.read` (Ting checks your public id), `self.profile.read`,
`obo:ting:subscriptions.register`, `obo:ting:tings.send`. Refresh never adds scopes: if a login
predates a scope, log in again (`reconsent_required`).

`spotify login status --json` checks **live**: it refreshes a near-expiry token (rotation-safe:
the idempotency key is derived from the refresh token, and the refresh runs under an exclusive
lock shared with the daemon) and asks the backend `GET /api/v1/auth/me`. No saved session or a
revoked session prints `{"authenticated": false, …}` with exit 0; network failures are errors
(exit 5) so outages are never mistaken for logouts.

Organization: `--org`, then `SILICON_ORG`, then config `org`, then the session's organization.

## Spotify sign-in (spotify_player)

```sh
spotify auth status                             # signed in? (spotify_player's version, or what is missing)
spotify auth status --json | jq .authenticated
spotify auth login     # opens Spotify's consent page in the browser on this Mac
```

`spotify auth login` asks the daemon to start `spotify_player authenticate`, which opens Spotify's
consent page in the default browser on this Mac (its output goes to
`~/.silicon-spotify/spotify-auth.log`). A Carbon clicks **Agree** once; the page then returns to a
local address on this Mac (`127.0.0.1:8989`), and spotify_player caches and refreshes the tokens.
The sign-in is per macOS user, so every Silicon home on the Mac shares it. If commands fail with
`spotify_auth_required`, run it again. Bring your own Spotify client id: see
`spotify docs config`.

**A headless Silicon** cannot click Agree, and the consent completes only in a browser on this
Mac. Ask a Carbon at the Mac to run `spotify auth login` once (or run it yourself and ask them to
click Agree in the tab it opens), then check with `spotify auth status`. Until then playback
still works through AppleScript (Spotify.app comes forward for a moment at each start); search,
lyrics, playlists, library, devices and queue reads fail with `spotify_auth_required`.

spotify-cli also reads two things from that cache. The Spotify user id in `credentials.json` names
your Liked Songs list, so `spotify play --liked` can play the list itself. The cached access
tokens (read for each request, never kept) serve what spotify_player has no command for, or
cannot do:

- starting a song, an episode, a show or Liked Songs without bringing Spotify.app forward
  (`spotify play <track|episode|show>`, `spotify play --liked`): `GET /v1/tracks/{id}` or
  `/v1/episodes/{id}`, `GET /v1/me/tracks?limit=1` (Liked Songs' size and first song),
  `GET /v1/me/player/devices` (at every start), `GET /v1/albums/{id}/tracks` when a start is
  retried by position, `PUT /v1/me/player/play`, and `PUT /v1/me/player/shuffle` for Liked Songs
  (`spotify docs playback`: Web API first);
- searching when spotify_player's own search fails (`GET /v1/search`; the result says
  `via: web_api`);
- whether a song is in Liked Songs (`liked` in `spotify track`);
- an episode's name, show and length (`spotify queue add`, and later `spotify queue` or the
  daemon in the background when a first lookup failed).

Those requests go only to `https://api.spotify.com` (HTTPS, no redirects), with a stale token
replaced by the next cached one. Tokens are never logged or stored anywhere else, and after a 429
nothing is sent (lookups, searches and starts share one pause) until Spotify's `Retry-After` has
passed (30 s when it gives none, at most 10 minutes).

## Testing planes

IAM testing environments are isolated planes that use the same code paths as production. Select
one with the spotify-cli **test application secret** of that environment:

```sh
printf %s "$SPOTIFY_TEST_APP_SECRET" | spotify testing use --app-secret-file -
spotify login si:test-silicon            # in a testing plane an SLT may be a test public id
spotify trigger test
spotify testing exit
```

`SPOTIFY_TEST_APP_SECRET` selects a plane for one process. Test and production sessions are stored
in separate slots and never fall back to each other. The backend validates the secret with IAM
(`X-Testing-Environment-Key` → `with_testing_application`) and forwards only *Ting's* test
credentials (from the OBO proof's testing context) to Ting.
