import React from "react";

export function CmdGrid({ items }) {
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
