import React, { useMemo, useState } from "react";
import { Section } from "../components/Section.jsx";
import { TierBadge } from "../components/TierBadge.jsx";
import { COMPAT_ROWS, OUTPUT_DIVERGENCES, OUT_OF_SCOPE } from "../data.js";

const TIER_ORDER = { T1: 0, T2: 1, T3: 2, OUT: 3 };
const TIERS = ["T1", "T2", "T3", "OUT"];

export function CompatibilityPage() {
  const rows = COMPAT_ROWS;
  const [query, setQuery] = useState("");
  const [tierFilter, setTierFilter] = useState(new Set(TIERS));
  const [sortBy, setSortBy] = useState("cmd");
  const [sortDir, setSortDir] = useState("asc");

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    let list = rows.filter((r) => {
      if (!tierFilter.has(r.tier)) return false;
      if (!q) return true;
      return (
        r.cmd.toLowerCase().includes(q) ||
        r.notes.toLowerCase().includes(q) ||
        r.tier.toLowerCase().includes(q)
      );
    });
    list = [...list].sort((a, b) => {
      let cmp;
      if (sortBy === "cmd") cmp = a.cmd.localeCompare(b.cmd);
      else cmp = TIER_ORDER[a.tier] - TIER_ORDER[b.tier] || a.cmd.localeCompare(b.cmd);
      return sortDir === "asc" ? cmp : -cmp;
    });
    return list;
  }, [rows, query, tierFilter, sortBy, sortDir]);

  const toggleTier = (t) => {
    setTierFilter((prev) => {
      const n = new Set(prev);
      if (n.has(t)) n.delete(t); else n.add(t);
      return n;
    });
  };

  const setSort = (col) => {
    if (sortBy === col) setSortDir(sortDir === "asc" ? "desc" : "asc");
    else { setSortBy(col); setSortDir("asc"); }
  };

  const tierCounts = useMemo(() => {
    const c = { T1: 0, T2: 0, T3: 0, OUT: 0 };
    rows.forEach((r) => c[r.tier]++);
    return c;
  }, [rows]);

  return (
    <main className="page page-compat">
      <Section
        eyebrow="page 04 / compatibility"
        title="Compatibility"
        lede={
          <>
            The full subcommand tier table from <code>COMPAT.md</code>. This page is
            for the tooling-author audience — searchable, filterable, no marketing.
          </>
        }
      />

      <Section eyebrow="legend" title="Four tiers">
        <div className="tier-legend">
          <div className="tier-legend-card tier-legend-t1">
            <div className="tier-legend-head"><TierBadge tier="T1" /><span className="tier-legend-name">byte-for-byte</span></div>
            <p>Output is bit-equal to upstream <code>git</code> for every documented input. Verified by the oracle test suite.</p>
            <div className="tier-legend-count mono">{tierCounts.T1} subcommands</div>
          </div>
          <div className="tier-legend-card tier-legend-t2">
            <div className="tier-legend-head"><TierBadge tier="T2" /><span className="tier-legend-name">semantic equiv.</span></div>
            <p>Same effect, format may differ. Typically: no ANSI colour, narrower flag surface, identical <code>--porcelain</code>.</p>
            <div className="tier-legend-count mono">{tierCounts.T2} subcommands</div>
          </div>
          <div className="tier-legend-card tier-legend-t3">
            <div className="tier-legend-head"><TierBadge tier="T3" /><span className="tier-legend-name">rustygit-specific</span></div>
            <p>Has no upstream counterpart. <code>doctor</code>, <code>prune-locks</code>, <code>bug-report</code>.</p>
            <div className="tier-legend-count mono">{tierCounts.T3} subcommands</div>
          </div>
          <div className="tier-legend-card tier-legend-out">
            <div className="tier-legend-head"><TierBadge tier="OUT" /><span className="tier-legend-name">out of scope</span></div>
            <p>Intentionally not shipped. Will not be added in v0.x. Use upstream <code>git</code> or a sibling tool.</p>
            <div className="tier-legend-count mono">{tierCounts.OUT} subcommands</div>
          </div>
        </div>
      </Section>

      <Section eyebrow={`subcommand table · ${rows.length} rows`} title="Full subcommand table">
        <div className="table-controls">
          <label className="search">
            <span className="search-glyph mono">⌕</span>
            <input
              type="text"
              placeholder="filter by name or note…"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              className="search-input mono"
              aria-label="Filter subcommands"
            />
            {query && (
              <button
                className="search-clear mono"
                onClick={() => setQuery("")}
                type="button"
                aria-label="Clear filter"
              >
                ×
              </button>
            )}
          </label>
          <div className="table-filters">
            <span className="table-filters-label mono">tiers:</span>
            {TIERS.map((t) => (
              <button
                key={t}
                type="button"
                onClick={() => toggleTier(t)}
                className={`tier-filter ${tierFilter.has(t) ? "is-on" : "is-off"}`}
                aria-pressed={tierFilter.has(t)}
              >
                <TierBadge tier={t} />
                <span className="mono">{tierCounts[t]}</span>
              </button>
            ))}
          </div>
          <div className="table-meta mono">
            {filtered.length}/{rows.length} rows
          </div>
        </div>

        <div className="compat-table-wrap">
          <table className="compat-table">
            <thead>
              <tr>
                <th
                  onClick={() => setSort("cmd")}
                  className={`th-sortable ${sortBy === "cmd" ? "is-sorted" : ""}`}
                >
                  <span>Subcommand</span>
                  <span className="th-sort-glyph mono">{sortBy === "cmd" ? (sortDir === "asc" ? "↑" : "↓") : "↕"}</span>
                </th>
                <th
                  onClick={() => setSort("tier")}
                  className={`th-sortable th-tier ${sortBy === "tier" ? "is-sorted" : ""}`}
                >
                  <span>Tier</span>
                  <span className="th-sort-glyph mono">{sortBy === "tier" ? (sortDir === "asc" ? "↑" : "↓") : "↕"}</span>
                </th>
                <th>Notes</th>
              </tr>
            </thead>
            <tbody>
              {filtered.length === 0 && (
                <tr><td colSpan={3} className="table-empty mono">no rows match "{query}"</td></tr>
              )}
              {filtered.map((r) => (
                <tr key={r.cmd} className={`row-${r.tier.toLowerCase()}`}>
                  <td className="td-cmd mono">
                    <span className="td-cmd-glyph">$</span> rustygit {r.cmd}
                  </td>
                  <td className="td-tier"><TierBadge tier={r.tier} /></td>
                  <td className="td-notes">{r.notes || <span className="muted">—</span>}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </Section>

      <Section eyebrow="top-level flags" title="Two of them, both T1.">
        <div className="compat-table-wrap compat-table-wrap-tight">
          <table className="compat-table">
            <thead>
              <tr><th>Flag</th><th className="th-tier">Tier</th><th>Notes</th></tr>
            </thead>
            <tbody>
              <tr>
                <td className="td-cmd mono">-C &lt;PATH&gt;</td>
                <td className="td-tier"><TierBadge tier="T1" /></td>
                <td className="td-notes">Run as if rustygit were started in <code>&lt;PATH&gt;</code>.</td>
              </tr>
              <tr>
                <td className="td-cmd mono">-c &lt;KEY=VALUE&gt;</td>
                <td className="td-tier"><TierBadge tier="T1" /></td>
                <td className="td-notes">Override a config key for this invocation only.</td>
              </tr>
            </tbody>
          </table>
        </div>
      </Section>

      <Section
        eyebrow="output divergences"
        title="Three known places output differs"
        lede="Same exit code, same on-disk effect, slightly different stdout. Listed so your scripts know what to expect."
      >
        <div className="diverge-grid">
          {OUTPUT_DIVERGENCES.map((d, i) => (
            <div key={d.title} className="diverge">
              <div className="diverge-num mono">D.{i + 1}</div>
              <h3 className="diverge-title">{d.title}</h3>
              <p>{d.body}</p>
            </div>
          ))}
        </div>
      </Section>

      <Section
        id="out-of-scope"
        eyebrow="out of scope"
        title="Permanently not in v0.x"
        lede="The /watch-out page frames the same list as warnings. Here it is framed as reference."
      >
        <ul className="oos-list">
          {OUT_OF_SCOPE.map((line) => (
            <li key={line} className="oos">
              <span className="oos-glyph mono">×</span>
              <span>{line}</span>
            </li>
          ))}
        </ul>
      </Section>

      <Section eyebrow="semver policy" title="What the version number promises">
        <p className="muted">
          rustygit follows SemVer applied to the on-disk format contract and the
          stable subset of the CLI surface, not to the rate of internal change.
        </p>
        <div className="semver-grid">
          <div className="semver-card">
            <div className="semver-num mono">MAJOR</div>
            <p>A change that can corrupt or refuse to read an on-disk format produced by the previous major version. Has never happened. Will be loud when it does.</p>
          </div>
          <div className="semver-card">
            <div className="semver-num mono">MINOR</div>
            <p>A new subcommand reaches T1 or T2, an OUT entry becomes implemented, a new on-disk feature is honoured, or a stable CLI flag is added.</p>
          </div>
          <div className="semver-card">
            <div className="semver-num mono">PATCH</div>
            <p>Bug fix, perf, internal refactor, dependency bump. No subcommand changes tiers, no on-disk format changes, no CLI surface added or removed.</p>
          </div>
        </div>
      </Section>
    </main>
  );
}
