import React from "react";

const LABELS = { T1: "T1", T2: "T2", T3: "T3", OUT: "OUT" };

export function TierBadge({ tier, size = "sm" }) {
  const cls = `tier-badge tier-${tier.toLowerCase()} tier-${size}`;
  return <span className={cls}>{LABELS[tier] ?? tier}</span>;
}
