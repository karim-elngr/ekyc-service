import assert from "node:assert/strict";
import test from "node:test";
import { EventEmitter } from "node:events";
import {
  demoRoute,
  demoProxies,
  permittedStaffRequest,
  permittedProductDocsRequest,
  localIssuer,
  invocationRedirect,
} from "../product-proxy.js";
import { openCase, provide } from "../src/product-demo.js";
import { readQuery, readRoute } from "../src/cases.js";

const id = "v2~wallet~" + "f".repeat(64),
  cases = "/product-demo/wallet/cases";
test("the local gateway binds observer, reviewer and product capabilities to distinct upstream routes", () => {
  assert.deepEqual(
    demoRoute("GET", "/product-demo/observer/cases/overview?product=wallet"),
    { product: "observer", path: "/v2/cases/overview?product=wallet" },
  );
  assert.equal(demoRoute("POST", "/product-demo/observer/cases"), null);
  assert.equal(demoRoute("GET", cases + "/" + id + "/review"), null);
  assert.ok(
    demoRoute("GET", "/product-demo/wallet-reviewer/cases/" + id + "/review"),
  );
  assert.equal(
    demoRoute("POST", "/product-demo/wallet-reviewer/cases/" + id + "/retry"),
    null,
  );
  assert.ok(
    demoRoute(
      "POST",
      cases + "/" + id + "/interactive/document?operation_id=operation%2F1",
    ),
  );
  assert.equal(
    demoRoute("POST", cases + "/" + id + "/interactive/document"),
    null,
  );
  for (const path of [
    "/product-demo/unknown/cases",
    "/product-demo/observer/cases?skip=99",
    cases + "/../evidence",
    "/product-demo/v1/resolve",
  ])
    assert.equal(demoRoute("GET", path), null);
  assert.ok(
    permittedProductDocsRequest(
      "GET",
      "/docs-api/v2/cases?page=1&page_size=20",
    ),
  );
  assert.ok(
    permittedProductDocsRequest("GET", "/docs-api/v2/cases/" + id + "/review"),
  );
  assert.equal(
    permittedProductDocsRequest("POST", "/docs-api/demo/evidence"),
    false,
  );
  assert.equal(permittedProductDocsRequest("GET", "/docs-api/health/reads"), true);
  assert.equal(permittedProductDocsRequest("GET", "/docs-api/v1/cases"), false);
  assert.throws(() => localIssuer("https://external.test"));
});
test("the credential graft stays on the server and strips incoming cookies", () => {
  const proxies = demoProxies("http://api:8090", {
    EKYC_OBSERVER_TOKEN: "observer",
    EKYC_WALLET_REVIEWER_TOKEN: "reviewer",
  });
  for (const [path, token] of [
    ["/product-demo/observer/cases", "observer"],
    ["/product-demo/wallet-reviewer/cases/" + id + "/review", "reviewer"],
  ]) {
    const [entry] = Object.entries(proxies).filter(([k]) =>
      new RegExp(k).test(path),
    );
    const server = new EventEmitter();
    entry[1].configure(server);
    const headers = new Map([
      ["Cookie", "session"],
      ["Authorization", "overwrite"],
    ]);
    server.emit("proxyReq", {
      removeHeader: headers.delete.bind(headers),
      setHeader: headers.set.bind(headers),
    });
    assert.equal(headers.get("Authorization"), "Bearer " + token);
    assert.ok(!headers.has("Cookie"));
  }
});
test("direct lending opens independently, while a reuse step omits evidence and preserves the subject", async () => {
  const calls = [],
    run = { subject: "subject", key: "stable-key" };
  async function send(path, body, key) {
    calls.push({ path, body, key });
    return { evidence_ref: "ref-" + body.kind, case_id: id };
  }
  await openCase(run, "loan_application", {}, false, send);
  assert.equal(calls.at(-1).path, "/product-demo/lending/cases");
  assert.equal(calls.at(-1).body.subject_ref, "subject");
  assert.deepEqual(calls.at(-1).body.evidence, {
    document: "ref-document",
    face: "ref-face",
    address: "ref-address",
  });
  assert.equal(calls[1].body.document_ref, "ref-document");
  const first = calls.splice(0);
  await openCase(run, "loan_application", {}, false, send);
  assert.deepEqual(calls, first);
  calls.length = 0;
  await openCase(run, "loan_application", {}, true, send);
  assert.equal(calls.length, 1);
  assert.deepEqual(calls[0].body, {
    subject_ref: "subject",
    use_case: "loan_application",
  });
});
test("full onboarding, address refresh and evidence corrections dispatch only their selected capture inputs", async () => {
  const calls = [],
    send = async (path, body) => {
      calls.push({ path, body });
      return { evidence_ref: "issued-" + body.kind, case_id: id };
    };
  await openCase(
    { subject: "s", key: "k" },
    "full_onboarding",
    { sanctions: "interactive", face: "missing" },
    false,
    send,
  );
  assert.equal(calls[0].body.screening_scenario, "interactive");
  assert.deepEqual(calls.at(-1).body.evidence, {
    document: "issued-document",
    address: "issued-address",
  });
  calls.length = 0;
  await openCase(
    { subject: "s", key: "k" },
    "address_refresh",
    { address: "stale" },
    false,
    send,
  );
  assert.equal(calls.length, 2);
  assert.equal(calls[0].body.scenario, "stale");
  assert.deepEqual(calls[1].body.evidence, { address: "issued-address" });
  calls.length = 0;
  await provide(
    { case_id: id, product: "wallet", subject_ref: "s" },
    { face: "pass", document: "pass" },
    "replacement-key",
    send,
  );
  assert.equal(calls[1].body.document_ref, "issued-document");
  assert.equal(calls[2].path, cases + "/" + id + "/evidence");
});
test("report queries keep scope and time filters across case drilldown and strip pagination from overview", () => {
  const filters = {
    from: "2026-01-01T00:00:00Z",
    product: "wallet",
    query: "A & B",
    state: "APPROVED",
    sort: "created_at_asc",
    page: 3,
    page_size: 10,
  };
  const list = new URLSearchParams(readQuery(filters, true)),
    overview = new URLSearchParams(readQuery(filters, false));
  assert.equal(list.get("query"), "A & B");
  assert.equal(list.get("page"), "3");
  assert.equal(overview.get("product"), "wallet");
  assert.ok(!overview.has("sort"));
  assert.equal(readRoute("#products").page, "lab");
  assert.equal(readRoute("#cases?product=lending").params.product, "lending");
});

