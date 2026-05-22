import React, { useState } from "react";
import { StatusPill } from "./StatusPill.jsx";
import { BUILD_META } from "../data.js";

const LINKS = [
  { to: "/",              label: "Home" },
  { to: "/why",           label: "Why" },
  { to: "/watch-out",     label: "Watch out" },
  { to: "/compatibility", label: "Compatibility" },
  { to: "/install",       label: "Install" },
];

export function Nav({ route, navigate, theme, onThemeToggle }) {
  const [open, setOpen] = useState(false);

  return (
    <header className="nav">
      <div className="nav-inner">
        <a
          className="nav-brand"
          href="#/"
          onClick={(e) => { e.preventDefault(); navigate("/"); setOpen(false); }}
        >
          <span className="brand-mark mono" aria-hidden="true">
            <span className="brand-prompt">$</span>
            <span className="brand-cursor" />
          </span>
          <span className="brand-word mono">rustygit</span>
          <StatusPill compact />
        </a>

        <button
          className="nav-burger"
          aria-label="Toggle menu"
          aria-expanded={open}
          onClick={() => setOpen((v) => !v)}
          type="button"
        >
          <span /><span /><span />
        </button>

        <nav className={`nav-links ${open ? "is-open" : ""}`}>
          {LINKS.map((l) => {
            const active = l.to === route || (l.to !== "/" && route.startsWith(l.to));
            return (
              <a
                key={l.to}
                href={`#${l.to}`}
                className={`nav-link ${active ? "is-active" : ""}`}
                onClick={(e) => { e.preventDefault(); navigate(l.to); setOpen(false); }}
              >
                {l.label}
              </a>
            );
          })}
          <div className="nav-divider" />
          <a
            className="nav-link nav-link-ghost mono"
            href={BUILD_META.repoUrl}
            target="_blank"
            rel="noreferrer"
          >
            github ↗
          </a>
          <button
            className="theme-toggle"
            type="button"
            onClick={onThemeToggle}
            aria-label="Toggle theme"
            title={theme === "dark" ? "Switch to light" : "Switch to dark"}
          >
            <span className="mono">{theme === "dark" ? "◐ dark" : "◑ light"}</span>
          </button>
        </nav>
      </div>
    </header>
  );
}
