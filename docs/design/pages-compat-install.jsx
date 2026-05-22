// pages-compat-install.jsx — /compatibility and /install pages.

// ════════════════════════════════════════════════════════════════════
// COMPATIBILITY
// ════════════════════════════════════════════════════════════════════
function CompatibilityPage({ navigate }) {
  const rows = window.RG_DATA.COMPAT_ROWS;
  const [query, setQuery] = React.useState("");
  const [tierFilter, setTierFilter] = React.useState(new Set(["T1", "T2", "T3", "OUT"]));
  const [sortBy, setSortBy] = React.useState("cmd"); // 'cmd' | 'tier'
  const [sortDir, setSortDir] = React.useState("asc");

  const tiers = ["T1", "T2", "T3", "OUT"];
  const tierOrder = { T1: 0, T2: 1, T3: 2, OUT: 3 };

  const filtered = React.useMemo(() => {
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
      else cmp = tierOrder[a.tier] - tierOrder[b.tier] || a.cmd.localeCompare(b.cmd);
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

  const tierCounts = React.useMemo(() => {
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

      {/* ─── Tier legend ─────────────────────────────────────────── */}
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

      {/* ─── Full subcommand table ──────────────────────────────── */}
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
            />
            {query && (
              <button className="search-clear mono" onClick={() => setQuery("")} type="button" aria-label="Clear">×</button>
            )}
          </label>
          <div className="table-filters">
            <span className="table-filters-label mono">tiers:</span>
            {tiers.map((t) => (
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
                <th onClick={() => setSort("cmd")} className={`th-sortable ${sortBy === "cmd" ? "is-sorted" : ""}`}>
                  <span>Subcommand</span>
                  <span className="th-sort-glyph mono">{sortBy === "cmd" ? (sortDir === "asc" ? "↑" : "↓") : "↕"}</span>
                </th>
                <th onClick={() => setSort("tier")} className={`th-sortable th-tier ${sortBy === "tier" ? "is-sorted" : ""}`}>
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

      {/* ─── Top-level flags table ──────────────────────────────── */}
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

      {/* ─── Known output divergences ───────────────────────────── */}
      <Section
        eyebrow="output divergences"
        title="Three known places output differs"
        lede="Same exit code, same on-disk effect, slightly different stdout. Listed so your scripts know what to expect."
      >
        <div className="diverge-grid">
          {window.RG_DATA.OUTPUT_DIVERGENCES.map((d, i) => (
            <div key={d.title} className="diverge">
              <div className="diverge-num mono">D.{i + 1}</div>
              <h3 className="diverge-title">{d.title}</h3>
              <p>{d.body}</p>
            </div>
          ))}
        </div>
      </Section>

      {/* ─── Out of scope ───────────────────────────────────────── */}
      <Section
        id="out-of-scope"
        eyebrow="out of scope"
        title="Permanently not in v0.x"
        lede="The /watch-out page frames the same list as warnings. Here it is framed as reference."
      >
        <ul className="oos-list">
          {window.RG_DATA.OUT_OF_SCOPE.map((line) => (
            <li key={line} className="oos">
              <span className="oos-glyph mono">×</span>
              <span>{line}</span>
            </li>
          ))}
        </ul>
      </Section>

      {/* ─── SemVer ─────────────────────────────────────────────── */}
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

// ════════════════════════════════════════════════════════════════════
// INSTALL & MIGRATE
// ════════════════════════════════════════════════════════════════════
function InstallPage({ navigate }) {
  const [installTab, setInstallTab] = React.useState("source");

  return (
    <main className="page page-install">
      <Section
        eyebrow="page 05 / install + migrate"
        title="Install & migrate"
        lede="From 'I want to try this' to 'I have rustygit running on a repo I care about', in under five minutes."
      />

      {/* ─── 1. Install ─────────────────────────────────────────── */}
      <Section eyebrow="01 · install" title="Install">
        <Tabs
          value={installTab}
          onChange={setInstallTab}
          tabs={[
            {
              id: "source", label: "From source",
              tag: "works today",
              content: (
                <>
                  <p>Requires a Rust toolchain (1.75+). The binary ends up in <code>~/.cargo/bin</code>.</p>
                  <CodeBlock>{`$ git clone https://github.com/bsadashi/rustygit
$ cd rustygit
$ cargo install --path .`}</CodeBlock>
                  <p className="muted small">
                    Build time on an M-class laptop: ~90 seconds release. The release profile uses LTO; expect 2–3× of that on a 4-core box.
                  </p>
                </>
              ),
            },
            {
              id: "brew", label: "Homebrew",
              tag: "coming soon",
              content: (
                <>
                  <Callout tone="yellow" icon="◇" title="Coming soon — see release notes">
                    <p>The tap goes live with the v0.1.0 final cut. Until then, build from source.</p>
                  </Callout>
                  <CodeBlock>{`$ brew install bsadashi/tap/rustygit`}</CodeBlock>
                </>
              ),
            },
            {
              id: "deb", label: ".deb / .rpm",
              tag: "coming soon",
              content: (
                <>
                  <Callout tone="yellow" icon="◇" title="Coming soon — see release notes">
                    <p>Signed Debian and RPM packages will ship alongside the v0.1.0 final tag on GitHub Releases.</p>
                  </Callout>
                  <p>
                    Watch the{" "}
                    <a href="https://github.com/bsadashi/rustygit/releases" target="_blank" rel="noreferrer">
                      Releases page
                    </a>{" "}
                    for the artefacts, or build from source today.
                  </p>
                </>
              ),
            },
          ]}
        />
      </Section>

      {/* ─── 2. First-run check ─────────────────────────────────── */}
      <Section eyebrow="02 · first-run check" title="Run doctor before anything else">
        <p>
          <code>doctor --import-config</code> reports which keys in your existing{" "}
          <code>~/.gitconfig</code> rustygit honours, ignores, or refuses.
        </p>
        <CodeBlock>{`$ rustygit doctor --import-config`}</CodeBlock>

        <div className="doctor-sample">
          <div className="doctor-sample-head mono">
            <span className="doctor-sample-prompt">$</span>
            <span>sample output</span>
          </div>
          <pre className="doctor-sample-body mono"><code>
<span className="doc-ok">[ok]    </span>user.name                    "Bharat Sadashi"{"\n"}
<span className="doc-ok">[ok]    </span>user.email                   "bharat@example.com"{"\n"}
<span className="doc-ok">[ok]    </span>core.editor                  "nvim"{"\n"}
<span className="doc-ok">[ok]    </span>pull.rebase                  true{"\n"}
<span className="doc-warn">[skip]  </span>color.ui                     auto                       <span className="doc-mute"># rustygit does not emit colour today</span>{"\n"}
<span className="doc-warn">[skip]  </span>includeIf "gitdir:~/work/"   path = ~/work/.gitconfig   <span className="doc-mute"># [includeIf] is silently skipped — inline the keys</span>{"\n"}
<span className="doc-err">[refuse]</span> alias.lol                    !"git log --oneline ..."   <span className="doc-mute"># shell aliases are refused; move to your shell rc</span>{"\n"}
{"\n"}
<span className="doc-out">summary: 14 ok · 2 skipped · 1 refused</span>{"\n"}
<span className="doc-out">see: /watch-out#refused-by-design</span>
          </code></pre>
        </div>
      </Section>

      {/* ─── 3. Identity setup ──────────────────────────────────── */}
      <Section eyebrow="03 · identity" title="Set your identity">
        <CodeBlock>{`$ rustygit config --global user.name "Your Name"
$ rustygit config --global user.email "you@example.com"`}</CodeBlock>
      </Section>

      {/* ─── 4. Aliases ─────────────────────────────────────────── */}
      <Section eyebrow="04 · aliases" title="The !-prefix refusal, and the workaround">
        <p>
          <code>[alias]</code> entries starting with <code>!</code> are refused at
          expansion time (
          <a href="#/watch-out" onClick={(e) => { e.preventDefault(); navigate("/watch-out"); }}>watch-out §7.5</a>
          ). Move them into a shell rc instead — same ergonomics, no
          config-file-as-code footgun.
        </p>
        <div className="alias-compare">
          <div className="alias-compare-side">
            <div className="alias-compare-label mono alias-bad">~/.gitconfig — refused</div>
            <CodeBlock chrome={false}>{`[alias]
  lol = !"git log --oneline --graph --all"`}</CodeBlock>
          </div>
          <div className="alias-compare-arrow mono">→</div>
          <div className="alias-compare-side">
            <div className="alias-compare-label mono alias-good">~/.zshrc — works</div>
            <CodeBlock chrome={false}>{`rgs() { rustygit status "$@" && rustygit diff --stat "$@"; }
lol() { rustygit log --oneline --graph --all "$@"; }`}</CodeBlock>
          </div>
        </div>
      </Section>

      {/* ─── 5. Escape-hatch alias ──────────────────────────────── */}
      <Section eyebrow="05 · escape hatch" title="Run rustygit by default; fall back to git per-repo">
        <p>
          Drop this in your shell rc to use rustygit everywhere, but
          transparently fall back to upstream git on repos you've flagged as
          incompatible:
        </p>
        <CodeBlock caption="~/.zshrc or ~/.bashrc">{`alias gitsafe='if grep -q RUSTYGIT_INCOMPAT .git/.rustygit-flags 2>/dev/null; then git "$@"; else rustygit "$@"; fi'`}</CodeBlock>
        <p>Per-repo opt-out — flips the alias back to upstream git for this checkout:</p>
        <CodeBlock>{`$ mkdir -p .git
$ echo RUSTYGIT_INCOMPAT > .git/.rustygit-flags`}</CodeBlock>
      </Section>

      {/* ─── 6. Silencing the beta banner ───────────────────────── */}
      <Section eyebrow="06 · banner" title="Silence the beta warning">
        <p>Once you're comfortable, acknowledge the banner globally:</p>
        <CodeBlock>{`$ rustygit config --global rustygit.beta.acknowledged true`}</CodeBlock>
        <p>…or one-shot it from CI, where you don't want it stamped in the config:</p>
        <CodeBlock>{`$ rustygit --i-know-this-is-beta status`}</CodeBlock>
      </Section>

      {/* ─── 7. Next ────────────────────────────────────────────── */}
      <Section eyebrow="07 · what now" title="Three places to go next">
        <div className="next-grid">
          <a className="next" href="#/why" onClick={(e) => { e.preventDefault(); navigate("/why"); }}>
            <div className="next-eyebrow mono">/why</div>
            <div className="next-title">Read where rustygit beats <code>git</code></div>
            <div className="next-arrow mono">→</div>
          </a>
          <a className="next" href="#/watch-out" onClick={(e) => { e.preventDefault(); navigate("/watch-out"); }}>
            <div className="next-eyebrow mono">/watch-out</div>
            <div className="next-title">Read where to be careful</div>
            <div className="next-arrow mono">→</div>
          </a>
          <a className="next" href="#/compatibility" onClick={(e) => { e.preventDefault(); navigate("/compatibility"); }}>
            <div className="next-eyebrow mono">/compatibility</div>
            <div className="next-title">Open the compatibility table</div>
            <div className="next-arrow mono">→</div>
          </a>
        </div>
      </Section>
    </main>
  );
}

Object.assign(window, { CompatibilityPage, InstallPage });
