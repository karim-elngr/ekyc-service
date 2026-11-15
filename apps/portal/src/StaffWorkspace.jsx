import { ProviderRouting } from "./ProviderRouting";
import { useEffect, useState } from "react";
import { api, useRead } from "./api";
import { dateTime, decisionLabel, href, products } from "./cases";
import { CaseDrawer } from "./CaseDetail";
import { Reports } from "./Reports";
import "./staff.css";

const endpoint = "/staff-api/v2";
const workLabels = {
  screening_review: "Screening review",
  late_alert: "Late screening alert",
};
const staffHref = (screen, params = {}) => href("staff", { ...params, screen });

function SignIn({ signedIn, demo }) {
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  async function submit(event) {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      const credential = token.trim();
      const identity = await api(endpoint + "/staff/session", {
        headers: { Authorization: "Bearer " + credential },
      });
      signedIn({ ...identity, token: credential });
    } catch (err) {
      setError(
        err.status === 401 || err.status === 403
          ? "Use a personal staff credential issued by your institution."
          : err.message,
      );
    } finally {
      setBusy(false);
    }
  }
  return (
    <main className="staff-page" id="workspace-content">
      <h1>Staff workspace</h1>
      <form className="panel ci-form staff-login" onSubmit={submit}>
        <h2>Sign in</h2>
        <label>
          Personal access token
          <input
            type="password"
            autoComplete="off"
            value={token}
            required
            onChange={(event) => setToken(event.target.value)}
          />
        </label>
        {error && (
          <p role="alert" className="error-text">
            {error}
          </p>
        )}
        <button className="primary" disabled={busy || !token.trim()}>
          {busy ? "Signing in…" : "Sign in"}
        </button>
      </form>
      {demo && (
        <a className="text-button" href="#overview">
          Return to local demo
        </a>
      )}
    </main>
  );
}

function WorkQueue({ staff, openCase, revision, refresh }) {
  const [filter, setFilter] = useState("open");
  const [cursors, setCursors] = useState([]);
  const [pending, setPending] = useState(false);
  const [feedback, setFeedback] = useState("");
  const query = new URLSearchParams({ status: filter });
  if (cursors.length) query.set("after", cursors.at(-1));
  const path = endpoint + "/investigations?" + query;
  const read = useRead(path, revision, 6000, path, staff.token);
  async function acknowledge(work) {
    setPending(true);
    setFeedback("");
    try {
      const receipt = await api(endpoint + "/investigations/acknowledgement", {
        method: "PUT",
        headers: {
          Authorization: "Bearer " + staff.token,
          "Content-Type": "application/json",
        },
        body: JSON.stringify({ work_id: work.work_id }),
      });
      setFeedback(
        "Acknowledged by " +
          receipt.acknowledged_by +
          " on " +
          dateTime(receipt.acknowledged_at) +
          ".",
      );
      refresh();
    } catch (err) {
      setFeedback(err.message);
    } finally {
      setPending(false);
    }
  }
  return (
    <section className="panel staff-queue" aria-label="Investigation intake">
      <div className="staff-queue-heading">
        <h2>Investigation intake</h2>
        <label>
          Show{" "}
          <select
            value={filter}
            onChange={(e) => {
              setCursors([]);
              setFilter(e.target.value);
              setFeedback("");
            }}
          >
            <option value="open">Open work</option>
            <option value="acknowledged">Acknowledgement history</option>
          </select>
        </label>
      </div>
      <p>
        Screening reviews and late alerts for your assigned product, across all
        case creation dates.
      </p>
      {feedback && <p role="status">{feedback}</p>}
      {read.error && (
        <p role="alert" className="error-text">
          {read.error}
        </p>
      )}
      {!read.data && !read.error && (
        <p role="status">Loading investigations…</p>
      )}
      {read.data?.items.length === 0 && (
        <p>
          No {filter === "open" ? "open investigations" : "acknowledgements"} on
          this page.
        </p>
      )}
      <ul className="staff-items">
        {read.data?.items.map((item) => (
          <li key={item.work_id}>
            <h3>{workLabels[item.kind]}</h3>
            <p>
              {products[item.product] || item.product} ·{" "}
              {decisionLabel(item.state, item.product)} ·{" "}
              {dateTime(item.opened_at)}
            </p>
            <div>
              Original operation <code>{item.operation_id}</code>
            </div>
            {item.acknowledged_by && (
              <p>
                Acknowledged by {item.acknowledged_by} ·{" "}
                {dateTime(item.acknowledged_at)}
              </p>
            )}
            <div className="ci-buttons">
              <button
                className="secondary"
                type="button"
                onClick={() => openCase(item)}
              >
                Inspect case
              </button>
              {!item.acknowledged_by && (
                <button
                  type="button"
                  className="primary"
                  disabled={pending}
                  onClick={() => acknowledge(item)}
                >
                  Acknowledge investigation
                </button>
              )}
            </div>
          </li>
        ))}
      </ul>
      {(cursors.length > 0 || read.data?.next_after) && (
        <nav className="ci-buttons" aria-label="Investigation pages">
          <button
            type="button"
            className="secondary"
            disabled={!cursors.length}
            onClick={() => setCursors(cursors.slice(0, -1))}
          >
            Previous page
          </button>
          <button
            type="button"
            className="secondary"
            disabled={!read.data?.next_after}
            onClick={() => setCursors([...cursors, read.data.next_after])}
          >
            Next page
          </button>
        </nav>
      )}
    </section>
  );
}

