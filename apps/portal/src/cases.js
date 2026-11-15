export const products = { wallet: "Wallet", lending: "Lending" };
export const useCases = {
  onboarding: "Wallet onboarding",
  full_onboarding: "Full onboarding",
  address_refresh: "Address refresh",
  loan_application: "Loan application",
};
export const kinds = {
  document: "Identity document",
  face: "Face match",
  address: "Proof of address",
  sanctions: "Sanctions screening",
};
export const states = {
  RUNNING: "Checking",
  AWAITING_EVIDENCE: "Evidence requested",
  PROVIDER_BLOCKED: "Provider blocked",
  AWAITING_REVIEW: "Awaiting review",
  APPROVED: "Approved",
  REJECTED: "Rejected",
};
export function decisionLabel(state, product) {
  if (product === "lending" && ["APPROVED", "REJECTED"].includes(state))
    return "Verification " + states[state].toLowerCase();
  return states[state];
}
export const statuses = {
  PENDING: "In progress",
  AWAITING_EVIDENCE: "Evidence requested",
  PASSED: "Passed",
  BLOCKED: "Provider blocked",
  REVIEW: "Needs review",
  FAILED: "Failed",
};
export const productFor = (useCase) =>
  useCase === "loan_application" ? "lending" : "wallet";
export const terminal = (view) =>
  ["APPROVED", "REJECTED"].includes(view?.state);
export const checkScope = (useCase) =>
  useCase === "address_refresh"
    ? ["address"]
    : useCase === "onboarding"
      ? ["document", "face", "sanctions"]
      : Object.keys(kinds);
export const dateTime = (value) =>
  value
    ? new Intl.DateTimeFormat(undefined, {
        dateStyle: "medium",
        timeStyle: "short",
      }).format(new Date(value))
    : "—";

export function href(page, params = {}) {
  const values = new URLSearchParams(
    Object.entries(params).filter(([, v]) => v != null && v !== ""),
  );
  return "#" + page + (values.size ? "?" + values : "");
}
export function readRoute(hash) {
  const [page, query] = hash.replace(/^#/, "").split("?");
  return {
    page: ["overview", "cases", "lab", "staff"].includes(page)
      ? page
      : ["products", "scenarios"].includes(page)
        ? "lab"
        : "overview",
    params: Object.fromEntries(new URLSearchParams(query)),
  };
}

export function readQuery(filters, list) {
  const query = new URLSearchParams();
  for (const key of ["from", "to", "state", "use_case", "query", "product"])
    if (filters[key]) query.set(key, filters[key]);
  if (list)
    for (const key of ["sort", "page", "page_size"])
      if (filters[key]) query.set(key, filters[key]);
  return query.size ? "?" + query : "";
}

function dayBoundary(value, end = false) {
  const parsed = new Date(value + "T00:00:00");
  if (end) parsed.setDate(parsed.getDate() + 1);
  return Number.isNaN(+parsed) ? null : parsed.toISOString();
}

export function reportWindow(params, now) {
  if (params.period === "custom") {
    if (!params.from || !params.to) return null;
    const from = dayBoundary(params.from),
      to = dayBoundary(params.to, true);
    return from &&
      to &&
      from < to &&
      Date.parse(to) - Date.parse(from) <= 366 * 86400000
      ? { from, to }
      : null;
  }
  const days = [1, 7, 30, 90].includes(Number(params.period))
    ? Number(params.period)
    : 7;
  const end = params.at ? Date.parse(params.at) : now;
  if (!Number.isFinite(end)) return null;
  return {
    from: new Date(end - days * 86400000).toISOString(),
    to: new Date(end).toISOString(),
  };
}

export function pinReportWindow(params, window) {
  return window && params.period !== "custom"
    ? { ...params, at: window.to }
    : params;
}
