import React from "react";

export function Tabs({ tabs, value, onChange }) {
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
