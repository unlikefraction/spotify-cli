import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { setImmediate } from "node:timers/promises";
import { createAccountController } from "../lib/account.mjs";

const key = "spotify.accounts.session.v1";
const pendingKey = "spotify.accounts.pending.v1";
const origin = "https://spotify.unlikefraction.com";
const session = () => ({ access_token: "access", refresh_token: "refresh", expires_at: Date.now() + 300000, actor: { public_id: "carbon" } });
const pending = () => ({ verifier: "verifier", state: "state", redirect: `${origin}/`, started: Date.now(), id: "exchange-key" });
const response = (data, status = 200) => new Response(status === 204 ? null : JSON.stringify(data), { status });
const flush = () => setImmediate();

function browser(t, { saved, callback = "", transaction, replies = [] } = {}) {
  const storage = (initial) => {
    const entries = new Map(initial);
    return { getItem: (name) => entries.get(name) ?? null, setItem: (name, value) => entries.set(name, String(value)), removeItem: (name) => entries.delete(name) };
  };
  const localStorage = storage(saved ? [[key, JSON.stringify(saved)]] : []);
  const sessionStorage = storage(transaction ? [[pendingKey, JSON.stringify(transaction)]] : []);
  const window = new EventTarget();
  const document = Object.assign(new EventTarget(), { hidden: false });
  const location = { origin, pathname: "/", search: callback, assign: (url) => { location.assigned = new URL(url); } };
  const calls = [], locks = [], timeouts = new Map(), intervals = new Map(), controllers = [];
  let timer = 0, queue = Promise.resolve(), state = {}, updates = 0;
  const globals = {
    localStorage, sessionStorage, window, document, location,
    history: { replaceState: (_state, _title, path) => { location.pathname = path; location.search = ""; } },
    navigator: { locks: { request: (name, work) => {
      locks.push(name);
      const result = queue.then(work);
      queue = result.catch(() => {});
      return result;
    } } },
    fetch: async (url, options) => {
      calls.push({ url, ...options, body: options.body && JSON.parse(options.body) });
      assert.ok(replies.length, `Unexpected request to ${url}`);
      const reply = replies.shift();
      if (reply instanceof Error) throw reply;
      return typeof reply === "function" ? reply() : reply;
    },
    setTimeout: (work) => { const id = ++timer; timeouts.set(id, work); return id; },
    clearTimeout: (id) => timeouts.delete(id),
    setInterval: (work) => { const id = ++timer; intervals.set(id, work); return id; },
    clearInterval: (id) => intervals.delete(id),
  };
  const descriptors = Object.fromEntries(Object.keys(globals).map(name => [name, Object.getOwnPropertyDescriptor(globalThis, name)]));
  for (const [name, value] of Object.entries(globals)) Object.defineProperty(globalThis, name, { value, configurable: true, writable: true });
  t.after(() => {
    for (const controller of controllers) controller.dispose();
    for (const [name, descriptor] of Object.entries(descriptors)) {
      if (descriptor) Object.defineProperty(globalThis, name, descriptor);
      else delete globalThis[name];
    }
  });
  const create = () => {
    const controller = createAccountController(update => { state = update(state); updates++; });
    controllers.push(controller);
    return controller;
  };
  return {
    ...globals, create, calls, locks, timeouts, intervals,
    get state() { return state; }, get updates() { return updates; },
    saved: () => JSON.parse(localStorage.getItem(key)),
    pending: () => JSON.parse(sessionStorage.getItem(pendingKey)),
    async start() {
      for (const [id, work] of [...timeouts]) { timeouts.delete(id); work(); }
      await flush();
    },
    async event(target, type, properties = {}) {
      target.dispatchEvent(Object.assign(new Event(type), properties));
      await flush();
    },
  };
}

test("sign-in binds the authorization URL to a saved state and S256 verifier", async t => {
  const b = browser(t);
  const controller = b.create();
  await b.start();
  await controller.signIn();
  const transaction = b.pending(), url = b.location.assigned;
  assert.equal(url.origin, "https://accounts.teamofsilicons.com");
  assert.equal(url.pathname, "/authorize");
  assert.equal(url.searchParams.get("state"), transaction.state);
  assert.equal(url.searchParams.get("redirect_uri"), `${origin}/`);
  assert.equal(url.searchParams.get("app_id"), "spotify");
  assert.equal(url.searchParams.get("code_challenge_method"), "S256");
  assert.equal(url.searchParams.get("code_challenge"), createHash("sha256").update(transaction.verifier).digest("base64url"));
  assert.match(transaction.verifier, /^[A-Za-z0-9_-]{43}$/);
  assert.equal(b.state.loginBusy, true);
});

for (const [name, transaction] of [
  ["missing", () => undefined],
  ["mismatched", () => ({ ...pending(), state: "different" })],
  ["expired", () => ({ ...pending(), started: Date.now() - 3600001 })],
]) test(`rejects a callback with ${name} browser state without exchanging it`, async t => {
  const b = browser(t, { callback: "?code=code&state=state", transaction: transaction() });
  b.create(); await b.start();
  assert.equal(b.calls.length, 0);
  assert.equal(b.location.search, "");
  assert.match(b.state.status, /did not start in this browser or has expired/);
});

test("a cancelled authorization clears its transaction and displays the reason", async t => {
  const b = browser(t, { callback: "?error=access_denied&error_description=Cancelled&state=state", transaction: pending() });
  b.create(); await b.start();
  assert.equal(b.pending(), null);
  assert.equal(b.location.search, "");
  assert.equal(b.state.status, "Cancelled");
  assert.equal(b.calls.length, 0);
});

