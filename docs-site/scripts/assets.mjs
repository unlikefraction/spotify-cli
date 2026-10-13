// Preserve the public installer, machine-readable documentation and search index.
import { copyFileSync, mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { DIST_DIR as dist, root, topics, origin, repo, install, getGuide, sections } from "../lib/docs-core.mjs";
const search = [];
const sitemap = [`${origin}/`];
const llms = [
  "# spotify-cli",
  "",
  "> Control the Spotify desktop app on macOS from the command line (Carbons and Silicons), with playback triggers delivered through Ting. CLI `spotify`, always-on `spotify-daemon`, Rust library `silicon-spotify-client`.",
  "",
  `Install: \`${install}\``,
  "",
  "Find anything, offline:",
  "",
  "- `spotify` with no arguments: what is set up, what is missing (each with its fix), what is playing, and what to try (`--json`: version, ready, now_playing, checks, try, help).",
  "- `spotify how \"<question>\"`: plain-words search over every command's help, the guides, the settings and the error codes; prints commands with ready-to-run examples made for the question (a song name becomes a search, a setting a `config set`), the guide section to read and matching error codes (`--json`: question, name, terms, commands, guides, errors, more).",
  "- `spotify commands --json`: the whole CLI as one JSON manifest. Per command: path, summary, goal, capabilities, keywords, arguments (flag, type, allowed_values, default, min, max, conflicts_with, requires), arg_groups, examples, output fields, errors with exit codes, requirements (macos, daemon, spotify_app, spotify_player_signed_in, accounts_login, premium, controls_playback, network), mutates, changes, read_only_when and has_read_only_form. Top level: goals, errors (code, exit, meaning, fix), exit_codes, argument_types, changes (library is Liked Songs, changed by like and unlike; playlists is playlists), output_contract. Commands that can run without changing anything: `spotify commands --json | jq -r '.commands[] | select(.mutates == false or .read_only_when != null).command'` (the same as `select(.has_read_only_form)`); what changes Liked Songs: `select(.changes | index(\"library\"))`. `spotify commands` marks changing commands with `✎ changes <what>`.",
  "- `spotify --help` groups commands by goal; every `spotify <command> --help` lists its own options, then the global ones, and ends with runnable examples; `spotify docs <topic> --section '<heading>'` prints one guide section and `spotify docs [<topic>] --search '<text>'` searches; `spotify completions --help` has the install lines for zsh, bash, fish and PowerShell.",
  "- Human output ends with a `Next:` line on stderr suggesting the likely next commands with real URIs, only when stdout is a terminal (never ahead of piped output; `SPOTIFY_HINTS=0` turns it off, `SPOTIFY_HINTS=always` forces it; never in `--json`).",
  "",
  "Common tasks:",
  "",
  "- Lyrics of any song, nothing has to play: `spotify lyrics <uri|link|id>` (`spotify lyrics` alone: the song playing now). It takes a track, not search words: find one by name with `spotify search '<words>' --type track`, then run `spotify lyrics <uri>`. `--json`: track, title, lines, text, synced.",
  "- Play anything: `spotify play spotify:track:<id>` (or `--search '<words>'`, `--context <playlist>`, `--liked`, an album, playlist, artist or show). Every start goes through the Spotify Web API first, so Spotify.app stays in the background (`via: \"web_api\"`): songs, episodes, shows and Liked Songs directly (a song inside its album, so the album plays on), albums, playlists, artists and radios through spotify_player. It plays where Spotify.app plays: on this Mac, or on the speaker or phone it controls (`note`). AppleScript is the fallback, except for a radio (`fallback.from: \"web_api\"`, `fallback.reason.code`), and the focus goes back to the previous app (`refocused`; config `keep_spotify_in_background`).",
  "- Get notified at a checkpoint: `spotify trigger add --remaining 30s --note '<what to do>'` (also `--elapsed 50%`, `--end`, `--change`; needs `spotify login '<SLT>'` with an SLT from `silicon-accounts`, the Silicon Accounts CLI: `silicon-apps install silicon-accounts`; or `--local` with `spotify trigger wait <id>`).",
  "- Output: `--json` prints exactly one JSON value on stdout; errors are one `{\"error\": {code, message, hint, retryable, details}}` object on stderr. Exit codes: 0 ok, 1 failed, 2 usage, 3 not signed in, 4 refused, 5 unavailable.",
  "",
  "## Docs",
];

mkdirSync(join(dist, "markdown"), { recursive: true });
for (const [name, title] of topics) {
  const guide = getGuide(name);
  writeFileSync(join(dist, "markdown", `${name}.md`), guide.markdown);
  search.push(...sections(guide.tokens, guide.toc, name, title));
  sitemap.push(`${origin}/docs/${name}/`);
  llms.push(`- [${title}](${origin}/markdown/${name}.md): ${guide.description}`);
}
copyFileSync(join(root, "scripts", "install.sh"), join(dist, "install.sh"));
writeFileSync(join(dist, "search-index.json"), JSON.stringify(search));
writeFileSync(join(dist, "llms.txt"), llms.concat(["", "## Source", `- ${repo}`, `- ${repo}/tree/main/crates/client`, ""]).join("\n"));
writeFileSync(join(dist, "robots.txt"), `User-agent: *\nAllow: /\nSitemap: ${origin}/sitemap.xml\n`);
writeFileSync(join(dist, "sitemap.xml"), `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${sitemap.map((u) => `  <url><loc>${u}</loc></url>`).join("\n")}\n</urlset>\n`);
console.log(`generated installer, search and machine-readable docs for ${topics.length} guides`);
