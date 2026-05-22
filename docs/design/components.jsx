// components.jsx — shared UI primitives for the rustygit site.

const { useState, useEffect, useRef, useMemo, useCallback } = React;

// ────────────────────────────────────────────────────────────────────
// TierBadge — small coloured pill used in the compat table + legend.
// ────────────────────────────────────────────────────────────────────
function TierBadge({ tier, size = "sm" }) {
  const labels = {
    T1: "T1",
    T2: "T2",
    T3: "T3",
    OUT: "OUT",
  };
  const cls = `tier-badge tier-${tier.toLowerCase()} tier-${size}`;
  return <span className={cls}>{labels[tier]}</span>;
}

// ────────────────────────────────────────────────────────────────────
// StatusPill — version + state + test count, used in hero and nav.
// ────────────────────────────────────────────────────────────────────
function StatusPill({ compact = false }) {
  if (compact) {
    return (
      <span className="status-pill status-pill-compact">
        <span className="status-dot" />
        <span className="mono">v0.1.0-beta.1</span>
      </span>
    );
  }
  return (
    <span className="status-pill">
      <span className="status-dot" />
      <span className="mono">v0.1.0</span>
      <span className="status-sep">·</span>
      <span>beta</span>
      <span className="status-sep">·</span>
      <span className="mono">941 tests passing</span>
    </span>
  );
}

// ────────────────────────────────────────────────────────────────────
// CodeBlock — terminal-style code sample. Optional copy button.
// ────────────────────────────────────────────────────────────────────
function CodeBlock({ children, lang = "sh", copyable = true, chrome = true, caption }) {
  const [copied, setCopied] = useState(false);
  const text = typeof children === "string"
    ? children
    : React.Children.toArray(children).join("");

  const onCopy = () => {
    navigator.clipboard?.writeText(text);
    setCopied(true);
    setTimeout(() => setCopied(false), 1400);
  };

  // Render with simple line-by-line highlighting:
  // lines starting "$ " are prompts; comments starting "#" are dimmed.
  const lines = text.replace(/\n$/, "").split("\n");

  return (
    <div className="codeblock">
      {chrome && (
        <div className="codeblock-chrome">
          <span className="codeblock-lang mono">{lang}</span>
          {copyable && (
            <button className="codeblock-copy mono" onClick={onCopy} type="button">
              {copied ? "copied" : "copy"}
            </button>
          )}
        </div>
      )}
      <pre className="codeblock-body mono"><code>
        {lines.map((ln, i) => {
          let cls = "code-line";
          let content = ln;
          if (ln.startsWith("$ ")) {
            cls += " code-line-cmd";
            content = (
              <>
                <span className="code-prompt">$</span>
                <span className="code-rest"> {ln.slice(2)}</span>
              </>
            );
          } else if (ln.trim().startsWith("#")) {
            cls += " code-line-comment";
          } else if (ln.length === 0) {
            cls += " code-line-blank";
            content = " ";
          } else {
            cls += " code-line-out";
          }
          return <div key={i} className={cls}>{content}</div>;
        })}
      </code></pre>
      {caption && <div className="codeblock-caption mono">{caption}</div>}
    </div>
  );
}

// ────────────────────────────────────────────────────────────────────
// Callout — coloured inset for warnings + notes. Tone: note | yellow | red.
// ────────────────────────────────────────────────────────────────────
function Callout({ tone = "note", title, children, icon }) {
  return (
    <aside className={`callout callout-${tone}`} role="note">
      <div className="callout-bar" />
      <div className="callout-body">
        {title && (
          <div className="callout-title">
            {icon && <span className="callout-icon mono">{icon}</span>}
            <span>{title}</span>
          </div>
        )}
        <div className="callout-content">{children}</div>
      </div>
    </aside>
  );
}

