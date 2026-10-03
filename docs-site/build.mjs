// Builds spotify.unlikefraction.com: a landing page, one page per guide in ../docs, install.sh,
// llms.txt, search-index.json, sitemap.xml, robots.txt and 404.html. Pages are readable without
// JavaScript; site.js only adds copy buttons, search and the telemetry toggle.
import { readFileSync, writeFileSync, mkdirSync, rmSync, copyFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { Marked, Renderer } from "marked";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
const dist = join(here, "dist");
const origin = "https://spotify.unlikefraction.com";
const version = /version = "([^"]+)"/.exec(readFileSync(join(root, "Cargo.toml"), "utf8"))[1];
const repo = "https://github.com/unlikefraction/spotify-cli";
const install = `curl -fsSL ${origin}/install.sh | sh`;

const topics = [
  ["usage", "Quick start", "Start"],
  ["triggers", "Triggers", "Start"],
  ["playback", "Playback control", "Using it"],
  ["queue", "The queue", "Using it"],
  ["auth", "Authentication", "Using it"],
  ["config", "Configuration", "Using it"],
  ["errors", "Errors", "Using it"],
  ["daemon", "The daemon", "How it works"],
  ["ting", "Ting integration", "How it works"],
  ["telemetry", "Telemetry", "How it works"],
  ["development", "Building on it", "Develop"],
  ["api", "Backend API", "Develop"],
  ["versioning", "Versioning", "Develop"],
];

