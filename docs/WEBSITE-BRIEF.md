# rustygit — Info & Help website design brief

> **For the design pass (Claude Design):** produce a complete visual + IA design
> + page-by-page content blocking for the website described below. Output
> should be detailed enough that a web developer can implement it without
> needing to reread the rustygit source repo. Hand off a sitemap, wireframes
> (low- or mid-fi is fine), a component inventory, a typography + colour
> system, the populated copy for every section, and a short tech-stack
> recommendation.
>
> **For the web developer (downstream):** the design pack from the previous
> step is authoritative on look, feel, and content. This brief is the raw
> source the design was built from — read it only if you need to settle a
> dispute between two interpretations of the design.

---

## 1. Project at a glance

**rustygit** is a from-scratch Rust reimplementation of git's core: object
store, refs, index, packfiles, plumbing **and** porcelain — single static
binary, no shell scripts, no Perl, no Tcl/Tk. Current release: **v0.1.0
(beta)**. 941 tests passing. Byte-for-byte oracle compatibility against
upstream git on every on-disk format.

The website's job is to **convince a working developer that rustygit is safe
to try** and **tell them clearly where it isn't safe to try yet**. It is not
a marketing site — it is an info + help site for an early-adopter audience.

Repo: `https://github.com/bsadashi/rustygit`
Licence: dual Apache-2.0 / MIT.

## 2. Audience

Primary: software engineers who already use git every day, are
Rust-curious or Rust-friendly, and are evaluating whether to swap out the
`git` binary on a machine they actually use.

Secondary: tooling authors / package maintainers / people researching
git-format compatibility (they want the compatibility table without
scrolling past marketing copy).

Both audiences are technical. **No condescension, no buzzwords, no "git is
hard, we make it easy" framing.** rustygit is *not* easier than git; it is
*Rust-safer than git for the documented scope.*

## 3. Tone & voice

- Direct, technical, lowercase-friendly.
- No emoji in body copy. No exclamation marks.
- Use code voice for command names: `rustygit clone`, `git rebase -i`.
- Honest about limits — every "what it can do" claim is paired with the
  matching "what it can't do" caveat. The credibility of the whole site
  rests on this.
- Avoid the words "blazingly fast", "modern", "next-generation",
  "revolutionary". rustygit isn't claiming to be any of those.
- One-line headline candidate: *"git, reimplemented in Rust, byte-for-byte
  compatible where it counts."* Designers may iterate; keep it factual.

## 4. Sitemap

Five pages. Single-page-app or static-site, designer's call (see §11).

1. **Home / `/`** — hero + 60-second pitch + status + CTA.
2. **Why rustygit / `/why`** — where it beats `git` and what "better" means
   here (it is not "faster").
3. **Watch out / `/watch-out`** — the help page. Footguns, divergences,
   migration gotchas. This is the **most important page on the site.**
4. **Compatibility / `/compatibility`** — the full subcommand tier table
   (T1 / T2 / T3 / OUT).
5. **Install & Migrate / `/install`** — install instructions + the six-step
   migration guide.

Optional secondary nav: `Security`, `Roadmap`, `GitHub` (external).

Footer on every page: version (`v0.1.0-beta.1`), licence, last-updated date,
GitHub link, security-report link.

## 5. Page 1 — Home

### Goal

Someone landing here for the first time should, in under 30 seconds, know:
(a) what rustygit is, (b) that it's beta, (c) where to go to evaluate it
deeper.

### Sections (top → bottom)

1. **Hero**
   - One-line proposition (designer-iterated; see §3 above).
   - One-paragraph descriptor (copy below).
   - Primary CTA: `Install` (→ `/install`). Secondary CTA: `See where to be
     careful` (→ `/watch-out`). The secondary CTA being prominent is
     deliberate — it sets the tone.
   - Status pill: `v0.1.0 · beta · 941 tests passing`.

   **Copy for the descriptor paragraph:**
   > rustygit is a from-scratch Rust reimplementation of git's core. Object
   > store, refs, index, packfiles, plumbing and porcelain — all in one
   > static binary, no shell, no Perl, no Tcl. On every on-disk format
   > (loose objects, packfiles, indexes, refs, reftable, commit-graph,
   > midx, reflog) it produces output that is byte-for-byte identical to
   > upstream git, verified by an oracle test suite that runs both
   > binaries side-by-side.

