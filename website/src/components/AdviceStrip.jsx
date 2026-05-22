import React from "react";

export function AdviceStrip({ navigate }) {
  return (
    <div className="advice-strip">
      <a
        className="advice advice-left"
        href="#/why"
        onClick={(e) => { e.preventDefault(); navigate("/why"); }}
      >
        <div className="advice-eyebrow mono">/why</div>
        <div className="advice-title">Curious where rustygit beats <code>git</code>?</div>
        <div className="advice-arrow mono">read why →</div>
      </a>
      <a
        className="advice advice-right"
        href="#/watch-out"
        onClick={(e) => { e.preventDefault(); navigate("/watch-out"); }}
      >
        <div className="advice-eyebrow mono">/watch-out</div>
        <div className="advice-title">About to install it on a real machine? Read this first.</div>
        <div className="advice-arrow mono">read first →</div>
      </a>
    </div>
  );
}
