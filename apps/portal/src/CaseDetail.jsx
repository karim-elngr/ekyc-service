import { useEffect, useId, useRef, useState } from "react";
import {
  ArrowLeft,
  ArrowRight,
  ChevronDown,
  ClipboardList,
  FileClock,
  FolderOpen,
  X,
} from "lucide-react";
import { post, useRead } from "./api";
import { Responses, ReviewerPanel } from "./CaseActions";
import {
  Badge,
  CheckRows,
  Identifier,
  checkName,
  actionLabel,
  readable,
  resultLabel,
} from "./CaseEvidence";
import { dateTime, decisionLabel, products, terminal, useCases } from "./cases";
import { fixtureLabels, gateway, options, provide } from "./product-demo";
import "./case-inspector.css";

export { Badge } from "./CaseEvidence";

function revisionChange(view, previous) {
  if (!previous) return "";
  const entries = view.checks.flatMap((check) => {
    const prior = previous.checks.find((c) => c.kind === check.kind);
    if (prior?.status !== check.status)
      return checkName(check.kind) + " · " + resultLabel(check.status);
    if (
      prior.operation_id !== check.operation_id ||
      prior.origin !== check.origin ||
      prior.origin_case_id !== check.origin_case_id
    )
      return checkName(check.kind) + " · Evidence changed";
    return [];
  });
  return entries.join(" / ");
}

export function Revisions({ rows }) {
  return (
    <div className="ci-revisions">
      {rows.map((row, index) => {
        const change = revisionChange(row.view, rows[index + 1]?.view);
        return (
          <details className="ci-revision" key={row.version}>
            <summary>
              <span className="ci-timeline-dot" aria-hidden="true" />
              <span className="ci-row-text">
                <strong>Revision {row.version}</strong>
                <small>{dateTime(row.recorded_at)}</small>
                {change && <small>{change}</small>}
              </span>
              <Badge state={row.view.state} product={row.view.product} />
              <ChevronDown className="ci-down" size={15} aria-hidden="true" />
            </summary>
            <div className="ci-revision-content">
              <p className="ci-note">
                Verification checks at revision {row.version}
              </p>
              <CheckRows checks={row.view.checks} />
            </div>
          </details>
        );
      })}
    </div>
  );
}

function History({ path, version, token }) {
  const [cursors, setCursors] = useState([]);
  const [retry, setRetry] = useState(0);
  const cursor = cursors.at(-1);
  const query = new URLSearchParams({ limit: "8" });
  if (cursor != null) query.set("before_version", cursor);
  const historyPath = path + "/history?" + query;
  const response = useRead(
    historyPath,
    (cursor == null ? version : 0) + ":" + retry,
    0,
    historyPath,
    token,
  );
  const rows = response.data?.revisions;
  return (
    <div className="ci-section-body">
      {response.error && (
        <p role="alert" className="ci-feedback">
          {response.error}{" "}
          <button className="text-button" onClick={() => setRetry(retry + 1)}>
            Try again
          </button>
        </p>
      )}
      {!rows && !response.error && (
        <p role="status" className="ci-note">
          Loading revisions…
        </p>
      )}
      {rows &&
        (rows.length ? (
          <Revisions rows={rows} />
        ) : (
          <p className="ci-note">No revisions recorded.</p>
        ))}
      {(cursors.length > 0 || response.data?.next_before_version != null) && (
        <nav className="ci-pager" aria-label="Revision pages">
          {cursors.length > 0 && (
            <button
              className="text-button"
              onClick={() => setCursors(cursors.slice(0, -1))}
            >
              <ArrowLeft size={14} />
              Newer revisions
            </button>
          )}
          {cursors.length > 1 && (
            <button className="text-button" onClick={() => setCursors([])}>
              Latest revisions
            </button>
          )}
          {response.data?.next_before_version != null && (
            <button
              className="text-button"
              onClick={() =>
                setCursors([...cursors, response.data.next_before_version])
              }
            >
              Older revisions
              <ArrowRight size={14} />
            </button>
          )}
        </nav>
      )}
    </div>
  );
}

