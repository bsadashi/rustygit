import React from "react";
import { Section } from "../components/Section.jsx";
import { Callout } from "../components/Callout.jsx";
import { CodeBlock } from "../components/CodeBlock.jsx";
import { BUILD_META } from "../data.js";

export function WhyPage({ navigate }) {
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
            <div className="bar-row">
              <span className="bar-label mono">upstream git</span>
              <div className="bar">
                <div className="bar-fill bar-fill-grey" style={{ width: "100%" }}>
                  <span className="mono">~240 MB · perl + tcl + sh + git-core</span>
                </div>
              </div>
            </div>
            <div className="bar-row">
              <span className="bar-label mono">rustygit</span>
              <div className="bar">
                <div className="bar-fill bar-fill-accent" style={{ width: "2.5%" }}>
                  <span className="mono">~6 MB</span>
                </div>
              </div>
            </div>
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
          <a href={BUILD_META.roadmapUrl} target="_blank" rel="noreferrer">ROADMAP.md</a>).
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
          <a
            className="advice advice-left"
            href="#/compatibility"
            onClick={(e) => { e.preventDefault(); navigate("/compatibility"); }}
          >
            <div className="advice-eyebrow mono">/compatibility</div>
            <div className="advice-title">Need the full subcommand-by-subcommand tier table?</div>
            <div className="advice-arrow mono">open table →</div>
          </a>
          <a
            className="advice advice-right"
            href="#/install"
            onClick={(e) => { e.preventDefault(); navigate("/install"); }}
          >
            <div className="advice-eyebrow mono">/install</div>
            <div className="advice-title">Ready to install? Five-minute migration guide.</div>
            <div className="advice-arrow mono">install →</div>
          </a>
        </div>
      </Section>
    </main>
  );
}
