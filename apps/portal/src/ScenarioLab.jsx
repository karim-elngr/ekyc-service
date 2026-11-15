import { useEffect, useRef, useState } from "react";
import {
  ChevronDown,
  FlaskConical,
  Layers3,
  Play,
  SlidersHorizontal,
} from "lucide-react";
import { CaseDetail } from "./CaseDetail";
import { checkScope, kinds, productFor, useCases } from "./cases";
import {
  fixtureLabels,
  freshRun,
  openCase,
  presets,
  providerGroups,
  scenarioGroups,
} from "./product-demo";
import "./scenario-lab.css";

const activeStorage = "verity.v2.scenario-lab";
const pendingStorage = activeStorage + ".pending";
function readStoredRun(key) {
  try {
    const run = JSON.parse(sessionStorage.getItem(key));
    const preset = presets.find((item) => item.id === run?.preset);
    if (
      !preset ||
      preset.useCase !== run.useCase ||
      !run.key ||
      !run.subject ||
      (key === activeStorage && !run.id)
    )
      return null;
    return run;
  } catch {
    return null;
  }
}
function remember(key, value) {
  try {
    if (value) sessionStorage.setItem(key, JSON.stringify(value));
    else sessionStorage.removeItem(key);
  } catch {
    // Opening and retrying a case also works when session storage is unavailable.
  }
}

function ProviderSettings({ preset, fixtures, change, reset, busy }) {
  const scope = checkScope(preset.useCase);
  const modified = scope.filter(
    (kind) => (fixtures[kind] || "pass") !== (preset.fixtures[kind] || "pass"),
  ).length;
  return (
    <details className="scenario-settings">
      <summary>
        <SlidersHorizontal size={17} aria-hidden="true" />
        <span>Provider settings</span>
        <small>
          {modified ? `${modified} customized` : "Scenario defaults"}
        </small>
        <ChevronDown
          size={16}
          className="settings-chevron"
          aria-hidden="true"
        />
      </summary>
      <fieldset disabled={busy}>
        <legend className="scenario-sr-only">Provider responses</legend>
        {scope.map((kind) => {
          const awaitsDocument =
            fixtures.document === "missing" &&
            (kind === "face" || kind === "sanctions");
          return (
            <div className="scenario-field" key={kind}>
              <label htmlFor={"lab-provider-" + kind}>{kinds[kind]}</label>
              <select
                id={"lab-provider-" + kind}
                value={awaitsDocument ? "" : fixtures[kind] || "pass"}
                disabled={awaitsDocument}
                onChange={(event) =>
                  change({ ...fixtures, [kind]: event.target.value })
                }
              >
                {awaitsDocument ? (
                  <option value="">Requires a document</option>
                ) : (
                  Object.entries(providerGroups(kind)).map(
                    ([label, values]) => (
                      <optgroup key={label} label={label}>
                        {values.map((value) => (
                          <option key={value} value={value}>
                            {value === "unavailable"
                              ? kind === "sanctions"
                                ? "Outage until screening is restored"
                                : "Persistent outage (case stays blocked)"
                              : fixtureLabels[value]}
                          </option>
                        ))}
                      </optgroup>
                    ),
                  )
                )}
              </select>
            </div>
          );
        })}
      </fieldset>
      {modified > 0 && (
        <button
          className="text-button scenario-reset"
          type="button"
          onClick={reset}
          disabled={busy}
        >
          Restore scenario defaults
        </button>
      )}
    </details>
  );
}

