import { useState } from "react";
import { api, useRead } from "./api";
import { dateTime, href } from "./cases";

const path = "/staff-api/v2/staff/provider-routing";
const actions = {
  paused: "Pause submissions",
  draining: "Drain new assignments",
  capped: "Cap traffic share",
  recovering: "Start recovery",
  normal: "Restore policy",
};

function TrafficControls({staff, overrides, refresh}) {
  const [provider, setProvider] = useState("address-a");
  const [mode, setMode] = useState("draining");
  const [cap, setCap] = useState(20);
  const [reason, setReason] = useState("");
  const [minutes, setMinutes] = useState(15);
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState(false);
  const capped = mode === "capped" || mode === "recovering";

  async function apply(event) {
    event.preventDefault();
    setBusy(true);
    const previous = overrides.find(row => row.product === "wallet" && row.provider === provider);
    try {
      await api(path, {
        method: "PUT",
        headers: {
          Authorization: "Bearer " + staff.token,
          "Content-Type": "application/json",
          "Idempotency-Key": crypto.randomUUID(),
        },
        body: JSON.stringify({
          product: "wallet",
          provider,
          expected_version: previous?.version ?? 0,
          mode,
          share_cap: capped ? Number(cap) : null,
          duration_seconds: Number(minutes) * 60,
          reason: reason.trim(),
        }),
      });
      setMessage("Traffic change recorded.");
      setReason("");
    } catch (error) {
      setMessage(error.message);
    } finally {
      setBusy(false);
      refresh();
    }
  }

  return (
    <form className="ci-form" onSubmit={apply}>
      <label>Vendor
        <select value={provider} onChange={e => setProvider(e.target.value)}>
          <option>address-a</option><option>address-b</option>
        </select>
      </label>
      <label>Traffic action
        <select value={mode} onChange={e => setMode(e.target.value)}>
          {Object.entries(actions).map(([value, label]) => <option key={value} value={value}>{label}</option>)}
        </select>
      </label>
      {capped && <label>Maximum share (%)
        <input type="number" min="0" max="100" value={cap} onChange={e => setCap(e.target.value)} required />
      </label>}
      <label>Duration (minutes)
        <input type="number" min="1" max="1440" value={minutes} onChange={e => setMinutes(e.target.value)} required />
      </label>
      <label>Reason
        <input value={reason} onChange={e => setReason(e.target.value)} required maxLength={512} />
      </label>
      <button disabled={busy || !reason.trim()}>Apply traffic change</button>
      <p role="status">{message}</p>
    </form>
  );
}

export function ProviderRouting({staff}) {
  const [revision, refresh] = useState(0);
  const [page, setPage] = useState(1);
  const [caseId, setCaseId] = useState("");
  const params = new URLSearchParams({page: String(page)});
  if (caseId.trim()) params.set("case_id", caseId.trim());
  const query = path + "?" + params.toString();
  const {data: report, error} = useRead(query, revision, 5000, query, staff.token);
  const canChange = staff.role === "routing_operator" && staff.products.includes("wallet");

  return (
    <section className="panel provider-routing" aria-label="Provider routing">
      <h2>Address provider traffic</h2>
      {error && <p role="alert">{error}</p>}
      <label>Find routes by exact case ID
        <input type="search" value={caseId} onChange={e => setCaseId(e.target.value)} />
      </label>
      {report && <>
        <h3>Cases needing an answer</h3>
        <p>{report.impact.outstanding} awaiting a vendor · {report.impact.blocked} blocked · {report.impact.rescued} approved through fallback</p>
        {report.outstanding.map(row => (
          <p key={row.operation_id}>
            {row.case_id} · {row.provider} · {row.status} since {dateTime(row.since)} · owner {row.owner}
          </p>
        ))}
        {(page > 1 || report.impact.operations > 50) && <nav aria-label="Outstanding operations pages">
          <button disabled={page <= 1} onClick={() => setPage(page - 1)}>Previous</button>
          <span> Page {page} </span>
          <button disabled={page * 50 >= report.impact.operations} onClick={() => setPage(page + 1)}>Next</button>
        </nav>}
        <h3>Recent routes</h3>
        <ul>{report.recent_assignments.map(row => (
          <li key={row.operation_id}>
            <a href={href("staff", {screen:"routing", case:row.case_id, owner:row.product})}>{row.case_id}</a>
            {" · "}{row.provider} · {row.reason} · {row.status} · {dateTime(row.created_at)}
            <details>
              <summary>Assignment details</summary>
              <p>{row.operation_id} · {row.policy_version} · bucket {row.bucket} · owner {row.owner}</p>
              <p>Eligible vendors · {row.candidates.join(", ")} · override version {row.override_version}</p>
              {row.predecessor && <p>Previous operation · {row.predecessor}</p>}
            </details>
          </li>
        ))}</ul>
        <h3>Initial assignments by vendor</h3>
        {report.placement.map(row => <p key={row.provider}>
          {row.provider} · {row.percent}% of initial assignments · {row.authorized} authorized for submission
        </p>)}
        <h3>Assignment reasons</h3>
        {report.share.length ? report.share.map(row => (
          <p key={row.provider + row.reason}>{row.provider} · {row.reason} · {row.assignments}</p>
        )) : <p>No routed cases yet.</p>}
        <h3>Calls and cost</h3>
        {report.calls.map(row => <p key={row.provider + row.lane}>
          {row.provider} · {row.lane} · {row.attempts} authorized · {row.finished} finished
        </p>)}
        {report.spend.map(row => <p key={row.account + row.period + row.status}>
          {row.account} · {row.period} · {row.minor_units} {row.currency} minor units · {row.status}
        </p>)}
        <h3>Recovery</h3>
        {report.recovery.map(row => <p key={row.product + row.provider}>
          {row.provider} · {row.cause} · stage {row.stage}% · {row.samples} observations
        </p>)}
        {report.overrides.map(row => <p key={row.product + row.provider}>
          {row.provider} · {row.mode} {row.share_cap != null && row.share_cap + "%"} · {row.active ? "until" : "expired"} {dateTime(row.expires_at)} · {row.actor} · {row.reason}
        </p>)}
        {canChange && <TrafficControls staff={staff} overrides={report.overrides} refresh={() => refresh(n => n + 1)} />}
      </>}
    </section>
  );
}
