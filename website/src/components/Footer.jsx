import React from "react";
import { StatusPill } from "./StatusPill.jsx";
import { BUILD_META } from "../data.js";

const SITE_LINKS = [
  ["/",              "Home"],
  ["/why",           "Why rustygit"],
  ["/watch-out",     "Watch out"],
  ["/compatibility", "Compatibility"],
  ["/install",       "Install & migrate"],
  ["/security",      "Security"],
];

export function Footer({ navigate }) {
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
          {SITE_LINKS.map(([to, label]) => (
            <a
              key={to}
              href={`#${to}`}
              onClick={(e) => { e.preventDefault(); navigate(to); }}
              className="footer-link"
            >
              {label}
            </a>
          ))}
        </div>

        <div className="footer-col">
          <div className="footer-col-title mono">links</div>
          <a className="footer-link" href={BUILD_META.repoUrl} target="_blank" rel="noreferrer">github ↗</a>
          <a className="footer-link" href={BUILD_META.securityUrl} target="_blank" rel="noreferrer">security report ↗</a>
          <a className="footer-link" href={BUILD_META.issuesUrl} target="_blank" rel="noreferrer">issue tracker ↗</a>
          <a className="footer-link" href={BUILD_META.roadmapUrl} target="_blank" rel="noreferrer">roadmap ↗</a>
        </div>

        <div className="footer-col footer-col-meta mono">
          <div className="footer-col-title">build</div>
          <div className="footer-meta-row"><span>version</span><span>{BUILD_META.version}</span></div>
          <div className="footer-meta-row"><span>licence</span><span>Apache-2.0 / MIT</span></div>
          <div className="footer-meta-row"><span>tests</span><span>{BUILD_META.testsPassing} passing</span></div>
          <div className="footer-meta-row"><span>updated</span><span>{BUILD_META.updated}</span></div>
          <div className="footer-meta-row"><span>sha</span><span>{BUILD_META.sha}</span></div>
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
