import assert from "node:assert/strict";
import test from "node:test";
import { readFileSync } from "node:fs";

const spec = JSON.parse(
  readFileSync(new URL("../public/openapi.json", import.meta.url), "utf8"),
);
const schemas = spec.components.schemas;
function resolve(ref) {
  return ref
    .slice(2)
    .split("/")
    .reduce((node, key) => node?.[key], spec);
}
test("every bundled contract reference resolves and only v2 cases are published", () => {
  function check(data) {
    if (!data || typeof data !== "object") return;
    if (data.$ref) assert.ok(resolve(data.$ref), data.$ref);
    else Object.values(data).forEach(check);
  }
  check(spec);
  assert.deepEqual(
    Object.keys(spec.paths).filter((path) => path.startsWith("/v1")),
    [],
  );
  assert.deepEqual(Object.keys(spec.webhooks).sort(), [
    "ekyc.case.action.v2",
    "ekyc.case.decided.v2",
    "ekyc.screening.alert.v2",
  ]);
  for (const [path, methods] of Object.entries(spec.paths).filter(([p]) =>
    p.startsWith("/v2"),
  ))
    for (const [method, operation] of Object.entries(methods).filter(
      ([m]) => m !== "parameters",
    ))
      assert.deepEqual(
        operation.security,
        [{ callerBearer: [] }],
        method + " " + path,
      );
});
test("reporting scopes, sort and time windows are part of the service contract", () => {
  const list = spec.paths["/v2/cases"].get,
    summary = spec.paths["/v2/cases/overview"].get;
  assert.deepEqual(summary.parameters.map((p) => p.name).sort(), [
    "from",
    "product",
    "query",
    "state",
    "to",
    "use_case",
  ]);
  assert.ok(
    list.parameters
      .find((p) => p.name === "sort")
      .schema.enum.includes("created_at_asc"),
  );
  assert.equal(
    list.parameters.find((p) => p.name === "page_size").schema.maximum,
    100,
  );
  assert.ok(
    spec.paths["/v2/cases/{id}/history"].get.parameters.some(
      (p) => p.name === "before_version",
    ),
  );
});
test("review and callback contracts pin the exact operation and limit issuer details to the reviewer", () => {
  assert.ok(schemas.ProductReview.required.includes("operation_id"));
  assert.ok(
    schemas.ProductReviewerView.required.includes("alerts") &&
      schemas.ProductReviewerView.required.includes("reviews"),
  );
  assert.ok(schemas.ProductReviewerCheck.required.includes("evidence_ref"));
  assert.ok(
    !schemas.ProductCheck.properties.evidence_ref &&
      !schemas.ProductReviewerCheck.properties.provider_ref,
  );
  assert.ok(schemas.ProductCheck.required.includes("interactive"));
  const callback = spec.paths["/v2/cases/{id}/interactive/{kind}"].post;
  assert.equal(
    callback.parameters.find((p) => p.name === "operation_id").required,
    true,
  );
  assert.ok(
    schemas.ProductOpen.properties.use_case.examples.includes(
      "full_onboarding",
    ),
  );
});
