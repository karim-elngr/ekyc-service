import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createServer } from "vite";

let server, Analytics, CaseTable, Reports;
before(async () => {
  server = await createServer({
    configFile: false,
    esbuild: { jsx: "automatic" },
    optimizeDeps: { noDiscovery: true, include: [] },
    server: { middlewareMode: true, ws: false },
    appType: "custom",
  });
  ({ default: Analytics } = await server.ssrLoadModule("/src/Analytics.jsx"));
  ({ Reports, CaseTable } = await server.ssrLoadModule("/src/Reports.jsx"));
});
after(async () => server?.close());
const html = (component, props) =>
  renderToStaticMarkup(createElement(component, props));

test("empty reports give one recovery path without zero-count charts", () => {
  const result = html(Analytics, {
    data: { total: 0, states: {}, checks: [] },
  });
  assert.match(result, /No cases in this period/);
  assert.doesNotMatch(result, /Approved|Identity document|report-donut/);
});

test("only observed outcomes and check types appear, with a keyboard-accessible drilldown", () => {
  const result = html(Analytics, {
    data: {
      total: 1,
      states: { APPROVED: 1, REJECTED: 0 },
      checks: [{ kind: "face", status: "PASSED", origin: "reused", count: 1 }],
    },
    statusHref: (state) => "#cases?state=" + state,
  });
  assert.match(result, /href="#cases\?state=APPROVED"/);
  assert.match(result, />case</);
  assert.doesNotMatch(result, /Rejected|Provider blocked|Awaiting dispatch/);
  assert.match(result, /Face match/);
  assert.match(result, /Reused/);
});

test("active filters offer individual removals and a reset", () => {
  const result = html(Reports, {
    page: "cases",
    params: {
      state: "APPROVED",
      product: "wallet",
      period: "30",
      query: "customer-7",
    },
    navigate() {},
    revision: 0,
  });
  assert.match(result, /aria-expanded="false" aria-controls="report-advanced"/);
  assert.match(result, /class="report-filter-count">2</);
  assert.match(result, /id="report-advanced" class="report-advanced" hidden/);
  for (const text of [
    "Remove status filter",
    "Remove product filter",
    "Remove time filter",
    "Remove search filter",
    "Clear filters",
  ])
    assert.ok(result.includes(text), text);
});

test("case references open through a link preserving the selected report window", () => {
  const row = {
    case_id: "v2~wallet~" + "c".repeat(64),
    subject_ref: "customer-7",
    product: "wallet",
    use_case: "onboarding",
    state: "APPROVED",
    created_at: "2026-11-15T00:00:00Z",
  };
  const result = html(CaseTable, {
    data: { cases: [row], total: 1 },
    caseHref: () => "#cases?at=fixed&amp=1",
  });
  assert.match(result, /href="#cases\?at=fixed/);
  assert.match(result, /customer-7/);
  assert.match(result, /dateTime="2026-11-15T00:00:00Z"/i);
});

test("lending decisions identify verification and pinned reports expose the cutoff", () => {
  const row = {
    case_id: "v2~lending~" + "1".repeat(64),
    product: "lending",
    use_case: "loan_application",
    state: "REJECTED",
  };
  assert.match(
    html(CaseTable, { data: { cases: [row] }, caseHref: () => "#case" }),
    /Verification rejected/,
  );
  const result = html(Reports, {
    page: "cases",
    params: { at: "2026-11-15T12:34:56Z" },
    navigate() {},
    revision: 0,
  });
  assert.match(result, /Created through/);
  assert.match(result, /Return to live/);
});

test("requirements count each case once, including reused and undispatched checks", () => {
  const result = html(Analytics, {
    data: {
      total: 4,
      states: { APPROVED: 2, AWAITING_EVIDENCE: 1, PROVIDER_BLOCKED: 1 },
      checks: [
        { kind: "face", origin: "fresh", status: "PASSED", count: 1 },
        { kind: "face", origin: "reused", status: "PASSED", count: 1 },
        { kind: "face", origin: null, status: "AWAITING_EVIDENCE", count: 1 },
        { kind: "face", origin: "fresh", status: "BLOCKED", count: 1 },
      ],
    },
  });
  assert.match(result, /Cases by status/);
  assert.match(result, /Verification requirements/);
  assert.match(result, /<summary><span>Face match<\/span><strong>4<\/strong>/);
  for (const label of [
    "Reused",
    "Fresh",
    "Awaiting dispatch",
    "Provider blocked",
  ])
    assert.ok(result.includes(label));
});
