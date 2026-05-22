import React from "react";
import { Section } from "../components/Section.jsx";
import { Card } from "../components/Card.jsx";
import { CmdGrid } from "../components/CmdGrid.jsx";
import { CodeBlock } from "../components/CodeBlock.jsx";
import { StatusPill } from "../components/StatusPill.jsx";
import { AdviceStrip } from "../components/AdviceStrip.jsx";
import { HOME_PLUMBING, HOME_PORCELAIN, HOME_MAINT, BUILD_META } from "../data.js";

export function HomePage({ navigate }) {
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
              <a href={BUILD_META.repoUrl} target="_blank" rel="noreferrer">
                github.com/bsadashi/rustygit
              </a>
            </div>
            <div className="hero-meta-row">
              <span className="hero-meta-key">licence</span>
              <span>Apache-2.0 / MIT</span>
            </div>
            <div className="hero-meta-row">
              <span className="hero-meta-key">binary</span>
              <span>{BUILD_META.binarySize} · static</span>
            </div>
          </div>
        </div>

        <aside className="hero-term" aria-label="terminal demonstration">
          <div className="hero-term-chrome mono">
            <span className="hero-term-dot" />
            <span className="hero-term-dot" />
            <span className="hero-term-dot" />
            <span className="hero-term-title">~/src/rustygit — zsh</span>
          </div>
          <pre className="hero-term-body mono"><code>
<span className="t-prompt">$</span> <span className="t-cmd">rustygit --version</span>{"\n"}
<span className="t-out">rustygit 0.1.0-beta.1 ({BUILD_META.sha} {BUILD_META.updated})</span>{"\n"}
<span className="t-out">built against {BUILD_META.oracleAgainst} oracle, {BUILD_META.testsPassing} / {BUILD_META.testsTotal} tests passing</span>{"\n"}
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
            <CmdGrid items={HOME_PLUMBING} />
          </Card>
          <Card eyebrow="layer 02" title="Porcelain">
            <p className="muted">The day-to-day surface. Every flag you reach for from the CLI lives here. <code>rebase -i</code> is the one notable absence — see <a href="#/watch-out" onClick={(e) => { e.preventDefault(); navigate("/watch-out"); }}>watch out</a>.</p>
            <CmdGrid items={HOME_PORCELAIN} />
          </Card>
          <Card eyebrow="layer 03" title="Maintenance">
            <p className="muted">House-keeping and repair. Two rustygit-only commands at the end: <code>doctor</code> for config audit, <code>prune-locks</code> for stale <code>.lock</code> files.</p>
            <CmdGrid items={HOME_MAINT} />
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
