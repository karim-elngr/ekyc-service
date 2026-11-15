// Server-side only. Browser paths select fixed credentials and upstream endpoints.
const id = "(v2~[a-zA-Z0-9-]{1,48}~[0-9a-f]{64})";
const casePath = new RegExp(
  "^/cases(?:/(overview|" +
    id +
    ")(?:/(history|evidence|retry|review|interactive)(?:/(document|face|address|sanctions))?)?)?$",
);
const reportParams = ["from", "to", "state", "use_case", "query", "product"];
const listParams = [...reportParams, "sort", "page", "page_size"];

function permitted(method, raw, role) {
  const [path, search = ""] = raw.split("?");
  const match = casePath.exec(path);
  if (!match || raw.split("?").length > 2) return false;
  const [, sub, caseId, action, kind] = match;
  const params = new URLSearchParams(search);
  const accepts = (names) =>
    [...params.keys()].every(
      (key) => names.includes(key) && params.getAll(key).length === 1,
    );
  if (!sub)
    return (
      (method === "GET" && accepts(listParams)) ||
      (role === "product" && method === "POST" && !search)
    );
  if (sub === "overview")
    return !action && !kind && method === "GET" && accepts(reportParams);
  if (!caseId || (kind && action !== "interactive")) return false;
  if (!action) return method === "GET" && !search;
  if (action === "history")
    return (
      method === "GET" && accepts(["before_version", "limit", "from", "to"])
    );
  if (action === "review")
    return role === "reviewer" && !search && ["GET", "POST"].includes(method);
  if (role !== "product") return false;
  if (action === "interactive")
    return kind
      ? method === "POST" &&
          accepts(["operation_id"]) &&
          Boolean(params.get("operation_id"))
      : method === "GET" && !search;
  return method === "POST" && !search;
}

export function permittedStaffRequest(method, raw) {
  const [path, search = ""] = raw.split("?");
  if (raw.split("?").length > 2) return false;
  if (path === "/v2/staff/session") return method === "GET" && !search;
  if (path === "/v2/investigations/acknowledgement")
    return method === "PUT" && !search;
  if (path === "/v2/investigations") {
    const params = new URLSearchParams(search);
    return (
      method === "GET" &&
      [...params.keys()].every(
        (key) =>
          ["status", "after", "limit"].includes(key) &&
          params.getAll(key).length === 1,
      )
    );
  }
  return (
    raw.startsWith("/v2/cases") && permitted(method, raw.slice(3), "reviewer")
  );
}

export function permittedProductDocsRequest(method, raw) {
  if (
    method === "GET" &&
    /^\/docs-api\/(health\/(?:reads|ready|live)|metrics)$/.test(raw)
  )
    return true;
  if (!raw.startsWith("/docs-api/v2/")) return false;
  if (permittedStaffRequest(method, raw.slice("/docs-api".length))) return true;
  const path = raw.slice("/docs-api/v2".length);
  return (
    permitted(method, path, "product") || permitted(method, path, "reviewer")
  );
}

export function demoRoute(method, raw) {
  if (
    method === "POST" &&
    ["/product-demo/evidence", "/product-demo/screening"].includes(raw)
  )
    return { product: "issuer", path: raw.replace("/product-demo", "/demo") };
  const match =
    /^\/product-demo\/(wallet|lending|observer|wallet-reviewer|lending-reviewer)(\/.*)$/.exec(
      raw,
    );
  if (!match) return null;
  const [, product, path] = match;
  const role = product.endsWith("-reviewer")
    ? "reviewer"
    : product === "observer"
      ? "observer"
      : "product";
  if (role === "reviewer" && !path.endsWith("/review")) return null;
  return permitted(method, path, role) ? { product, path: "/v2" + path } : null;
}

export function localIssuer(value) {
  const url = new URL(value);
  if (
    url.protocol !== "http:" ||
    !["127.0.0.1", "[::1]", "localhost", "issuer"].includes(url.hostname) ||
    url.username ||
    url.password ||
    url.pathname !== "/" ||
    url.search ||
    url.hash
  )
    throw new Error("EKYC_ISSUER_URL must point to the local demo issuer.");
  return url.origin;
}

const credentials = {
  wallet: ["EKYC_WALLET_TOKEN", "local-demo-wallet-product"],
  lending: ["EKYC_LENDING_TOKEN", "local-demo-lending-product"],
  observer: ["EKYC_OBSERVER_TOKEN", "local-demo-institution-observer"],
  "wallet-reviewer": [
    "EKYC_WALLET_REVIEWER_TOKEN",
    "local-demo-wallet-reviewer",
  ],
  "lending-reviewer": [
    "EKYC_LENDING_REVIEWER_TOKEN",
    "local-demo-lending-reviewer",
  ],
};

export function demoProxies(target, env) {
  return Object.fromEntries(
    Object.entries(credentials).map(([name, [key, defaultToken]]) => [
      "^/product-demo/" + name + "/",
      {
        target,
        changeOrigin: true,
        rewrite: (path) => path.replace("/product-demo/" + name, "/v2"),
        configure(proxy) {
          proxy.on("proxyReq", (req) => {
            req.removeHeader("Cookie");
            req.setHeader(
              "Authorization",
              "Bearer " + (env[key] || defaultToken),
            );
          });
        },
      },
    ]),
  );
}

export async function issueDemoEvidence(req, res, path, issuer) {
  const reply = (status, body) =>
    res
      .writeHead(status, {
        "Content-Type": "application/json",
        "Cache-Control": "no-store",
      })
      .end(JSON.stringify(body));
  const key = req.headers["idempotency-key"];
  if (typeof key !== "string" || !/^[!-~]{1,128}$/.test(key))
    return reply(400, { error: "invalid_idempotency_key" });
  try {
    const chunks = [];
    let size = 0;
    for await (const chunk of req) {
      size += chunk.length;
      if (size > 16384) return reply(413, { error: "body_too_large" });
      chunks.push(chunk);
    }
    const data = JSON.parse(Buffer.concat(chunks).toString());
    const allowed =
      path === "/demo/screening"
        ? ["subject_ref", "scenario"]
        : [
            "subject_ref",
            "kind",
            "scenario",
            "document_ref",
            "screening_scenario",
          ];
    if (
      !data ||
      Array.isArray(data) ||
      typeof data !== "object" ||
      !Object.keys(data).every((k) => allowed.includes(k))
    )
      return reply(422, { error: "invalid_demo_capture" });
    const response = await fetch(issuer + path, {
      method: "POST",
      headers: { "Content-Type": "application/json", "Idempotency-Key": key },
      body: JSON.stringify({ ...data, institution: "demo-bank" }),
      redirect: "error",
      signal: AbortSignal.timeout(8000),
    });
    return reply(response.status, await response.json());
  } catch (err) {
    return reply(err instanceof SyntaxError ? 400 : 503, {
      error: err instanceof SyntaxError ? "invalid_json" : "issuer_unavailable",
    });
  }
}

export function invocationRedirect(path, port = "9070") {
  const match = /^\/invocations\/([A-Za-z0-9_-]{1,256})?$/.exec(path);
  if (
    !match ||
    !/^\d{1,5}$/.test(port) ||
    Number(port) < 1 ||
    Number(port) > 65535
  )
    return null;
  const ui = "http://127.0.0.1:" + port + "/ui/";
  return match[1] ? ui + "invocations/" + match[1] : ui;
}
