// Progressive enhancements only: copy buttons on code blocks and the installer, docs search, and
// the telemetry opt-out toggle (localStorage). Pages work without it.
(() => {
  const copy = async (text, button) => {
    try { await navigator.clipboard.writeText(text); button.textContent = "Copied"; }
    catch { button.textContent = "Select & copy"; }
    setTimeout(() => (button.textContent = "Copy"), 1500);
  };
  document.querySelectorAll("button[data-copy]").forEach((b) =>
    b.addEventListener("click", () => copy(document.getElementById(b.dataset.copy).textContent, b)));
  // Code blocks come with a header strip (build.mjs); the button goes there, never over the code.
  document.querySelectorAll(".article .codeblock").forEach((block) => {
    const b = document.createElement("button");
    b.type = "button"; b.className = "copy"; b.textContent = "Copy";
    b.addEventListener("click", () => copy(block.querySelector("pre").textContent.replace(/\n$/, ""), b));
    block.querySelector(".codeblock-head").append(b);
  });

  // Docs search over search-index.json (one entry per guide section), fetched on first focus.
  // "/" focuses it; arrow keys move through results; Enter opens the first; Escape closes.
  const form = document.querySelector("form.search");
  if (form) {
    const input = form.querySelector("input");
    const status = form.querySelector("[role=status]");
    const pop = form.querySelector(".search-pop");
    const note = pop.querySelector(".search-note");
    const list = pop.querySelector("ul");
    let index, entries;
    const load = () => (index ||= fetch("/search-index.json")
      .then((r) => (r.ok ? r.json() : Promise.reject(new Error(`HTTP ${r.status}`))))
      .then((data) => (entries = data))
      .catch((e) => { index = undefined; throw e; }));
    const show = (message, items = []) => {
      note.textContent = message;
      note.hidden = !message;
      list.replaceChildren(...items);
      pop.hidden = !message && !items.length;
      status.textContent = message || (items.length ? `${items.length} result${items.length === 1 ? "" : "s"}` : "");
    };
    const normal = () => input.value.trim().toLowerCase().replace(/\s+/g, " ");
    // Terms match at the start of a word ("trig" finds "trigger", "ting" does not find "muting").
    const pattern = (terms) => new RegExp(`(?<![\\p{L}\\p{N}_])(${terms.map((t) => t.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|")})`, "giu");
    const snippet = (text, terms, query) => {
      let at = text.toLowerCase().indexOf(query);
      if (at < 0) at = text.search(pattern(terms));
      const start = at > 40 ? text.lastIndexOf(" ", at - 30) + 1 : 0;
      return `${start ? "…" : ""}${text.slice(start, start + 150)}${start + 150 < text.length ? "…" : ""}`;
    };
    const result = (entry, terms, query) => {
      const a = document.createElement("a");
      a.href = entry.url;
      const title = document.createElement("strong");
      title.textContent = entry.title;
      a.append(title);
      if (entry.section) {
        const where = document.createElement("span");
        where.className = "where";
        where.textContent = ` › ${entry.section}`;
        a.append(where);
      }
      if (entry.text) {
        const text = document.createElement("span");
        text.className = "snippet";
        // Split on a capturing group: odd parts are the matches.
        text.append(...snippet(entry.text, terms, query).split(pattern(terms)).map((part, i) => {
          if (!(i % 2)) return part;
          const m = document.createElement("mark");
          m.textContent = part;
          return m;
        }));
        a.append(text);
      }
      const li = document.createElement("li");
      li.append(a);
      return li;
    };
    const run = async () => {
      const query = normal();
      if (!query) return show("");
      if (!entries) {
        show("Loading the index…");
        try { await load(); } catch { return show("Search is unavailable right now. Offline: spotify docs"); }
        if (query !== normal()) return; // a newer keystroke is already searching
      }
      const terms = query.split(" ");
      const patterns = terms.map((term) => pattern([term]));
      const scored = [];
      for (const entry of entries) {
        const section = entry.section || "";
        let score = 0;
        for (const re of patterns) {
          const hits = Math.min(entry.text.match(re)?.length ?? 0, 5);
          const points = (entry.title.search(re) >= 0 ? 8 : 0) + (section.search(re) >= 0 ? 6 : 0) + hits;
          if (!points) { score = 0; break; } // every term must match somewhere
          score += points;
        }
        if (score && terms.length > 1 && `${section} ${entry.text}`.toLowerCase().includes(query)) score += 6;
        if (score) scored.push([score, entry]);
      }
      scored.sort((a, b) => b[0] - a[0]);
      const items = scored.slice(0, 8).map(([, entry]) => result(entry, terms, query));
      show(items.length ? "" : `No results for “${input.value.trim()}”.`, items);
    };
    const links = () => [...list.querySelectorAll("a")];

    input.addEventListener("focus", () => { load().catch(() => {}); if (normal()) run(); });
    input.addEventListener("input", run);
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      const first = links()[0];
      if (!first) return;
      pop.hidden = true; // a result on this page only scrolls, so the list must not stay over it
      location.assign(first.href);
    });
    form.addEventListener("keydown", (event) => {
      const all = links();
      const at = all.indexOf(document.activeElement);
      if (event.key === "ArrowDown" && all.length) {
        event.preventDefault();
        if (pop.hidden) pop.hidden = false; // reopen what Escape closed
        else all[Math.min(at + 1, all.length - 1)].focus();
      } else if (event.key === "ArrowUp" && at >= 0) {
        event.preventDefault();
        (all[at - 1] || input).focus();
      } else if (event.key === "Escape" && !pop.hidden) {
        event.preventDefault();
        pop.hidden = true;
        input.focus();
      }
    });
    // Keep focus in the input while a result is clicked, so focusout does not close the list first.
    pop.addEventListener("mousedown", (event) => event.preventDefault());
    list.addEventListener("click", () => (pop.hidden = true));
    form.addEventListener("focusout", (event) => { if (!form.contains(event.relatedTarget)) pop.hidden = true; });
    document.addEventListener("keydown", (event) => {
      if (event.key !== "/" || event.metaKey || event.ctrlKey || event.altKey) return;
      if (event.target.closest?.("input, textarea, select, [contenteditable]")) return;
      event.preventDefault();
      input.focus();
      input.select();
    });
    form.hidden = false;
  }

  // Telemetry: anonymous page views and install-command copies, sent to the backend gateway (keys
  // stay server-side). On by default; off when the browser sends Global Privacy Control or Do Not
  // Track, or when the footer toggle is off (remembered in localStorage; no cookies).
  const KEY = "spotify-cli.telemetry";
  const OLD_KEY = "silicon-spotify.telemetry"; // the key before the rename, migrated once
  let choice = null;
  try {
    choice = localStorage.getItem(KEY);
    const old = localStorage.getItem(OLD_KEY);
    if (old !== null) {
      if (choice === null) localStorage.setItem(KEY, (choice = old));
      localStorage.removeItem(OLD_KEY);
    }
  } catch {}
  const refused = navigator.globalPrivacyControl === true ? "Global Privacy Control" : navigator.doNotTrack === "1" ? "Do Not Track" : null;
  let enabled = !refused && choice !== "off";
  const toggle = document.getElementById("telemetry-toggle");
  if (toggle) {
    toggle.checked = enabled;
    if (refused) {
      toggle.disabled = true;
      document.getElementById("telemetry-note").textContent = `: off, because your browser sends ${refused}`;
    }
    toggle.addEventListener("change", () => {
      enabled = toggle.checked;
      try { localStorage.setItem(KEY, enabled ? "on" : "off"); } catch {}
    });
    toggle.closest(".telemetry").hidden = false;
  }
  const endpoint = "https://backend.spotify.unlikefraction.com/api/v1/telemetry";
  const send = (table, type, data) => {
    if (!enabled || refused) return;
    const body = JSON.stringify({ table, events: [{ id: crypto.randomUUID(), type, data, metadata: { path: location.pathname, referrer: document.referrer ? new URL(document.referrer).host : null, viewport: innerWidth + "x" + innerHeight } }] });
    try { fetch(endpoint, { method: "POST", headers: { "content-type": "application/json", "x-spotify-source": "web" }, body, keepalive: true, mode: "cors" }).catch(() => {}); } catch {}
  };
  send("spotifyfrontendanalytics", "page_view", {});
  document.querySelectorAll("button[data-copy]").forEach((b) => b.addEventListener("click", () => send("spotifyfrontendevents", "install_command_copied", {})));
})();
