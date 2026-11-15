import { post } from "./api.js";
import { checkScope, productFor } from "./cases.js";

export const options = {
  document: ["pass", "review", "threshold", "expired"],
  face: ["pass", "review", "threshold"],
  address: ["pass", "review", "threshold", "stale"],
  sanctions: ["pass", "hit", "inconclusive"],
};
export function providerGroups(kind) {
  return {
    "Check result": options[kind],
    ...(kind === "sanctions" ? {} : { "Evidence collection": ["missing"] }),
    "Live response": ["interactive"],
    "Provider availability": [
      "unavailable",
      "unavailable-once",
      "throttle-once",
      "timeout-once",
    ],
  };
}
export const fixtureLabels = {
  pass: "Pass",
  review: "Needs corroboration",
  threshold: "At score threshold",
  expired: "Expired document",
  stale: "Stale proof",
  hit: "Potential match",
  inconclusive: "Inconclusive",
  interactive: "Choose a live response",
  unavailable: "Provider unavailable",
  "unavailable-once": "Transient outage",
  "throttle-once": "Rate limited once",
  "timeout-once": "Timeout once",
  missing: "Request evidence later",
};
export const scenarioGroups = {
  "Product journeys": [
    {
      id: "direct-loan",
      name: "Direct loan application",
      useCase: "loan_application",
      description:
        "Verify a new borrower’s document, face, address and sanctions status.",
      fixtures: {},
    },
    {
      id: "wallet",
      name: "Wallet onboarding",
      useCase: "onboarding",
      description:
        "Open a wallet using a document, a face match and a sanctions check.",
      fixtures: {},
    },
    {
      id: "reuse",
      name: "Wallet to loan",
      useCase: "onboarding",
      description:
        "Approve a wallet, then open a loan for the same customer to see which checks lending can reuse.",
      fixtures: {},
    },
    {
      id: "full",
      name: "Full onboarding",
      useCase: "full_onboarding",
      description:
        "Run document, face, address and sanctions checks for a new wallet.",
      fixtures: {},
    },
    {
      id: "refresh",
      name: "Address refresh",
      useCase: "address_refresh",
      description: "Verify an updated address for a wallet customer.",
      fixtures: {},
    },
  ],
  "Provider behavior": [
    {id:"routing",name:"Two address vendors",useCase:"address-pilot",description:"Issue proof for two authorized vendors, then inspect the selected route in the staff workspace.",fixtures:{}},
    {
      id: "interactive",
      name: "Live provider responses",
      useCase: "full_onboarding",
      description:
        "Answer each of the four wallet checks as its provider is called.",
      fixtures: Object.fromEntries(
        checkScope("full_onboarding").map((k) => [k, "interactive"]),
      ),
    },
    {
      id: "outage",
      name: "Provider recovery",
      useCase: "loan_application",
      description:
        "Restore an unavailable sanctions provider and retry the loan check.",
      fixtures: { sanctions: "unavailable" },
    },
  ],
  "Evidence and review": [
    {
      id: "review",
      name: "Sanctions review",
      useCase: "loan_application",
      description:
        "Investigate a potential sanctions match on a loan application.",
      fixtures: { sanctions: "hit" },
    },
    {
      id: "correction",
      name: "Address correction",
      useCase: "address_refresh",
      description:
        "Replace stale address evidence and finish the same wallet case.",
      fixtures: { address: "stale" },
    },
  ],
};
export const presets = Object.values(scenarioGroups).flat();

export const gateway = (product, id) =>
  "/product-demo/" +
  product +
  "/cases" +
  (id ? "/" + encodeURIComponent(id) : "");
export function freshRun() {
  return {
    subject: "customer-" + crypto.randomUUID(),
    key: crypto.randomUUID(),
  };
}
export async function issue(
  subject,
  kind,
  scenario,
  key,
  extra = {},
  send = post,
) {
  const result = await send(
    "/product-demo/evidence",
    { subject_ref: subject, kind, scenario, ...extra },
    key,
  );
  if (!result.evidence_ref)
    throw new Error("The issuer returned no evidence reference.");
  return result.evidence_ref;
}
export async function openCase(
  run,
  useCase,
  fixtures,
  reuse = false,
  send = post,
) {
  const evidence = {};
  if (!reuse)
    for (const kind of checkScope(useCase)) {
      if (kind === "sanctions" || fixtures[kind] === "missing") continue;
      if (kind === "face" && !evidence.document) continue;
      const extra =
        kind === "face"
          ? { document_ref: evidence.document }
          : kind === "document"
            ? { screening_scenario: fixtures.sanctions || "pass" }
            : {};
      if (kind==="address" && useCase.startsWith("address-")) extra.providers=["address-a","address-b"];
      evidence[kind] = await issue(
        run.subject,
        kind,
        fixtures[kind] || "pass",
        run.key + "-" + kind,
        extra,
        send,
      );
    }
  const body = { subject_ref: run.subject, use_case: useCase };
  if (Object.keys(evidence).length) body.evidence = evidence;
  return send(gateway(productFor(useCase)), body, run.key + "-open-" + useCase);
}
export async function provide(view, fixtures, key, send = post) {
  const evidence = {};
  for (const kind of ["document", "face", "address"]) {
    const scenario = fixtures[kind];
    if (!scenario) continue;
    const extra =
      kind === "face" && evidence.document
        ? { document_ref: evidence.document }
        : {};
    if(kind==="address" && view.use_case?.startsWith("address-")) extra.providers=["address-a","address-b"];
    evidence[kind] = await issue(
      view.subject_ref,
      kind,
      scenario,
      key + "-" + kind,
      extra,
      send,
    );
  }
  return send(
    gateway(view.product, view.case_id) + "/evidence",
    { evidence },
    key + "-submit",
  );
}
