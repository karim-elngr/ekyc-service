import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createServer } from "vite";

let server, CheckRows, Identifier, Revisions, ReviewRecords, CaseDetail;
before(async () => {
  server = await createServer({
    configFile: false,
    esbuild: { jsx: "automatic" },
    optimizeDeps: { noDiscovery: true, include: [] },
    server: { middlewareMode: true, ws: false },
    appType: "custom",
  });
  ({ CheckRows, Identifier } = await server.ssrLoadModule(
    "/src/CaseEvidence.jsx",
  ));
  ({ Revisions, CaseDetail } = await server.ssrLoadModule(
    "/src/CaseDetail.jsx",
  ));
  ({ ReviewRecords } = await server.ssrLoadModule("/src/CaseActions.jsx"));
});
after(() => server?.close());
const html = (component, props) =>
  renderToStaticMarkup(createElement(component, props));
const check = {
  kind: "face",
  status: "PASSED",
  origin: "reused",
  operation_id: "previous/face/1",
  origin_case_id: "previous-wallet",
  observed_at: "2026-02-03T00:00:00Z",
  attempt: 0,
  reasons: [],
};
const caseView = {
  case_id: "loan-18",
  product: "lending",
  use_case: "loan_application",
  policy_version: "lending.loan_application.v1",
  subject_ref: "customer",
  version: 2,
  state: "RUNNING",
  checks: [check],
  created_at: "2026-02-02T00:00:00Z",
};

test("long identifiers wrap at policy separators and digest boundaries and retain their exact text", () => {
  const policy = "lending.loan_application.v1";
  const digest = "0123456789abcdef".repeat(4);
  const id = "v2~lending~" + digest + "/face/1";
  const policyMarkup = html(Identifier, {
    value: policy,
    breakAtSeparators: true,
  });
  const idMarkup = html(Identifier, { value: id });
  assert.match(policyMarkup, /application\.<wbr\s*\/?\s*>v1/);
  assert.match(idMarkup, /01234567<wbr\s*\/?\s*>89abcdef/);
  assert.equal(policyMarkup.replace(/<[^>]+>/g, ""), policy);
  assert.equal(idMarkup.replace(/<[^>]+>/g, ""), id);
});

test("per-check summaries explain failures and reuse, with exact provenance in the disclosure", () => {
  const markup = html(CheckRows, {
    checks: [
      check,
      {
        kind: "document",
        status: "AWAITING_EVIDENCE",
        reasons: ["DOCUMENT_EXPIRED"],
        next_action: "provide_evidence",
      },
    ],
  });
  assert.match(
    markup,
    /<summary>.*Reused from an earlier case.*Passed.*<\/summary>/,
  );
  assert.match(markup, /The identity document has expired/);
  assert.match(markup, /<code>previous-wallet<\/code>/);
  assert.match(markup, /<code>previous\/face\/1<\/code>/);
  assert.match(markup, /<code>DOCUMENT_EXPIRED<\/code>/);
  assert.match(markup, /Submit requested evidence/);
  for (const summary of markup.matchAll(/<summary>(.*?)<\/summary>/g))
    assert.doesNotMatch(summary[1], /DOCUMENT_EXPIRED/);
});

test("older revisions expose their own check outcomes, original observations and reasons", () => {
  const older = {
    ...check,
    origin: "fresh",
    operation_id: "old/face/2",
    reasons: ["FACE_SIMILARITY_LOW"],
    status: "AWAITING_EVIDENCE",
  };
  const markup = html(Revisions, {
    rows: [
      {
        version: 4,
        recorded_at: check.observed_at,
        view: { state: "APPROVED", checks: [check] },
      },
      {
        version: 3,
        recorded_at: check.observed_at,
        view: { state: "AWAITING_EVIDENCE", checks: [older] },
      },
    ],
  });
  assert.match(
    markup,
    /Revision 4.*Approved.*previous\/face\/1.*Revision 3.*Evidence requested.*old\/face\/2/,
  );
  assert.match(markup, /FACE_SIMILARITY_LOW/);
});

test("reviewer records distinguish late alerts, decisions and evidence and preserve investigation fields", () => {
  const fact = {
    ...check,
    method: "face_comparison",
    evidence_ref: "issuer/12",
    evidence_revision: "12-r1",
    provider: "face",
  };
  const markup = html(ReviewRecords, {
    data: {
      checks: [fact],
      alerts: [
        { ...fact, event_id: "late-17", reason: "POTENTIAL_SANCTIONS_MATCH" },
      ],
      reviews: [
        {
          kind: "sanctions",
          actor: "reviewer-1",
          action: "clear",
          reason: "Separate identities",
          operation_id: "screen/2",
          review_ref: "ticket-4",
          recorded_at: check.observed_at,
        },
      ],
    },
  });
  for (const value of [
    "Late screening alerts",
    "The screening found a potential match",
    "late-17",
    "Match cleared",
    "reviewer-1",
    "Separate identities",
    "ticket-4",
    "screen/2",
    "Provider evidence",
    "Face comparison",
    "issuer/12",
    "12-r1",
  ])
    assert.ok(markup.includes(value), value);
});

test("a pending screening review opens its accessible disclosure and history stays independent", () => {
  const pending = {
    kind: "sanctions",
    status: "REVIEW",
    operation_id: "screen/3",
    reasons: ["SANCTIONS_POTENTIAL_MATCH"],
  };
  const markup = html(CaseDetail, {
    product: "lending",
    id: caseView.case_id,
    initial: { ...caseView, state: "AWAITING_REVIEW", checks: [pending] },
  });
  assert.match(
    markup,
    /aria-expanded="true" aria-controls="[^"]+"[^>]*>.*?Reviewer handoff/,
  );
  assert.match(
    markup,
    /aria-expanded="false" aria-controls="[^"]+"[^>]*>.*?Case history/,
  );
  assert.match(markup, /Loading reviewer records/);
  assert.doesNotMatch(markup, /Record disposition/);
});

test("review notes use Unicode scalar values, allow line feeds and explain invalid input", async () => {
  const { reviewNoteError } = await server.ssrLoadModule(
    "/src/CaseActions.jsx",
  );
  assert.equal(typeof reviewNoteError, "function");
  for (const text of ["two\nlines", "ا".repeat(512), "🙂".repeat(512)])
    assert.equal(reviewNoteError(text), "");
  for (const text of ["  ", "one\ttwo", "🙂".repeat(513)])
    assert.ok(reviewNoteError(text));
});
