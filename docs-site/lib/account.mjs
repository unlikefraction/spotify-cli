// Silicon Accounts: state + PKCE, serialized refresh, and idempotent retries.
export function createAccountController(update) {
  const api = "https://backend.spotify.unlikefraction.com/api/v1";
  const key = "spotify.accounts.session.v1";
  const pendingKey = "spotify.accounts.pending.v1";
  let disposed = false;
  const set = (state) => { if (!disposed) update((value) => ({ ...value, ...state })); };
  let busy = false;
  const read = () => {
    try { return JSON.parse(localStorage.getItem(key) || "null"); }
    catch { return null; }
  };
  const clear = () => { localStorage.removeItem(key); render(); };
  const render = () => {
    const session = read();
    set({ signedIn: !!session, status: session ? `Signed in as ${session.actor?.public_id || session.account?.id || "your account"}` : "Sign in as a Carbon or Silicon" });
  };
  const call = async (path, body, id = crypto.randomUUID(), token) => {
    const response = await fetch(`${api}${path}`, {
      method: body ? "POST" : "GET",
      headers: { ...(body ? { "Content-Type": "application/json", "Idempotency-Key": id } : {}), ...(token ? { Authorization: `Bearer ${token}` } : {}) },
      ...(body ? { body: JSON.stringify(body) } : {}),
    });
    const data = response.status === 204 ? null : await response.json();
    if (!response.ok) {
      const error = new Error(data.error?.message || `Sign-in request failed (${response.status})`);
      error.status = response.status;
      error.code = data.error?.code;
      throw error;
    }
    return data;
  };
  const save = (data) => {
    if (!data.access_token || !data.refresh_token || !(data.expires_in > 0)) throw new Error("The server returned an incomplete session.");
    localStorage.setItem(key, JSON.stringify({ ...data, expires_at: Date.now() + data.expires_in * 1000 }));
    render();
  };
  const locked = (work) => navigator.locks ? navigator.locks.request(key, work) : work();
  const sync = async () => {
    if (disposed || busy) return;
    busy = true;
    try {
      await locked(async () => {
        let session = read();
        if (!session) return render();
        const refreshExpiry = Date.parse(session.refresh_token_expires_at);
        if (Number.isFinite(refreshExpiry) && refreshExpiry <= Date.now()) return clear();
        if (session.expires_at <= Date.now() + 60000) {
          // Save the retry key before rotating a single-use refresh token.
          session.refresh_key ||= crypto.randomUUID();
          localStorage.setItem(key, JSON.stringify(session));
          save(await call("/auth/refresh", { refresh_token: session.refresh_token }, session.refresh_key));
          session = read();
        }
        const account = await call("/auth/me", null, null, session.access_token);
        localStorage.setItem(key, JSON.stringify({ ...session, ...account, access_token: session.access_token, refresh_token: session.refresh_token }));
        render();
      });
    } catch (error) {
      if (["invalid_grant", "session_revoked", "unauthorized", "not_authenticated"].includes(error.code) || error.status === 401) clear();
      set({ status: error.message + (read() ? " Your saved sign-in is retained; retry when connected." : "") });
    } finally { busy = false; }
  };
  const base64url = (bytes) => btoa(String.fromCharCode(...bytes)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  const signIn = async () => {
    set({ loginBusy: true });
    try {
      const verifier = base64url(crypto.getRandomValues(new Uint8Array(32)));
      const state = base64url(crypto.getRandomValues(new Uint8Array(32)));
      const redirect = `${location.origin}/`;
      const challenge = base64url(new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(verifier))));
      sessionStorage.setItem(pendingKey, JSON.stringify({ verifier, state, redirect, started: Date.now(), id: crypto.randomUUID() }));
      const url = new URL("https://accounts.teamofsilicons.com/authorize");
      url.search = new URLSearchParams({ app_id: "spotify", response_type: "code", redirect_uri: redirect, scope: "profile", state, code_challenge: challenge, code_challenge_method: "S256" });
      location.assign(url);
    } catch (error) { set({ status: error.message, loginBusy: false }); }
  };
  const signOut = async () => {
    set({ logoutBusy: true });
    try {
      await locked(async () => {
        const session = read();
        if (session) await call("/auth/logout", { token: session.refresh_token });
        clear();
      });
    } catch (error) { set({ status: `Could not sign out: ${error.message}. Retry to revoke this session.` }); }
    finally { set({ logoutBusy: false }); }
  };
  const finish = async () => {
    const params = new URLSearchParams(location.search);
    if (!params.has("code") && !params.has("error")) return sync();
    let pending;
    try { pending = JSON.parse(sessionStorage.getItem(pendingKey) || "null"); } catch { /* invalid state */ }
    if (!pending || pending.state !== params.get("state") || Date.now() - pending.started > 3600000) {
      history.replaceState(null, "", location.pathname);
      set({ status: "This sign-in did not start in this browser or has expired. Sign in again." });
      return;
    }
    if (params.has("error")) {
      history.replaceState(null, "", location.pathname);
      sessionStorage.removeItem(pendingKey);
      set({ status: params.get("error_description") || "Sign-in was cancelled." });
      return;
    }
    // Retain the code only in this tab until the idempotent exchange succeeds.
    pending.code = params.get("code");
    sessionStorage.setItem(pendingKey, JSON.stringify(pending));
    history.replaceState(null, "", location.pathname);
    await exchange(pending);
  };
  const exchange = async (pending) => {
    try {
      await locked(async () => save(await call("/auth/code", { code: pending.code, redirect_uri: pending.redirect, code_verifier: pending.verifier }, pending.id)));
      sessionStorage.removeItem(pendingKey);
    } catch (error) {
      set({ status: `${error.message}. Reload to retry this sign-in.` });
    }
  };
  const start = () => {
    render();
    let pending;
    try { pending = JSON.parse(sessionStorage.getItem(pendingKey) || "null"); } catch { /* no pending sign-in */ }
    if (pending?.code && Date.now() - pending.started <= 3600000) exchange(pending);
    else finish();
  };
  const storage = (event) => { if (event.key === key) render(); };
  const visible = () => { if (!document.hidden) sync(); };
  window.addEventListener("storage", storage);
  window.addEventListener("online", sync);
  document.addEventListener("visibilitychange", visible);
  const interval = setInterval(visible, 60000);
  // Defer initial I/O so React Strict Mode's discarded effect never exchanges a code twice.
  const initial = setTimeout(start, 0);
  return { signIn, signOut, dispose() {
    disposed = true;
    clearTimeout(initial); clearInterval(interval);
    window.removeEventListener("storage", storage);
    window.removeEventListener("online", sync);
    document.removeEventListener("visibilitychange", visible);
  } };
}