// ────────────────────────────────────────────────────────────────────
// Section — page-level section with optional eyebrow, title, lede.
// ────────────────────────────────────────────────────────────────────
function Section({ eyebrow, title, lede, children, id, anchor, narrow = false }) {
  return (
    <section className={`section ${narrow ? "section-narrow" : ""}`} id={id}>
      {(eyebrow || title || lede) && (
        <header className="section-head">
          {eyebrow && (
            <div className="section-eyebrow mono">
              <span className="eyebrow-mark">§</span>
              <span>{eyebrow}</span>
            </div>
          )}
          {title && (
            <h2 className="section-title">
              {anchor && <a href={`#${anchor}`} className="anchor-link" aria-label="anchor">#</a>}
              {title}
            </h2>
          )}
          {lede && <p className="section-lede">{lede}</p>}
        </header>
      )}
      <div className="section-body">{children}</div>
    </section>
  );
}

// ────────────────────────────────────────────────────────────────────
// Card — generic content card.
// ────────────────────────────────────────────────────────────────────
function Card({ title, eyebrow, children, accent, className = "" }) {
  return (
    <article className={`card ${className}`}>
      {(eyebrow || title) && (
        <header className="card-head">
          {eyebrow && <div className="card-eyebrow mono">{eyebrow}</div>}
          {title && <h3 className="card-title">{title}</h3>}
        </header>
      )}
      <div className="card-body">{children}</div>
      {accent && <div className="card-accent">{accent}</div>}
    </article>
  );
}

// ────────────────────────────────────────────────────────────────────
// CmdGrid — dense column of monospace command names, used on the home
// page "what it actually does" cards.
// ────────────────────────────────────────────────────────────────────
function CmdGrid({ items }) {
  return (
    <ul className="cmdgrid mono">
      {items.map((c) => (
        <li key={c} className="cmdgrid-item">
          <span className="cmdgrid-glyph">›</span>
          <span>{c}</span>
        </li>
      ))}
    </ul>
  );
}

// ────────────────────────────────────────────────────────────────────
// Nav — sticky top nav with router-aware link state.
// ────────────────────────────────────────────────────────────────────
function Nav({ route, navigate, theme, onThemeToggle }) {
  const links = [
    { to: "/",              label: "Home" },
    { to: "/why",           label: "Why" },
    { to: "/watch-out",     label: "Watch out" },
    { to: "/compatibility", label: "Compatibility" },
    { to: "/install",       label: "Install" },
  ];

  const [open, setOpen] = useState(false);

  return (
    <header className="nav">
      <div className="nav-inner">
        <a className="nav-brand" href="#/" onClick={(e) => { e.preventDefault(); navigate("/"); setOpen(false); }}>
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
          {links.map((l) => {
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
            href="https://github.com/bsadashi/rustygit"
            target="_blank" rel="noreferrer"
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

// ────────────────────────────────────────────────────────────────────
// Footer — version, license, github, security, last updated.
// ────────────────────────────────────────────────────────────────────
function Footer({ navigate }) {
  return (
    <footer className="footer">
      <div className="footer-inner">
        <div className="footer-brand">
          <div className="footer-brand-row mono">
            <span className="brand-prompt">$</span>
            <span>rustygit</span>
            <StatusPill compact />
          </div>
          <p className="footer-desc">
            git, reimplemented in Rust, byte-for-byte compatible where it
            counts. An info + help site for an early-adopter audience.
          </p>
        </div>

        <div className="footer-col">
          <div className="footer-col-title mono">site</div>
          {[
            ["/",              "Home"],
            ["/why",           "Why rustygit"],
            ["/watch-out",     "Watch out"],
            ["/compatibility", "Compatibility"],
            ["/install",       "Install & migrate"],
          ].map(([to, label]) => (
            <a key={to} href={`#${to}`} onClick={(e) => { e.preventDefault(); navigate(to); }} className="footer-link">
              {label}
            </a>
          ))}
        </div>

        <div className="footer-col">
          <div className="footer-col-title mono">links</div>
          <a className="footer-link" href="https://github.com/bsadashi/rustygit" target="_blank" rel="noreferrer">github ↗</a>
          <a className="footer-link" href="https://github.com/bsadashi/rustygit/security/advisories/new" target="_blank" rel="noreferrer">security report ↗</a>
          <a className="footer-link" href="https://github.com/bsadashi/rustygit/issues" target="_blank" rel="noreferrer">issue tracker ↗</a>
          <a className="footer-link" href="https://github.com/bsadashi/rustygit/blob/main/ROADMAP.md" target="_blank" rel="noreferrer">roadmap ↗</a>
        </div>

        <div className="footer-col footer-col-meta mono">
          <div className="footer-col-title">build</div>
          <div className="footer-meta-row"><span>version</span><span>v0.1.0-beta.1</span></div>
          <div className="footer-meta-row"><span>licence</span><span>Apache-2.0 / MIT</span></div>
          <div className="footer-meta-row"><span>tests</span><span>941 passing</span></div>
          <div className="footer-meta-row"><span>updated</span><span>2026-05-19</span></div>
          <div className="footer-meta-row"><span>sha</span><span>4f1c8a2</span></div>
        </div>
      </div>
      <div className="footer-bottom mono">
        <span>© 2026 the rustygit authors</span>
        <span className="footer-bottom-sep">·</span>
        <span>no analytics</span>
        <span className="footer-bottom-sep">·</span>
        <span>no cookies</span>
        <span className="footer-bottom-sep">·</span>
        <span>static html</span>
      </div>
    </footer>
  );
}

// ────────────────────────────────────────────────────────────────────
// AdviceStrip — the paired-CTA strip used on home.
// ────────────────────────────────────────────────────────────────────
function AdviceStrip({ navigate }) {
  return (
    <div className="advice-strip">
      <a className="advice advice-left" href="#/why" onClick={(e) => { e.preventDefault(); navigate("/why"); }}>
        <div className="advice-eyebrow mono">/why</div>
        <div className="advice-title">Curious where rustygit beats <code>git</code>?</div>
        <div className="advice-arrow mono">read why →</div>
      </a>
      <a className="advice advice-right" href="#/watch-out" onClick={(e) => { e.preventDefault(); navigate("/watch-out"); }}>
        <div className="advice-eyebrow mono">/watch-out</div>
        <div className="advice-title">About to install it on a real machine? Read this first.</div>
        <div className="advice-arrow mono">read first →</div>
      </a>
    </div>
  );
}

// ────────────────────────────────────────────────────────────────────
// Tabs — accessible tabs.
// ────────────────────────────────────────────────────────────────────
function Tabs({ tabs, value, onChange }) {
  return (
    <div className="tabs">
      <div className="tabs-bar mono" role="tablist">
        {tabs.map((t) => (
          <button
            key={t.id}
            role="tab"
            aria-selected={value === t.id}
            className={`tab ${value === t.id ? "is-active" : ""}`}
            onClick={() => onChange(t.id)}
            type="button"
          >
            {t.label}
            {t.tag && <span className="tab-tag mono">{t.tag}</span>}
          </button>
        ))}
      </div>
      <div className="tabs-panels">
        {tabs.map((t) => (
          <div key={t.id} role="tabpanel" hidden={value !== t.id} className="tab-panel">
            {t.content}
          </div>
        ))}
      </div>
    </div>
  );
}

Object.assign(window, {
  TierBadge, StatusPill, CodeBlock, Callout, Section, Card,
  CmdGrid, Nav, Footer, AdviceStrip, Tabs,
});
