import { ArrowUpRight, ChevronDown, Inbox } from "lucide-react";
import { Cell, Pie, PieChart } from "recharts";
import { kinds, states, statuses } from "./cases";

const colors = {
  APPROVED: "#35816d",
  AWAITING_REVIEW: "#8b6db5",
  AWAITING_EVIDENCE: "#c89339",
  PROVIDER_BLOCKED: "#b9744c",
  RUNNING: "#648caf",
  REJECTED: "#c66f69",
};
const originLabel = { fresh: "Fresh", reused: "Reused" };
const numbers = new Intl.NumberFormat();

function DecisionChart({ data, statusHref, select }) {
  const slices = Object.entries(states)
    .map(([key, label]) => ({ key, label, count: data.states[key] || 0 }))
    .filter((slice) => slice.count > 0)
    .sort((a, b) => b.count - a.count);
  return (
    <section className="panel report-metric">
      <h2>Cases by status</h2>
      <div className="report-decisions">
        <div className="report-donut">
          <div aria-hidden="true">
            <PieChart width={148} height={148} accessibilityLayer={false}>
              <Pie
                data={slices}
                dataKey="count"
                cx="50%"
                cy="50%"
                innerRadius={55}
                outerRadius={70}
                startAngle={90}
                endAngle={-270}
                stroke="none"
                rootTabIndex={-1}
                paddingAngle={slices.length > 1 ? 2 : 0}
                isAnimationActive={false}
                onClick={({ payload }) => select?.(payload.key)}
              >
                {slices.map((slice) => (
                  <Cell key={slice.key} fill={colors[slice.key]} />
                ))}
              </Pie>
            </PieChart>
          </div>
          <div className="report-donut-total">
            <strong>{numbers.format(data.total)}</strong>
            <span>{data.total === 1 ? "case" : "cases"}</span>
          </div>
        </div>
        <ul className="report-legend" aria-label="Case counts by status">
          {slices.map(({ key, label, count }) => (
            <li key={key}>
              <a
                href={statusHref?.(key)}
                aria-label={`View ${numbers.format(count)} ${label.toLowerCase()} ${count === 1 ? "case" : "cases"}`}
              >
                <i aria-hidden="true" style={{ background: colors[key] }} />
                <span>{label}</span>
                <strong>{numbers.format(count)}</strong>
                <ArrowUpRight size={15} aria-hidden="true" />
              </a>
            </li>
          ))}
        </ul>
      </div>
    </section>
  );
}

function ProviderChart({ checks }) {
  const groups = Object.entries(kinds)
    .map(([kind, label]) => {
      const rows = checks.filter((row) => row.kind === kind && row.count > 0);
      return {
        kind,
        label,
        rows,
        total: rows.reduce((sum, row) => sum + row.count, 0),
      };
    })
    .filter(({ total }) => total > 0);
  return (
    <section className="panel report-metric report-provider-card">
      <h2>Verification requirements</h2>
      <p className="report-no-checks">
        Each requirement is counted once per case, including reused results and
        checks awaiting dispatch.
      </p>
      {groups.length === 0 ? (
        <p className="report-no-checks">No requirements recorded yet.</p>
      ) : (
        <div className="report-providers">
          {groups.map(({ kind, label, total, rows }) => (
            <details key={kind}>
              <summary>
                <span>{label}</span>
                <strong>{numbers.format(total)}</strong>
                <ChevronDown size={16} aria-hidden="true" />
              </summary>
              <dl>
                {rows.map(({ origin, status, count }) => (
                  <div key={`${origin}-${status}`}>
                    <dt>
                      {statuses[status] || status}
                      <small>
                        {originLabel[origin] || "Awaiting dispatch"}
                      </small>
                    </dt>
                    <dd>{numbers.format(count)}</dd>
                  </div>
                ))}
              </dl>
            </details>
          ))}
        </div>
      )}
    </section>
  );
}

export default function Analytics({
  data,
  statusHref,
  select,
  filtered = false,
}) {
  if (!data.total)
    return (
      <section className="panel report-empty-overview">
        <div className="report-empty-icon">
          <Inbox size={22} aria-hidden="true" />
        </div>
        <div>
          <h2>
            {filtered
              ? "No cases match these filters"
              : "No cases in this period"}
          </h2>
          <p>
            {filtered
              ? "Change a filter or choose a different time range."
              : "Choose a different time range."}
          </p>
        </div>
      </section>
    );
  return (
    <div className="report-metrics">
      <DecisionChart data={data} statusHref={statusHref} select={select} />
      <ProviderChart checks={data.checks} />
    </div>
  );
}
