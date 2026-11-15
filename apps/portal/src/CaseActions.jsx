import { useId, useState } from "react";
import { ArrowUpRight, ChevronRight, ShieldAlert } from "lucide-react";
import { post, useRead } from "./api";
import {
  Badge,
  checkName,
  Fields,
  readable,
  reasonLabel,
} from "./CaseEvidence";
import { dateTime, terminal } from "./cases";
import { fixtureLabels, gateway, options } from "./product-demo";

const dispositionLabels = {
  clear: "Match cleared",
  confirm: "Match confirmed",
};
const methods = {
  document_verification: "Document verification",
  face_match: "Face matching",
  address_verification: "Address verification",
  sanctions_screening: "Sanctions screening",
};
const providers = {
  document: "Document provider",
  face: "Face provider",
  address: "Address provider",
  sanctions: "Screening provider",
};

function evidenceFields(record) {
  return [
    ["Provider", providers[record.provider] || record.provider],
    ["Verification method", methods[record.method] || readable(record.method)],
    ["Observed", record.observed_at && dateTime(record.observed_at)],
    ["Source case", record.origin_case_id, true],
    ["Provider operation", record.operation_id, true],
    ["Issuer reference", record.evidence_ref, true],
    ["Evidence revision", record.evidence_revision, true],
  ];
}

function Record({ title, subtitle, status, children }) {
  return (
    <details className="ci-record">
      <summary>
        <span className="ci-row-text">
          <strong>{title}</strong>
          {subtitle && <small>{subtitle}</small>}
        </span>
        {status && <Badge state={status} />}
        <ChevronRight className="ci-chevron" size={15} aria-hidden="true" />
      </summary>
      <div className="ci-inset">{children}</div>
    </details>
  );
}

export function ReviewRecords({ data }) {
  return (
    <>
      {data.screening_obligations?.length > 0 && (
        <section className="ci-record-group">
          <h4>Screening operations</h4>
          {data.screening_obligations.map((hold) => (
            <Record key={hold.original_operation} title={hold.provider}
              subtitle={hold.outcome ? readable(hold.outcome) : "Awaiting vendor result"}>
              <Fields entries={[
                ["Vendor operation", hold.original_operation, true],
                ["Replacement operation", hold.replacement_operation, true],
                ["Evidence revision", hold.evidence_revision, true],
                ["Assigned to", hold.assigned_to],
                ["Assignment due", dateTime(hold.assign_at)],
              ]} />
            </Record>
          ))}
        </section>
      )}
      {data.alerts?.length > 0 && (
        <section className="ci-record-group">
          <h4>
            <ShieldAlert size={15} aria-hidden="true" />
            Late screening alerts <span>{data.alerts.length}</span>
          </h4>
          {data.alerts.map((alert) => (
            <Record
              key={alert.event_id}
              title={reasonLabel(alert.reason)}
              subtitle={dateTime(alert.observed_at)}
            >
              <Fields
                entries={[
                  ...evidenceFields(alert),
                  ["Event ID", alert.event_id, true],
                  ["Reason code", alert.reason, true],
                ]}
              />
            </Record>
          ))}
        </section>
      )}
      {data.reviews?.length > 0 && (
        <section className="ci-record-group">
          <h4>
            Recorded decisions <span>{data.reviews.length}</span>
          </h4>
          {data.reviews.map((review) => (
            <Record
              key={review.operation_id}
              title={
                dispositionLabels[review.action] || readable(review.action)
              }
              subtitle={review.actor + " · " + dateTime(review.recorded_at)}
            >
              <Fields
                entries={[
                  ["Decision reason", review.reason],
                  ["Investigation reference", review.review_ref, true],
                  ["Check", checkName(review.kind)],
                  ["Provider operation", review.operation_id, true],
                ]}
              />
            </Record>
          ))}
        </section>
      )}
      {data.checks?.length > 0 && (
        <section className="ci-record-group">
          <h4>
            Provider evidence <span>{data.checks.length}</span>
          </h4>
          {data.checks.map((check) => (
            <Record
              key={check.kind}
              title={checkName(check.kind)}
              subtitle={methods[check.method] || readable(check.method)}
              status={check.status}
            >
              <Fields
                entries={[
                  ...evidenceFields(check),
                  ["Reasons", check.reasons?.map(reasonLabel).join(" · ")],
                  ["Reason codes", check.reasons?.join(", "), true],
                ]}
              />
            </Record>
          ))}
        </section>
      )}
      {!data.screening_obligations?.length &&
        !data.checks?.length &&
        !data.reviews?.length &&
        !data.alerts?.length && (
          <p className="ci-note">No reviewer records yet.</p>
        )}
    </>
  );
}

