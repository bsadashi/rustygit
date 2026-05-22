import React, { useEffect, useRef, useState } from "react";
import { Section } from "../components/Section.jsx";
import { Callout } from "../components/Callout.jsx";
import { CodeBlock } from "../components/CodeBlock.jsx";
import { BUILD_META } from "../data.js";

const WATCHOUT_SECTIONS = [
  { id: "beta-status",         label: "7.1 Beta status" },
  { id: "silent-differences",  label: "7.2 Silent differences" },
  { id: "unsupported",         label: "7.3 Doesn't work at all" },
  { id: "data-loss",           label: "7.4 Data-loss risks" },
  { id: "refused-by-design",   label: "7.5 Refused by design" },
  { id: "windows",             label: "7.6 Windows users" },
  { id: "hooks",               label: "7.7 Hooks behaviour" },
  { id: "filing-bugs",         label: "7.8 Filing bugs" },
];

export function WatchOutPage({ navigate }) {
  const [active, setActive] = useState(WATCHOUT_SECTIONS[0].id);
  const observerRef = useRef(null);

  useEffect(() => {
    const opts = { rootMargin: "-30% 0px -55% 0px", threshold: 0 };
    const obs = new IntersectionObserver((entries) => {
      const visible = entries
        .filter((e) => e.isIntersecting)
        .sort((a, b) => a.target.offsetTop - b.target.offsetTop);
      if (visible[0]) setActive(visible[0].target.id);
    }, opts);
    WATCHOUT_SECTIONS.forEach((s) => {
      const el = document.getElementById(s.id);
      if (el) obs.observe(el);
    });
    observerRef.current = obs;
    return () => obs.disconnect();
  }, []);

  const tocClick = (e, id) => {
    e.preventDefault();
    const el = document.getElementById(id);
    if (el) {
      const y = el.getBoundingClientRect().top + window.scrollY - 90;
      window.scrollTo({ top: y, behavior: "smooth" });
      setActive(id);
      history.replaceState(null, "", `#/watch-out#${id}`);
    }
  };

  return (
    <main className="page page-watchout">
      <Section
        eyebrow="page 03 / the help page"
        title="Watch out"
        lede={
          <>
            <strong>The single most important page on the site.</strong> Ordered by
            how likely you are to hit it on day one. Every item below has been
            pulled from <code>MIGRATION.md</code>, <code>BETA.md</code>,{" "}
            <code>COMPAT.md</code>, <code>SECURITY.md</code> and{" "}
            <code>NON_GOALS.md</code> — they are real, not hypothetical.
          </>
        }
      />

      <div className="watchout-shell">
        {/* TOC */}
        <aside className="watchout-toc" aria-label="Contents">
          <div className="toc-head mono">on this page</div>
          <ol className="toc-list">
            {WATCHOUT_SECTIONS.map((s) => (
              <li key={s.id} className={`toc-item ${active === s.id ? "is-active" : ""}`}>
                <a
                  href={`#${s.id}`}
                  className="toc-link mono"
                  onClick={(e) => tocClick(e, s.id)}
                >
                  <span className="toc-rail" />
                  <span>{s.label}</span>
                </a>
              </li>
            ))}
          </ol>
          <div className="toc-foot mono">
            <a
              href="#top"
              onClick={(e) => { e.preventDefault(); window.scrollTo({ top: 0, behavior: "smooth" }); }}
            >
              ↑ top
            </a>
          </div>
        </aside>

        <article className="watchout-body">
          {/* 7.1 */}
          <section id="beta-status" className="watchout-sec">
            <header className="watchout-sec-head">
              <div className="watchout-sec-num mono">7.1</div>
              <h2 className="watchout-sec-title">Beta status</h2>
            </header>
            <Callout tone="yellow" icon="!" title="rustygit v0.1.0 is beta">
              <p>
                It is feature-complete for the documented scope and every format
                invariant is enforced by the test suite, but it has not yet seen
                14+ days of real-world traffic across more than a handful of
                repos.{" "}
                <strong>
                  Don't make it load-bearing for irreplaceable state without a
                  backup.
                </strong>
              </p>
              <p className="callout-rules">
                Use it for day-to-day work · keep a <code>git</code> binary
                nearby · back up anything you can't afford to lose.
              </p>
            </Callout>
          </section>

          {/* 7.2 */}
          <section id="silent-differences" className="watchout-sec">
            <header className="watchout-sec-head">
              <div className="watchout-sec-num mono">7.2</div>
              <h2 className="watchout-sec-title">Things that silently differ from git</h2>
            </header>
            <p className="watchout-lede">
              Both rustygit and git succeed, but produce different output or behaviour.{" "}
              <strong>None of these will eat your commits.</strong> They will
              however surprise you in scripts.
            </p>
            <ul className="footgun-list">
              <li className="footgun">
                <div className="footgun-head"><span className="footgun-mark mono">≠</span><h3>No ANSI colour</h3></div>
                <p>
                  rustygit does not emit colour codes today. <code>git</code>{" "}
                  honours <code>color.ui</code> and <code>--color</code>. If you
                  grep rustygit output by appearance, it'll look wrong; by exit
                  code or <code>--porcelain</code>, it's identical.
                </p>
              </li>
              <li className="footgun">
                <div className="footgun-head"><span className="footgun-mark mono">≠</span><h3>ASCII English dates</h3></div>
                <p>
                  <code>Mon Jan 1 00:00:00 2026 +0000</code> on every machine.{" "}
                  <code>LC_ALL</code> is ignored.
                </p>
              </li>
              <li className="footgun">
                <div className="footgun-head"><span className="footgun-mark mono">≠</span><h3><code>status --porcelain</code> lists submodule typechange entries</h3></div>
                <p>Upstream git silently skips them. rustygit lists them.</p>
              </li>
              <li className="footgun">
                <div className="footgun-head"><span className="footgun-mark mono">≠</span><h3><code>[includeIf]</code> and <code>[include]</code> are silently skipped</h3></div>
                <p>
                  …with a one-time warning. Inline the referenced content into the
                  parent config file if you depend on them.
                </p>
              </li>
            </ul>
          </section>

          {/* 7.3 */}
          <section id="unsupported" className="watchout-sec">
            <header className="watchout-sec-head">
              <div className="watchout-sec-num mono">7.3</div>
              <h2 className="watchout-sec-title">Things that don't work at all — fall back to git</h2>
            </header>
            <p className="watchout-lede">
              The big six. If you need any of these, the rustygit command will
              refuse clearly;{" "}
              <strong>use upstream <code>git</code> for that operation.</strong>
            </p>
            <div className="bigsix">
              {[
                {
                  name: "Submodules",
                  body: (
                    <p>
                      <code>submodule add</code> / <code>update</code> /{" "}
                      <code>foreach</code> are not implemented. Repos{" "}
                      <em>containing</em> submodules clone and check out fine
                      (gitlink mode 160000 is preserved) — but you can't manage
                      the submodules with rustygit.
                    </p>
                  ),
                },
                { name: "Sparse-checkout", body: <p>Cone and non-cone both unimplemented.</p> },
                {
                  name: ".gitattributes filters",
                  body: (
                    <p>
                      Smudge / clean / textconv filters are not run.{" "}
                      <strong>
                        Therefore Git LFS — LFS repos must stay on upstream{" "}
                        <code>git</code>.
                      </strong>
                    </p>
                  ),
                },
                {
                  name: "Interactive rebase",
                  body: (
                    <p>
                      <code>rebase -i</code>, <code>--autosquash</code>,{" "}
                      <code>--rebase-merges</code>, <code>--exec</code> are not
                      implemented. Non-interactive{" "}
                      <code>rebase &lt;upstream&gt;</code> works.
                    </p>
                  ),
                },
                {
                  name: "Partial clone",
                  body: (
                    <p>
                      <code>--filter=blob:none</code>, promisor remotes. rustygit
                      silently does a full clone instead of erroring — you lose
                      the bandwidth savings.
                    </p>
                  ),
                },
                {
                  name: "Old transports",
                  body: (
                    <p>
                      <code>git://</code>, <code>ftp://</code>,{" "}
                      <code>ftps://</code>, <code>rsync://</code>, and protocol
                      v0/v1 are refused with a named error. v2 over HTTPS is the
                      only supported transport.
                    </p>
                  ),
                },
              ].map((it, i) => (
                <article className="bigsix-card" key={it.name}>
                  <div className="bigsix-num mono">0{i + 1}</div>
                  <h3 className="bigsix-title">{it.name}</h3>
                  <div className="bigsix-body">{it.body}</div>
                  <div className="bigsix-foot mono">↳ use upstream <code>git</code> for this</div>
                </article>
              ))}
            </div>
          </section>

          {/* 7.4 */}
          <section id="data-loss" className="watchout-sec">
            <header className="watchout-sec-head">
              <div className="watchout-sec-num mono">7.4</div>
              <h2 className="watchout-sec-title">Data-loss-class risks</h2>
            </header>
            <Callout tone="red" icon="!!" title="Read carefully">
              <p>
                These are the three known ways a malicious or misconfigured repo
                can cause real damage. They are small but unmissable.
              </p>
            </Callout>

            <div className="danger-list">
              <article className="danger">
                <div className="danger-num mono">D1</div>
                <div className="danger-content">
                  <h3>Clones from untrusted remotes are not fully fsck'd at fetch time.</h3>
                  <p>After cloning from a remote you don't operate yourself, run:</p>
                  <CodeBlock>{`$ rustygit fsck --full`}</CodeBlock>
                  <p>…before committing anything against the result.</p>
                </div>
              </article>

              <article className="danger">
                <div className="danger-num mono">D2</div>
                <div className="danger-content">
                  <h3><code>~/.gitconfig</code> is code-equivalent.</h3>
                  <p>
                    Don't run rustygit against someone else's config file.{" "}
                    <code>-c key=value</code> overrides are honoured literally.
                    (This is true of upstream <code>git</code> too; we just don't
                    fix it.)
                  </p>
                </div>
              </article>

              <article className="danger">
                <div className="danger-num mono">D3</div>
                <div className="danger-content">
                  <h3>DoS via maliciously crafted packs.</h3>
                  <p>
                    A pack file declaring 2<sup>31</sup> entries will, today,
                    attempt to allocate that much. Bounds work is on the roadmap;
                    until then a malicious repo can wedge rustygit.{" "}
                    <strong>
                      Don't open repos from untrusted sources without ulimits.
                    </strong>
                  </p>
                </div>
              </article>
            </div>
          </section>

          {/* 7.5 */}
          <section id="refused-by-design" className="watchout-sec">
            <header className="watchout-sec-head">
              <div className="watchout-sec-num mono">7.5</div>
              <h2 className="watchout-sec-title">Refused-by-design</h2>
            </header>
            <p className="watchout-lede">
              These will <em>fail loudly</em> — they're not bugs. Listed so you
              know it's deliberate.
            </p>

            <div className="refused-list">
              <div className="refused-row">
                <div className="refused-head"><span className="mono">refuses</span><h3>Aliases starting with <code>!</code></h3></div>
                <p>Shell execution from <code>[alias]</code> is refused. Move the alias into your shell rc:</p>
                <CodeBlock caption="~/.zshrc">{`# ~/.zshrc
rgs() { rustygit status "$@" && rustygit diff --stat "$@"; }`}</CodeBlock>
              </div>

              <div className="refused-row">
                <div className="refused-head"><span className="mono">refuses</span><h3>Symlink writes during checkout on Windows</h3></div>
                <p>…unless <code>core.symlinks = false</code> is set explicitly.</p>
              </div>

              <div className="refused-row">
                <div className="refused-head"><span className="mono">refuses</span><h3>Non-UTF-8 path bytes on any platform</h3></div>
                <p>The conversion is lossy; we refuse rather than silently corrupt.</p>
              </div>

              <div className="refused-row">
                <div className="refused-head"><span className="mono">refuses</span><h3>Mutating <code>git replace</code></h3></div>
                <p><code>--delete</code>, <code>--edit</code>, <code>--graft</code>, positional create. Only <code>replace --list</code> works; the rest exit 128 with a named message.</p>
              </div>

              <div className="refused-row">
                <div className="refused-head"><span className="mono">refuses</span><h3><code>rerere</code> — the conflict-resolution database</h3></div>
                <p>Stub — every form exits 128 with "not implemented".</p>
              </div>
            </div>
          </section>

          {/* 7.6 */}
          <section id="windows" className="watchout-sec">
            <header className="watchout-sec-head">
              <div className="watchout-sec-num mono">7.6</div>
              <h2 className="watchout-sec-title">Windows users</h2>
            </header>
            <Callout tone="yellow" icon="◇" title="Windows in v0.1.x is best-effort, not load-bearing.">
              <p>
                CI does not run the porcelain integration test suite on Windows;
                library unit tests do. Concretely:
              </p>
            </Callout>
            <ul className="checks checks-lg">
              <li><span className="check mono">▸</span> Symlink checkout refuses.</li>
              <li><span className="check mono">▸</span> Non-UTF-8 paths refuse.</li>
              <li><span className="check mono">▸</span> <code>core.autocrlf</code> honours <code>true</code> / <code>input</code> / <code>false</code> only; the <code>.gitattributes</code> <code>text=auto</code> path is <strong>not</strong> honoured.</li>
              <li><span className="check mono">▸</span> Path-normalisation and case-insensitive-FS edge cases are not in the test matrix.</li>
            </ul>
            <p className="watchout-tail">
              If your daily driver is Windows and you hit any of these, stay on
              upstream <code>git</code> and revisit when v0.2 lands.
            </p>
          </section>

          {/* 7.7 */}
          <section id="hooks" className="watchout-sec">
            <header className="watchout-sec-head">
              <div className="watchout-sec-num mono">7.7</div>
              <h2 className="watchout-sec-title">Hooks behaviour</h2>
            </header>
            <p className="watchout-lede">
              Client hooks work. Server hooks don't — rustygit doesn't run as a
              server. The shipped set:
            </p>
            <div className="hooks-grid mono">
              {["pre-commit","prepare-commit-msg","commit-msg","post-commit","pre-push","pre-rebase","post-rewrite","pre-merge-commit","post-merge","post-checkout","pre-auto-gc"].map((h) => (
                <div className="hook-pill" key={h}><span className="hook-dot" />{h}</div>
              ))}
            </div>
            <ul className="checks">
              <li><span className="check mono">▸</span> <code>--no-verify</code> works on <code>commit</code> and <code>push</code> and skips <code>pre-commit</code> + <code>commit-msg</code> (but not <code>prepare-commit-msg</code>, matching upstream).</li>
              <li><span className="check mono">▸</span> A blocking hook returning non-zero aborts the parent op with exit code <strong>1</strong> (git's convention, not 128).</li>
            </ul>
          </section>

          {/* 7.8 */}
          <section id="filing-bugs" className="watchout-sec">
            <header className="watchout-sec-head">
              <div className="watchout-sec-num mono">7.8</div>
              <h2 className="watchout-sec-title">Filing bugs</h2>
            </header>
            <CodeBlock>{`$ rustygit bug-report`}</CodeBlock>
            <p>
              Paste the output into a new issue at{" "}
              <a href={BUILD_META.issuesUrl} target="_blank" rel="noreferrer">
                {BUILD_META.issuesUrl.replace("https://", "")}
              </a>.
            </p>
            <Callout tone="red" icon="!!" title="Data-loss / silent-corruption / segfault">
              <p>
                Use{" "}
                <a href={BUILD_META.securityUrl} target="_blank" rel="noreferrer">
                  GitHub Security Advisories
                </a>{" "}
                instead of a public issue, so the discussion stays private until
                a fix ships.
              </p>
            </Callout>
          </section>
        </article>
      </div>
    </main>
  );
}
