import React from "react";
import { Section } from "../components/Section.jsx";

const ROUTES = [
  ["/",              "Home"],
  ["/why",           "Why rustygit"],
  ["/watch-out",     "Watch out"],
  ["/compatibility", "Compatibility"],
  ["/install",       "Install & migrate"],
  ["/security",      "Security"],
];

export function NotFoundPage({ navigate }) {
  return (
    <main className="page page-404">
      <Section
        eyebrow="404"
        title="Page not found"
        lede="That route doesn't exist on this site. Try one of the six below."
      >
        <div className="next-grid">
          {ROUTES.map(([to, label]) => (
            <a
              key={to}
              className="next"
              href={`#${to}`}
              onClick={(e) => { e.preventDefault(); navigate(to); }}
            >
              <div className="next-eyebrow mono">{to}</div>
              <div className="next-title">{label}</div>
              <div className="next-arrow mono">→</div>
            </a>
          ))}
        </div>
      </Section>
    </main>
  );
}
