// Progressive enhancements only: copy buttons on code blocks and the installer, and the
// telemetry opt-out toggle (localStorage). Pages work without it.
(() => {
  const copy = async (text, button) => {
    try { await navigator.clipboard.writeText(text); button.textContent = "Copied"; }
    catch { button.textContent = "Select & copy"; }
    setTimeout(() => (button.textContent = "Copy"), 1500);
  };
  document.querySelectorAll("button[data-copy]").forEach((b) =>
    b.addEventListener("click", () => copy(document.getElementById(b.dataset.copy).textContent, b)));
  document.querySelectorAll(".article pre").forEach((pre) => {
    const b = document.createElement("button");
    b.type = "button"; b.className = "copy"; b.textContent = "Copy";
    b.style.cssText = "float:right;margin:-6px -8px 0 8px;padding:4px 10px;font-size:12px";
    b.addEventListener("click", () => copy(pre.innerText.replace(/^Copy\n?/, ""), b));
    pre.prepend(b);
  });

  // Telemetry: on by default, stored per browser, sent to the backend gateway (keys stay server-side).
  const KEY = "silicon-spotify.telemetry";
  let enabled = true;
  try { enabled = localStorage.getItem(KEY) !== "off"; } catch {}
  const toggle = document.getElementById("telemetry-toggle");
  if (toggle) {
    toggle.checked = enabled;
    toggle.addEventListener("change", () => {
      enabled = toggle.checked;
      try { localStorage.setItem(KEY, enabled ? "on" : "off"); } catch {}
    });
  }
  const endpoint = "https://backend.spotify.unlikefraction.com/api/v1/telemetry";
  const send = (table, type, data) => {
    if (!enabled || navigator.doNotTrack === "1") return;
    const body = JSON.stringify({ table, events: [{ id: crypto.randomUUID(), type, data, metadata: { path: location.pathname, referrer: document.referrer ? new URL(document.referrer).host : null, viewport: innerWidth + "x" + innerHeight } }] });
    try { fetch(endpoint, { method: "POST", headers: { "content-type": "application/json", "x-spotify-source": "web" }, body, keepalive: true, mode: "cors" }).catch(() => {}); } catch {}
  };
  send("spotifyfrontendanalytics", "page_view", {});
  document.querySelectorAll("button[data-copy]").forEach((b) => b.addEventListener("click", () => send("spotifyfrontendevents", "install_command_copied", {})));
})();
