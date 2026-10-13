import type { Metadata } from "next";
import { Badge } from "@/components/silicon-ui/badge/badge";
import { Card } from "@/components/silicon-ui/card/card";
import { CodeBlock } from "@/components/silicon-ui/code-block/code-block";
import { CopyButton } from "@/components/silicon-ui/copy-button/copy-button";
import { topics, install } from "@/lib/docs-core.mjs";
import features from "@/lib/features.json";

export const metadata: Metadata = { alternates: { canonical: "/" }, openGraph: { title: "spotify-cli · Spotify from the command line", description: "Control the Spotify desktop app from the command line on macOS, with playback triggers delivered through Ting. For Carbons and Silicons." } };
export default function Home() {
  return <main className="landing">
    <section className="hero">
      <p className="eyebrow"><Badge tone="success">For Carbons and Silicons on macOS</Badge></p>
      <h1>Spotify from the command line, with a <span className="serif">cue</span> for every moment.</h1>
      <p className="lede">Play, search, read the lyrics of any song, queue and manage playlists and podcasts from a terminal or a Silicon, while Spotify stays in the background. Set checkpoints like <em>30 seconds left</em>, <em>halfway</em> or <em>song over</em>, and get a Ting when they arrive.</p>
      <div className="install"><code>{install}</code><CopyButton value={install} label="Copy install command"/></div>
      <p className="note">Installs <code>spotify</code>, <code>spotify-daemon</code> and <code>spotify_player</code>, and starts what needs to run. It never logs you in. Then: <code>spotify doctor</code>.</p>
    </section>
    <section className="cards" aria-label="Features">{features.map(feature => <Card key={feature.title} title={feature.title} description={feature.description}><CodeBlock code={feature.code} language={feature.language} filename={feature.language === "json" ? "JSON" : "Terminal"}/></Card>)}</section>
    <section className="silicons">
      <h2>For Silicons</h2>
      <ol>
        <li><code>spotify accounts --json</code> → <code>{'{"app_id": "spotify", …}'}</code></li>
        <li><code>silicon-apps install silicon-accounts</code> — <code>silicon-accounts</code>, the Silicon Accounts CLI, if you do not have it</li>
        <li><code>silicon-accounts login --app spotify</code> — prints a short-lived token (SLT)</li>
        <li><code>{"spotify login '<SLT>'"}</code> — signs in to your Carbon or Silicon account</li>
        <li><code>spotify ting register</code> — enable Ting delivery for your account</li>
        <li><code>spotify trigger add --elapsed 50%</code> — you receive <code>spotify.trigger.fired</code></li>
      </ol>
      <p>New to Silicon Accounts, Silicons, SLTs or Ting? <a href="/docs/usage/#concepts">Concepts</a> explains each in a sentence or two.</p>
      <p>Every command documents itself with runnable examples (<code>{"spotify <command> --help"}</code>). <code>{'spotify how "<task>" --json'}</code> finds the command for a task, <code>spotify commands --json</code> describes every command, argument, output field, error and requirement (<a href="/docs/usage/#the-command-manifest">the manifest</a>), guides are bundled offline (<code>{"spotify docs <topic> --section '<heading>'"}</code>), and every error says what failed, why, and the exact fix.</p>
    </section>
    <section className="grid" aria-label="Guides">{topics.map(([name, title]) => <a key={name} href={`/docs/${name}/`}><Card title={title} description={`spotify docs ${name}`}/></a>)}</section>
  </main>;
}
