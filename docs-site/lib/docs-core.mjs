import { readFileSync } from "node:fs";
import { join } from "node:path";
import { Marked } from "marked";
// npm runs Next and the export scripts from docs-site; bundled modules cannot use their file URL.
export const here = process.cwd();
export const root = join(here, "..");
export const DIST_DIR = join(here, "dist");
export const origin = "https://spotify.unlikefraction.com";
export const version = /version = "([^"]+)"/.exec(readFileSync(join(root, "Cargo.toml"), "utf8"))[1];
export const repo = "https://github.com/unlikefraction/spotify-cli";
export const install = `curl -fsSL ${origin}/install.sh | sh`;

export const topics = [
  ["usage", "Quick start", "Start"],
  ["triggers", "Triggers", "Start"],
  ["playback", "Playback control", "Using it"],
  ["queue", "The queue", "Using it"],
  ["auth", "Authentication", "Using it"],
  ["config", "Configuration", "Using it"],
  ["errors", "Errors", "Using it"],
  ["daemon", "The daemon", "How it works"],
  ["ting", "Ting integration", "How it works"],
  ["development", "Building on it", "Develop"],
  ["api", "Backend API", "Develop"],
  ["versioning", "Versioning", "Develop"],
];

// Inline tokens as plain text: code spans, emphasis and links keep only their text.
export const plain = (tokens) => tokens.map((t) => (t.type === "html" ? "" : t.tokens ? plain(t.tokens) : (t.text ?? " "))).join("");

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
export function summarize(tokens, title) {
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
export function sections(tokens, toc, name, title) {
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


export const slug = (s) => s.toLowerCase().replace(/<[^>]+>/g, "").replace(/`/g, "").replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");
export function getGuide(name) {
  const topic = topics.find(([id]) => id === name);
  if (!topic) return null;
  const [, title, group] = topic;
  const markdown = readFileSync(join(root, "docs", `${name}.md`), "utf8");
  const marked = new Marked();
  const tokens = marked.lexer(markdown);
  const toc = [];
  marked.walkTokens(tokens, (token) => {
    if (token.type !== "heading") return;
    token.id = slug(marked.parseInline(token.text));
    if (token.depth === 2) toc.push({ id: token.id, text: plain(token.tokens) });
  });
  return { name, title, group, markdown, tokens, toc, description: summarize(tokens, title) };
}