2. **"What it actually does"** — three-card grid:
   - **Plumbing.** `hash-object`, `cat-file`, `write-tree`, `commit-tree`,
     `rev-parse`, `rev-list`, `pack-objects`, `unpack-objects`, `index-pack`
     (stub), `verify-pack`, `update-ref`, `read-tree`, `merge-tree`,
     `merge-file`, `merge-base`, `diff-tree`, `diff-index`, `diff-files`,
     `mktree`, `mktag`, `patch-id`, `show-index`, `for-each-ref`,
     `check-ref-format`, `check-ignore`, `check-attr`, `interpret-trailers`.
   - **Porcelain.** `init`, `clone`, `fetch`, `pull`, `push`, `add`, `rm`,
     `mv`, `commit`, `status`, `log`, `show`, `diff`, `branch`, `checkout`,
     `switch`, `restore`, `reset`, `merge`, `rebase` (non-interactive),
     `cherry-pick`, `revert`, `tag` (incl. signed), `stash`, `notes`,
     `worktree`, `bisect`, `blame`, `grep`, `clean`, `describe`, `archive`,
     `bundle`, `shortlog`, `name-rev`, `show-branch`, `range-diff`.
   - **Maintenance.** `gc`, `fsck`, `prune`, `prune-packed`, `repack`,
     `pack-refs`, `commit-graph`, `multi-pack-index`, `reflog`, plus
     rustygit-specific `doctor` and `prune-locks`.

3. **Two-side advice strip** (paired CTAs, equal weight):
   - Left: *"Curious where rustygit beats `git`?"* → `/why`.
   - Right: *"About to install it on a real machine? Read this first."* →
     `/watch-out`.

4. **Code teaser** — a single Bash codeblock showing rustygit drop-in:
   ```sh
   $ cargo install --path .
   $ rustygit clone https://github.com/bsadashi/rustygit
   $ rustygit log --oneline -n 3
   ```
   Below it: *"It is `git`, except the binary is named `rustygit`. Every
   argv, every exit code, every byte of on-disk output is verified against
   the upstream `git` binary in CI."*

5. **Footer + status info.**

## 6. Page 2 — Why rustygit

### Goal

For an engineer who already has working git: explain the actual reasons to
swap. Be honest that performance is not one of them today.

### Lede paragraph

> rustygit is **not** faster than upstream git in v0.1. Speed isn't the
> contract; safety, single-binary deployment, and a clean Rust-native core
> to build on are. Here's what you actually get.

### Sections (each card or row)

1. **Memory-safety on a tool that touches your commit history.**
   - The git binary is millions of lines of C with a long CVE list.
     rustygit is `#![deny(unsafe_code)]` outside of three audited perf
     spots, all of which are documented in-tree.