test("overview, directory and later pages share a precise reporting window until refreshed", async () => {
  const { reportWindow, pinReportWindow } = await import("../src/cases.js");
  const before = Date.parse("2026-11-15T10:30:00Z");
  const query = { period: "7", product: "wallet" };
  const range = reportWindow(query, before);
  for (const isList of [true, false]) {
    const sent = new URLSearchParams(readQuery({ ...query, ...range }, isList));
    assert.equal(sent.get("from"), range.from);
    assert.equal(sent.get("to"), range.to);
  }
  const later = before + 45_000;
  const pinned = pinReportWindow(query, range);
  assert.deepEqual(reportWindow({ ...pinned, page: 2 }, later), range);
  assert.deepEqual(reportWindow({ ...pinned, page: 1 }, later), range);
  const refreshed = reportWindow({ ...pinned, at: "", page: 1 }, later);
  assert.ok(refreshed.to > range.to && refreshed.from > range.from);
});

test("invocation links use the configured loopback Restate admin port", () => {
  assert.equal(
    invocationRedirect("/invocations/", "19070"),
    "http://127.0.0.1:19070/ui/",
  );
  assert.equal(invocationRedirect("/invocations/", "0"), null);
  assert.equal(
    invocationRedirect("/invocations/inv_abc123", "19070"),
    "http://127.0.0.1:19070/ui/invocations/inv_abc123",
  );
  assert.equal(
    invocationRedirect("/invocations/inv_abc123"),
    "http://127.0.0.1:9070/ui/invocations/inv_abc123",
  );
  for (const raw of [
    "/invocations//",
    "/invocations/../health",
    "/invocations/inv_abc?host=external",
    "/invocations/http://example.org",
  ]) {
    assert.equal(invocationRedirect(raw), null);
  }
});

test("staff routes allow only scoped reads, review and acknowledgements", () => {
  assert.ok(permittedStaffRequest("GET", "/v2/staff/session"));
  assert.ok(
    permittedStaffRequest(
      "GET",
      "/v2/investigations?status=acknowledged&limit=10",
    ),
  );
  assert.ok(permittedStaffRequest("PUT", "/v2/investigations/acknowledgement"));
  assert.ok(permittedStaffRequest("POST", "/v2/cases/" + id + "/review"));
  for (const path of [
    "/v2/cases",
    "/v2/cases/" + id + "/retry",
    "/v2/cases/" + id + "/evidence",
    "/demo/screening",
  ])
    assert.equal(permittedStaffRequest("POST", path), false);
  assert.equal(
    permittedStaffRequest("GET", "/v2/investigations?limit=1&limit=2"),
    false,
  );
});