function Fold({ title, subtitle, icon: Icon, open, toggle, children }) {
  const id = useId();
  const [visited, setVisited] = useState(open);
  return (
    <section className="ci-fold">
      <h3>
        <button
          type="button"
          aria-expanded={open}
          aria-controls={id}
          onClick={() => {
            setVisited(true);
            toggle();
          }}
        >
          <Icon size={18} aria-hidden="true" />
          <span className="ci-row-text">
            <strong>{title}</strong>
            <small>{subtitle}</small>
          </span>
          <ChevronDown size={16} className="ci-fold-arrow" aria-hidden="true" />
        </button>
      </h3>
      <div id={id} hidden={!open}>
        {(open || visited) && children}
      </div>
    </section>
  );
}

export function CaseDetail({
  product,
  id,
  initial,
  changed,
  readScope = product,
  staff,
  onLoan,
  heading,
}) {
  const path = gateway(product, id);
  const [revision, setRevision] = useState(0);
  const readPath = staff
    ? "/staff-api/v2/cases/" + encodeURIComponent(id)
    : gateway(readScope, id);
  const read = useRead(readPath, revision, 1500, readPath, staff?.token);
  const canOperate = !staff && readScope === product;
  const canReview = canOperate || staff?.role === "reviewer";
  const view = read.data || initial;
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [reviewChoice, setReviewChoice] = useState(null);
  const [historyOpen, setHistoryOpen] = useState(false);
  const [fixes, setFixes] = useState({});
  const lock = useRef(false);
  const receipts = useRef(new Map());
  const priorTransition = useRef(null);
  const focusedControl = useRef(null);
  const headingRef = useRef(null);
  const [announcement, setAnnouncement] = useState("");
  const transition =
    view &&
    [
      decisionLabel(view.state, view.product),
      ...view.checks
        .filter((c) => c.next_action && c.next_action !== "wait_for_provider")
        .map((c) => checkName(c.kind) + " · " + actionLabel(c.next_action)),
    ]
      .filter(Boolean)
      .join(". ");
  useEffect(() => {
    if (!transition) return;
    const previous = priorTransition.current;
    priorTransition.current = transition;
    if (previous === null || previous === transition) return;
    const target = focusedControl.current;
    const active = document.activeElement;
    if (
      target &&
      (!target.isConnected || target.closest("[hidden]")) &&
      (active === document.body ||
        active === target ||
        active?.tagName === "DIALOG")
    ) {
      headingRef.current?.focus({ preventScroll: true });
    }
    // Let readers process the focus change before delivering the outcome.
    const timer = window.setTimeout(() => setAnnouncement(transition), 250);
    return () => window.clearTimeout(timer);
  }, [transition]);

  async function execute(name, body, send) {
    if (lock.current) return;
    lock.current = true;
    setBusy(true);
    setError("");
    setNotice("");
    setAnnouncement("");
    const fingerprint = JSON.stringify([view.version, name, body]);
    const key = receipts.current.get(fingerprint) || crypto.randomUUID();
    receipts.current.set(fingerprint, key);
    try {
      await send(body, key);
      if (name === "restore-screening") {
        const restored = "Mock screening restored. Retry the check to continue.";
        setNotice(restored);
        setAnnouncement(restored);
      } else {
        setNotice("Submission received.");
      }
      setRevision((v) => v + 1);
      changed?.();
    } catch (err) {
      setError(err.message);
    } finally {
      lock.current = false;
      setBusy(false);
    }
  }
  if (!view)
    return (
      <div className="ci-case ci-load">
        {read.error ? (
          <>
            <p role="alert">{read.error}</p>
            <button
              className="secondary"
              onClick={() => setRevision((r) => r + 1)}
            >
              Reload case
            </button>
          </>
        ) : (
          <p role="status">Loading case…</p>
        )}
      </div>
    );
  const missing = view.checks.filter(
    (c) => c.status === "AWAITING_EVIDENCE" && c.kind !== "sanctions",
  );
  const blocked = view.checks.filter((c) => c.status === "BLOCKED");
  const task = view.checks.find(
    (c) => c.status === "REVIEW" && c.kind === "sanctions",
  );
  const originalReview = view.reasons?.some((reason) => reason.code === "ORIGINAL_SCREENING_REVIEW");
  const taskId = task?.operation_id || (originalReview ? "original-screening" : "");
  const reviewOpen =
    reviewChoice?.operation === taskId ? reviewChoice.open : Boolean(taskId);
  const passed = view.checks.filter((c) => c.status === "PASSED").length;
  const title =
    heading ||
    useCases[view.use_case] ||
    readable(view.use_case) ||
    "Case detail";
  return (
    <section
      className="ci-case"
      aria-label={title}
      onFocusCapture={(event) => {
        focusedControl.current = event.target;
      }}
    >
      <header className="ci-header">
        <div className="ci-title-line">
          <h2 tabIndex={-1} ref={headingRef}>
            {title}
          </h2>
          <Badge state={view.state} product={view.product} />
        </div>
        <div className="ci-id">
          <span>Case ID</span>
          <code>
            <Identifier value={view.case_id} />
          </code>
        </div>
      </header>
      <dl className="ci-meta">
        <div>
          <dt>Customer</dt>
          <dd>
            <Identifier value={view.subject_ref} />
          </dd>
        </div>
        <div>
          <dt>Product</dt>
          <dd>{products[view.product] || readable(view.product)}</dd>
        </div>
        <div>
          <dt>Policy</dt>
          <dd>
            <Identifier value={view.policy_version} breakAtSeparators />
          </dd>
        </div>
        <div>
          <dt>Revision</dt>
          <dd>{view.version}</dd>
        </div>
        <div>
          <dt>Opened</dt>
          <dd>{dateTime(view.created_at)}</dd>
        </div>
        {view.decided_at && (
          <div>
            <dt>Decided</dt>
            <dd>{dateTime(view.decided_at)}</dd>
          </div>
        )}
      </dl>
      {(error || read.error) && (
        <p className="ci-feedback" role="alert">
          {error || read.error}
        </p>
      )}
      <div
        role="status"
        aria-live="polite"
        aria-atomic="true"
        className="ci-transition"
      >
        {announcement}
      </div>
      {notice && !announcement && <p className="ci-notice">{notice}</p>}
      <section className="ci-group" aria-label="Verification checks">
        <div className="ci-section-title">
          <h3>Verification checks</h3>
          <span>
            {passed} of {view.checks.length} passed
          </span>
        </div>
        <CheckRows checks={view.checks} />
      </section>
      {canOperate && !terminal(view) && missing.length > 0 && (
        <section className="ci-action">
          <h3>Requested evidence</h3>
          <form
            className="ci-form"
            onSubmit={(e) => {
              e.preventDefault();
              const collected = Object.fromEntries(
                missing.map((c) => [c.kind, fixes[c.kind] || "pass"]),
              );
              execute("evidence", collected, (values, key) =>
                provide(view, values, key),
              );
            }}
          >
            {missing.map((c) => (
              <label key={c.kind}>
                {checkName(c.kind)} evidence
                <select
                  disabled={busy}
                  value={fixes[c.kind] || "pass"}
                  onChange={(e) =>
                    setFixes({ ...fixes, [c.kind]: e.target.value })
                  }
                >
                  {[...options[c.kind], "interactive"].map((opt) => (
                    <option value={opt} key={opt}>
                      {fixtureLabels[opt]}
                    </option>
                  ))}
                </select>
              </label>
            ))}
            <button className="primary" disabled={busy}>
              {missing.length === 1 && missing[0].kind === "address"
                ? "Submit proof of address"
                : "Submit requested evidence"}
            </button>
          </form>
        </section>
      )}
      {canOperate && !terminal(view) && blocked.length > 0 && (
        <section className="ci-action">
          <h3>Provider recovery</h3>
          {blocked.map((c) => (
            <div className="ci-buttons" key={c.kind}>
              {c.kind === "sanctions" && (
                <button
                  className="secondary"
                  disabled={busy}
                  onClick={() =>
                    execute(
                      "restore-screening",
                      { subject_ref: view.subject_ref, scenario: "pass" },
                      (body, key) => post("/product-demo/screening", body, key),
                    )
                  }
                >
                  Restore mock screening
                </button>
              )}
              <button
                className="primary"
                disabled={busy}
                onClick={() =>
                  execute("retry-" + c.kind, { check: c.kind }, (body, key) =>
                    post(path + "/retry", body, key),
                  )
                }
              >
                Retry {checkName(c.kind).toLowerCase()}
              </button>
            </div>
          ))}
        </section>
      )}
      {canOperate && view.checks.some((c) => c.interactive) && (
        <Responses view={view} path={path} busy={busy} execute={execute} />
      )}
      <div className="ci-folds">
        {canReview && (
          <Fold
            title="Reviewer handoff"
            subtitle={
              taskId
                ? "Screening disposition required"
                : "Evidence, alerts and recorded decisions"
            }
            icon={ClipboardList}
            open={reviewOpen}
            toggle={() =>
              setReviewChoice({ operation: taskId, open: !reviewOpen })
            }
          >
            <ReviewerPanel
              view={view}
              task={task}
              token={staff?.token}
              execute={execute}
              busy={busy}
            />
          </Fold>
        )}
        <Fold
          title="Case history"
          subtitle={"Current revision " + view.version}
          icon={FileClock}
          open={historyOpen}
          toggle={() => setHistoryOpen(!historyOpen)}
        >
          <History
            path={readPath}
            version={view.version}
            token={staff?.token}
          />
        </Fold>
      </div>
      {onLoan && view.state === "APPROVED" && (
        <button className="primary ci-loan" disabled={busy} onClick={onLoan}>
          Open loan case <ArrowRight size={14} />
        </button>
      )}
    </section>
  );
}

