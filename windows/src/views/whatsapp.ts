// The WhatsApp card (overview, left card) — DOM port of WhatsAppCardView, WhatsAppRow and
// WhatsAppConversationView (WhatsAppViews.swift): one row per conversation with initials, name,
// the last message cut short and the unread count; a click opens the last ten messages and a
// reply field. Return sends, Shift-Return starts a new line, Escape goes back.
//
// Nothing is sent without the user's own Return or click, and the card never shows more than
// Rust gives it: the conversations live there, in memory only. Like Spotify's card it is kept
// and updated in place, so the reply being typed is never rebuilt under the user's fingers.

import { h, svg, clear, dot } from "./dom";
import { ICONS } from "./icons";
import { Bridge } from "../core/bridge";
import type { WhatsAppConversation, WhatsAppMessage, WhatsAppSnapshot } from "../core/bridge";
import { State } from "../core/state";
import {
  WHATSAPP_GREEN, WhatsApp, bubbleParts, displayNumber, findConversation, initials, linkColor, linkLabel, preview, sendsOn,
} from "../core/whatsapp";
import { timeAgo } from "./integrations";
import { t } from "../i18n/i18n";

export interface WhatsAppCardHost {
  openSettings(): void;
  /** Keeps the island open while a reply is being typed, and lets it fold again after. */
  hold(on: boolean): void;
}

export interface WhatsAppCardView {
  el: HTMLElement;
  /** Draws the last picture; cheap when nothing changed. */
  sync(): void;
  /** The card went off screen (another pill, the island folded): the conversation closes. */
  leave(): void;
}

function avatar(name: string, isGroup: boolean, size: number): HTMLElement {
  const letters = initials(name);
  const el = h("i", { class: "wa-avatar", text: isGroup && letters === "#" ? "👥" : letters });
  el.style.width = `${size}px`;
  el.style.height = `${size}px`;
  el.style.fontSize = `${Math.round(size * 0.42)}px`;
  return el;
}

function unreadBadge(count: number): HTMLElement {
  return h("i", { class: "wa-count", text: count > 99 ? "99+" : String(count) });
}