2. **One static binary. No Perl, no shell, no Tcl/Tk.**
   - `git` ships with a couple-hundred-megabyte install footprint of
     scripting runtimes for things like `git-svn`, `gitweb`, `gitk`,
     `git-gui`, `request-pull`. rustygit ships **none** of those — by
     design (see `/compatibility` — they're out of scope), and the binary
     is ~6 MB stripped.

3. **No silent failures in analysis paths.**
   - `Result` everywhere, typed errors only. The "this read failed but we
     pretended it didn't" class of bugs is statically excluded. A reviewer
     pass (the `silent-failure-hunter`) gates safety-critical PRs.

4. **Modern defaults.**
   - UTF-8 path bytes (lossy conversion is refused, not done silently).
   - SHA-256 object format ready (sha-1 is still default).
   - Reftable on by request.
   - English-only — no LC_ALL surprises (see `/watch-out` for the catch).

5. **Byte-for-byte format compatibility.**
   - Every on-disk format (loose objects, packfiles, `.idx` v2 + v3,
     reftable, packed-refs, commit-graph, midx, reflog) is verified
     bit-equal to `git`'s by an oracle test suite. You can run rustygit and
     upstream git on the same repo, alternating between them, and neither
     binary will notice.

6. **No `!` shell aliases.**
   - `[alias]` entries that begin with `!` (i.e. arbitrary shell execution
     from a config file) are **refused** at expansion time. This is a
     supply-chain footgun that upstream git ships as a feature. rustygit
     does not. (See `/watch-out` for the workaround.)

7. **Strict path traversal protection.**
   - rustygit refuses to write through symlinks during checkout. Tree
     entries with `..` segments are rejected before the working-tree write.
     Non-UTF-8 paths on Windows are refused rather than silently corrupted.

8. **Better diagnostics for things we don't do.**
   - Running `rustygit gitweb` doesn't error with "unrecognized command" —
     it explains *why* it's not implemented and what upstream tool to use
     (e.g. `tig`, `lazygit`, `gitui`, `git-filter-repo`).
   - `git://` / `ftp://` / `rsync://` URLs return a named
     `UnsupportedTransport` error with a clear reason rather than a cryptic
     "didn't advertise v2".

### Closing paragraph

> Performance work is on the roadmap (Phase 3, see `ROADMAP.md`). If you're
> waiting for "rustygit is 4× faster than git" — that's not what v0.1.0 is
> selling. v0.1.0 is selling a Rust core you can build on, with the format
> compatibility locked down so the work above it doesn't drift.

## 7. Page 3 — Watch out (the help page)

### Goal

**This page is the single most important asset on the site.** It must
prevent a user from losing data, getting confused, or ending up in a
broken-repo state. Every item below has been pulled from `MIGRATION.md`,
`BETA.md`, `COMPAT.md`, `SECURITY.md`, and `NON_GOALS.md` — they are real,
not hypothetical.

### Layout

A taxonomy of risk, top to bottom, ordered by *how likely you are to hit
it on day one*:

#### 7.1 — Beta status

> rustygit v0.1.0 is **beta**. It is feature-complete for the documented
> scope and every format invariant is enforced by the test suite, but it
> has not yet seen 14+ days of real-world traffic across more than a
> handful of repos. **Don't make it load-bearing for irreplaceable state
> without a backup.** Use it for day-to-day work; keep a `git` binary
> nearby; back up anything you can't afford to lose.

Visual: large yellow inset, near the top. Not red — red is reserved for
data-loss-class items below.

#### 7.2 — Things that silently differ from `git`

Items where rustygit and `git` both succeed but produce different output or
behaviour. **None of these will eat your commits.** They will however
surprise you in scripts.

- **No ANSI colour.** rustygit does not emit colour codes today. `git`
  honours `color.ui` and `--color`. If you grep rustygit output by
  appearance, it'll look wrong; by exit code or `--porcelain`, it's
  identical.
- **ASCII English dates.** `Mon Jan 1 00:00:00 2026 +0000`. `LC_ALL` is
  ignored.
- **`status --porcelain` lists submodule typechange entries.** `git`
  silently skips them.
- **`[includeIf]` and `[include]` config directives are silently skipped**
  (with a one-time warning). Inline the referenced content into the
  parent config file if you depend on them.

#### 7.3 — Things that **don't work at all** — fall back to `git`

The big six. If you need any of these, the rustygit command will refuse
clearly; **use upstream `git` for that operation.**

- **Submodules.** `submodule add` / `update` / `foreach` are not
  implemented. Repos *containing* submodules clone and check out fine
  (gitlink mode 160000 is preserved) — but you can't manage the
  submodules with rustygit.
- **Sparse-checkout** (cone or non-cone).
- **`.gitattributes` filters** (smudge / clean / textconv). And therefore
  **Git LFS** — LFS repos must stay on upstream `git`.
- **Interactive rebase.** `rebase -i`, `--autosquash`, `--rebase-merges`,
  `--exec` are not implemented. Non-interactive `rebase <upstream>`
  works.
- **Partial clone** (`--filter=blob:none`, promisor remotes). rustygit
  silently does a full clone instead of erroring — you lose the
  bandwidth savings.
- **Old transports.** `git://`, `ftp://`, `ftps://`, `rsync://`, and
  protocol v0/v1 are refused with a named error. v2 over HTTPS is the
  only supported transport.

Visual: each item gets its own card with a `Use upstream git for this`
secondary line.

#### 7.4 — Data-loss-class risks (read carefully)

Visual: red inset. Small but unmissable.

- **Clones from untrusted remotes are not fully fsck'd at fetch time.**
  After cloning from a remote you don't operate yourself, run:
  ```sh
  rustygit fsck --full
  ```
  before committing anything against the result.
- **`~/.gitconfig` is code-equivalent.** Don't run rustygit against
  someone else's config file. `-c key=value` overrides are honoured
  literally. (This is true of upstream `git` too; we just don't fix it.)
- **DoS via maliciously crafted packs.** A pack file declaring 2^31
  entries will, today, attempt to allocate that much. Bounds work is on
  the roadmap; until then a malicious repo can wedge rustygit. Don't
  open repos from untrusted sources without ulimits.

#### 7.5 — Refused-by-design (different from "doesn't work")

These will *fail loudly* — they're not bugs. Listed so you know it's
deliberate.

- **Aliases starting with `!`** (shell execution from `[alias]`) are
  refused. Move the alias into your shell rc:
  ```sh
  # ~/.zshrc
  rgs() { rustygit status "$@" && rustygit diff --stat "$@"; }
  ```
- **Symlink writes during checkout on Windows** unless `core.symlinks =
  false` is set explicitly.
- **Non-UTF-8 path bytes** on any platform.
- **Mutating `git replace`** (`--delete`, `--edit`, `--graft`, positional
  create). Only `replace --list` works; the rest exit 128 with a named
  message.
- **`rerere`** (the conflict-resolution database). Stub — every form
  exits 128 with "not implemented".

#### 7.6 — Windows users

Windows in v0.1.x is **best-effort**, not load-bearing. CI does not run
the porcelain integration test suite on Windows; library unit tests do.
Concretely:

- Symlink checkout refuses (see above).
- Non-UTF-8 paths refuse (see above).
- `core.autocrlf` honours `true` / `input` / `false` only; the
  `.gitattributes` `text=auto` path is **not** honoured.
- Path-normalisation and case-insensitive-FS edge cases are not in the
  test matrix.

If your daily driver is Windows and you hit any of these, stay on
upstream `git` and revisit when v0.2 lands.

#### 7.7 — Hooks behaviour

Client hooks work. Server hooks don't (rustygit doesn't run as a server).
The shipped hook set: `pre-commit`, `prepare-commit-msg`, `commit-msg`,
`post-commit`, `pre-push`, `pre-rebase`, `post-rewrite`,
`pre-merge-commit`, `post-merge`, `post-checkout`, `pre-auto-gc`.

`--no-verify` works on `commit` and `push` and skips `pre-commit` +
`commit-msg` (but not `prepare-commit-msg`, matching upstream).

A blocking hook returning non-zero aborts the parent op with exit code
**1** (git's convention, not 128).

#### 7.8 — Filing bugs

```sh
rustygit bug-report
```

Paste the output into a new issue at
`https://github.com/bsadashi/rustygit/issues`.

**Data-loss / silent-corruption / segfault: use [GitHub Security
Advisories](https://github.com/bsadashi/rustygit/security/advisories/new)
instead** so the discussion stays private until a fix ships.

### Visual treatment

This is a long page. Designer: use a sticky TOC on the left (sections
7.1–7.8) so a user reading mid-page can see how far they have to go and
jump back to top. Each section gets a level-2 anchor (`#beta-status`,
`#silent-differences`, `#unsupported`, `#data-loss`, etc.) so we can
deep-link from error messages in the binary itself.

## 8. Page 4 — Compatibility

### Goal

Render the full subcommand tier table from `COMPAT.md`. This page is for
the *tooling author* audience — give them a searchable, filterable table
they can scan.

### Sections

1. **Tier legend** — four pills:
   - **T1** (green) — byte-for-byte match with upstream `git`.
   - **T2** (blue) — semantically equivalent, format may differ.
   - **T3** (purple) — rustygit-specific.
   - **OUT** (grey) — out of scope.

2. **Full subcommand table** — sortable, filterable by tier + by search
   string. Columns: `Subcommand`, `Tier`, `Notes`. Source data: the
   "Porcelain" table in `COMPAT.md`. Roughly 90 rows.

3. **Top-level flags table** — small, two rows: `-C <PATH>`, `-c
   <KEY=VALUE>`. Both T1.

4. **Known output divergences** — three bullets from `COMPAT.md` §
   "Output divergences".

5. **Out-of-scope features** — bulleted list from `COMPAT.md` § "Out of
   scope". Same content as `/watch-out` §7.3 — but here it's framed as
   reference, not as a warning.

6. **SemVer policy** — single paragraph + the MAJOR/MINOR/PATCH
   breakdown.

Designer: this is the only page where a **table-first layout** is the
right answer. Resist the urge to art-direct it — the table itself is the
content.

## 9. Page 5 — Install & Migrate

### Goal

Get someone from "I want to try this" to "I have rustygit running on a
repo I care about" in under five minutes.

### Sections

1. **Install** — from source is the only path today:
   ```sh
   git clone https://github.com/bsadashi/rustygit
   cd rustygit
   cargo install --path .
   ```
   Prebuilt distribution (Homebrew, `.deb`, `.rpm`, crates.io) is
   deferred. The brief lists the *forward-looking* shape these tabs
   *will* take, but until the packaging pipeline lands, render only
   the "From source" block.

2. **First-run check** — single command:
   ```sh
   rustygit doctor --import-config
   ```
   Tell the user: this reports which keys in their existing `~/.gitconfig`
   rustygit honours, ignores, or refuses. **Run this before anything
   else.**

3. **Identity setup**:
   ```sh
   rustygit config --global user.name "Your Name"
   rustygit config --global user.email "you@example.com"
   ```

4. **Aliases** — explain the `!`-prefix refusal (link to `/watch-out`
   §7.5). Show the shell-function workaround.

5. **The escape-hatch alias** — drop-in for users who want rustygit by
   default but a clean fallback to upstream git on incompatible repos:
   ```sh
   # ~/.zshrc or ~/.bashrc
   alias gitsafe='if grep -q RUSTYGIT_INCOMPAT .git/.rustygit-flags 2>/dev/null; then git "$@"; else rustygit "$@"; fi'
   ```
   Per-repo opt-out:
   ```sh
   mkdir -p .git
   echo RUSTYGIT_INCOMPAT > .git/.rustygit-flags
   ```

6. **Silencing the beta banner**:
   ```sh
   rustygit config --global rustygit.beta.acknowledged true
   ```
   Or for a single command (useful in CI):
   ```sh
   rustygit --i-know-this-is-beta status
   ```

7. **What now?** — three follow-up CTAs:
   - "Read where rustygit beats `git`" → `/why`
   - "Read where to be careful" → `/watch-out`
   - "Open the compatibility table" → `/compatibility`

## 10. Visual direction (notes to designer)

This is a personal-project / tooling site, not an enterprise product. Aim
for:

- **High contrast, terminal-leaning palette.** Dark mode by default with a
  light-mode toggle. Code samples should look like a terminal, not like a
  prettified GitHub README. Monospace for inline `commands` everywhere.
- **Typography:** one display sans (e.g. Inter, Geist, Söhne, JetBrains
  Sans), one monospace (JetBrains Mono, IBM Plex Mono, or Berkeley Mono).
  Resist three+ typefaces.
- **No stock illustrations.** No vector "developer at laptop" art. If a
  page needs visual interest, use real terminal output, real
  syntax-highlighted code, or a real diff. Authenticity over decoration.
- **Tier badges** are the one place where colour does heavy lifting —
  pick four distinct hues that pass WCAG AA at small sizes.
- **No animations except micro (focus rings, hover state).** No
  on-scroll reveals. Engineers reading the help page are stressed; do not
  add friction.
- **Mobile is secondary but not broken.** People will read `/watch-out`
  on their phone when something goes wrong on a desktop. The TOC should
  collapse cleanly.

## 11. Tech stack (recommendation for the dev)

Designer should recommend one of these, and stop there. Both are fine:

- **Astro** (static site generator, content-first). Best fit since the
  site is mostly documentation. Use Astro Content Collections for the
  compatibility table (typed frontmatter). MDX for the long-form pages.
- **Next.js (App Router) with `output: 'export'`.** If the team is more
  React-fluent than Astro-fluent.

In either case:
- TailwindCSS for styling. No CSS-in-JS.
- Shiki for code-block syntax highlighting (pulls VS Code grammars; ships
  zero JS to the client when used at build time).
- No analytics on launch. Add Plausible later if needed.
- Deploy: GitHub Pages or Cloudflare Pages (the project owner runs a K3s
  homelab; if they want self-host, that works too — the site is fully
  static).

## 12. Content sources (for the design pass)

When in doubt about wording on a specific page, **use the rustygit repo
docs as source of truth.** All are in the repo root:

| Page         | Primary source                |
|--------------|-------------------------------|
| Home         | `README.md`                   |
| Why          | `MIGRATION.md` §"Why switch?" |
| Watch out    | `MIGRATION.md`, `BETA.md`, `SECURITY.md`, `NON_GOALS.md`, `COMPAT.md` §"Output divergences" |
| Compatibility| `COMPAT.md` (full file)       |
| Install      | `README.md`, `MIGRATION.md`, `BETA.md` §"Acknowledging beta" |

Do not invent capabilities. Do not soften the limits. If the source says
"deferred", the site says "deferred", not "coming soon" — those mean
different things.

## 13. Deliverable checklist (design pass)

Designer hands the developer:

- [ ] Sitemap (5 pages + footer pages).
- [ ] Wireframes for each of the 5 pages (low- or mid-fi).
- [ ] Component inventory (header, footer, hero, card, tier-badge,
      callout-yellow, callout-red, code-block, copy-to-clipboard, TOC,
      tabbed-install-block, sortable-table).
- [ ] Typography + colour system (with WCAG-AA contrast notes on the
      tier-badge palette).
- [ ] **Populated copy for every section** — not lorem ipsum. Use the
      content above (and the source docs) verbatim where possible.
- [ ] Recommended tech stack from §11.
- [ ] Mobile breakpoint sketch for `/watch-out` (the long page).

## 14. Non-goals (things the site does **not** do)

So the design doesn't accidentally drift into them:

- **No interactive demo / WASM playground.** The binary is a system tool;
  faking it in the browser would lie about what the user is evaluating.
- **No "compare rustygit vs git" benchmark charts.** v0.1.0 isn't
  benchmarked. Adding a chart would imply a claim we can't back.
- **No mailing-list signup.** GitHub stars + the issue tracker are the
  only feedback channels.
- **No testimonials.** It's beta; there aren't any.
- **No light marketing-site sections** ("Trusted by", "Featured in",
  "Companies using"). If something like that ever applies, it ships in
  a different release.
