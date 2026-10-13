"use client";
import { useCallback, useEffect, useRef, useState } from "react";
import { SearchField } from "@/components/silicon-ui/search-field/search-field";
import { normalize, pattern, searchEntries, snippet } from "@/lib/search.mjs";

type Entry = { title: string; section: string | null; url: string; text: string };
let cachedIndex: Promise<Entry[]> | undefined;
function loadIndex() {
  return cachedIndex ??= fetch("/search-index.json").then(response => {
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    return response.json() as Promise<Entry[]>;
  }).catch(error => { cachedIndex = undefined; throw error; });
}
export default function DocsSearch() {
  const [value, setValue] = useState("");
  const [entries, setEntries] = useState<Entry[]>([]);
  const [message, setMessage] = useState("");
  const [open, setOpen] = useState(false);
  const input = useRef<HTMLInputElement>(null);
  const form = useRef<HTMLFormElement>(null);
  const revision = useRef(0);
  const latestValue = useRef("");
  const run = useCallback(async (next: string) => {
    latestValue.current = next;
    setValue(next);
    const current = ++revision.current;
    const query = normalize(next);
    if (!query) { setEntries([]); setMessage(""); setOpen(false); return; }
    setOpen(true); setEntries([]); setMessage("Loading the index…");
    try {
      const index = await loadIndex();
      if (revision.current !== current) return;
      const results = searchEntries(index, query) as Entry[];
      setEntries(results); setMessage(results.length ? "" : `No results for “${next.trim()}”.`);
    } catch {
      if (revision.current !== current) return;
      setEntries([]); setMessage("Search is unavailable right now. Offline: spotify docs");
    }
  }, []);
  useEffect(() => {
    const shortcut = (event: KeyboardEvent) => {
      if (event.key !== "/" || event.metaKey || event.ctrlKey || event.altKey || (event.target instanceof Element && event.target.closest("input, textarea, select, [contenteditable]"))) return;
      event.preventDefault(); input.current?.focus(); input.current?.select();
    };
    document.addEventListener("keydown", shortcut);
    return () => { document.removeEventListener("keydown", shortcut); revision.current++; };
  }, []);
  const query = normalize(value);
  return <form className="search" role="search" ref={form} onSubmit={event => {
    event.preventDefault(); if (entries[0]) { setOpen(false); location.assign(entries[0].url); }
  }} onKeyDown={event => {
    const links = [...(form.current?.querySelectorAll<HTMLAnchorElement>(".search-pop a") ?? [])];
    const at = links.indexOf(document.activeElement as HTMLAnchorElement);
    if (event.key === "ArrowDown" && links.length) {
      event.preventDefault(); if (!open) setOpen(true); else links[Math.min(at + 1, links.length - 1)].focus();
    } else if (event.key === "ArrowUp" && at >= 0) {
      event.preventDefault(); (links[at - 1] || input.current)?.focus();
    } else if (event.key === "Escape" && open) {
      event.preventDefault(); input.current?.focus(); setOpen(false);
    }
  }} onBlur={event => { if (!event.currentTarget.contains(event.relatedTarget)) setOpen(false); }}>
    <SearchField ref={input} label="Search the docs" value={value} onValueChange={next => { void run(next); }} onFocus={() => { void loadIndex().catch(() => {}); if (normalize(latestValue.current)) void run(latestValue.current); }} placeholder="Search the docs" autoComplete="off" spellCheck={false} enterKeyHint="go" aria-keyshortcuts="/" aria-controls="search-results" aria-expanded={open}/>
    <p className="sr-only" role="status">{message || (entries.length ? `${entries.length} result${entries.length === 1 ? "" : "s"}` : "")}</p>
    <div className="search-pop" id="search-results" hidden={!open} onMouseDown={event => event.preventDefault()}>
      {message && <p className="search-note">{message}</p>}
      <ul>{entries.map(entry => <li key={entry.url}><a href={entry.url} onClick={() => setOpen(false)}><strong>{entry.title}</strong>{entry.section && <span className="where"> › {entry.section}</span>}{entry.text && <span className="snippet">{snippet(entry.text, query).split(pattern(query.split(" "))).map((part: string, index: number) => index % 2 ? <mark key={index}>{part}</mark> : part)}</span>}</a></li>)}</ul>
    </div>
  </form>;
}
