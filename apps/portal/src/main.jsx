import { useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import { FlaskConical, RefreshCw } from "lucide-react";
import "@fontsource-variable/dm-sans";
import "./style.css";
import { href, readRoute } from "./cases";
import { Reports } from "./Reports";
import { CaseDrawer } from "./CaseDetail";
import { StaffWorkspace } from "./StaffWorkspace";
import { useRead } from "./api";
import { ScenarioLab } from "./ScenarioLab";
import { pages, WorkspaceNavigation } from "./WorkspaceNavigation";

function App() {
  const [route, setRoute] = useState(() => readRoute(location.hash));
  const [revision, setRevision] = useState(0);
  const config = useRead("/portal-config");
  const { page, params } = route;
  useEffect(() => {
    const change = () => setRoute(readRoute(location.hash));
    window.addEventListener("hashchange", change);
    return () => window.removeEventListener("hashchange", change);
  }, []);
  useEffect(() => {
    document.title = pages[page].name + " · Verity eKYC";
    window.scrollTo(0, 0);
  }, [page]);
  const navigate = (name, query) => {
    location.hash = href(name, query);
  };
  const refresh = () => setRevision((n) => n + 1);

  if (!config.data)
    return (
      <main>
        <p role={config.error ? "alert" : "status"}>
          {config.error || "Loading workspace…"}
        </p>
      </main>
    );
  if (config.data.mode !== "demo" || page === "staff")
    return (
      <StaffWorkspace params={params} demo={config.data.mode === "demo"} />
    );
  return (
    <div className="shell">
      <a
        className="skip-link"
        href="#workspace-content"
        onClick={(event) => {
          event.preventDefault();
          document.getElementById("workspace-content").focus();
        }}
      >
        Skip to content
      </a>
      <WorkspaceNavigation page={page} />
      <main className="workspace-content" id="workspace-content" tabIndex={-1}>
        <header className="page-heading">
          <h1>{pages[page].title}</h1>
          {page !== "lab" && (
            <div className="page-actions">
              <button
                type="button"
                className="icon-button"
                title="Refresh report"
                aria-label="Refresh report"
                onClick={() => {
                  navigate(page, { ...params, at: "", page: "" });
                  refresh();
                }}
              >
                <RefreshCw size={17} aria-hidden="true" />
              </button>
              <a className="primary" href="#lab">
                <FlaskConical size={16} aria-hidden="true" />
                Open scenario lab
              </a>
            </div>
          )}
        </header>
        {page === "lab" ? (
          <ScenarioLab key={params.preset || "default"} changed={refresh} initialPreset={params.preset} />
        ) : (
          <Reports
            page={page}
            params={params}
            navigate={navigate}
            revision={revision}
          />
        )}
      </main>
      {page !== "lab" &&
        params.case &&
        ["wallet", "lending"].includes(params.owner) && (
          <CaseDrawer
            product={params.owner}
            id={params.case}
            changed={refresh}
            onClose={() => navigate(page, { ...params, case: "", owner: "" })}
          />
        )}
    </div>
  );
}
createRoot(document.getElementById("root")).render(<App />);
