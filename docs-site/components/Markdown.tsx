import { Fragment, createElement, type ReactNode } from "react";
import type { Token, Tokens } from "marked";
import { CodeBlock } from "@/components/silicon-ui/code-block/code-block";
import { topics } from "@/lib/docs-core.mjs";
type HeadingToken = Tokens.Heading & { id: string };

// Renders the Markdown tokens prepared by lib/docs-core.mjs as React, so fenced code becomes Silicon UI's
// CodeBlock (copy button, language label) at any depth, including inside list items. Raw HTML in
// the Markdown is shown as text: the docs are also read in a terminal with `spotify docs`.

const ENTITIES: Record<string, string> = { amp: "&", lt: "<", gt: ">", quot: '"', apos: "'", nbsp: " ", copy: "©", mdash: "—", ndash: "–", hellip: "…" };
const decode = (text: string) =>
  text.replace(/&(#\d+|#x[0-9a-f]+|[a-z]+);/gi, (entity, name: string) => {
    if (name[0] === "#") return String.fromCodePoint(name[1] === "x" || name[1] === "X" ? parseInt(name.slice(2), 16) : Number(name.slice(1)));
    return ENTITIES[name.toLowerCase()] ?? entity;
  });

const LANGUAGE_NAMES: Record<string, string> = { sh: "Terminal", bash: "Terminal", zsh: "Terminal", js: "JavaScript", ts: "TypeScript", json: "JSON", jsonc: "JSON", text: "Text" };

function inline(tokens: Token[] | undefined): ReactNode {
  return tokens?.map((token, key) => {
    switch (token.type) {
      case "text":
        return "tokens" in token && token.tokens?.length ? <Fragment key={key}>{inline(token.tokens)}</Fragment> : decode(token.text);
      case "escape":
        return token.text;
      case "strong":
        return <strong key={key}>{inline((token as Tokens.Strong).tokens)}</strong>;
      case "em":
        return <em key={key}>{inline((token as Tokens.Em).tokens)}</em>;
      case "del":
        return <del key={key}>{inline((token as Tokens.Del).tokens)}</del>;
      case "codespan": {
        const topic = /^spotify docs ([a-z]+)$/.exec(token.text)?.[1];
        return topic && topics.some(([name]) => name === topic) ? <a key={key} href={`/docs/${topic}/`}><code>{token.text}</code></a> : <code key={key}>{token.text}</code>;
      }
      case "br":
        return <br key={key} />;
      case "link": {
        const link = token as Tokens.Link;
        const guide = /^(?:\.\/)?([a-z]+)\.md(#[^ ]*)?$/.exec(link.href);
        const href = guide && topics.some(([name]) => name === guide[1]) ? `/docs/${guide[1]}/${guide[2] ?? ""}` : link.href;
        return (
          <a key={key} href={href} title={link.title ?? undefined}>
            {inline(link.tokens)}
          </a>
        );
      }
      case "image": {
        const image = token as Tokens.Image;
        // eslint-disable-next-line @next/next/no-img-element -- a static export with unoptimized images
        return <img key={key} src={image.href} alt={image.text} title={image.title ?? undefined} />;
      }
      default:
        return "raw" in token ? token.raw : null;
    }
  });
}

function block(token: Token, key: number): ReactNode {
  switch (token.type) {
    case "space":
    case "def":
      return null;
    case "heading": {
      const heading = token as HeadingToken;
      return createElement(`h${heading.depth}`, { key, id: heading.id }, <a className="anchor" href={`#${heading.id}`}>{inline(heading.tokens)}</a>);
    }
    case "paragraph":
      return <p key={key}>{inline((token as Tokens.Paragraph).tokens)}</p>;
    case "text":
      return <Fragment key={key}>{"tokens" in token && token.tokens?.length ? inline(token.tokens) : decode(token.text)}</Fragment>;
    case "code": {
      const code = token as Tokens.Code;
      const language = (code.lang ?? "").split(/\s/)[0] || "text";
      return (
        <div className="code" key={key}>
          <CodeBlock code={code.text} language={language} filename={LANGUAGE_NAMES[language] ?? language} />
        </div>
      );
    }
    case "list": {
      const list = token as Tokens.List;
      const items = list.items.map((item, i) => <li key={i}>{blocks(item.tokens)}</li>);
      return list.ordered ? (
        <ol key={key} start={list.start !== "" && list.start !== 1 ? list.start : undefined}>
          {items}
        </ol>
      ) : (
        <ul key={key}>{items}</ul>
      );
    }
    case "table": {
      const table = token as Tokens.Table;
      const align = (i: number) => (table.align[i] ? { textAlign: table.align[i]! } : undefined);
      return (
        <div key={key} className="table-wrap" tabIndex={0} role="region" aria-label="Table">
          <table>
            <thead>
              <tr>
                {table.header.map((cell, i) => (
                  <th key={i} style={align(i)}>
                    {inline(cell.tokens)}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {table.rows.map((row, r) => (
                <tr key={r}>
                  {row.map((cell, i) => (
                    <td key={i} style={align(i)}>
                      {inline(cell.tokens)}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      );
    }
    case "blockquote":
      return <blockquote key={key}>{blocks((token as Tokens.Blockquote).tokens)}</blockquote>;
    case "hr":
      return <hr key={key} />;
    case "html":
      return <p key={key}>{token.text}</p>;
    default:
      return null;
  }
}

function blocks(tokens: Token[]): ReactNode {
  return tokens.map(block);
}

export default function Markdown({ tokens }: { tokens: Token[] }) {
  return <>{blocks(tokens)}</>;
}
