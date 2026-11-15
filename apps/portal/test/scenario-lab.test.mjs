import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createServer } from "vite";
import { options } from "../src/product-demo.js";

let server, ScenarioLab, providerGroups;
before(async () => {
  server = await createServer({
    configFile: false,
    esbuild: { jsx: "automatic" },
    optimizeDeps: { noDiscovery: true, include: [] },
    server: { middlewareMode: true, ws: false },
    appType: "custom",
  });
  ({ ScenarioLab } = await server.ssrLoadModule("/src/ScenarioLab.jsx"));
  ({ providerGroups } = await server.ssrLoadModule("/src/product-demo.js"));
});
after(() => server?.close());

test("the launcher presents one grouped scenario choice and keeps provider overrides collapsed", () => {
  const html = renderToStaticMarkup(createElement(ScenarioLab));
  const picker = html.match(
    /<select[^>]*id="lab-scenario"[^>]*>(.*?)<\/select>/s,
  )?.[1];
  assert.ok(picker, "scenario picker");
  assert.equal([...picker.matchAll(/<option\b/g)].length, 10);
  assert.equal([...picker.matchAll(/<optgroup\b/g)].length, 3);
  assert.match(html, /<details[^>]*><summary/);
  assert.doesNotMatch(html, /<details[^>]*\bopen\b/);
  assert.match(html, /Provider settings/);
  assert.match(html, /Persistent outage \(case stays blocked\)/);
  assert.match(html, /Outage until screening is restored/);
  assert.match(
    html,
    /<label for="lab-provider-sanctions">Sanctions screening<\/label>/,
  );
  assert.match(html, /Start case/);
  assert.doesNotMatch(html, /Investigate a potential sanctions match/);
});

test("each provider keeps all supported outcomes grouped by behavior and capture type", () => {
  for (const [kind, results] of Object.entries(options)) {
    const groups = providerGroups(kind);
    assert.deepEqual(groups["Check result"], results);
    assert.deepEqual(groups["Live response"], ["interactive"]);
    assert.deepEqual(groups["Provider availability"], [
      "unavailable",
      "unavailable-once",
      "throttle-once",
      "timeout-once",
    ]);
    assert.deepEqual(
      groups["Evidence collection"],
      kind === "sanctions" ? undefined : ["missing"],
    );
    const values = Object.values(groups).flat();
    assert.equal(new Set(values).size, values.length);
  }
});

test("an interrupted opening restores its scenario and provider choices after reload", () => {
  const original = Object.getOwnPropertyDescriptor(
    globalThis,
    "sessionStorage",
  );
  const draft = {
    key: "stable-opening",
    subject: "customer-42",
    preset: "reuse",
    useCase: "onboarding",
    fixtures: { document: "interactive" },
  };
  Object.defineProperty(globalThis, "sessionStorage", {
    configurable: true,
    value: {
      getItem(key) {
        return key === "verity.v2.scenario-lab.pending"
          ? JSON.stringify(draft)
          : null;
      },
    },
  });
  try {
    const html = renderToStaticMarkup(createElement(ScenarioLab));
    assert.match(html, /value="reuse" selected=""/);
    assert.match(html, /value="interactive" selected=""/);
    assert.match(html, /Retry opening case/);
  } finally {
    if (original) Object.defineProperty(globalThis, "sessionStorage", original);
    else delete globalThis.sessionStorage;
  }
});

test("a persistent non-screening outage describes its endpoint before launch", () => {
  const descriptor = Object.getOwnPropertyDescriptor(
    globalThis,
    "sessionStorage",
  );
  const draft = {
    preset: "refresh",
    useCase: "address_refresh",
    subject: "synthetic",
    key: "opening",
    fixtures: { address: "unavailable" },
  };
  Object.defineProperty(globalThis, "sessionStorage", {
    configurable: true,
    value: {
      getItem(key) {
        return key.endsWith(".pending") ? JSON.stringify(draft) : null;
      },
    },
  });
  try {
    const html = renderToStaticMarkup(createElement(ScenarioLab));
    assert.match(html, /keeps the affected check blocked on every retry/);
    assert.match(html, /Choose Transient outage/);
  } finally {
    if (descriptor)
      Object.defineProperty(globalThis, "sessionStorage", descriptor);
    else delete globalThis.sessionStorage;
  }
});
