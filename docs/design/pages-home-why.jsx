// pages.jsx — page-level components for the rustygit site.
// All five pages live in one file; each is exported to window so
// app.jsx can reach them through the simple hash router.

const { useState: usePageState, useEffect: usePageEffect, useMemo: usePageMemo, useRef: usePageRef } = React;

// ════════════════════════════════════════════════════════════════════
// HOME
// ════════════════════════════════════════════════════════════════════
function HomePage({ navigate }) {
  return (
    <main className="page page-home">
      {/* ─── Hero ────────────────────────────────────────────────── */}
      <section className="hero">
        <div className="hero-grid" aria-hidden="true" />
        <div className="hero-inner">
          <div className="hero-status">
            <StatusPill />
          </div>
          <h1 className="hero-title">
            <span className="hero-title-line">git, reimplemented in</span>{" "}
            <span className="hero-title-line hero-title-rust">Rust</span><span className="hero-title-comma">,</span>{" "}
            <span className="hero-title-line">byte-for-byte compatible</span>{" "}
            <span className="hero-title-line hero-title-where">where it counts.</span>
          </h1>

          <p className="hero-descriptor">
            rustygit is a from-scratch Rust reimplementation of git's core. Object
            store, refs, index, packfiles, plumbing and porcelain — all in one
            static binary, no shell, no Perl, no Tcl. On every on-disk format
            (loose objects, packfiles, indexes, refs, reftable, commit-graph,
            midx, reflog) it produces output that is byte-for-byte identical to
            upstream <code>git</code>, verified by an oracle test suite that runs
            both binaries side-by-side.
          </p>

          <div className="hero-ctas">
            <a
              className="btn btn-primary"
              href="#/install"
              onClick={(e) => { e.preventDefault(); navigate("/install"); }}
            >
              <span>Install</span>
              <span className="btn-arrow mono">↗</span>
            </a>
            <a
              className="btn btn-secondary"
              href="#/watch-out"
              onClick={(e) => { e.preventDefault(); navigate("/watch-out"); }}
            >
              <span>See where to be careful</span>
              <span className="btn-arrow mono">→</span>
            </a>
          </div>

          <div className="hero-meta mono">
            <div className="hero-meta-row">
              <span className="hero-meta-key">repo</span>
              <a href="https://github.com/bsadashi/rustygit" target="_blank" rel="noreferrer">
                github.com/bsadashi/rustygit
              </a>
            </div>
            <div className="hero-meta-row">
              <span className="hero-meta-key">licence</span>
              <span>Apache-2.0 / MIT</span>
            </div>
            <div className="hero-meta-row">
              <span className="hero-meta-key">binary</span>
              <span>~6 MB stripped · static</span>
            </div>
          </div>
        </div>

        {/* terminal proof, occupies the right half on desktop */}
        <aside className="hero-term" aria-label="terminal demonstration">
          <div className="hero-term-chrome mono">
            <span className="hero-term-dot" />
            <span className="hero-term-dot" />
            <span className="hero-term-dot" />
            <span className="hero-term-title">~/src/rustygit — zsh</span>
          </div>
          <pre className="hero-term-body mono"><code>
<span className="t-prompt">$</span> <span className="t-cmd">rustygit --version</span>{"\n"}
<span className="t-out">rustygit 0.1.0-beta.1 (4f1c8a2 2026-05-19)</span>{"\n"}
<span className="t-out">built against git 2.45.2 oracle, 941 / 941 tests passing</span>{"\n"}
{"\n"}
<span className="t-prompt">$</span> <span className="t-cmd">rustygit clone https://github.com/bsadashi/rustygit</span>{"\n"}
<span className="t-out">Cloning into 'rustygit'...</span>{"\n"}
<span className="t-out">remote: enumerating objects: 8421, done.</span>{"\n"}
<span className="t-out">remote: counting objects: 100% (8421/8421), 14.2 MiB | 11.1 MiB/s, done.</span>{"\n"}
<span className="t-out">resolving deltas: 100% (5103/5103), done.</span>{"\n"}
<span className="t-warn">note: rustygit is beta — see /watch-out before using on irreplaceable repos.</span>{"\n"}
<span className="t-warn">      silence: rustygit config --global rustygit.beta.acknowledged true</span>{"\n"}
{"\n"}
<span className="t-prompt">$</span> <span className="t-cmd">rustygit log --oneline -n 3</span>{"\n"}
<span className="t-hash">4f1c8a2</span> <span className="t-out">reftable: extend backref index for noop-update edge case</span>{"\n"}
<span className="t-hash">a07b1d9</span> <span className="t-out">pack-objects: tighten delta-window heap bound (#412)</span>{"\n"}
<span className="t-hash">2d6e44b</span> <span className="t-out">doctor: report includeIf directives the parser skipped</span>{"\n"}
<span className="t-prompt">$</span> <span className="t-cursor" />
          </code></pre>
        </aside>
      </section>

      {/* ─── What it actually does ──────────────────────────────── */}
      <Section
        eyebrow="what it actually does"
        title="One binary. Three layers. No scripts."
        lede="The exhaustive list, grouped by where it sits in git's mental model. If a subcommand isn't here, it isn't in v0.1."
      >
        <div className="trio">
          <Card eyebrow="layer 01" title="Plumbing">
            <p className="muted">Low-level commands that act on objects, refs, indexes, packs. Scripted-in tooling depends on these; they're all byte-for-byte.</p>
            <CmdGrid items={window.RG_DATA.HOME_PLUMBING} />
          </Card>
          <Card eyebrow="layer 02" title="Porcelain">
            <p className="muted">The day-to-day surface. Every flag you reach for from the CLI lives here. <code>rebase -i</code> is the one notable absence — see <a href="#/watch-out" onClick={(e) => { e.preventDefault(); navigate("/watch-out"); }}>watch out</a>.</p>
            <CmdGrid items={window.RG_DATA.HOME_PORCELAIN} />
          </Card>
          <Card eyebrow="layer 03" title="Maintenance">
            <p className="muted">House-keeping and repair. Two rustygit-only commands at the end: <code>doctor</code> for config audit, <code>prune-locks</code> for stale <code>.lock</code> files.</p>
            <CmdGrid items={window.RG_DATA.HOME_MAINT} />
          </Card>
        </div>
      </Section>

      {/* ─── Advice strip ───────────────────────────────────────── */}
      <Section narrow>
        <AdviceStrip navigate={navigate} />
      </Section>

      {/* ─── Code teaser ────────────────────────────────────────── */}
      <Section
        eyebrow="thirty seconds in"
        title="It is git, except the binary is named rustygit."
        lede="Every argv, every exit code, every byte of on-disk output is verified against the upstream git binary in CI."
      >
        <CodeBlock lang="bash">{`$ cargo install --path .
$ rustygit clone https://github.com/bsadashi/rustygit
$ rustygit log --oneline -n 3`}</CodeBlock>
      </Section>
    </main>
  );
}

// ════════════════════════════════════════════════════════════════════
// WHY
// ════════════════════════════════════════════════════════════════════
function WhyPage({ navigate }) {
  const reasons = [
    {
      n: "01",
      title: "Memory-safety on a tool that touches your commit history.",
      body: (
        <>
          <p>
            The <code>git</code> binary is millions of lines of C with a long CVE list.
            rustygit is <code>#![deny(unsafe_code)]</code> outside of three audited
            performance hotspots — all of them documented in-tree with their
            invariants and the property tests that guard them.
          </p>
          <div className="kv mono">
            <span className="kv-k">unsafe blocks</span><span className="kv-v">3 (audited)</span>
            <span className="kv-k">cve history</span><span className="kv-v">0 (n.b. v0.1)</span>
            <span className="kv-k">fuzz corpora</span><span className="kv-v">pack, index, reftable</span>
          </div>
        </>
      ),
    },
    {
      n: "02",
      title: "One static binary. No Perl, no shell, no Tcl/Tk.",
      body: (
        <>
          <p>
            <code>git</code> ships with a couple-hundred-megabyte install footprint
            of scripting runtimes for things like <code>git-svn</code>,{" "}
            <code>gitweb</code>, <code>gitk</code>, <code>git-gui</code>,{" "}
            <code>request-pull</code>. rustygit ships <strong>none</strong> of
            those — by design — and the binary is ~6 MB stripped.
          </p>
          <div className="bar-compare">
            <div className="bar-row"><span className="bar-label mono">upstream git</span><div className="bar"><div className="bar-fill bar-fill-grey" style={{ width: "100%" }}><span className="mono">~240 MB · perl + tcl + sh + git-core</span></div></div></div>
            <div className="bar-row"><span className="bar-label mono">rustygit</span><div className="bar"><div className="bar-fill bar-fill-accent" style={{ width: "2.5%" }}><span className="mono">~6 MB</span></div></div></div>
          </div>
          <p className="muted small">
            Footprint figures from a Debian 12 install (<code>git</code> +{" "}
            <code>git-svn</code> + <code>gitk</code>) vs. <code>cargo install</code>{" "}
            of rustygit's release profile. Your distro will vary.
          </p>
        </>
      ),
    },
    {
      n: "03",
      title: "No silent failures in analysis paths.",
      body: (
        <p>
          <code>Result</code> everywhere, typed errors only. The "this read failed
          but we pretended it didn't" class of bugs is statically excluded.
          A reviewer pass — the <code>silent-failure-hunter</code> — gates
          safety-critical PRs before they land.
        </p>
      ),
    },
    {
      n: "04",
      title: "Modern defaults.",
      body: (
        <ul className="checks">
          <li><span className="check mono">▸</span> UTF-8 path bytes. Lossy conversion is refused, not done silently.</li>
          <li><span className="check mono">▸</span> SHA-256 object format ready (SHA-1 is still default).</li>
          <li><span className="check mono">▸</span> Reftable on by request.</li>
          <li><span className="check mono">▸</span> English-only — no <code>LC_ALL</code> surprises. (See <a href="#/watch-out" onClick={(e) => { e.preventDefault(); navigate("/watch-out"); }}>watch out</a> for the catch.)</li>
        </ul>
      ),
    },
    {
      n: "05",
      title: "Byte-for-byte format compatibility.",
      body: (
        <>
          <p>
            Every on-disk format is verified bit-equal to <code>git</code>'s by an
            oracle test suite. You can run rustygit and upstream git on the same
            repo, alternating between them, and neither binary will notice.
          </p>
          <div className="format-grid mono">
            {[
              "loose objects", "packfiles .pack",
              "pack indexes .idx v2", "pack indexes .idx v3",
              "reftable", "packed-refs",
              "commit-graph", "midx",
              "reflog", "index v2/v3/v4",
            ].map((f) => (
              <span className="format-pill" key={f}><span className="format-pill-dot" />{f}</span>
            ))}
          </div>
        </>
      ),
    },
    {
      n: "06",
      title: "No ! shell aliases.",
      body: (
        <>
          <p>
            <code>[alias]</code> entries that begin with <code>!</code> — i.e.
            arbitrary shell execution from a config file — are <strong>refused</strong>{" "}
            at expansion time. This is a supply-chain footgun that upstream git
            ships as a feature. rustygit does not. (See{" "}
            <a href="#/watch-out" onClick={(e) => { e.preventDefault(); navigate("/watch-out"); }}>watch out</a>{" "}
            for the workaround.)
          </p>
          <CodeBlock chrome={false}>{`$ rustygit lol
error: alias 'lol' starts with '!' — shell aliases are refused.
       run the command directly, or move it into your shell rc.`}</CodeBlock>
        </>
      ),
    },
    {
      n: "07",
      title: "Strict path-traversal protection.",
      body: (
        <ul className="checks">
          <li><span className="check mono">▸</span> Refuses to write through symlinks during checkout.</li>
          <li><span className="check mono">▸</span> Tree entries with <code>..</code> segments are rejected before the working-tree write.</li>
          <li><span className="check mono">▸</span> Non-UTF-8 paths on Windows are refused rather than silently corrupted.</li>
        </ul>
      ),
    },
    {
      n: "08",
      title: "Better diagnostics for things we don't do.",
      body: (
        <>
          <p>
            Running <code>rustygit gitweb</code> doesn't error with "unrecognized
            command" — it explains <em>why</em> it's not implemented and what
            upstream tool to use:
          </p>
          <CodeBlock chrome={false}>{`$ rustygit gitweb
error: 'gitweb' is intentionally out of scope.
       gitweb is a Perl CGI; rustygit ships a single static binary.
       for a local web view try: gitui, lazygit, or tig.
       see: /compatibility#out-of-scope`}</CodeBlock>
          <p className="muted small">
            Old transports get the same treatment: <code>git://</code>,{" "}
            <code>ftp://</code>, <code>rsync://</code> all return a named{" "}
            <code>UnsupportedTransport</code> error with a clear reason rather than a
            cryptic "didn't advertise v2".
          </p>
        </>
      ),
    },
  ];

  return (
    <main className="page page-why">
      <Section
        eyebrow="page 02 / why"
        title="Why rustygit"
        lede={
          <>
            rustygit is <strong>not</strong> faster than upstream git in v0.1.
            Speed isn't the contract; safety, single-binary deployment, and a
            clean Rust-native core to build on are. Here's what you actually get.
          </>
        }
      >
        <Callout tone="note" icon="↳" title="for the impatient">
          If you're waiting for "rustygit is 4× faster than git" — that's not
          what v0.1.0 is selling. v0.1.0 is selling a Rust core you can build
          on, with the format compatibility locked down so the work above it
          doesn't drift. Performance work is on the roadmap (Phase 3, see{" "}
          <a href="https://github.com/bsadashi/rustygit/blob/main/ROADMAP.md" target="_blank" rel="noreferrer">ROADMAP.md</a>).
        </Callout>
      </Section>

      <Section>
        <ol className="reasons">
          {reasons.map((r) => (
            <li key={r.n} className="reason">
              <div className="reason-num mono">{r.n}</div>
              <div className="reason-content">
                <h3 className="reason-title">{r.title}</h3>
                <div className="reason-body">{r.body}</div>
              </div>
            </li>
          ))}
        </ol>
      </Section>

      <Section narrow>
        <div className="advice-strip">
          <a className="advice advice-left" href="#/compatibility" onClick={(e) => { e.preventDefault(); navigate("/compatibility"); }}>
            <div className="advice-eyebrow mono">/compatibility</div>
            <div className="advice-title">Need the full subcommand-by-subcommand tier table?</div>
            <div className="advice-arrow mono">open table →</div>
          </a>
          <a className="advice advice-right" href="#/install" onClick={(e) => { e.preventDefault(); navigate("/install"); }}>
            <div className="advice-eyebrow mono">/install</div>
            <div className="advice-title">Ready to install? Five-minute migration guide.</div>
            <div className="advice-arrow mono">install →</div>
          </a>
        </div>
      </Section>
    </main>
  );
}

Object.assign(window, { HomePage, WhyPage });
