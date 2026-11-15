import { useEffect, useState } from "react";
import { ChevronDown, Search, SlidersHorizontal, X } from "lucide-react";
import { productFor, products, states, useCases } from "./cases";

const periods = {
  1: "Last 24 hours",
  7: "Last 7 days",
  30: "Last 30 days",
  90: "Last 90 days",
  custom: "Custom dates",
};
function SelectField({ id, label, value, onChange, choices }) {
  return (
    <label className="report-field" htmlFor={id}>
      {label}
      <select id={id} value={value} onChange={(e) => onChange(e.target.value)}>
        {Object.entries(choices).map(([key, name]) => (
          <option key={key} value={key}>
            {name}
          </option>
        ))}
      </select>
    </label>
  );
}

function activeFilters(params) {
  const filters = [];
  if (params.product)
    filters.push({
      key: "product",
      name: "product",
      label: products[params.product] || params.product,
    });
  if (params.use_case)
    filters.push({
      key: "use_case",
      name: "use case",
      label: useCases[params.use_case] || params.use_case,
    });
  if (params.state)
    filters.push({
      key: "state",
      name: "status",
      label: states[params.state] || params.state,
    });
  if (params.period && params.period !== "7") {
    const label =
      params.period === "custom" && params.from && params.to
        ? `${params.from} – ${params.to}`
        : periods[params.period];
    if (label) filters.push({ key: "period", name: "time", label });
  }
  if (params.query)
    filters.push({ key: "query", name: "search", label: params.query });
  return filters;
}

export function ReportFilters({ params, update }) {
  const active = activeFilters(params);
  const [text, setText] = useState(params.query || "");
  const [expanded, setExpanded] = useState(false);
  const categoryKeys = ["product", "use_case", "state"];
  const selectedCount = categoryKeys.filter((key) => params[key]).length;
  useEffect(() => setText(params.query || ""), [params.query]);
  function remove(key) {
    update(
      key === "period"
        ? { period: "", from: "", to: "", at: "" }
        : { [key]: "" },
    );
    const control =
      !expanded && categoryKeys.includes(key)
        ? "report-filter-toggle"
        : "report-" + key;
    document.getElementById(control)?.focus();
  }
  return (
    <section className="panel report-controls" aria-label="Report filters">
      <div className="report-fields">
        <form
          className="report-searchbox"
          role="search"
          onSubmit={(e) => {
            e.preventDefault();
            update({ query: text.trim() });
          }}
        >
          <label className="report-field" htmlFor="report-query">
            Case or customer
          </label>
          <div className="report-search-input">
            <input
              id="report-query"
              type="search"
              maxLength={256}
              placeholder="Search IDs"
              value={text}
              onChange={(e) => setText(e.target.value)}
            />
            <button type="submit" aria-label="Apply search">
              <Search size={17} aria-hidden="true" />
            </button>
          </div>
        </form>
        <SelectField
          id="report-period"
          label="Time range"
          value={params.period || "7"}
          onChange={(period) => update({ period, from: "", to: "", at: "" })}
          choices={periods}
        />
        <button
          id="report-filter-toggle"
          type="button"
          className="report-filter-toggle"
          aria-expanded={expanded}
          aria-controls="report-advanced"
          onClick={() => setExpanded(!expanded)}
        >
          <SlidersHorizontal size={16} aria-hidden="true" /> Filters
          {selectedCount > 0 && (
            <span className="report-filter-count">{selectedCount}</span>
          )}
          <ChevronDown size={14} aria-hidden="true" />
        </button>
      </div>
      <div id="report-advanced" className="report-advanced" hidden={!expanded}>
        <SelectField
          id="report-product"
          label="Product"
          value={params.product || ""}
          onChange={(product) => update({ product, use_case: "" })}
          choices={{ "": "All products", ...products }}
        />
        <SelectField
          id="report-use_case"
          label="Use case"
          value={params.use_case || ""}
          onChange={(use_case) => update({ use_case })}
          choices={{
            "": "All use cases",
            ...Object.fromEntries(
              Object.entries(useCases).filter(
                ([key]) =>
                  !params.product || productFor(key) === params.product,
              ),
            ),
          }}
        />
        <SelectField
          id="report-state"
          label="Status"
          value={params.state || ""}
          onChange={(state) => update({ state })}
          choices={{ "": "All statuses", ...states }}
        />
      </div>
      {params.period === "custom" && (
        <div className="report-dates">
          <label className="report-field">
            From
            <input
              type="date"
              value={params.from || ""}
              max={params.to || undefined}
              onChange={(e) => update({ from: e.target.value })}
            />
          </label>
          <label className="report-field">
            Through
            <input
              type="date"
              value={params.to || ""}
              min={params.from || undefined}
              onChange={(e) => update({ to: e.target.value })}
            />
          </label>
        </div>
      )}
      {active.length > 0 && (
        <div className="report-active" role="group" aria-label="Active filters">
          {active.map(({ key, name, label }) => (
            <button
              key={key}
              className="report-chip"
              type="button"
              aria-label={`Remove ${name} filter, ${label}`}
              title={label}
              onClick={() => remove(key)}
            >
              <span>{label}</span>
              <X size={13} aria-hidden="true" />
            </button>
          ))}
          <button
            type="button"
            className="text-button report-clear"
            onClick={() => {
              setText("");
              update({
                product: "",
                use_case: "",
                state: "",
                query: "",
                period: "",
                from: "",
                to: "",
                at: "",
              });
              document.getElementById("report-query")?.focus();
            }}
          >
            Clear filters
          </button>
        </div>
      )}
    </section>
  );
}
