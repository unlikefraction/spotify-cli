import type { Metadata } from "next";
import { Header, Footer } from "@/components/SiteChrome";
import AccountPanel from "@/components/AccountPanel";
import { origin, repo, version } from "@/lib/docs-core.mjs";
import "@/components/silicon-ui/foundation.css";
import "./globals.css";

export const metadata: Metadata = {
  metadataBase: new URL(origin),
  title: { default: "spotify-cli · Spotify from the command line", template: "%s · spotify-cli" },
  description: "Control the Spotify desktop app from the command line on macOS, with playback triggers delivered through Ting. For Carbons and Silicons.",
  icons: { icon: "/favicon.svg" },
};
export default function RootLayout({ children }: { children: React.ReactNode }) {
  const software = { "@context": "https://schema.org", "@type": "SoftwareApplication", name: "spotify-cli", operatingSystem: "macOS", applicationCategory: "DeveloperApplication", softwareVersion: version, url: origin, codeRepository: repo, offers: { "@type": "Offer", price: "0" } };
  return <html lang="en" data-accent="green" suppressHydrationWarning><head><script src="/theme.js"/><script type="application/ld+json" dangerouslySetInnerHTML={{ __html: JSON.stringify(software).replace(/</g, "\\u003c") }}/></head><body><Header/><AccountPanel/>{children}<Footer/></body></html>;
}
