import type { Metadata } from "next";
import { notFound } from "next/navigation";
import Markdown from "@/components/Markdown";
import DocsSearch from "@/components/DocsSearch";
import { Breadcrumb } from "@/components/silicon-ui/breadcrumb/breadcrumb";
import { topics, getGuide, repo } from "@/lib/docs-core.mjs";

export const dynamicParams = false;
export function generateStaticParams() { return topics.map(([topic]) => ({ topic })); }
type Props = { params: Promise<{ topic: string }> };
export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const { topic } = await params;
  const guide = getGuide(topic);
  if (!guide) notFound();
  return { title: guide.title, description: guide.description, alternates: { canonical: `/docs/${topic}/` }, openGraph: { title: `${guide.title} · spotify-cli`, description: guide.description } };
}
export default async function GuidePage({ params }: Props) {
  const { topic } = await params;
  const guide = getGuide(topic);
  if (!guide) notFound();
  const groups = [...new Set(topics.map(([, , group]) => group))];
  return <div className="docs">
    <div className="side"><DocsSearch/><nav aria-label="Guides">{groups.map(group => <div key={group}><p className="group">{group}</p>{topics.filter(([, , name]) => name === group).map(([name, title]) => <a key={name} href={`/docs/${name}/`} aria-current={name === topic ? "page" : undefined}>{title}</a>)}</div>)}</nav></div>
    <main className="article"><Breadcrumb items={[{ label: "spotify-cli", href: "/" }, { label: "Docs", href: "/docs/usage/" }, { label: guide.title }]}/><Markdown tokens={guide.tokens}/><p className="offline">Offline: <code>spotify docs {topic}</code> · Source: <a href={`${repo}/blob/main/docs/${topic}.md`}>docs/{topic}.md</a></p></main>
    {guide.toc.length > 0 && <aside className="toc"><p>On this page</p>{guide.toc.map(({ id, text }: { id: string; text: string }) => <a key={id} href={`#${id}`}>{text}</a>)}</aside>}
  </div>;
}
