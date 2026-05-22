import React from "react";
import { BUILD_META } from "../data.js";

export function StatusPill({ compact = false }) {
  if (compact) {
    return (
      <span className="status-pill status-pill-compact">
        <span className="status-dot" />
        <span className="mono">{BUILD_META.version}</span>
      </span>
    );
  }
  return (
    <span className="status-pill">
      <span className="status-dot" />
      <span className="mono">{BUILD_META.version.replace(/-beta.*/, "")}</span>
      <span className="status-sep">·</span>
      <span>beta</span>
      <span className="status-sep">·</span>
      <span className="mono">{BUILD_META.testsPassing} tests passing</span>
    </span>
  );
}
