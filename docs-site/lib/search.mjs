// Prefix matching keeps “trig” useful without letting “ting” match “muting”.
export const pattern = (terms) => new RegExp(`(?<![\\p{L}\\p{N}_])(${terms.map((term) => term.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|")})`, "giu");
export const normalize = (value) => value.trim().toLowerCase().replace(/\s+/g, " ");
export function snippet(text, query) {
  const terms = normalize(query).split(" ");
  let at = text.toLowerCase().indexOf(query);
  if (at < 0) at = text.search(pattern(terms));
  const start = at > 40 ? text.lastIndexOf(" ", at - 30) + 1 : 0;
  return `${start ? "…" : ""}${text.slice(start, start + 150)}${start + 150 < text.length ? "…" : ""}`;
}
export function searchEntries(entries, input) {
  const query = normalize(input);
  if (!query) return [];
  const terms = query.split(" ");
  const patterns = terms.map((term) => pattern([term]));
  const scored = [];
  for (const entry of entries) {
    const section = entry.section || "";
    let score = 0;
    for (const re of patterns) {
      const hits = Math.min(entry.text.match(re)?.length ?? 0, 5);
      const points = (entry.title.search(re) >= 0 ? 8 : 0) + (section.search(re) >= 0 ? 6 : 0) + hits;
      if (!points) { score = 0; break; }
      score += points;
    }
    if (score && terms.length > 1 && `${section} ${entry.text}`.toLowerCase().includes(query)) score += 6;
    if (score) scored.push({ score, entry });
  }
  return scored.sort((a, b) => b.score - a.score).slice(0, 8).map(({ entry }) => entry);
}
