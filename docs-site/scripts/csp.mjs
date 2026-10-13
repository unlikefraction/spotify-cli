// Pins every exported page's inline scripts with a hash-based Content-Security-Policy.
//
// The App Router inlines the page's RSC payload and bootstrap in <script> tags, which a static
// `script-src 'self'` would block. vercel.json's header therefore allows inline scripts, and this
// adds a stricter <meta> policy to each page listing the SHA-256 of exactly its own inline scripts.
// Browsers enforce both policies, so only those scripts and same-origin files can run.
// check.mjs verifies that every page carries the policy and that it matches.
import { createHash } from "node:crypto";
import { readFile, readdir, writeFile } from "node:fs/promises";
import { join, relative } from "node:path";
import { pathToFileURL } from "node:url";
import { DIST_DIR } from "../lib/docs-core.mjs";

export const INLINE_SCRIPT = /<script(?![^>]*\bsrc=)[^>]*>([\s\S]*?)<\/script>/g;
export const META_CSP = /<meta http-equiv="Content-Security-Policy" content="([^"]*)"\/>/;

export function scriptHashes(html) {
  return [...new Set([...html.matchAll(INLINE_SCRIPT)].map((m) => `'sha256-${createHash("sha256").update(m[1], "utf8").digest("base64")}'`))];
}

export function policyFor(html) {
  return `script-src 'self' ${scriptHashes(html).join(" ")}`.trim();
}

async function htmlFiles(dir) {
  const out = [];
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) out.push(...(await htmlFiles(path)));
    else if (entry.name.endsWith(".html")) out.push(path);
  }
  return out;
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const files = await htmlFiles(DIST_DIR);
  let scripts = 0;
  for (const file of files) {
    let html = await readFile(file, "utf8");
    html = html.replace(META_CSP, "");
    const meta = `<meta http-equiv="Content-Security-Policy" content="${policyFor(html)}"/>`;
    const head = html.indexOf("<head>");
    if (head < 0) throw new Error(`csp: ${relative(DIST_DIR, file)} has no <head>`);
    // Right after <head> (and its charset), before any script.
    const charset = /^<meta charSet="utf-8"\/>/.exec(html.slice(head + 6));
    const at = head + 6 + (charset ? charset[0].length : 0);
    html = html.slice(0, at) + meta + html.slice(at);
    scripts += scriptHashes(html).length;
    await writeFile(file, html);
  }
  console.log(`csp: pinned ${scripts} inline scripts across ${files.length} pages`);
}