export function buildWhatsAppCard(host: WhatsAppCardHost): WhatsAppCardView {
  const el = h("div", { class: "wa-card" });
  /** The conversation open in the card. */
  let openChat: string | null = null;
  /** What the card last drew, to redraw only when it changed. */
  let drawn = "";
  let holding = false;
  let draft: HTMLTextAreaElement | null = null;
  let sendButton: HTMLButtonElement | null = null;
  let errorLine: HTMLElement | null = null;

  function hold(on: boolean) {
    if (on === holding) return;
    // An island the user pinned (Ctrl+P) stays pinned: only a hold of ours is released.
    if (on && State.isPinned) return;
    holding = on;
    host.hold(on);
  }

  function closeConversation() {
    if (openChat != null) void Bridge.whatsappClose(openChat);
    openChat = null;
    draft = null;
    sendButton = null;
    errorLine = null;
    hold(false);
    void Bridge.focusWindow(false);
    drawn = "";
    State.notify();
  }

  function openConversation(chat: string) {
    openChat = chat;
    drawn = "";
    void Bridge.whatsappOpen(chat);
    // The island takes the keyboard for the reply field, as it does for the chat.
    void Bridge.focusWindow(true);
    State.notify();
    window.setTimeout(() => draft?.focus(), 120);
  }

  // ── The list ───────────────────────────────────────────────────────────────

  function header(snapshot: WhatsAppSnapshot | null): HTMLElement {
    const row = h("div", { class: "wa-head" }, dot(WHATSAPP_GREEN, 7), h("b", { text: "WhatsApp" }));
    const unread = snapshot?.link === "connected" ? snapshot.unread : 0;
    if (unread > 0) row.append(h("span", { class: "wa-unread", text: t("{0} unread", { 0: unread }) }));
    return row;
  }

  function conversationRow(c: WhatsAppConversation): HTMLElement {
    const last = c.messages[c.messages.length - 1];
    const row = h("button", {
      class: c.unread > 0 ? "wa-row unread" : "wa-row",
      title: displayNumber(c.id),
      onclick: () => openConversation(c.id),
      oncontextmenu: (e: Event) => {
        // Right-click: ignore this chat, as the Mac's context menu does.
        e.preventDefault();
        void Bridge.whatsappIgnore(c.id);
      },
    });
    row.append(
      avatar(c.name, c.isGroup, 16),
      h("span", { class: "wa-name", text: c.name }),
      h("span", { class: "wa-ago", text: timeAgo(c.lastActivity * 1000) }),
      h("span", { class: "wa-last", text: last ? preview(last, c.isGroup) : "" }),
    );
    if (c.unread > 0) row.append(unreadBadge(c.unread));
    return row;
  }

  function drawList(snapshot: WhatsAppSnapshot | null) {
    const link = snapshot?.link ?? "off";
    // The "5m" beside a conversation moves on with the clock, so the minute is part of the key.
    const key = `list~${Math.floor(Date.now() / 60_000)}~${JSON.stringify(snapshot)}`;
    if (key === drawn) return;
    drawn = key;
    clear(el);
    el.append(header(snapshot));
    if (snapshot && link === "connected") {
      const rows = h("div", { class: "wa-rows" });
      if (snapshot.conversations.length === 0) {
        rows.append(h("div", { class: "int-empty", text: t("No new messages") }));
      }
      for (const c of snapshot.conversations) rows.append(conversationRow(c));
      el.append(rows);
      return;
    }
    // The gateway isn't linked: the card says why and leads to Settings.
    el.append(
      h("div", { class: "int-status" }, dot(linkColor(link), 5), h("span", { text: linkLabel(link, snapshot?.linkMessage ?? null, snapshot?.me ?? null) })),
      h("div", { class: "int-actions" }, h("button", { class: "link-btn", style: "color:#8e939c", text: t("Settings…"), onclick: host.openSettings })),
    );
  }

  // ── A conversation ─────────────────────────────────────────────────────────

  function bubble(message: WhatsAppMessage, isGroup: boolean): HTMLElement {
    const { label, value } = bubbleParts(message);
    const body = h("div", { class: message.fromMe ? "wa-bubble mine" : "wa-bubble" });
    if (isGroup && !message.fromMe && message.senderName !== "") body.append(h("div", { class: "wa-who", text: message.senderName }));
    if (label != null) body.append(h("div", { class: "wa-media", text: label }));
    if (value !== "") body.append(h("div", { class: "wa-text", text: value }));
    return h("div", { class: message.fromMe ? "wa-line mine" : "wa-line" }, body);
  }

  function canSend(): boolean {
    return (draft?.value.trim() ?? "") !== "" && !WhatsApp.snapshot?.sending;
  }

  function refreshComposer() {
    if (!draft || !sendButton) return;
    const sending = WhatsApp.snapshot?.sending ?? false;
    draft.disabled = sending;
    sendButton.disabled = !canSend();
    sendButton.classList.toggle("ready", canSend());
    hold((draft.value.trim() !== ""));
  }

  async function send() {
    const chat = openChat;
    if (chat == null || !draft || !canSend()) return;
    const text = draft.value;
    try {
      await Bridge.whatsappSend(chat, text);
      if (draft && openChat === chat) draft.value = "";
    } catch {
      // The reason is in the picture (sendError); the draft stays.
    }
    refreshComposer();
    draft?.focus();
  }

  function drawConversation(c: WhatsAppConversation, snapshot: WhatsAppSnapshot) {
    const shown = c.messages.slice(-10);
    const key = `chat~${c.id}~${c.name}~${shown.map((m) => m.id).join(",")}~${snapshot.link}~${snapshot.sending}~${snapshot.sendError ?? ""}`;
    if (key === drawn && draft) {
      refreshComposer();
      return;
    }
    const keep = draft?.value ?? "";
    const hadFocus = document.activeElement === draft;
    drawn = key;
    clear(el);

    const back = h("button", { class: "int-back", onclick: closeConversation, title: t("Back") }, svg(ICONS.chevronLeft, 10, { stroke: 2.4 }));
    const messages = h("div", { class: "wa-messages" });
    for (const m of shown) messages.append(bubble(m, c.isGroup));

    draft = h("textarea", {
      class: "wa-draft",
      rows: 1,
      placeholder: t("Reply…  (⇧↩ new line)"),
      spellcheck: "false",
    }) as HTMLTextAreaElement;
    draft.value = keep;
    sendButton = h("button", { class: "wa-send", onclick: () => void send() }, svg(ICONS.arrowUp, 11)) as HTMLButtonElement;
    draft.addEventListener("input", refreshComposer);
    draft.addEventListener("keydown", (e) => {
      const event = e as KeyboardEvent;
      if (sendsOn(event.key, event.shiftKey)) {
        event.preventDefault();
        void send();
      } else if (event.key === "Escape") {
        // Back to the list, not out of the island.
        event.preventDefault();
        closeConversation();
      }
      event.stopPropagation();
    });
    errorLine = h("div", { class: "wa-error" });
    el.append(
      h("div", { class: "wa-chat-head" }, back, avatar(c.name, c.isGroup, 16), h("b", { text: c.name, title: displayNumber(c.id) })),
      messages,
      h("div", { class: "wa-composer" }, draft, sendButton),
      errorLine,
    );
    if (snapshot.link !== "connected") {
      errorLine.style.color = "#F5B83D";
      errorLine.textContent = linkLabel(snapshot.link, snapshot.linkMessage, snapshot.me);
    } else if (snapshot.sendError) {
      errorLine.textContent = snapshot.sendError;
    }
    messages.scrollTop = messages.scrollHeight;
    refreshComposer();
    if (hadFocus) draft.focus();
  }

  return {
    el,
    sync() {
      const snapshot = WhatsApp.snapshot;
      // Kept open through a missed check (the draft too): only gone when WhatsApp is off or the
      // conversation is.
      const conversation = snapshot && snapshot.link !== "off" ? findConversation(snapshot, openChat) : null;
      if (openChat != null && !conversation) {
        closeConversation();
      }
      if (conversation && snapshot) drawConversation(conversation, snapshot);
      else drawList(snapshot);
    },
    leave() {
      if (openChat != null) closeConversation();
    },
  };
}
