// The Cloudflare and Hetzner cards (src/views/integrations.ts). The Rust half — what is asked of the
// two APIs and how the answers are read — is tested in src-tauri/src/cloudflare.rs and hetzner.rs.

import { test } from "node:test";
import assert from "node:assert/strict";
import { installFakeDom } from "./fakedom.mjs";

installFakeDom();

const { formatRate, hasIntegrationData, renderIntegrationCard } = await import("../src/views/integrations.ts");
const { State } = await import("../src/core/state.ts");

const CLOUDFLARE = "integration_cloudflare";
const HETZNER = "integration_hetzner";
const hooks = { detailOpen: false, openDetail() {}, closeDetail() {}, openSettings() {} };
const task = (id, name) => ({ id, name, color: "#fff", state: "idle", steps: [], stepIndex: 0 });

function load(id, data, error = null) {
  State.integrations[id] = { data, error, loaded: error == null, configured: true };
}

test("rates are written short", () => {
  assert.equal(formatRate(0), "0 B/s");
  assert.equal(formatRate(512.4), "512 B/s");
  assert.equal(formatRate(2048), "2.0 KB/s");
  assert.equal(formatRate(1.5 * 1024 * 1024), "1.5 MB/s");
  assert.equal(formatRate(-1), "");
  assert.equal(formatRate(Number.NaN), "");
});

test("until a poll has answered, both are the idle card; a failed poll keeps it", () => {
  for (const id of [CLOUDFLARE, HETZNER]) {
    delete State.integrations[id];
    assert.equal(hasIntegrationData(id), false);
    load(id, {}, "Invalid API token (401)");
    assert.equal(hasIntegrationData(id), false);
    const card = renderIntegrationCard(task(id, id), hooks);
    assert.ok(card.textContent.includes("Invalid API token (401)"), card.textContent);
  }
});

test("Cloudflare: Workers with their errors, then the state of the domains", () => {
  load(CLOUDFLARE, {
    workers: [
      { name: "relay", deployedAt: new Date().toISOString(), errors: 0, requests: 50 },
      { name: "api", deployedAt: new Date(Date.now() - 3 * 3600_000).toISOString(), errors: 8, requests: 1500 },
      { name: "cron", deployedAt: null, errors: null, requests: null },
      { name: "fourth", deployedAt: null },
    ],
    zones: [
      { name: "orbitra-studio.com", status: "active", paused: false },
      { name: "nuevo.example", status: "pending", paused: false },
    ],
    workersError: null, analyticsError: null, zonesError: null,
  });
  assert.equal(hasIntegrationData(CLOUDFLARE), true);
  const text = renderIntegrationCard(task(CLOUDFLARE, "Cloudflare"), hooks).textContent;
  assert.ok(text.includes("Cloudflare") && text.includes("relay") && text.includes("api") && text.includes("cron"), text);
  assert.ok(!text.includes("fourth"), "three rows at most");
  assert.ok(text.includes("Errors: 8") && !text.includes("Errors: 0"), text);
  assert.ok(text.includes("Domains: 1/2 active"), text);
});

test("Cloudflare: a part the token cannot read says why, the rest still shows", () => {
  load(CLOUDFLARE, {
    workers: [],
    zones: [{ name: "a.example", status: "active", paused: false }],
    workersError: "The token needs the permission: Account > Workers Scripts > Read",
    analyticsError: null,
    zonesError: null,
  });
  const text = renderIntegrationCard(task(CLOUDFLARE, "Cloudflare"), hooks).textContent;
  assert.ok(text.includes("Account > Workers Scripts > Read"), text);
  assert.ok(text.includes("Domains: 1/1 active"), text);
  assert.ok(!text.includes("No Workers yet"));
});

test("Cloudflare: a paused domain does not count as active", () => {
  load(CLOUDFLARE, { workers: [], zones: [{ name: "p.example", status: "active", paused: true }] });
  assert.ok(renderIntegrationCard(task(CLOUDFLARE, "Cloudflare"), hooks).textContent.includes("Domains: 0/1 active"));
});

test("Hetzner: what is off first, CPU for the running ones, and the project's traffic", () => {
  load(HETZNER, {
    servers: [
      { id: 12, name: "backup", status: "off", ip: "203.0.113.11", cpu: null, netIn: null, netOut: null },
      { id: 11, name: "web", status: "running", ip: "203.0.113.10", type: "cx22", cpu: 7.25, netIn: 2048, netOut: 512 },
      { id: 14, name: "booting", status: "starting", ip: null, cpu: null },
    ],
    total: 3, running: 1, metricsError: null,
  });
  assert.equal(hasIntegrationData(HETZNER), true);
  const card = renderIntegrationCard(task(HETZNER, "Hetzner"), hooks);
  const text = card.textContent;
  assert.ok(text.includes("backup") && text.includes("Off"), text);
  assert.ok(text.includes("web") && text.includes("CPU 7%"), text);
  assert.ok(text.includes("starting"), text);
  assert.ok(text.includes("Running: 1/3 · ↓ 2.0 KB/s ↑ 512 B/s"), text);
  const rows = card.find(".int-row");
  assert.equal(rows[1].title, "203.0.113.10 · cx22", "the address and the plan in the tooltip");
});

test("Hetzner: an empty project and missing metrics", () => {
  load(HETZNER, { servers: [], total: 0, running: 0, metricsError: null });
  assert.ok(renderIntegrationCard(task(HETZNER, "Hetzner"), hooks).textContent.includes("No servers yet"));
  load(HETZNER, {
    servers: [{ id: 1, name: "web", status: "running", cpu: null, netIn: null, netOut: null }],
    total: 1, running: 1, metricsError: "The token needs the permission: Read",
  });
  const text = renderIntegrationCard(task(HETZNER, "Hetzner"), hooks).textContent;
  assert.ok(text.includes("Running: 1/1") && !text.includes("↓"), text);
  assert.ok(text.includes("The token needs the permission: Read"), text);
});