const esc = (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
const slug = (s) => s.toLowerCase().replace(/<[^>]+>/g, "").replace(/`/g, "").replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");
const base = new Renderer();

function render(markdown) {
  const toc = [];
  const marked = new Marked({
    renderer: {
      heading({ tokens, depth }) {
        const text = this.parser.parseInline(tokens);
        const id = slug(text);
        if (depth === 2) toc.push({ id, text });
        return `<h${depth} id="${id}"><a class="anchor" href="#${id}">${text}</a></h${depth}>\n`;
      },
      link({ href, title, tokens }) {
        const text = this.parser.parseInline(tokens);
        const external = /^https?:/.test(href);
        return `<a href="${esc(href)}"${title ? ` title="${esc(title)}"` : ""}${external ? ' rel="noopener"' : ""}>${text}</a>`;
      },
      // A header strip above each block holds the language and the copy button site.js adds,
      // so the button never covers code at any width.
      code(token) {
        const lang = (token.lang || "").split(/\s/)[0];
        return `<div class="codeblock"><div class="codeblock-head"><span>${esc(lang)}</span></div>${base.code(token)}</div>\n`;
      },
    },
  });
  let html = marked.parse(markdown);
  // `spotify docs <topic>` references become links to the page.
  html = html.replace(/<code>spotify docs ([a-z]+)<\/code>/g, (m, t) => (topics.some(([n]) => n === t) ? `<a href="/docs/${t}/"><code>spotify docs ${t}</code></a>` : m));
  return { html, toc };
}

// Inline tokens as plain text: code spans, emphasis and links keep only their text.
const plain = (tokens) => tokens.map((t) => (t.type === "html" ? "" : t.tokens ? plain(t.tokens) : (t.text ?? " "))).join("");

// Whole sentences within `max` characters. A longer first sentence is kept whole up to 200,
// beyond that it is cut at a word.
function clip(text, max = 160) {
  let out = "";
  for (const sentence of text.split(/(?<=[.!?])(?<!\b(?:e\.g|i\.e|vs)\.)\s+/)) {
    if (out && out.length + 1 + sentence.length > max) break;
    out = out ? `${out} ${sentence}` : sentence;
  }
  const cut = out.length <= 200 ? out : `${out.slice(0, out.lastIndexOf(" ", max - 1)).replace(/[,;:]$/, "")}…`;
  return /[.!?…]$/.test(cut) ? cut : `${cut}.`; // a paragraph that ends in a URL or code span
}

// A guide's summary for <meta name="description"> and llms.txt: the first prose paragraph of its
// intro or first section (headings, tables, code and lists are not paragraphs; a lead-in's colon
// becomes a full stop, and one-line lead-ins like "Rules:" are skipped). A guide without one is
// summarised by its section headings.
function summarize(tokens, title) {
  let sections = 0;
  for (const t of tokens) {
    if (t.type === "heading" && t.depth === 2 && ++sections > 1) break;
    if (t.type !== "paragraph") continue;
    const text = plain(t.tokens).replace(/\s+/g, " ").trim().replace(/:$/, ".");
    if (text.split(" ").length >= 5) return clip(text);
  }
  const headings = tokens.filter((t) => t.type === "heading" && t.depth === 2).map((t) => plain(t.tokens));
  return clip(headings.length ? `${title}: ${headings.join(", ")}.` : title);
}

// Search entries: the intro and each `##` section of a guide, linked to its anchor.
function sections(tokens, toc, name, title) {
  const out = [{ topic: name, title, section: null, url: `/docs/${name}/`, text: "" }];
  let h2 = 0;
  for (const t of tokens) {
    if (t.type === "heading" && t.depth === 2) {
      const { id } = toc[h2++];
      out.push({ topic: name, title, section: plain(t.tokens), url: `/docs/${name}/#${id}`, text: "" });
    } else if (t.type !== "heading" || t.depth > 2) {
      out.at(-1).text += ` ${t.type === "code" ? t.text : t.raw}`;
    }
  }
  for (const entry of out) entry.text = entry.text.replace(/[#`*|>]/g, " ").replace(/-{3,}/g, " ").replace(/\s+/g, " ").trim();
  return out.filter((entry) => entry.section || entry.text);
}

const mark = `<svg viewBox="0 0 64 64" aria-hidden="true" class="mark"><rect x="4" y="4" width="56" height="56" rx="14" fill="currentColor"/><path d="M18 40c9-4 19-4 28 2M20 31c8-3.5 17-3.5 25 1.5M22 23c7-3 14.5-3 21 1" stroke="var(--paper)" stroke-width="4" fill="none" stroke-linecap="round"/></svg>`;

function shell({ title, description, body, path, nav = "" }) {
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${esc(title)}</title>
<meta name="description" content="${esc(description)}">
<link rel="canonical" href="${origin}${path}">
<link rel="stylesheet" href="/styles.css">
<link rel="icon" href="/favicon.svg" type="image/svg+xml">
<meta property="og:title" content="${esc(title)}">
<meta property="og:description" content="${esc(description)}">
<script type="application/ld+json">${JSON.stringify({ "@context": "https://schema.org", "@type": "SoftwareApplication", name: "spotify-cli", operatingSystem: "macOS", applicationCategory: "DeveloperApplication", softwareVersion: version, url: origin, codeRepository: repo, offers: { "@type": "Offer", price: "0" } })}</script>
</head>
<body>
<header class="top">
  <a class="brand" href="/">${mark}<span>spotify-cli</span></a>
  <nav>${nav}<a href="/docs/usage/">Docs</a><a href="/docs/triggers/">Triggers</a><a href="/docs/development/">Develop</a><a href="${repo}" rel="noopener">GitHub ↗</a></nav>
</header>
${body}
<footer class="foot">
  <span>spotify-cli ${esc(version)} · open source · <a href="${repo}">${repo.replace("https://", "")}</a> · <a href="/llms.txt">llms.txt</a> · <a href="/docs/versioning/">Version policy</a></span>
  <div class="telemetry" hidden><label><input type="checkbox" id="telemetry-toggle">Anonymous usage stats<span id="telemetry-note">: page path, referrer host, window size; no cookies</span></label> <a href="/docs/telemetry/">Details</a></div>
</footer>
<script src="/site.js" defer></script>
</body>
</html>
`;
}

// Docs search: hidden until site.js wires it up, since it needs the index and script.
const searchForm = `<form class="search" role="search" hidden>
<label class="sr-only" for="search-input">Search the docs</label>
<input id="search-input" type="search" placeholder="Search the docs" autocomplete="off" spellcheck="false" enterkeyhint="go" aria-keyshortcuts="/" aria-controls="search-results"><kbd aria-hidden="true">/</kbd>
<p class="sr-only" id="search-status" role="status"></p>
<div class="search-pop" id="search-results" hidden><p class="search-note"></p><ul></ul></div>
</form>`;

function docsNav(current) {
  const groups = {};
  for (const [name, title, group] of topics) (groups[group] ||= []).push([name, title]);
  return Object.entries(groups)
    .map(([group, items]) => `<p class="group">${esc(group)}</p>` + items.map(([n, t]) => `<a href="/docs/${n}/"${n === current ? ' aria-current="page"' : ""}>${esc(t)}</a>`).join(""))
    .join("");
}

rmSync(dist, { recursive: true, force: true });
mkdirSync(dist, { recursive: true });
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
  "- `spotify commands --json`: the whole CLI as one JSON manifest. Per command: path, summary, goal, capabilities, keywords, arguments (flag, type, allowed_values, default, min, max, conflicts_with, requires), arg_groups, examples, output fields, errors with exit codes, requirements (macos, daemon, spotify_app, spotify_player_signed_in, iam_login, premium, controls_playback, network), mutates, changes, read_only_when and has_read_only_form. Top level: goals, errors (code, exit, meaning, fix), exit_codes, argument_types, changes (library is Liked Songs, changed by like and unlike; playlists is playlists), output_contract. Commands that can run without changing anything: `spotify commands --json | jq -r '.commands[] | select(.mutates == false or .read_only_when != null).command'` (the same as `select(.has_read_only_form)`); what changes Liked Songs: `select(.changes | index(\"library\"))`. `spotify commands` marks changing commands with `✎ changes <what>`.",
  "- `spotify --help` groups commands by goal; every `spotify <command> --help` lists its own options, then the global ones, and ends with runnable examples; `spotify docs <topic> --section '<heading>'` prints one guide section and `spotify docs [<topic>] --search '<text>'` searches; `spotify completions --help` has the install lines for zsh, bash, fish and PowerShell.",
  "- Human output ends with a `Next:` line on stderr suggesting the likely next commands with real URIs, only when stdout is a terminal (never ahead of piped output; `SPOTIFY_HINTS=0` turns it off, `SPOTIFY_HINTS=always` forces it; never in `--json`).",
  "",
  "Common tasks:",
  "",
  "- Lyrics of any song, nothing has to play: `spotify lyrics <uri|link|id>` (`spotify lyrics` alone: the song playing now). It takes a track, not search words: find one by name with `spotify search '<words>' --type track`, then run `spotify lyrics <uri>`. `--json`: track, title, lines, text, synced.",
  "- Play anything: `spotify play spotify:track:<id>` (or `--search '<words>'`, `--context <playlist>`, `--liked`, an album, playlist, artist or show). Every start goes through the Spotify Web API first, so Spotify.app stays in the background (`via: \"web_api\"`): songs, episodes, shows and Liked Songs directly (a song inside its album, so the album plays on), albums, playlists, artists and radios through spotify_player. It plays where Spotify.app plays: on this Mac, or on the speaker or phone it controls (`note`). AppleScript is the fallback, except for a radio (`fallback.from: \"web_api\"`, `fallback.reason.code`), and the focus goes back to the previous app (`refocused`; config `keep_spotify_in_background`).",
  "- Get notified at a checkpoint: `spotify trigger add --remaining 30s --note '<what to do>'` (also `--elapsed 50%`, `--end`, `--change`; needs `spotify login '<SLT>'` with an SLT from `iam`, Silicon IAM's own CLI, separate from spotify-cli: `cargo install silicon-iam-cli`; or `--local` with `spotify trigger wait <id>`).",
  "- Output: `--json` prints exactly one JSON value on stdout; errors are one `{\"error\": {code, message, hint, retryable, details}}` object on stderr. Exit codes: 0 ok, 1 failed, 2 usage, 3 not signed in, 4 refused, 5 unavailable.",
  "",
  "## Docs",
];

for (const [name, title] of topics) {
  const markdown = readFileSync(join(root, "docs", `${name}.md`), "utf8");
  const { html, toc } = render(markdown);
  const tokens = new Marked().lexer(markdown);
  const description = summarize(tokens, title);
  const tocHtml = toc.length ? `<aside class="toc"><p>On this page</p>${toc.map((t) => `<a href="#${t.id}">${t.text}</a>`).join("")}</aside>` : "";
  const body = `<div class="docs">
<div class="side">${searchForm}<nav aria-label="Guides">${docsNav(name)}</nav></div>
<main class="article"><p class="eyebrow">spotify-cli / Docs</p>${html}
<p class="offline">Offline: <code>spotify docs ${name}</code> · Source: <a href="${repo}/blob/main/docs/${name}.md">docs/${name}.md</a></p></main>
${tocHtml}
</div>`;
  mkdirSync(join(dist, "docs", name), { recursive: true });
  writeFileSync(join(dist, "docs", name, "index.html"), shell({ title: `${title} · spotify-cli`, description, body, path: `/docs/${name}/` }));
  mkdirSync(join(dist, "markdown"), { recursive: true });
  writeFileSync(join(dist, "markdown", `${name}.md`), markdown);
  search.push(...sections(tokens, toc, name, title));
  sitemap.push(`${origin}/docs/${name}/`);
  llms.push(`- [${title}](${origin}/markdown/${name}.md): ${description}`);
}

const landing = `<main class="landing">
<section class="hero">
  <p class="eyebrow"><span class="dot"></span> For Carbons and Silicons on macOS</p>
  <h1>Spotify from the command line, with a <span class="serif">cue</span> for every moment.</h1>
  <p class="lede">Play, search, read the lyrics of any song, queue and manage playlists and podcasts from a terminal or a Silicon, while Spotify stays in the background. Set checkpoints like <em>30 seconds left</em>, <em>halfway</em> or <em>song over</em>, and get a Ting when they arrive.</p>
  <div class="install"><code id="install-cmd">${esc(install)}</code><button class="copy" data-copy="install-cmd" type="button">Copy</button></div>
  <p class="note">Installs <code>spotify</code>, <code>spotify-daemon</code> and <code>spotify_player</code>, and starts what needs to run. It never logs you in. Then: <code>spotify doctor</code>.</p>
</section>
<section class="cards">
  <article><h2>Every control</h2><p>Play anything by link, URI or search. Pause, skip, seek, volume, shuffle, repeat, likes, devices, playlists, podcasts, and a real queue you can reorder and trim.</p><pre><code>spotify play --search 'arctic monkeys 505'
spotify seek 50%
spotify queue add spotify:track:… --next
spotify playlist create 'Deep focus'</code></pre></article>
  <article><h2>Lyrics of any song</h2><p>The song playing now, or any track by URI, link or id. Nothing has to play, and Spotify does not need to be open. Only know its name? Search for the track, then pass its URI.</p><pre><code>spotify lyrics
spotify lyrics \\
  spotify:track:0BxE4FqsDD1Ot4YuBXwAPp
spotify search 'bohemian rhapsody' \\
  --type track</code></pre></article>
  <article><h2>Find anything</h2><p>Ask in plain words, offline. <code>spotify</code> alone shows what is set up and what to try, <code>--help</code> groups every command by goal with an example, and each result suggests what comes next. Tab completion for zsh, bash, fish and PowerShell, with install lines that work as they are.</p><pre><code>spotify how "play my liked songs shuffled"
spotify --help
spotify completions --help</code></pre></article>
  <article><h2>Out of your way, and verified</h2><p>Every start goes through the Spotify Web API first, so Spotify.app stays in the background, a song's album plays on after it, and a speaker you picked keeps playing. Every effect is checked against Spotify.app; if AppleScript has to step in, your app gets the focus back. The result says which path worked and why.</p><pre><code>{"action": "play", "via": "web_api",
 "playback": {"state": "playing", …}}</code></pre></article>
  <article><h2>Triggers through Ting</h2><p>Checkpoints by time left, time played, percentage, song end or song change. Firings are durable, retried, never duplicated, and carry your note and ISI.</p><pre><code>spotify trigger add --remaining 30s \\
  --note 'wrap up the call'
spotify trigger add --end --scope every</code></pre></article>
  <article><h2>Describes itself</h2><p>One JSON manifest holds every command: arguments with types, allowed values and defaults, examples, output fields, error codes with exit codes, what each needs (daemon, Spotify.app, login, Premium) and whether it changes anything.</p><pre><code>spotify commands --json | jq \\
  '.commands[] | select(.path == "lyrics")'</code></pre></article>
</section>
<section class="silicons">
  <h2>For Silicons</h2>
  <ol>
    <li><code>spotify iam --json</code> → <code>{"app_id": "spotify", …}</code></li>
    <li><code>cargo install silicon-iam-cli</code> — <code>iam</code>, Silicon IAM's own CLI (separate from spotify-cli), if you do not have it</li>
    <li>In a fresh <code>SILICON_HOME</code>, once: <code>iam silicon-login --sid si:&lt;handle&gt;</code> — the Silicon's own IAM sign-in (it asks for its STK)</li>
    <li><code>iam silicon-login --app-id spotify --grant-org "$SILICON_ORG" --approve-scopes</code> — prints a short-lived token (SLT)</li>
    <li><code>spotify login '&lt;SLT&gt;'</code> — signs in to one account and organization</li>
    <li><code>spotify ting authorize</code> — review notification permissions in IAM, then <code>spotify ting complete REQUEST_ID --code-file FILE</code></li>
    <li><code>spotify trigger add --elapsed 50%</code> — you receive <code>spotify.trigger.fired</code></li>
  </ol>
  <p>New to IAM, Silicons, SLTs or Ting? <a href="/docs/usage/#concepts">Concepts</a> explains each in a sentence or two.</p>
  <p>Every command documents itself with runnable examples (<code>spotify &lt;command&gt; --help</code>). <code>spotify how "&lt;task&gt;" --json</code> finds the command for a task, <code>spotify commands --json</code> describes every command, argument, output field, error and requirement (<a href="/docs/usage/#the-command-manifest">the manifest</a>), guides are bundled offline (<code>spotify docs &lt;topic&gt; --section '&lt;heading&gt;'</code>), and every error says what failed, why, and the exact fix.</p>
</section>
<section class="grid">
${topics.map(([n, t]) => `<a href="/docs/${n}/"><strong>${esc(t)}</strong><span>spotify docs ${n}</span></a>`).join("\n")}
</section>
</main>`;
writeFileSync(join(dist, "index.html"), shell({ title: "spotify-cli · Spotify from the command line", description: "Control the Spotify desktop app from the command line on macOS, with playback triggers delivered through Ting. For Carbons and Silicons.", body: landing, path: "/" }));
writeFileSync(join(dist, "404.html"), shell({ title: "Not found · spotify-cli", description: "Page not found.", body: `<main class="landing"><section class="hero"><h1>Not <span class="serif">here</span>.</h1><p class="lede">Try the <a href="/docs/usage/">quick start</a> or <code>spotify docs</code>.</p></section></main>`, path: "/404.html" }));
copyFileSync(join(here, "styles.css"), join(dist, "styles.css"));
copyFileSync(join(here, "site.js"), join(dist, "site.js"));
copyFileSync(join(here, "favicon.svg"), join(dist, "favicon.svg"));
copyFileSync(join(root, "scripts", "install.sh"), join(dist, "install.sh"));
writeFileSync(join(dist, "search-index.json"), JSON.stringify(search));
writeFileSync(join(dist, "llms.txt"), llms.concat(["", "## Source", `- ${repo}`, `- https://github.com/unlikefraction/spotify-cli/tree/main/crates/client`, ""]).join("\n"));
writeFileSync(join(dist, "robots.txt"), `User-agent: *\nAllow: /\nSitemap: ${origin}/sitemap.xml\n`);
writeFileSync(join(dist, "sitemap.xml"), `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${sitemap.map((u) => `  <url><loc>${u}</loc></url>`).join("\n")}\n</urlset>\n`);
if (!existsSync(join(dist, "install.sh"))) throw new Error("install.sh missing");
console.log(`built ${topics.length} guides + landing into ${dist}`);
