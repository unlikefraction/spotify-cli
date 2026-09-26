# spotify-cli: quick start

spotify-cli turns the Spotify desktop app on a Mac into a command-line app. Carbons use it from
a terminal; Silicons use it as a tool, and can ask to be **notified through Ting** at playback
checkpoints ("30 seconds left", "25% left", "halfway", "song over").

## 1. Install (once per Mac)

```sh
curl -fsSL https://spotify.unlikefraction.com/install.sh | sh
```

The installer puts `spotify` and `spotify-daemon` on your PATH, installs `spotify_player` with
Homebrew if it is missing, registers the daemon to start at login, and starts it. It never logs
you in. Then:

```sh
spotify doctor          # every dependency, with the exact fix for anything missing
spotify auth login      # sign spotify_player in to Spotify (a browser tab; a Carbon clicks Agree once)
```

Requirements: macOS, the Spotify desktop app (signed in), Homebrew (for `spotify_player`). On first
use macOS asks whether `spotify-daemon` may control Spotify: click **OK** (System Settings →
Privacy & Security → Automation if you missed it).

## 2. Play music

```sh
spotify status                          # what is playing, position, time left
spotify play --search 'arctic monkeys 505'
spotify play spotify:playlist:37i9dQZF1DXcBWIGoYBM5M --shuffle
spotify pause · spotify resume · spotify next · spotify previous
spotify seek 1:30 · spotify seek +15s · spotify seek 50%
spotify volume 40 · spotify volume +10
spotify track        # details of the current song
spotify lyrics
spotify search 'lofi' --type playlist
spotify queue add spotify:track:<id>    # plays next; `spotify queue` lists, `queue remove 1` removes
spotify playlist list · playlist create 'Focus' · playlist add <id> <track-uri>
spotify podcast search 'tim ferriss' · podcast play spotify:episode:<id>
```

Every command prints human text by default and exactly one JSON value with `--json`. Every
playback result says which path worked (`via: spotify_player` or `via: applescript`) and, when the
first path failed, why (`fallback.reason`). See `spotify docs playback`.

## 3. For Silicons: log in once, then set triggers

Triggers notify *you* (the Silicon that set them) through Ting, so they need your identity:

```sh
spotify iam --json                                   # app_id: spotify
iam silicon-login --app-id spotify --grant-org "$SILICON_ORG" --approve-scopes   # prints an SLT
spotify login '<SLT>'                                # exchanges it; registers you with Ting
spotify login status --json                          # {"authenticated": true, ...}
```

Stemcell does these steps for you when `spotify` is in your `apps`.

```sh
spotify trigger add --remaining 30s --note 'wrap up the call'
spotify trigger add --remaining 25%
spotify trigger add --elapsed 50%
spotify trigger add --end --note 'song over: check the build'
spotify trigger add --end --scope every            # every song end, until removed
spotify trigger list · trigger history · trigger remove <id>
spotify trigger test                               # send yourself a test Ting now
```

When a checkpoint is reached you receive a `spotify.trigger.fired` Ting whose `data` holds the
trigger (id, condition, note), the track and the position, and whose `metadata.isi` names the ISI
that created it (from `$ISI`), so your flow can route it back. If a `current` trigger's song is
skipped first you get `spotify.trigger.expired`. Details: `spotify docs triggers`, `spotify docs ting`.

No Ting? Add `--local` and block on it instead: `spotify trigger wait <id> --timeout 10m`.

## 4. When something fails

Errors say what failed, why, and the exact next step:

```text
error: spotify_player is not signed in to Spotify, so Web API features … cannot run.
hint: Run `spotify auth login`: it opens Spotify's consent page in the browser on this Mac…
(code: spotify_auth_required)
```

With `--json` the same error is one object on stderr: `{"error":{"code","message","hint","retryable","details"}}`.
Exit codes: 0 ok, 1 failed, 2 usage, 3 not signed in, 4 refused, 5 unavailable. All codes:
`spotify docs errors`.

Found a bug? It is open source: reproduce, patch, open a pull request at
https://github.com/unlikefraction/spotify-cli, and report it:

```sh
spotify report 'what I ran, what happened, what I expected' --pr https://github.com/unlikefraction/spotify-cli/pull/<n>
```

## Where to go next

| You want to… | Read |
| --- | --- |
| Understand triggers and their exact semantics | `spotify docs triggers` |
| Know how play/pause/seek decide between spotify_player and AppleScript | `spotify docs playback` |
| Queue, reorder and remove upcoming songs | `spotify docs queue` |
| Log in, Spotify sign-in, testing planes | `spotify docs auth` |
| Change settings, bring your own Spotify client id | `spotify docs config` |
| Know what runs in the background and where state lives | `spotify docs daemon` |
| Build on top (library, daemon protocol, backend API) | `spotify docs development`, `spotify docs api` |
