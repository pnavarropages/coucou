// Settings → Integrations → WhatsApp (experimental), under the pill's switch: one click to
// install GOWA, the QR to link WhatsApp, what is shown and what is ignored. Port of
// WhatsAppSettingsSection (WhatsAppViews.swift). Off until the pill is among the active ones.
//
// Nothing here ever holds a message: the status comes from src-tauri/src/whatsapp without them.

import { Bridge, type WhatsAppStatus } from "../core/bridge";
import { displayNumber, installFailure, installStepLabel, linkColor, linkLabel } from "../core/whatsapp";
import { h } from "../views/dom";
import { t } from "../i18n/i18n";

type Toggle = (on: boolean, onChange: (v: boolean) => void) => HTMLElement;

const AMBER = "color:#f5b83d";
const MUTED = "color:#8e939c";
const RED = "color:#f4505e";

/**
 * The block under the WhatsApp row. `isOn` says whether the pill is among the active ones; the block
 * shows only then (`redraw` is called when the switch moves).
 */
export function whatsappBlock(makeToggle: Toggle, isOn: () => boolean): { el: HTMLElement; redraw: () => void } {
  const el = h("div", { style: "display:flex;flex-direction:column;gap:7px;margin-top:2px" });
  const installBox = h("div", { style: "display:flex;flex-direction:column;gap:6px" });
  const linkBox = h("div", { style: "display:flex;flex-direction:column;gap:6px" });
  const ignoredBox = h("div", { style: "display:flex;flex-direction:column;gap:4px" });
  let last = "";
  let built = false;
  let status: WhatsAppStatus | null = null;

  const hint = (text: string, style = MUTED) => h("div", { class: "hint", style, text });

  // ── Install ────────────────────────────────────────────────────────────────

  function drawInstall(s: WhatsAppStatus) {
    installBox.replaceChildren();
    const busy = installStepLabel(s.install);
    if (busy != null) {
      installBox.append(h("div", { class: "row" }, h("span", { class: "spinner" }), h("span", { style: "font-size:11.5px", text: busy })));
      return;
    }
    if (s.installed) {
      const done = s.version
        ? t("Gateway GOWA {0} installed, started by Coucou", { 0: s.version })
        : t("Your GOWA gateway is set up");
      installBox.append(
        h("div", { class: "row" },
          h("span", { style: "color:#25d366", text: "✓" }),
          h("span", { style: "font-size:11.5px;flex:1 1 auto", text: done }),
          h("button", { text: t("Update"), onclick: () => void Bridge.whatsappInstall() }),
        ),
      );
    } else {
      const install = h("button", { text: t("Install WhatsApp"), onclick: () => void Bridge.whatsappInstall() });
      install.disabled = !s.supported;
      installBox.append(
        h("div", { class: "row" }, install),
        hint(s.supported
          ? t("Coucou downloads GOWA, the open-source gateway, from its GitHub page (about 15 MB, once), checks it and starts it. Then scan the QR code with your phone. No Terminal needed.")
          : t("No GOWA build for this computer was found on GitHub. Try again later, or set your own gateway under Advanced.")),
      );
    }
    const failure = installFailure(s.install);
    if (failure != null) installBox.append(hint(failure, RED));
  }

  // ── Link and QR ────────────────────────────────────────────────────────────

  function drawLink(s: WhatsAppStatus) {
    linkBox.replaceChildren();
    // Before the install, "the gateway doesn't answer" would only confuse.
    if (!s.installed && s.link !== "connected" && s.link !== "needsQr") return;
    linkBox.append(
      h("div", { class: "row", style: "gap:6px" },
        h("i", { style: `width:7px;height:7px;border-radius:50%;background:${linkColor(s.link)};flex:0 0 auto` }),
        h("span", { style: "font-size:11.5px", text: linkLabel(s.link, s.linkMessage, s.me) }),
      ),
    );
    if (s.link === "connected") linkBox.append(hint(s.webhookLine));
    if (s.link === "needsQr") {
      if (s.qr) {
        linkBox.append(
          h("div", { style: "display:flex;gap:12px;align-items:flex-start" },
            h("img", {
              src: s.qr, width: 168, height: 168, alt: "",
              style: "background:#fff;border-radius:6px;image-rendering:pixelated;flex:0 0 auto",
            }),
            hint(t("On your phone: WhatsApp › Settings › Linked devices › Link a device, then scan this code.")),
          ),
        );
      } else {
        linkBox.append(h("span", { class: "spinner" }));
      }
    }
  }

  // ── Ignored chats ──────────────────────────────────────────────────────────

  function drawIgnored(s: WhatsAppStatus) {
    ignoredBox.replaceChildren(h("div", { style: "font-size:11.5px;font-weight:500", text: t("Ignored chats") }));
    for (const chat of s.ignored) {
      ignoredBox.append(
        h("div", { class: "row", style: "gap:8px" },
          h("span", { style: "font-size:11.5px;flex:1 1 auto", text: chat.name, title: displayNumber(chat.id) }),
          h("button", { text: t("Remove"), onclick: () => void Bridge.whatsappUnignore(chat.id).then(refresh) }),
        ),
      );
    }
    const entry = h("input", { type: "text", placeholder: t("Phone number or chat id to ignore"), autocomplete: "off", spellcheck: "false", style: "flex:1 1 auto;min-width:0" }) as HTMLInputElement;
    const add = () => {
      const raw = entry.value.trim();
      if (raw === "") return;
      // A phone number ("+33 6 12…") or a full chat id ("…@g.us").
      const chat = raw.includes("@") ? raw : `${raw.replace(/\D/g, "")}@s.whatsapp.net`;
      if (chat.length <= "@s.whatsapp.net".length) return;
      entry.value = "";
      void Bridge.whatsappIgnore(chat).then(refresh);
    };
    entry.addEventListener("keydown", (e) => {
      if ((e as KeyboardEvent).key === "Enter") add();
    });
    ignoredBox.append(h("div", { class: "row" }, entry, h("button", { text: t("Add"), onclick: add })));
  }

  // ── Everything else, built once ────────────────────────────────────────────

  function build(s: WhatsAppStatus) {
    built = true;
    const change = async (patch: Partial<{ url: string; webhookPort: number; autoStart: boolean; markReadOnOpen: boolean; muteGroups: boolean }>, note?: HTMLElement) => {
      const now = status ?? s;
      try {
        await Bridge.whatsappSetPrefs({
          url: now.url, webhookPort: now.webhookPort, autoStart: now.autoStart,
          markReadOnOpen: now.markReadOnOpen, muteGroups: now.muteGroups, ...patch,
        });
        if (note) note.textContent = "";
      } catch (err) {
        if (note) note.textContent = String(err).replace(/^Error:\s*/, "");
      }
      await refresh();
    };

    const advancedNote = hint("", RED);
    const url = h("input", { type: "text", value: s.url, placeholder: "http://127.0.0.1:3000", autocomplete: "off", spellcheck: "false", style: "flex:1 1 auto;min-width:0" }) as HTMLInputElement;
    const port = h("input", { type: "number", min: "1024", max: "65535", value: String(s.webhookPort), style: "width:84px" }) as HTMLInputElement;
    const save = h("button", {
      text: t("Save WhatsApp"),
      onclick: () => void change({ url: url.value.trim(), webhookPort: Number(port.value) || s.webhookPort }, advancedNote),
    });
    const advanced = h("details", { style: "margin-top:2px" },
      h("summary", { style: "font-size:11.5px;font-weight:500;cursor:default", text: t("Advanced: your own gateway") }),
      h("div", { style: "display:flex;flex-direction:column;gap:6px;padding-top:6px" },
        h("div", { class: "row" }, h("label", { style: "min-width:104px", text: t("Gateway") }), url),
        h("div", { class: "row" }, h("label", { style: "min-width:104px", text: t("Coucou’s webhook port") }), port),
        h("div", { class: "row" },
          h("label", { style: "min-width:104px", text: t("Start GOWA automatically") }),
          makeToggle(s.autoStart, (v) => void change({ autoStart: v })),
        ),
        hint(t("Coucou runs it on this computer only (127.0.0.1, the URL’s port), without downloading media, and stops it when Coucou quits. Its WhatsApp session stays in {0}.", { 0: s.folder })),
        h("div", { class: "row" }, save),
        advancedNote,
      ),
    );

    el.append(
      hint(t("Unofficial integration: it goes through a gateway you run on this computer. Use it on your own account only. WhatsApp may restrict accounts that abuse it."), AMBER),
      installBox,
      linkBox,
      h("div", { class: "row" },
        h("label", { style: "flex:1 1 auto", text: t("Mute groups (unless I’m mentioned)") }),
        makeToggle(s.muteGroups, (v) => void change({ muteGroups: v })),
      ),
      h("div", { class: "row" },
        h("label", { style: "flex:1 1 auto", text: t("Mark as read when I open a conversation") }),
        makeToggle(s.markReadOnOpen, (v) => void change({ markReadOnOpen: v })),
      ),
      hint(t("Off: marked read only when you reply. Never when a message just arrives.")),
      ignoredBox,
      hint(t("Coucou only talks to the gateway on this computer. Messages stay in memory, are never logged or saved, and are forgotten when Coucou quits. No photo or media is ever downloaded.")),
      advanced,
    );
  }

  async function refresh() {
    const s = await Bridge.whatsappStatus();
    if (!s) return;
    status = s;
    if (!built) build(s);
    // Redrawn only when something changed: a QR code redrawn every two seconds would flicker.
    const key = JSON.stringify([s.install, s.installed, s.version, s.supported, s.link, s.linkMessage, s.me, s.webhook, s.webhookLine, s.qr, s.ignored]);
    if (key === last) return;
    last = key;
    drawInstall(s);
    drawLink(s);
    drawIgnored(s);
  }

  // Kept up to date while the window shows it and the pill is on; nothing runs otherwise.
  const watch = window.setInterval(() => {
    if (!el.isConnected) return window.clearInterval(watch);
    if (isOn() && el.style.display !== "none") void refresh();
  }, 2000);

  const redraw = () => {
    const on = isOn();
    el.style.display = on ? "flex" : "none";
    if (on) void refresh();
  };
  el.style.display = isOn() ? "flex" : "none";
  void refresh();
  return { el, redraw };
}

