import React from "react";

// Coloured inset for warnings + notes. tone: note | yellow | red.
export function Callout({ tone = "note", title, children, icon }) {
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