export function ScenarioLab({ changed, initialPreset }) {
  const requested = presets.find(item => item.id === initialPreset);
  const [run, setRun] = useState(() => requested ? null : readStoredRun(activeStorage));
  const [draft, setDraft] = useState(() =>
    run || requested ? null : readStoredRun(pendingStorage),
  );
  const [selected, setSelected] = useState(
    run?.preset || draft?.preset || requested?.id || "direct-loan",
  );
  const preset = presets.find((item) => item.id === selected);
  const [fixtures, setFixtures] = useState(
    () => draft?.fixtures || preset.fixtures,
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const lock = useRef(false);
  const picker = useRef(null);
  const activeTitle = useRef(null);
  const lastRun = useRef(Boolean(run));

  useEffect(() => {
    if (lastRun.current !== Boolean(run)) {
      (run ? activeTitle : picker).current?.focus();
      lastRun.current = Boolean(run);
    }
  }, [run]);

  function changeFixtures(values) {
    setFixtures(values);
    setDraft(null);
    remember(pendingStorage, null);
    setError("");
  }
  function choose(id) {
    const next = presets.find((item) => item.id === id);
    setSelected(next.id);
    changeFixtures(next.fixtures);
  }
  function reset() {
    setRun(null);
    remember(activeStorage, null);
    choose(run.preset);
  }
  async function launch(second = false) {
    if (lock.current) return;
    lock.current = true;
    setBusy(true);
    setError("");
    try {
      const current = second
        ? run
        : draft || {
            ...freshRun(),
            preset: preset.id,
            useCase: preset.useCase,
            fixtures: { ...fixtures },
          };
      if (!second) {
        setDraft(current);
        remember(pendingStorage, current);
      }
      const useCase = second ? "loan_application" : current.useCase;
      const result = await openCase(current, useCase, current.fixtures, second);
      if (!result.case_id) throw new Error("The service returned no case ID.");
      const next = { ...current, [second ? "nextId" : "id"]: result.case_id };
      remember(activeStorage, next);
      remember(pendingStorage, null);
      setRun(next);
      setDraft(null);
      changed?.();
    } catch (err) {
      setError(err.message);
    } finally {
      lock.current = false;
      setBusy(false);
    }
  }
  if (run)
    return (
      <div className="lab-active">
        <div className="lab-toolbar panel">
          <h2 tabIndex={-1} ref={activeTitle}>
            <FlaskConical size={18} aria-hidden="true" />
            {preset.name}
          </h2>
          <button
            type="button"
            className="secondary"
            disabled={busy}
            onClick={reset}
          >
            Choose another scenario
          </button>
        </div>
        {error && (
          <p role="alert" className="error-text">
            {error}
          </p>
        )}
        <div
          className={run.preset === "reuse" ? "lab-cases pair" : "lab-cases"}
        >
          <article className="panel">
            <CaseDetail
              key={run.id}
              id={run.id}
              product={productFor(run.useCase)}
              heading={useCases[run.useCase]}
              changed={changed}
              onLoan={
                run.preset === "reuse" && !run.nextId
                  ? () => launch(true)
                  : null
              }
            />
          </article>
          {run.nextId && (
            <article className="panel">
              <CaseDetail
                key={run.nextId}
                id={run.nextId}
                product="lending"
                heading="Loan application"
                changed={changed}
              />
            </article>
          )}
          {run.preset === "reuse" && !run.nextId && (
            <article className="panel waiting-loan">
              <Layers3 size={20} aria-hidden="true" />
              <h2>Loan application</h2>
              <p>Open the loan after the wallet approves this customer.</p>
            </article>
          )}
        </div>
      </div>
    );
  return (
    <form
      className="panel scenario-launcher"
      aria-label="Configure verification scenario"
      onSubmit={(event) => {
        event.preventDefault();
        launch();
      }}
    >
      <div className="scenario-choice">
        <label htmlFor="lab-scenario">Scenario</label>
        <select
          ref={picker}
          id="lab-scenario"
          aria-describedby="lab-description"
          disabled={busy}
          value={preset.id}
          onChange={(event) => choose(event.target.value)}
        >
          {Object.entries(scenarioGroups).map(([group, items]) => (
            <optgroup label={group} key={group}>
              {items.map((item) => (
                <option key={item.id} value={item.id}>
                  {item.name}
                </option>
              ))}
            </optgroup>
          ))}
        </select>
        <p id="lab-description">{preset.description}</p>
      </div>
      <ProviderSettings
        key={preset.id}
        preset={preset}
        fixtures={fixtures}
        change={changeFixtures}
        reset={() => changeFixtures(preset.fixtures)}
        busy={busy}
      />
      {checkScope(preset.useCase).some(
        (kind) => kind !== "sanctions" && fixtures[kind] === "unavailable",
      ) && (
        <p role="status">
          This persistent outage keeps the affected check blocked on every
          retry. Choose Transient outage to test a provider that recovers.
        </p>
      )}
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
      <button type="submit" className="primary" disabled={busy}>
        <Play size={14} aria-hidden="true" />
        {busy ? "Submitting…" : draft ? "Retry opening case" : "Start case"}
      </button>
    </form>
  );
}
