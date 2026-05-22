import React from "react";

export function Section({ eyebrow, title, lede, children, id, anchor, narrow = false }) {
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
