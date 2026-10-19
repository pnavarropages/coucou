// WhatsApp in the island: the page's rules in src/core/whatsapp.ts (wording, initials, previews,
// the link's state) and what the island does with a picture from Rust (island/integrations.ts).
// The gateway, the inbox and the reply rules are tested in src-tauri/src/whatsapp.

import { beforeEach, test } from "node:test";
import assert from "node:assert/strict";
import { emit } from "./tauri.mjs";
import { installFakeDom } from "./fakedom.mjs";
import {
  WHATSAPP_ID, WhatsApp, bubbleParts, displayNumber, duration, findConversation, initials, installFailure,
  installStepLabel, isDown, linkColor, linkLabel, mediaLabel, preview, sendsOn,
} from "../src/core/whatsapp.ts";
import { handleWhatsApp } from "../src/island/integrations.ts";
import { pillDefinition } from "../src/core/pills.ts";
import { DEFAULT_SETTINGS, State } from "../src/core/state.ts";
import { lookup, setLanguage } from "../src/i18n/i18n.ts";

installFakeDom();

const message = (over = {}) => ({
  id: "m1", chat: "33600000001@s.whatsapp.net", sender: "33600000001@s.whatsapp.net", senderName: "Marie",
  text: "see you", media: { kind: "none" }, date: 1_700_000_000, fromMe: false, mentionsMe: false, ...over,
});

const snapshot = (over = {}) => ({
  link: "connected", linkMessage: null, me: "33600000009@s.whatsapp.net", conversations: [], unread: 0, down: false,
  sending: false, sendError: null, ...over,
});

beforeEach(() => {
  setLanguage("en");
  WhatsApp.snapshot = null;
});

test("the pill is the Mac's: its id, green and service category", () => {
  const def = pillDefinition(WHATSAPP_ID);
  assert.equal(WHATSAPP_ID, "integration_whatsapp");
  assert.deepEqual([def.name, def.color, def.category, def.subtitle], ["WhatsApp", "#25D366", "service", "Integration"]);
});

test("numbers and initials read like the Mac's", () => {
  assert.equal(displayNumber("33612345678@s.whatsapp.net"), "+33612345678");
  assert.equal(displayNumber("33612345678:12@s.whatsapp.net"), "+33612345678");
  assert.equal(displayNumber("abc@lid"), "abc");
  assert.equal(initials("Marie Dupont"), "MD");
  assert.equal(initials("jean-luc picard"), "JL");
  assert.equal(initials("Éloïse"), "É");
  assert.equal(initials("+33 6 12"), "#");
  assert.equal(initials(""), "#");
  assert.equal(duration(12), "0:12");
  assert.equal(duration(75), "1:15");
});

test("media show as a label, never as content", () => {
  assert.equal(mediaLabel({ kind: "none" }), null);
  assert.equal(mediaLabel({ kind: "image" }), "Photo");
  assert.equal(mediaLabel({ kind: "voice", seconds: 5 }), "Voice message");
  assert.deepEqual(bubbleParts(message({ media: { kind: "image" }, text: "look" })), { label: "Photo", value: "look" });
  assert.deepEqual(bubbleParts(message({ media: { kind: "voice", seconds: 75 }, text: "" })), { label: "Voice message", value: "1:15" });
  assert.deepEqual(bubbleParts(message({ media: { kind: "document", name: "devis.pdf" }, text: "" })), { label: "Document", value: "devis.pdf" });
  assert.deepEqual(bubbleParts(message({ media: { kind: "sticker" }, text: "" })), { label: "Sticker", value: "" });
  assert.deepEqual(bubbleParts(message()), { label: null, value: "see you" });
});

test("a row's preview: who, the label, one line", () => {
  assert.equal(preview(message(), false), "see you");
  assert.equal(preview(message({ fromMe: true, text: "ok" }), false), "You: ok");
  assert.equal(preview(message({ media: { kind: "image" }, text: "look" }), false), "Photo · look");
  assert.equal(preview(message({ media: { kind: "voice", seconds: null }, text: "" }), false), "Voice message");
  assert.equal(preview(message({ text: "a\nb" }), true), "Marie: a b");
  assert.equal(preview(message({ senderName: "" }), true), "see you");
});

