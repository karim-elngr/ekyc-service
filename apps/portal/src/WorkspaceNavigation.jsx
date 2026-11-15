import { useEffect, useRef, useState } from "react";
import {
  ArrowUpRight,
  BookOpen,
  Database,
  FlaskConical,
  FolderOpen,
  LayoutDashboard,
  Menu,
  Monitor,
  ClipboardList,
  X,
} from "lucide-react";
import { href } from "./cases";

export const pages = {
  overview: {
    name: "Overview",
    title: "Verification overview",
    icon: LayoutDashboard,
  },
  cases: { name: "Cases", title: "Verification cases", icon: FolderOpen },
  lab: { name: "Scenario lab", title: "Scenario lab", icon: FlaskConical },
  staff: {
    name: "Staff workspace",
    title: "Investigations",
    icon: ClipboardList,
  },
};

function Brand({ onNavigate }) {
  return (
    <a
      className="brand"
      href="#overview"
      onClick={onNavigate}
      aria-label="Verity overview"
    >
      <img src="/verity.svg" alt="" width="38" height="38" />
      <span>
        <strong>Verity</strong>
        <small>eKYC Console</small>
      </span>
    </a>
  );
}

function Navigation({ page, onNavigate }) {
  return (
    <>
      <nav className="workspace-links" aria-label="Workspace">
        {Object.entries(pages).map(([key, item]) => (
          <a
            key={key}
            href={href(key)}
            aria-current={key === page ? "page" : undefined}
            onClick={onNavigate}
          >
            <item.icon size={18} aria-hidden="true" />
            {item.name}
          </a>
        ))}
        {[
          { name: "API guide", path: "/docs/", icon: BookOpen, className: "guide-link" },
          { name: "Database schema", path: "/schema/", icon: Database },
        ].map(({ name, path, icon: Icon, className }) => (
          <a
            key={path}
            className={className}
            href={path}
            target="_blank"
            rel="noopener noreferrer"
            onClick={onNavigate}
            aria-label={`${name} (opens in a new tab)`}
          >
            <Icon size={18} aria-hidden="true" />
            {name}
            <ArrowUpRight className="external-link-icon" size={14} aria-hidden="true" />
          </a>
        ))}
      </nav>
      <div className="sidebar-footer">
        <Monitor size={17} aria-hidden="true" />
        <span>
          <strong>Local environment</strong>
          <small>Demo institution</small>
        </span>
      </div>
    </>
  );
}

export function WorkspaceNavigation({ page }) {
  const drawer = useRef(null);
  const [expanded, setExpanded] = useState(false);
  const close = () => drawer.current?.close();

  useEffect(() => {
    const mobile = window.matchMedia("(max-width: 800px)");
    const onResize = () => {
      if (!mobile.matches) close();
    };
    mobile.addEventListener("change", onResize);
    window.addEventListener("hashchange", close);
    return () => {
      mobile.removeEventListener("change", onResize);
      window.removeEventListener("hashchange", close);
    };
  }, []);

  return (
    <>
      <aside className="sidebar">
        <Brand />
        <Navigation page={page} />
      </aside>
      <header className="mobile-bar">
        <Brand />
        <button
          type="button"
          className="icon-button"
          aria-label="Open navigation"
          aria-haspopup="dialog"
          aria-controls="workspace-menu"
          aria-expanded={expanded}
          onClick={() => {
            drawer.current.showModal();
            setExpanded(true);
          }}
        >
          <Menu size={20} aria-hidden="true" />
        </button>
      </header>
      <dialog
        id="workspace-menu"
        className="mobile-drawer"
        ref={drawer}
        aria-label="Workspace navigation"
        onClose={() => setExpanded(false)}
        onClick={(event) => {
          if (
            event.target === drawer.current &&
            event.clientX >= drawer.current.getBoundingClientRect().right
          )
            close();
        }}
      >
        <div className="mobile-drawer-heading">
          <Brand onNavigate={close} />
          <button
            type="button"
            className="icon-button"
            aria-label="Close navigation"
            onClick={close}
          >
            <X size={19} aria-hidden="true" />
          </button>
        </div>
        <Navigation page={page} onNavigate={close} />
      </dialog>
    </>
  );
}
