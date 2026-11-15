import { Fragment } from "react";
import {
  ChevronRight,
  FileCheck2,
  Fingerprint,
  ScanFace,
  ShieldCheck,
} from "lucide-react";
import { dateTime, decisionLabel, kinds, states, statuses } from "./cases";

export function readable(value) {
  if (value == null || value === "") return "";
  const words = String(value).replace(/[_-]+/g, " ").toLowerCase();
  return words[0].toUpperCase() + words.slice(1);
}

export function Identifier({ value, breakAtSeparators = false }) {
  const text = String(value ?? "");
  if (!breakAtSeparators && text.length < 32) return text;
  const parts = text
    .split(/(?<=[._~/-])/)
    .flatMap((part) =>
      /^[a-f\d]{32,}[._~/-]?$/i.test(part) ? part.match(/.{1,8}/g) : [part],
    );
  return parts.map((part, index) => (
    <Fragment key={index}>
      {index > 0 && <wbr />}
      {part}
    </Fragment>
  ));
}

const explanations = {
  ADDRESS_PROOF_FUTURE: "The proof is dated in the future.",
  ADDRESS_PROOF_STALE: "The proof is too old for this policy.",
  DOCUMENT_EXPIRED: "The identity document has expired.",
  EVIDENCE_SUPERSEDED: "Newer evidence replaced this record.",
  MISSING_EVIDENCE: "Evidence has been requested.",
  REUSE_EXPIRED: "The earlier verification has expired.",
  REUSE_SUPERSEDED: "A later observation supersedes this verification.",
  SANCTIONS_CLEARED_BY_REVIEW: "A reviewer cleared the potential match.",
  SANCTIONS_CONFIRMED: "A reviewer confirmed the match.",
  SANCTIONS_POTENTIAL_MATCH: "The screening found a potential match.",
  POTENTIAL_SANCTIONS_MATCH: "The screening found a potential match.",
  SANCTIONS_UNAVAILABLE: "The screening provider was unavailable.",
  SCREENING_IDENTITY_UNAVAILABLE:
    "Screening is waiting for a verified identity.",
  VERIFIED_IDENTITY_REQUIRED: "A verified identity is required.",
};

export const reasonLabel = (code) => explanations[code] || readable(code);
export const resultLabel = (state) =>
  states[state] || statuses[state] || readable(state);
export const checkName = (kind) => kinds[kind] || readable(kind);
const origins = {
  fresh: "Fresh provider check",
  reused: "Reused from an earlier case",
};
const nextActions = {
  provide_evidence: "Submit requested evidence",
  verify_document: "Verify the identity document",
  wait_for_provider: "Await provider response",
  retry: "Retry the provider check",
  review: "Review the screening result",
};
export const actionLabel = (action) => nextActions[action] || readable(action);

export function Badge({ state, product }) {
  return (
    <span className={"badge " + state.toLowerCase()}>
      <i aria-hidden="true" />
      {decisionLabel(state, product) || resultLabel(state)}
    </span>
  );
}

export function Fields({ entries }) {
  const present = entries.filter(([, value]) => value != null && value !== "");
  return (
    present.length > 0 && (
      <dl className="ci-fields">
        {present.map(([name, value, identifier]) => (
          <div key={name}>
            <dt>{name}</dt>
            <dd>
              {identifier ? (
                <code>
                  <Identifier value={value} />
                </code>
              ) : (
                value
              )}
            </dd>
          </div>
        ))}
      </dl>
    )
  );
}

export function CheckRows({ checks }) {
  const icons = {
    document: Fingerprint,
    face: ScanFace,
    address: FileCheck2,
    sanctions: ShieldCheck,
  };
  return (
    <div className="ci-checks">
      {checks.map((check) => {
        const Icon = icons[check.kind] || FileCheck2;
        const reasons = check.reasons || [];
        const origin = origins[check.origin] || readable(check.origin);
        const summary =
          reasons.length && !["PASSED", "PENDING"].includes(check.status)
            ? reasons.map(reasonLabel).join(" · ")
            : origin || actionLabel(check.next_action);
        const fields = [
          ["Evidence source", origin],
          ["Source case", check.origin_case_id, true],
          ["Provider operation", check.operation_id, true],
          ["Attempt", check.attempt > 0 ? check.attempt : null],
          ["Observed", check.observed_at && dateTime(check.observed_at)],
          ["Next step", actionLabel(check.next_action)],
          ["Reasons", reasons.map(reasonLabel).join(" · ")],
          ["Reason codes", reasons.join(", "), true],
        ];
        return (
          <details key={check.kind} className="ci-check">
            <summary>
              <span className="ci-check-icon" aria-hidden="true">
                <Icon size={17} />
              </span>
              <span className="ci-row-text">
                <strong>{checkName(check.kind)}</strong>
                {summary && <small>{summary}</small>}
              </span>
              <Badge state={check.status} />
              <ChevronRight
                className="ci-chevron"
                size={15}
                aria-hidden="true"
              />
            </summary>
            <div className="ci-inset">
              <Fields entries={fields} />
            </div>
          </details>
        );
      })}
    </div>
  );
}
