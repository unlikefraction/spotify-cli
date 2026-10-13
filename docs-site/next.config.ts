import { dirname } from "node:path";
import { fileURLToPath } from "node:url";
import type { NextConfig } from "next";

// A static export: every page, the docs and the generated text files are prerendered into dist/,
// which Vercel serves with vercel.json (redirects and the security headers).
// scripts/csp.mjs then pins each page's inline scripts with a hash-based CSP.
const config: NextConfig = {
  output: "export",
  distDir: "dist",
  trailingSlash: true,
  images: { unoptimized: true },
  devIndicators: false,
  poweredByHeader: false,
  reactStrictMode: true,
  turbopack: { root: dirname(fileURLToPath(import.meta.url)) },
};

export default config;