export function CaseDrawer({ product, id, onClose, changed, staff }) {
  const ref = useRef(null);
  const closeButton = useRef(null);
  const titleId = useId();
  useEffect(() => {
    const dialog = ref.current;
    const opener = document.activeElement;
    dialog.showModal();
    closeButton.current.focus({ preventScroll: true });
    return () => {
      dialog.close();
      if (
        opener instanceof HTMLElement &&
        opener !== document.body &&
        opener.isConnected
      ) {
        opener.focus({ preventScroll: true });
      } else {
        const replacement = "Open case " + id;
        const returnTarget =
          (replacement &&
            document.querySelector(
              'a[aria-label="' + CSS.escape(replacement) + '"]',
            )) ||
          document.querySelector("main[tabindex]");
        returnTarget?.focus({ preventScroll: true });
      }
    };
  }, [id]);
  return (
    <dialog
      ref={ref}
      className="ci-drawer"
      aria-labelledby={titleId}
      onCancel={(e) => {
        e.preventDefault();
        onClose();
      }}
    >
      <div className="ci-toolbar">
        <span id={titleId}>
          <FolderOpen size={17} aria-hidden="true" />
          Case inspector
        </span>
        <button
          ref={closeButton}
          className="icon-button"
          aria-label="Close case"
          onClick={onClose}
        >
          <X size={18} />
        </button>
      </div>
      <div className="ci-drawer-content">
        <CaseDetail
          key={id}
          id={id}
          product={product}
          staff={staff}
          readScope="observer"
          changed={changed}
        />
      </div>
    </dialog>
  );
}
