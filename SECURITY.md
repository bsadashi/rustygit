# Security Policy

## Reporting a vulnerability

Please use [GitHub Security Advisories](https://github.com/bsadashi/rustygit/security/advisories/new)
to report security issues privately. **Do not open a public issue** for a
security bug — once a CVE-class problem is on a public tracker it's been
made trivially exploitable for everyone running the affected version.

We aim to acknowledge new advisories within 72 hours.

## Supported versions

| Version    | Supported |
|------------|:---------:|
| `v0.1.x` (current) | Yes |
| earlier    | n/a (none released) |

## Disclosure window

We coordinate on a **90-day** disclosure window starting from the initial
report. If the issue meets the relevant criteria we'll request a CVE
through GitHub's CNA. If a fix is shipped sooner we'll publish the
advisory at that point; if more time is needed we'll explain why and
agree on an extension with the reporter.

## Threat model summary

* **Malicious `.git` directories.** Cloning or operating on a hand-crafted
  repository is part of the threat model. We validate refs
  (`src/refs/name.rs`), object oids and connectivity (`src/fsck.rs`), and
  pack indexes (`src/pack/file.rs`) on read. Treat structural exceptions
  here as in-scope.
* **Malicious remotes.** Server-supplied pack contents pass through the
  same on-read validation as any other pack file, but rustygit does not
  yet run a full `fsck` walk during fetch. **If you clone from an
  untrusted remote, run `rustygit fsck --full` afterwards.**
* **Malicious config.** `~/.gitconfig`, `$XDG_CONFIG_HOME/git/config`,
  `<gitdir>/config`, and any `-c key=value` overrides are honored
  literally. **Don't run rustygit with someone else's config file.** A
  config file is code-equivalent.
* **Hooks.** Hooks live in `.git/hooks/` and must be marked executable.
  rustygit does not auto-execute hooks dropped by a clone — matching
  upstream's `core.hooksPath` policy — but a hook you've already enabled
  will run for every applicable operation.
* **Path traversal in checkout.** rustygit refuses to write through
  symlinks during checkout and refuses non-UTF-8 paths on Windows; tree
  entries with `..` segments are rejected before the working-tree write.

## What's NOT in scope

* **Vulnerabilities in upstream git itself.** Report those to
  <git-security@googlegroups.com>. We track upstream advisories and patch
  rustygit when the same defect applies to our implementation, but the
  upstream report comes first.
* **Vulnerabilities in dependencies.** Report to the respective crate
  (`flate2`, `ureq`, `clap`, etc.). We will of course bump the affected
  dependency once a fix is available, but the upstream advisory is the
  authoritative record.
* **Denial-of-service via maliciously crafted repos.** rustygit is not yet
  memory-bounded enough to make a hard guarantee here — a pack file
  declaring 2^31 entries will, today, attempt to allocate that much.
  Bounds work is on the roadmap; until then we don't treat DoS-by-size
  as a security defect.