export function StaffWorkspace({ params, demo }) {
  const [staff, setStaff] = useState(null);
  const [revision, setRevision] = useState(0);
  const navigate = (screen, query) => {
    location.hash = staffHref(screen, query);
  };
  const refresh = () => setRevision((n) => n + 1);
  const reviewer = staff?.role === "reviewer";
  const screen =
    params.screen === "routing" || (!params.screen && staff?.role==="routing_operator") ? "routing" : reviewer && !["overview", "cases"].includes(params.screen)
      ? "investigations"
      : params.screen === "overview"
        ? "overview"
        : "cases";
  useEffect(() => {
    document.title =
      (staff
        ? screen === "investigations"
          ? "Investigations"
          : "Verification reports"
        : "Staff sign in") + " · Verity eKYC";
  }, [staff, screen]);
  if (!staff) return <SignIn signedIn={setStaff} demo={demo} />;
  return (
    <main className="staff-page" id="workspace-content">
      <header className="staff-header">
        <div>
          <h1>
            {screen === "routing" ? "Provider routing" : screen === "investigations"
              ? "Investigations"
              : screen === "overview"
                ? "Verification overview"
                : "Verification cases"}
          </h1>
          <p>
            {staff.actor_id} · {staff.institution} ·{" "}
            {staff.products
              .map((product) => products[product] || product)
              .join(", ")}
          </p>
        </div>
        <button
          className="secondary"
          onClick={() => {
            setStaff(null);
            location.hash = "#staff";
          }}
        >
          Sign out
        </button>
      </header>
      <nav className="staff-nav" aria-label="Staff workspace">
        {Object.entries({
          ...(reviewer ? { investigations: "Investigations" } : {}),
          overview: "Overview",
          cases: "Cases",
          routing: "Provider routing",
        }).map(([key, label]) => (
          <a
            className="text-button"
            key={key}
            aria-current={screen === key ? "page" : undefined}
            href={staffHref(key)}
          >
            {label}
          </a>
        ))}
      </nav>
      {screen === "routing" ? <ProviderRouting staff={staff}/> : screen === "investigations" ? (
        <WorkQueue
          staff={staff}
          revision={revision}
          refresh={refresh}
          openCase={(item) =>
            navigate(screen, {
              ...params,
              case: item.case_id,
              owner: item.product,
            })
          }
        />
      ) : (
        <Reports
          page={screen}
          params={params}
          navigate={navigate}
          revision={revision}
          staffToken={staff.token}
          reportHref={staffHref}
        />
      )}
      {params.case && params.owner && (
        <CaseDrawer
          id={params.case}
          product={params.owner}
          staff={staff}
          changed={refresh}
          onClose={() => navigate(screen, { ...params, case: "", owner: "" })}
        />
      )}
    </main>
  );
}