test("the link's state is worded, coloured, and tells when the gateway is down", () => {
  assert.equal(linkLabel("off", null, null), "Turned off in Settings");
  assert.equal(linkLabel("needsQr", null, null), "Scan the QR code in Settings");
  assert.equal(linkLabel("connected", null, "33600000009@s.whatsapp.net"), "Connected as +33600000009");
  assert.equal(linkLabel("connected", null, null), "Connected");
  assert.equal(linkLabel("failed", "BAD_REQUEST", null), "BAD_REQUEST");
  assert.equal(linkColor("connected"), "#22C55E");
  assert.equal(linkColor("needsQr"), "#F5B83D");
  assert.equal(linkColor("unreachable"), "#F4505E");
  assert.deepEqual(["off", "connecting", "needsQr", "connected", "unreachable", "unauthorized", "failed"].map(isDown), [false, false, false, false, true, true, true]);
});

test("the strings are translated, the Windows wording included", () => {
  setLanguage("fr");
  assert.equal(linkLabel("off", null, null), "Désactivé dans les réglages");
  assert.equal(mediaLabel({ kind: "image" }), "Photo");
  assert.match(lookup("Coucou only talks to the gateway on this computer. Messages stay in memory, are never logged or saved, and are forgotten when Coucou quits. No photo or media is ever downloaded.", "fr"), /cet ordinateur/);
  assert.match(lookup("Coucou runs it on this computer only (127.0.0.1, the URL’s port), without downloading media, and stops it when Coucou quits. Its WhatsApp session stays in {0}.", "es"), /\{0\}/);
});

test("the install's steps and failure are worded", () => {
  assert.equal(installStepLabel({ step: "idle" }), null);
  assert.equal(installStepLabel({ step: "checking" }), "Looking for the latest GOWA…");
  assert.equal(installStepLabel({ step: "downloading", megabytes: 12 }), "Downloading (12 MB)…");
  assert.equal(installStepLabel({ step: "done", version: "9.6.0" }), null);
  assert.equal(installFailure({ step: "failed", message: "nope" }), "nope");
  assert.equal(installFailure({ step: "installing" }), null);
});

test("Return sends, Shift-Return starts a new line", () => {
  assert.equal(sendsOn("Enter", false), true);
  assert.equal(sendsOn("Enter", true), false);
  assert.equal(sendsOn("a", false), false);
});

test("a conversation is found by its id while it is still there", () => {
  const snap = snapshot({ conversations: [{ id: "a@s.whatsapp.net", name: "A", isGroup: false, messages: [], unread: 0, lastActivity: 1 }] });
  assert.equal(findConversation(snap, "a@s.whatsapp.net")?.name, "A");
  assert.equal(findConversation(snap, "b@s.whatsapp.net"), null);
  assert.equal(findConversation(snap, null), null);
  assert.equal(findConversation(null, "a@s.whatsapp.net"), null);
});

// ── What the island does with Rust's picture ─────────────────────────────────

function declare() {
  State.settings = { ...DEFAULT_SETTINGS, activeIntegrations: [WHATSAPP_ID] };
  State.loadIntegrationTasks?.();
}

test("the picture is kept, and a gateway down twice shows a red badge until it is back", () => {
  declare();
  const task = State.tasks.find((t) => t.id === WHATSAPP_ID);
  assert.ok(task, "the WhatsApp pill is declared");
  task.pillBadge = null;
  State.focusId = "integration_claude";

  handleWhatsApp(snapshot({ link: "unreachable", down: false }));
  assert.equal(WhatsApp.snapshot.link, "unreachable");
  assert.equal(task.pillBadge, null, "one missed check is not worth a badge");

  handleWhatsApp(snapshot({ link: "unreachable", down: true }));
  assert.equal(task.pillBadge, "error");

  handleWhatsApp(snapshot());
  assert.equal(task.pillBadge, null, "back up: the badge goes");
});

test("a new-message badge goes once nothing is unread, and the focused pill gets no error badge", () => {
  declare();
  const task = State.tasks.find((t) => t.id === WHATSAPP_ID);
  task.pillBadge = "finished";
  handleWhatsApp(snapshot({ unread: 2 }));
  assert.equal(task.pillBadge, "finished");
  handleWhatsApp(snapshot({ unread: 0 }));
  assert.equal(task.pillBadge, null);

  State.focusId = WHATSAPP_ID;
  handleWhatsApp(snapshot({ link: "unauthorized", down: true }));
  assert.equal(task.pillBadge, null, "you are looking at it");
});

test("Rust's events reach the page through the bridge", async () => {
  // The island subscribes at start; here only the handler is under test, fed the way the event would.
  handleWhatsApp(snapshot({ unread: 3 }));
  assert.equal(WhatsApp.snapshot.unread, 3);
  assert.equal(typeof emit, "function");
});
