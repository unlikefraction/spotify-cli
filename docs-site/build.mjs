// Builds spotify.unlikefraction.com: a landing page, one page per guide in ../docs, install.sh,
// llms.txt, search-index.json, sitemap.xml, robots.txt and 404.html. Pages are readable without
// JavaScript; site.js only adds copy buttons, search and the telemetry toggle.
import { readFileSync, writeFileSync, mkdirSync, rmSync, copyFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { Marked } from "marked";

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
    },
  });
  let html = marked.parse(markdown);
  // `spotify docs <topic>` references become links to the page.
  html = html.replace(/<code>spotify docs ([a-z]+)<\/code>/g, (m, t) => (topics.some(([n]) => n === t) ? `<a href="/docs/${t}/"><code>spotify docs ${t}</code></a>` : m));
  return { html, toc };
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
  <label class="telemetry"><input type="checkbox" id="telemetry-toggle" checked> Share anonymous usage</label>
</footer>
<script src="/site.js" defer></script>
</body>
</html>
`;
}

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
  "## Docs",
];

for (const [name, title] of topics) {
  const markdown = readFileSync(join(root, "docs", `${name}.md`), "utf8");
  const { html, toc } = render(markdown);
  const first = markdown.split("\n").find((l) => l && !l.startsWith("#")) || title;
  const description = first.replace(/[`*]/g, "").slice(0, 160);
  const tocHtml = toc.length ? `<aside class="toc"><p>On this page</p>${toc.map((t) => `<a href="#${t.id}">${t.text}</a>`).join("")}</aside>` : "";
  const body = `<div class="docs">
<nav class="side">${docsNav(name)}</nav>
<main class="article"><p class="eyebrow">spotify-cli / Docs</p>${html}
<p class="offline">Offline: <code>spotify docs ${name}</code> · Source: <a href="${repo}/blob/main/docs/${name}.md">docs/${name}.md</a></p></main>
${tocHtml}
</div>`;
  mkdirSync(join(dist, "docs", name), { recursive: true });
  writeFileSync(join(dist, "docs", name, "index.html"), shell({ title: `${title} · spotify-cli`, description, body, path: `/docs/${name}/` }));
  mkdirSync(join(dist, "markdown"), { recursive: true });
  writeFileSync(join(dist, "markdown", `${name}.md`), markdown);
  search.push({ topic: name, title, url: `/docs/${name}/`, text: markdown.replace(/[#`*|>]/g, " ").replace(/\s+/g, " ").slice(0, 6000) });
  sitemap.push(`${origin}/docs/${name}/`);
  llms.push(`- [${title}](${origin}/markdown/${name}.md): ${description}`);
}

const landing = `<main class="landing">
<section class="hero">
  <p class="eyebrow"><span class="dot"></span> For Carbons and Silicons on macOS</p>
  <h1>Spotify from the command line, with a <span class="serif">cue</span> for every moment.</h1>
  <p class="lede">Play, search, queue and manage playlists and podcasts from a terminal or a Silicon. Set checkpoints like <em>30 seconds left</em>, <em>halfway</em> or <em>song over</em>, and get a Ting when they arrive.</p>
  <div class="install"><code id="install-cmd">${esc(install)}</code><button class="copy" data-copy="install-cmd" type="button">Copy</button></div>
  <p class="note">Installs <code>spotify</code>, <code>spotify-daemon</code> and <code>spotify_player</code>, and starts what needs to run. It never logs you in. Then: <code>spotify doctor</code>.</p>
</section>
<section class="cards">
  <article><h2>Every control</h2><p>Play anything by link, URI or search. Pause, skip, seek, volume, shuffle, repeat, likes, lyrics, devices, playlists, podcasts, and a real queue you can reorder and trim.</p><pre><code>spotify play --search 'arctic monkeys 505'
spotify seek 50%
spotify queue add spotify:track:… --next
spotify playlist create 'Deep focus'</code></pre></article>
  <article><h2>Verified, not assumed</h2><p>Each command goes through spotify_player first, is checked against Spotify.app itself, and falls back to AppleScript when it did not take effect. The result says which path worked and why.</p><pre><code>{"action": "play", "via": "applescript",
 "fallback": {"from": "spotify_player",
  "reason": {"code": "no_effect", …}}}</code></pre></article>
  <article><h2>Triggers through Ting</h2><p>Checkpoints by time left, time played, percentage, song end or song change. Firings are durable, retried, never duplicated, and carry your note and ISI.</p><pre><code>spotify trigger add --remaining 30s \\
  --note 'wrap up the call'
spotify trigger add --end --scope every</code></pre></article>
</section>
<section class="silicons">
  <h2>For Silicons</h2>
  <ol>
    <li><code>spotify iam --json</code> → <code>{"app_id": "spotify", …}</code></li>
    <li><code>iam silicon-login --app-id spotify --grant-org "$SILICON_ORG" --approve-scopes</code></li>
    <li><code>spotify login '&lt;SLT&gt;'</code> — exchanges it and registers you with Ting</li>
    <li><code>spotify trigger add --elapsed 50%</code> — you receive <code>spotify.trigger.fired</code></li>
  </ol>
  <p>Every command documents itself (<code>spotify &lt;command&gt; --help</code>), the whole tree is machine-readable (<code>spotify commands --json</code>), guides are bundled offline (<code>spotify docs</code>), and every error says what failed, why, and the exact fix.</p>
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