export function reviewNoteError(value) {
  const text = value.replace(/\r\n/g, "\n");
  if (!/[^\p{White_Space}]/u.test(text)) return "Enter a decision reason.";
  if (Array.from(text).length > 512) return "Use 512 characters or fewer.";
  if (/[\u0000-\u0009\u000b-\u001f\u007f-\u009f\ud800-\udfff]/u.test(text))
    return "Use text and line breaks only. Remove tabs and other control characters.";
  return "";
}

function DispositionForm({ operation, path, execute, busy, token }) {
  const [action, setAction] = useState("");
  const [reason, setReason] = useState("");
  const [reviewRef, setReviewRef] = useState("");
  const [touched, setTouched] = useState(false);
  const reasonId = useId();
  const [serverNoteError, setServerNoteError] = useState("");
  const noteError = serverNoteError || reviewNoteError(reason);
  const refError =
    new TextEncoder().encode(reviewRef).length > 256 ||
    /[\u0000-\u001f\u007f-\u009f]/u.test(reviewRef);

  return (
    <form
      className="ci-form ci-disposition"
      onSubmit={(event) => {
        event.preventDefault();
        setTouched(true);
        if (noteError || refError) return;
        execute(
          "review",
          {
            check: "sanctions",
            operation_id: operation,
            disposition: action,
            reason: reason.replace(/\r\n/g, "\n"),
            review_ref: reviewRef.trim(),
          },
          async (body, key) => {
            try {
              await post(path, body, key, token);
            } catch (error) {
              if (error.code !== "invalid_review_reason") throw error;
              const hint =
                "Enter 1–512 characters using text and line breaks only.";
              setServerNoteError(hint);
              throw new Error(hint);
            }
          },
        );
      }}
    >
      <h4>Screening decision</h4>
      <label>
        Disposition
        <select
          required
          value={action}
          disabled={busy}
          onChange={(e) => setAction(e.target.value)}
        >
          <option value="" disabled>
            Choose a disposition
          </option>
          <option value="clear">Clear match</option>
          <option value="confirm">Confirm match</option>
        </select>
      </label>
      <label>
        Investigation reference
        <input
          required
          pattern=".*\S.*"
          aria-invalid={refError || undefined}
          aria-describedby={refError ? reasonId + "-ref" : undefined}
          disabled={busy}
          value={reviewRef}
          onChange={(e) => setReviewRef(e.target.value)}
        />
      </label>
      {refError && (
        <p id={reasonId + "-ref"} className="error-text">
          Use an investigation reference of up to 256 UTF-8 bytes without
          control characters.
        </p>
      )}
      <label>
        Decision reason
        <textarea
          required
          rows={3}
          onBlur={() => setTouched(true)}
          aria-invalid={Boolean((touched || reason) && noteError)}
          aria-describedby={reasonId}
          disabled={busy}
          value={reason}
          onChange={(e) => {
            setServerNoteError("");
            setReason(e.target.value);
          }}
        />
      </label>
      <p
        id={reasonId}
        className={noteError && (touched || reason) ? "error-text" : "ci-note"}
      >
        {(touched || reason) && noteError
          ? noteError
          : "Up to 512 characters. Line breaks are allowed."}
      </p>
      <button
        className="primary"
        disabled={
          busy || !action || Boolean(noteError) || refError || !reviewRef.trim()
        }
      >
        Record disposition
      </button>
    </form>
  );
}

