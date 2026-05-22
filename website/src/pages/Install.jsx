import React from "react";
import { Section } from "../components/Section.jsx";
import { CodeBlock } from "../components/CodeBlock.jsx";

export function InstallPage({ navigate }) {
  return (
    <main className="page page-install">
      <Section
        eyebrow="page 05 / install + migrate"
        title="Install & migrate"
        lede="From 'I want to try this' to 'I have rustygit running on a repo I care about', in under five minutes."
      />

      {/* 1. Install */}
      <Section eyebrow="01 · install" title="Install from source">
        <p>Requires a Rust toolchain (1.85+). The binary ends up in <code>~/.cargo/bin</code>.</p>
        <CodeBlock>{`$ git clone https://github.com/bsadashi/rustygit
$ cd rustygit
$ cargo install --path .`}</CodeBlock>
        <p className="muted small">
          Build time on an M-class laptop: ~90 seconds release. The release profile uses LTO; expect 2–3× of that on a 4-core box. Prebuilt binaries (Homebrew, <code>.deb</code>, <code>.rpm</code>) will be added when the packaging pipeline is in place.
        </p>
      </Section>

      {/* 2. First-run check */}
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

      {/* 3. Identity */}
      <Section eyebrow="03 · identity" title="Set your identity">
        <CodeBlock>{`$ rustygit config --global user.name "Your Name"
$ rustygit config --global user.email "you@example.com"`}</CodeBlock>
      </Section>

      {/* 4. Aliases */}
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

      {/* 5. Escape hatch */}
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

      {/* 6. Beta banner */}
      <Section eyebrow="06 · banner" title="Silence the beta warning">
        <p>Once you're comfortable, acknowledge the banner globally:</p>
        <CodeBlock>{`$ rustygit config --global rustygit.beta.acknowledged true`}</CodeBlock>
        <p>…or one-shot it from CI, where you don't want it stamped in the config:</p>
        <CodeBlock>{`$ rustygit --i-know-this-is-beta status`}</CodeBlock>
      </Section>

      {/* 7. Next */}
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
