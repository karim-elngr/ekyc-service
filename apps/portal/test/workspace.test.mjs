import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createServer } from "vite";
let server, ScenarioLab, Analytics, CaseTable, CaseDetail, WorkspaceNavigation;
before(async () => {
  server = await createServer({
    configFile: false,
    esbuild: { jsx: "automatic" },
    optimizeDeps: { noDiscovery: true, include: [] },
    server: { middlewareMode: true, ws: false },
    appType: "custom",
  });
  ({ ScenarioLab } = await server.ssrLoadModule("/src/ScenarioLab.jsx"));
  ({ default: Analytics } = await server.ssrLoadModule("/src/Analytics.jsx"));
  ({ CaseTable } = await server.ssrLoadModule("/src/Reports.jsx"));
  ({ CaseDetail } = await server.ssrLoadModule("/src/CaseDetail.jsx"));
  ({ WorkspaceNavigation } = await server.ssrLoadModule(
    "/src/WorkspaceNavigation.jsx",
  ));
});
after(async () => server?.close());
const html = (component, props) =>
  renderToStaticMarkup(createElement(component, props));
test("desktop and mobile navigation expose the active page and open the API guide and schema separately", () => {
  const rendered = html(WorkspaceNavigation, { page: "lab" });
  const menus = [...rendered.matchAll(/<nav\b[^>]*>(.*?)<\/nav>/gs)];
  assert.equal(menus.length, 2);
  for (const [, content] of menus) {
    const links = [...content.matchAll(/<a\b[^>]*>/g)].map(([tag]) => tag);
    assert.equal(links.length, 6);
    const current = links.filter((tag) => tag.includes('aria-current="page"'));
    assert.equal(current.length, 1);
    assert.match(current[0], /href="#lab"/);
    for (const path of ["/docs/", "/schema/"]) {
      const resource = links.find((tag) => tag.includes(`href="${path}"`));
      assert.match(resource, /target="_blank"/);
      assert.match(resource, /rel="[^"]*noopener/);
    }
  }
  assert.match(rendered, /aria-controls="workspace-menu"/);
  assert.match(rendered, /<dialog id="workspace-menu"/);
});
test("one scenario launcher includes direct lending, reuse, interactive providers, review and all wallet use cases", () => {
  const rendered = html(ScenarioLab, {});
  for (const item of [
    "Direct loan application",
    "Wallet onboarding",
    "Full onboarding",
    "Wallet to loan",
    "Address refresh",
    "Live provider responses",
    "Sanctions review",
    "Provider recovery",
    "Address correction",
  ])
    assert.ok(rendered.includes(item), item);
  assert.match(rendered, /Start case/);
});
test("overview and directory use totals and rows returned by the scoped server", () => {
  const rendered = html(Analytics, {
    data: {
      total: 999,
      states: { APPROVED: 976 },
      checks: [
        { kind: "face", status: "PASSED", origin: "reused", count: 300 },
      ],
    },
    select() {},
  });
  for (const value of [
    "999",
    "976",
    "300",
    "Cases by status",
    "Verification requirements",
  ])
    assert.ok(rendered.includes(value), value);
  assert.match(html(CaseTable, { data: { cases: [] } }), /No matching cases/);
});
test("the case inspector renders per-check provenance, actionable evidence and revision history", () => {
  const view = {
    case_id: "v2~lending~" + "a".repeat(64),
    product: "lending",
    subject_ref: "synthetic",
    policy_version: "p1",
    use_case: "loan_application",
    state: "AWAITING_EVIDENCE",
    version: 3,
    checks: [
      {
        kind: "face",
        status: "PASSED",
        origin: "reused",
        origin_case_id: "wallet-source",
        operation_id: "origin/face/1",
        reasons: [],
      },
      {
        kind: "address",
        status: "AWAITING_EVIDENCE",
        origin: null,
        reasons: ["MISSING_EVIDENCE"],
      },
    ],
  };
  const rendered = html(CaseDetail, {
    product: "lending",
    id: view.case_id,
    initial: view,
  });
  for (const text of [
    "wallet-source",
    "Reused from an earlier case",
    "Submit proof of address",
    "Case history",
    "Reviewer handoff",
    "p1",
  ])
    assert.ok(rendered.includes(text), text);
  const observer = html(CaseDetail, {
    id: view.case_id,
    product: "lending",
    readScope: "observer",
    initial: view,
  });
  assert.ok(
    observer.includes("wallet-source") && observer.includes("Case history"),
  );
  assert.ok(!observer.includes("Reviewer handoff"));
  assert.ok(!observer.includes("Submit proof of address"));
});