export function ReviewerPanel({ view, task, execute, busy, token }) {
  const path = token
    ? "/staff-api/v2/cases/" + encodeURIComponent(view.case_id) + "/review"
    : gateway(view.product + "-reviewer", view.case_id) + "/review";
  const read = useRead(path, view.version, 5000, path, token);
  const record =
    task?.operation_id &&
    read.data?.checks.find(
      (c) =>
        c.kind === "sanctions" &&
        c.status === "REVIEW" &&
        c.operation_id === task.operation_id,
    );
  return (
    <div className="ci-review ci-section-body">
      {read.error && (
        <p className="ci-feedback" role="alert">
          {read.error}
        </p>
      )}
      {!read.data && !read.error && (
        <p className="ci-note" role="status">
          Loading reviewer records…
        </p>
      )}
      {!terminal(view) && read.data?.screening_obligations?.filter(
        (hold) => ["hit", "inconclusive"].includes(hold.outcome) && !hold.review &&
          hold.original_operation !== record?.operation_id,
      ).map((hold) => (
        <section key={hold.original_operation}>
          <h4>Review screening at {hold.provider}</h4>
          <p>{hold.original_operation}</p>
          <DispositionForm operation={hold.original_operation} path={path} token={token}
            execute={execute} busy={busy || Boolean(read.error)} />
        </section>
      ))}
      {record && (
        <DispositionForm
          key={record.operation_id}
          operation={record.operation_id}
          path={path}
          token={token}
          execute={execute}
          busy={busy || Boolean(read.error)}
        />
      )}
      {read.data && <ReviewRecords data={read.data} />}
    </div>
  );
}

const responseStates = {
  registering: "Registering",
  awaiting_response: "Awaiting response",
  sending: "Sending",
  sent: "Sent",
  closed: "Closed",
  unavailable: "Unavailable",
};

export function Responses({ view, path, execute, busy }) {
  const read = useRead(
    path + "/interactive",
    view.version,
    terminal(view) ? 0 : 1300,
  );
  const [selections, setSelections] = useState({});
  return (
    <section className="ci-action">
      <h3>Live provider responses</h3>
      {read.error && (
        <p className="ci-feedback" role="alert">
          {read.error}
        </p>
      )}
      {!read.data && !read.error && (
        <p className="ci-note" role="status">
          Loading provider operations…
        </p>
      )}
      {read.data?.checks.map((check) => {
        const value = selections[check.operation_id] || "pass";
        const submitted = check.response?.replace(/^demo:/, "");
        return (
          <div className="ci-live" key={check.operation_id || check.kind}>
            <div className="ci-section-title">
              <h4>{checkName(check.kind)}</h4>
              <span>
                {responseStates[check.status] || readable(check.status)}
              </span>
            </div>
            {check.invocation_id && (
              <a
                className="text-button"
                href={"/invocations/" + encodeURIComponent(check.invocation_id)}
                target="_blank"
                rel="noreferrer"
              >
                Restate invocation <ArrowUpRight size={13} />
              </a>
            )}
            {check.status === "awaiting_response" && !terminal(view) && (
              <form
                className="ci-form ci-inline"
                onSubmit={(e) => {
                  e.preventDefault();
                  execute(
                    check.operation_id,
                    { response: "demo:" + value },
                    (body, key) =>
                      post(
                        path +
                          "/interactive/" +
                          check.kind +
                          "?" +
                          new URLSearchParams({
                            operation_id: check.operation_id,
                          }),
                        body,
                        key,
                      ),
                  );
                }}
              >
                <label>
                  {checkName(check.kind)} response
                  <select
                    disabled={busy}
                    value={value}
                    onChange={(e) =>
                      setSelections({
                        ...selections,
                        [check.operation_id]: e.target.value,
                      })
                    }
                  >
                    {[...options[check.kind], "unavailable"].map((opt) => (
                      <option key={opt} value={opt}>
                        {fixtureLabels[opt]}
                      </option>
                    ))}
                  </select>
                </label>
                <button className="secondary" disabled={busy}>
                  Send response
                </button>
              </form>
            )}
            {submitted && (
              <p className="ci-note">
                Submitted response ·{" "}
                {fixtureLabels[submitted] || readable(submitted)}
              </p>
            )}
          </div>
        );
      })}
    </section>
  );
}
