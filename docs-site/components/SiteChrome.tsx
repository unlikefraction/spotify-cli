import { SiteHeader } from "@/components/silicon-ui/blocks/site-header/site-header";
import { SiteFooter } from "@/components/silicon-ui/blocks/site-footer/site-footer";
import { repo, version } from "@/lib/docs-core.mjs";

function Mark() {
  return <svg viewBox="0 0 64 64" aria-hidden="true" className="mark"><rect x="4" y="4" width="56" height="56" rx="14" fill="currentColor"/><path d="M18 40c9-4 19-4 28 2M20 31c8-3.5 17-3.5 25 1.5M22 23c7-3 14.5-3 21 1" stroke="var(--background)" strokeWidth="4" fill="none" strokeLinecap="round"/></svg>;
}
const brand = { name: "spotify-cli", href: "/", mark: <Mark/> };
export function Header() {
  return <SiteHeader className="site-header" variant="simple" current="" brand={brand} items={[
    { value: "docs", label: "Docs", href: "/docs/usage/" },
    { value: "triggers", label: "Triggers", href: "/docs/triggers/" },
    { value: "develop", label: "Develop", href: "/docs/development/" },
    { value: "github", label: "GitHub ↗", href: repo },
  ]} secondaryAction={null} primaryAction={null} />;
}
export function Footer() {
  return <SiteFooter className="site-footer" variant="minimal" brand={brand} tagline={`spotify-cli ${version} · open source`} columns={[]} links={[
    { label: "GitHub", href: repo, external: true },
    { label: "llms.txt", href: "/llms.txt" },
    { label: "Version policy", href: "/docs/versioning/" },
  ]} legal={[]} socials={[]} newsletter={null} status={null} />;
}