test("an interrupted code exchange strips the URL and retries with the same key and PKCE verifier", async t => {
  const b = browser(t, { callback: "?code=code&state=state", transaction: pending(), replies: [new Error("Offline"), response({ ...session(), expires_in: 300 })] });
  const first = b.create(); await b.start();
  assert.equal(b.location.search, "");
  assert.equal(b.pending().code, "code");
  assert.match(b.state.status, /Reload to retry/);
  first.dispose(); b.create(); await b.start();
  assert.equal(b.pending(), null);
  assert.equal(b.state.signedIn, true);
  assert.equal(b.saved().access_token, "access");
  assert.ok(b.saved().expires_at > Date.now());
  assert.equal(b.calls.length, 2);
  for (const call of b.calls) {
    assert.equal(call.url, "https://backend.spotify.unlikefraction.com/api/v1/auth/code");
    assert.equal(call.headers["Idempotency-Key"], "exchange-key");
    assert.deepEqual(call.body, { code: "code", redirect_uri: `${origin}/`, code_verifier: "verifier" });
  }
});

test("refresh retains its retry key on failure and uses the rotated token for the account request", async t => {
  const b = browser(t, { saved: { ...session(), expires_at: Date.now() }, replies: [new Error("Offline"), response({ access_token: "rotated-access", refresh_token: "rotated-refresh", expires_in: 300 }), response({ actor: { public_id: "silicon" } })] });
  b.create(); await b.start();
  const retryKey = b.saved().refresh_key;
  assert.ok(retryKey);
  assert.equal(b.saved().refresh_token, "refresh");
  assert.match(b.state.status, /saved sign-in is retained/);
  await b.event(b.window, "online");
  assert.equal(b.calls[0].headers["Idempotency-Key"], retryKey);
  assert.equal(b.calls[1].headers["Idempotency-Key"], retryKey);
  assert.equal(b.calls[2].headers.Authorization, "Bearer rotated-access");
  assert.equal(b.saved().refresh_token, "rotated-refresh");
  assert.equal(b.saved().refresh_key, undefined);
  assert.equal(b.state.status, "Signed in as silicon");
  assert.ok(b.locks.every(name => name === key));
});

test("an expired refresh token clears the session without a network request", async t => {
  const b = browser(t, { saved: { ...session(), refresh_token_expires_at: new Date(Date.now() - 1000).toISOString() } });
  b.create(); await b.start();
  assert.equal(b.saved(), null);
  assert.equal(b.state.signedIn, false);
  assert.equal(b.calls.length, 0);
});

test("a rejected account session is cleared", async t => {
  const b = browser(t, { saved: session(), replies: [response({ error: { code: "session_revoked", message: "Revoked" } }, 401)] });
  b.create(); await b.start();
  assert.equal(b.saved(), null);
  assert.equal(b.state.signedIn, false);
  assert.equal(b.state.status, "Revoked");
});

test("failed logout retains the session until revocation succeeds", async t => {
  const b = browser(t, { saved: session(), replies: [new Error("Offline"), response(null, 204)] });
  const controller = b.create();
  await controller.signOut();
  assert.equal(b.saved().refresh_token, "refresh");
  assert.equal(b.state.logoutBusy, false);
  assert.match(b.state.status, /Retry to revoke/);
  await controller.signOut();
  assert.equal(b.saved(), null);
  assert.equal(b.state.signedIn, false);
  assert.equal(b.state.logoutBusy, false);
  assert.ok(b.calls.every(call => call.url.endsWith("/auth/logout") && call.body.token === "refresh"));
});

test("Strict Mode cleanup cancels discarded startup and removes lifecycle listeners", async t => {
  const b = browser(t, { saved: session(), replies: [response({ actor: { public_id: "silicon" } })] });
  const discarded = b.create(); discarded.dispose();
  const current = b.create(); await b.start();
  assert.equal(b.calls.length, 1);
  assert.equal(b.intervals.size, 1);
  b.localStorage.removeItem(key);
  await b.event(b.window, "storage", { key });
  assert.equal(b.state.signedIn, false);
  current.dispose();
  const updates = b.updates;
  b.localStorage.setItem(key, JSON.stringify(session()));
  await b.event(b.window, "storage", { key });
  await b.event(b.window, "online");
  await b.event(b.document, "visibilitychange");
  assert.equal(b.calls.length, 1);
  assert.equal(b.updates, updates);
  assert.equal(b.timeouts.size, 0);
  assert.equal(b.intervals.size, 0);
});

test("visibility and minute timers refresh only a visible page and serialize overlapping syncs", async t => {
  let resolve;
  const b = browser(t, { saved: session(), replies: [() => new Promise(done => { resolve = done; }), response({ actor: { public_id: "silicon" } })] });
  b.create(); await b.start();
  await b.event(b.window, "online");
  assert.equal(b.calls.length, 1);
  resolve(response({ actor: { public_id: "carbon" } })); await flush();
  b.document.hidden = true;
  await b.event(b.document, "visibilitychange");
  for (const tick of b.intervals.values()) tick();
  await flush();
  assert.equal(b.calls.length, 1);
  b.document.hidden = false;
  for (const tick of b.intervals.values()) tick();
  await flush();
  assert.equal(b.calls.length, 2);
  assert.equal(b.state.status, "Signed in as silicon");
});

test("an in-flight response can save the session without updating a disposed React panel", async t => {
  let resolve;
  const b = browser(t, { saved: session(), replies: [() => new Promise(done => { resolve = done; })] });
  const controller = b.create(); await b.start();
  controller.dispose();
  const updates = b.updates;
  resolve(response({ actor: { public_id: "silicon" } })); await flush();
  assert.equal(b.saved().actor.public_id, "silicon");
  assert.equal(b.updates, updates);
});
