// WhatsApp in the island — what the page keeps of src-tauri/src/whatsapp, and the small pure
// helpers that word it (WhatsAppViews.swift on the Mac: waPreview, waTimeAgo, WAJID.initials).
//
// The conversations live in Rust, in memory only; the island hears of every change as a
// "whatsapp-state" event and shows them. Nothing is stored here beyond the last picture.

import type {
  WhatsAppConversation, WhatsAppInstall, WhatsAppLink, WhatsAppMedia, WhatsAppMessage, WhatsAppSnapshot,
} from "./bridge";
import { t } from "../i18n/i18n";

export const WHATSAPP_ID = "integration_whatsapp";

/** WhatsApp's own green: the pill keeps it whatever, like the brand. */
export const WHATSAPP_GREEN = "#25D366";

/** The last picture Rust sent (null before the first). */
export const WhatsApp = {
  snapshot: null as WhatsAppSnapshot | null,
};

/** "33612345678@s.whatsapp.net" → "+33612345678". */
export function displayNumber(jid: string): string {
  const user = jid.split("@")[0] ?? jid;
  const digits = user.split(":")[0] ?? user;
  return digits !== "" && /^\d+$/.test(digits) ? `+${digits}` : digits;
}

/** Two letters for the avatar: "Marie Dupont" → "MD", "+33 6…" → "#". */
export function initials(name: string): string {
  const words = name.split(/[ -]/).filter((w) => /^\p{L}/u.test(w));
  const letters = words.slice(0, 2).map((w) => Array.from(w)[0]!.toUpperCase());
  return letters.length > 0 ? letters.join("") : "#";
}

/** 12 → "0:12", 75 → "1:15". */
export function duration(seconds: number): string {
  const s = Math.max(0, Math.floor(seconds));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

/** The label a message's media shows (its content is never fetched), or null for plain text. */
export function mediaLabel(media: WhatsAppMedia): string | null {
  switch (media.kind) {
    case "none": return null;
    case "image": return t("Photo");
    case "video": return t("Video");
    case "audio":
    case "voice": return t("Voice message");
    case "document": return t("Document");
    case "sticker": return t("Sticker");
    case "location": return t("Location");
    case "contact": return t("Contact");
    case "other": return t("Message");
  }
}

/** What follows a media label: the text, a voice note's length, a document's name. */
function mediaValue(message: WhatsAppMessage): string {
  const media = message.media;
  switch (media.kind) {
    case "audio":
    case "voice": return media.seconds != null ? duration(media.seconds) : "";
    case "document": return media.name ?? message.text;
    case "sticker":
    case "location":
    case "contact": return "";
    default: return message.text;
  }
}

/** The message as the bubble shows it: the label (if media) and the text. */
export function bubbleParts(message: WhatsAppMessage): { label: string | null; value: string } {
  const label = mediaLabel(message.media);
  return { label, value: label == null ? message.text : mediaValue(message) };
}

/** "Marie: see you", "You: ok", "Photo · look", "Voice message": one line, media as a label only. */
export function preview(message: WhatsAppMessage, isGroup: boolean): string {
  const { label, value } = bubbleParts(message);
  let line = label == null ? value : value === "" ? label : `${label} · ${value}`;
  line = line.replace(/\n/g, " ");
  if (message.fromMe) return `${t("You")}: ${line}`;
  if (isGroup && message.senderName !== "") return `${message.senderName}: ${line}`;
  return line;
}

/** What the link's state reads like, in the card and in Settings. */
export function linkLabel(link: WhatsAppLink, message: string | null, me: string | null): string {
  switch (link) {
    case "off": return t("Turned off in Settings");
    case "connecting": return t("Connecting to the gateway…");
    case "needsQr": return t("Scan the QR code in Settings");
    case "connected": return me ? t("Connected as {0}", { 0: displayNumber(me) }) : t("Connected");
    case "unreachable": return t("The gateway doesn’t answer: is it running?");
    case "unauthorized": return t("The gateway refuses the credentials");
    case "failed": return message ?? t("The gateway doesn’t answer: is it running?");
  }
}

/** The dot next to the link's state. */
export function linkColor(link: WhatsAppLink): string {
  switch (link) {
    case "connected": return "#22C55E";
    case "connecting":
    case "needsQr": return "#F5B83D";
    case "off": return "#6B7079";
    default: return "#F4505E";
  }
}

/** Whether the gateway is down: the pill's small red badge. */
export function isDown(link: WhatsAppLink): boolean {
  return link === "unreachable" || link === "unauthorized" || link === "failed";
}

/** The conversation `id` in a picture, if it is still there. */
export function findConversation(snapshot: WhatsAppSnapshot | null, id: string | null): WhatsAppConversation | null {
  if (!snapshot || id == null) return null;
  return snapshot.conversations.find((c) => c.id === id) ?? null;
}

/** Whether Return sends (Shift-Return starts a new line). */
export function sendsOn(key: string, shift: boolean): boolean {
  return key === "Enter" && !shift;
}

/** The label of the install's current step, or null when nothing is going on. */
export function installStepLabel(install: WhatsAppInstall): string | null {
  switch (install.step) {
    case "checking": return t("Looking for the latest GOWA…");
    case "downloading": return t("Downloading ({0} MB)…", { 0: install.megabytes });
    case "verifying": return t("Checking the download…");
    case "installing": return t("Installing and starting…");
    default: return null;
  }
}

/** Why the last install failed, in a sentence (already worded by Rust), else null. */
export function installFailure(install: WhatsAppInstall): string | null {
  return install.step === "failed" ? install.message : null;
}
