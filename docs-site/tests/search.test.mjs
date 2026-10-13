import { test } from "node:test";
import assert from "node:assert/strict";
import { searchEntries } from "../lib/search.mjs";
const entries = [
  { title: "Playback", section: "Volume", text: "Muting the player", url: "/playback" },
  { title: "Triggers", section: "Ting delivery", text: "Send a Ting when a trigger fires", url: "/triggers" },
  { title: "Queue", section: "Play next", text: "Queue tracks and trigger playback", url: "/queue" },
];
test("matches complete query terms at word starts", () => {
  assert.deepEqual(searchEntries(entries, "ting").map(entry => entry.url), ["/triggers"]);
  assert.deepEqual(searchEntries(entries, "TRIG   ting").map(entry => entry.url), ["/triggers"]);
  assert.deepEqual(searchEntries(entries, "trig missing"), []);
});
test("ranks title hits before text-only hits and treats special characters literally", () => {
  assert.deepEqual(searchEntries(entries, "trig").map(entry => entry.url), ["/triggers", "/queue"]);
  assert.deepEqual(searchEntries(entries, ".*"), []);
  assert.deepEqual(searchEntries(entries, "  "), []);
});
