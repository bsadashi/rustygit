import React from "react";
import { Section } from "../components/Section.jsx";
import { Callout } from "../components/Callout.jsx";
import { CodeBlock } from "../components/CodeBlock.jsx";
import { BUILD_META } from "../data.js";

export function SecurityPage({ navigate }) {
  return (
    <main className="page page-security">
      <Section
        eyebrow="security policy"
        title="Security"
        lede={
          <>
            Mirrored from <code>SECURITY.md</code>. If something here disagrees
            with the repo file, the repo file wins — open an issue.
          </>
        }
      />

      <Section eyebrow="01 · reporting" title="Reporting a vulnerability">
        <Callout tone="red" icon="!!" title="Don't open a public issue for a security bug">
          <p>
            Once a CVE-class problem is on a public tracker it's been made
            trivially exploitable for everyone running the affected version.
          </p>
        </Callout>
        <p>
          Use{" "}
          <a href={BUILD_META.securityUrl} target="_blank" rel="noreferrer">
            GitHub Security Advisories
          </a>{" "}
          to report privately. We aim to acknowledge new advisories within{" "}
          <strong>72 hours</strong>.
        </p>
      </Section>

      <Section eyebrow="02 · supported versions" title="Supported versions">
        <div className="compat-table-wrap compat-table-wrap-tight">
          <table className="compat-table">
            <thead>
              <tr><th>Version</th><th>Supported</th></tr>
            </thead>
            <tbody>
              <tr>
                <td className="td-cmd mono">v0.1.x (current)</td>
                <td className="td-notes">Yes</td>
              </tr>
              <tr>
                <td className="td-cmd mono">earlier</td>
                <td className="td-notes muted">n/a (none released)</td>
              </tr>
            </tbody>
          </table>
        </div>
      </Section>

      <Section eyebrow="03 · disclosure" title="Disclosure window">
        <p>
          We coordinate on a <strong>90-day</strong> disclosure window starting
          from the initial report. If the issue meets the relevant criteria we'll
          request a CVE through GitHub's CNA. If a fix is shipped sooner we'll
          publish the advisory at that point; if more time is needed we'll
          explain why and agree on an extension with the reporter.
        </p>
      </Section>

      <Section eyebrow="04 · threat model" title="What we treat as in-scope">
        <div className="threat-grid">
          <article className="threat">
            <div className="threat-num mono">T.1</div>
            <h3>Malicious <code>.git</code> directories</h3>
            <p>
              Cloning or operating on a hand-crafted repository is part of the
              threat model. We validate refs (<code>src/refs/name.rs</code>),
              object oids and connectivity (<code>src/fsck.rs</code>), and pack
              indexes (<code>src/pack/file.rs</code>) on read. Treat structural
              exceptions here as in-scope.
            </p>
          </article>
          <article className="threat">
            <div className="threat-num mono">T.2</div>
            <h3>Malicious remotes</h3>
            <p>
              Server-supplied pack contents pass through the same on-read
              validation as any other pack file, but rustygit does not yet run a
              full <code>fsck</code> walk during fetch.{" "}
              <strong>
                If you clone from an untrusted remote, run{" "}
                <code>rustygit fsck --full</code> afterwards.
              </strong>
            </p>
            <CodeBlock>{`$ rustygit fsck --full`}</CodeBlock>
          </article>
          <article className="threat">
            <div className="threat-num mono">T.3</div>
            <h3>Malicious config</h3>
            <p>
              <code>~/.gitconfig</code>,{" "}
              <code>$XDG_CONFIG_HOME/git/config</code>,{" "}
              <code>&lt;gitdir&gt;/config</code>, and any{" "}
              <code>-c key=value</code> overrides are honoured literally.{" "}
              <strong>Don't run rustygit with someone else's config file.</strong>{" "}
              A config file is code-equivalent.
            </p>
          </article>
          <article className="threat">
            <div className="threat-num mono">T.4</div>
            <h3>Hooks</h3>
            <p>
              Hooks live in <code>.git/hooks/</code> and must be marked
              executable. rustygit does not auto-execute hooks dropped by a
              clone — matching upstream's <code>core.hooksPath</code> policy — but
              a hook you've already enabled will run for every applicable
              operation.
            </p>
          </article>
          <article className="threat">
            <div className="threat-num mono">T.5</div>
            <h3>Path traversal in checkout</h3>
            <p>
              rustygit refuses to write through symlinks during checkout and
              refuses non-UTF-8 paths on Windows; tree entries with <code>..</code>{" "}
              segments are rejected before the working-tree write.
            </p>
          </article>
        </div>
      </Section>

      <Section eyebrow="05 · out of scope" title="What's NOT in scope">
        <ul className="oos-list">
          <li className="oos">
            <span className="oos-glyph mono">×</span>
            <span>
              <strong>Vulnerabilities in upstream git itself.</strong> Report
              those to{" "}
              <a href="mailto:git-security@googlegroups.com">git-security@googlegroups.com</a>.
              We track upstream advisories and patch rustygit when the same
              defect applies to our implementation, but the upstream report
              comes first.
            </span>
          </li>
          <li className="oos">
            <span className="oos-glyph mono">×</span>
            <span>
              <strong>Vulnerabilities in dependencies.</strong> Report to the
              respective crate (<code>flate2</code>, <code>ureq</code>,{" "}
              <code>clap</code>, etc.). We will of course bump the affected
              dependency once a fix is available, but the upstream advisory is
              the authoritative record.
            </span>
          </li>
          <li className="oos">
            <span className="oos-glyph mono">×</span>
            <span>
              <strong>Denial-of-service via maliciously crafted repos.</strong>{" "}
              rustygit is not yet memory-bounded enough to make a hard guarantee
              here — a pack file declaring 2<sup>31</sup> entries will, today,
              attempt to allocate that much. Bounds work is on the roadmap;
              until then we don't treat DoS-by-size as a security defect.
            </span>
          </li>
        </ul>
      </Section>

      <Section narrow>
        <div className="advice-strip">
          <a
            className="advice advice-left"
            href={BUILD_META.securityUrl}
            target="_blank"
            rel="noreferrer"
          >
            <div className="advice-eyebrow mono">↗ private</div>
            <div className="advice-title">Open a private security advisory</div>
            <div className="advice-arrow mono">github advisories →</div>
          </a>
          <a
            className="advice advice-right"
            href="#/watch-out"
            onClick={(e) => { e.preventDefault(); navigate("/watch-out#data-loss"); }}
          >
            <div className="advice-eyebrow mono">/watch-out#data-loss</div>
            <div className="advice-title">Three known data-loss-class risks</div>
            <div className="advice-arrow mono">read →</div>
          </a>
        </div>
      </Section>
    </main>
  );
}
