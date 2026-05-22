import React from "react";

export function Card({ title, eyebrow, children, accent, className = "" }) {
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
