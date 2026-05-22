# rustygit — website

The static info & help site for rustygit. Six pages, ~60 KB of JS gzipped,
no analytics, no cookies, no server. Builds to a single `dist/` directory
that drops into GitHub Pages, Cloudflare Pages, S3, nginx, or any other
static host.

> If you only want to read the content, the source-of-truth is the
> Markdown in the repo root (`README.md`, `COMPAT.md`, `MIGRATION.md`,
> `BETA.md`, `SECURITY.md`, `NON_GOALS.md`). The website is a UI on top
> of those.

---

## What's in the box

| Path                | What it is                                            |
|---------------------|-------------------------------------------------------|
| `#/`                | Home — hero + 60-second pitch + three command grids   |
| `#/why`             | Where rustygit beats `git` (eight reasons, in detail) |
| `#/watch-out`       | **The help page** — eight footgun categories          |
| `#/compatibility`   | Full subcommand tier table, sortable + filterable     |
| `#/install`         | Five-minute install + migrate, tabbed                 |
| `#/security`        | Mirrored `SECURITY.md` (reporting flow, threat model) |

Routing is hash-based (`#/foo`), so the site works on any static host
without server rewrites. Anchors inside a page deep-link with a second
hash: `#/watch-out#data-loss`.

## Prerequisites

- **Node** 18.18 or newer (20 LTS in CI).
- **npm** (lockfile + `npm ci` in CI; any equivalent works locally —
  pnpm / yarn / bun).

That's it. No global tools, no `gh` CLI, no Rust toolchain.

## First-time setup

```sh
cd website
npm install
```

`npm install` is fine the first time; CI uses `npm ci` against the
lockfile. The first build commits the lockfile if it doesn't exist yet.

## Day-to-day commands

| Command            | What it does                                              |
|--------------------|-----------------------------------------------------------|
| `npm run dev`      | Vite dev server with HMR at <http://localhost:5173>       |
| `npm run build`    | Production build → `dist/` (minified, hashed assets)      |
| `npm run preview`  | Serve the production build locally at <http://localhost:4173> |
| `npm run check:html` | Smoke-test that `dist/index.html` looks sane (post-build) |

### Develop

```sh
npm run dev
```

Then open the URL it prints. Edits to anything in `src/`, including
`styles.css`, hot-reload without a page refresh.

### Build for production

```sh
npm run build
```

Produces `dist/` with:

- `index.html` — bundled meta tags, hashed asset links.
- `assets/*.js` and `assets/*.css` — minified, source-map-free,
  cache-friendly hashes.
- `favicon.svg`, `og-image.svg`, `robots.txt`, `sitemap.xml` — pass through
  from `public/`.

### Preview the production build locally

```sh
npm run preview
```

This is what catches anything that worked in `dev` but breaks under
production minification (e.g. `process.env` references, dead-code
elimination over a React conditional).

## Deploy

### Path 1 — GitHub Pages (default, automatic)

`.github/workflows/website.yml` builds and deploys on every push to
`main` that touches `website/**`. To enable:

1. Go to the repo's **Settings → Pages**.
2. Under **Source**, pick **GitHub Actions**.
3. The next push to `main` (or **Run workflow** in the Actions tab) will
   publish to `https://<user>.github.io/rustygit/`.

The workflow sets `VITE_BASE=/rustygit/` so all asset URLs resolve under
that subpath. If your fork lives at a different URL, change `VITE_BASE`
in the workflow.

### Path 2 — Any other static host

```sh
npm run build
# Upload everything inside dist/ to your host of choice.
```

Cloudflare Pages, Netlify, S3+CloudFront, nginx, Caddy, your homelab
ingress — anything that serves static files and respects `index.html`
fallback works. **Don't gzip-recompress** `dist/` before upload; Vite
already produces optimal-size assets.

If you serve from a non-root path (e.g. `/rustygit/`), set
`VITE_BASE=/rustygit/` at build time:

```sh
VITE_BASE=/rustygit/ npm run build
```

The site uses **hash routing** (`#/watch-out`, not `/watch-out`), so you
do **not** need to configure SPA fallbacks. The same `index.html` is
served for any URL the user lands on.

## Customising

### Update build metadata (version, commit, test count)

Edit `src/data.js` → `BUILD_META`:

```js
export const BUILD_META = {
  version: "v0.1.0-beta.1",
  testsPassing: 941,
  testsTotal: 941,
  updated: "2026-05-19",
  sha: "4f1c8a2",
  // ...
};
```

These values are referenced by the hero status pill, the footer, and the
hero terminal demo. Bump them at every rustygit release.

### Update the compatibility table

Edit `src/data.js` → `COMPAT_ROWS`. Each row is `{ cmd, tier, notes }`
where `tier` is one of `T1`, `T2`, `T3`, `OUT`. The table on
`/compatibility` rebuilds automatically. Tier counts in the legend are
derived.

### Update "what's out of scope"

Two places — keep them in sync:

- `src/data.js` → `OUT_OF_SCOPE` — bulleted list on `/compatibility`.
- `src/pages/WatchOut.jsx` — the "Big Six" section, rendered as cards.

The site has one canonical list per audience: tooling authors get the
reference framing (`/compatibility`), users get the warning framing
(`/watch-out`).

### Theme tokens

`src/styles.css` is split into dark + light token blocks (`oklch`-based).
Search for `:root[data-theme="dark"]` to find them. The theme toggle in
the nav flips a `data-theme` attribute on `<html>` — no JS-driven style
changes, just CSS variables.

The accent colour token (`--accent`) is amber by default. To rebrand:

```css
:root, :root[data-accent="amber"] {
  --accent:      oklch(0.74 0.16 50);
  --accent-soft: oklch(0.32 0.10 50);
  --accent-on:   oklch(0.16 0.005 80);
}
```

### Fonts

Loaded from Google Fonts via `index.html` (`<link>` + preconnect). To
self-host:

1. Download the WOFF2 files from <https://fonts.google.com>.
2. Drop them in `public/fonts/`.
3. Replace the `<link>` in `index.html` with `@font-face` declarations
   added to the top of `src/styles.css`.

Two families are used: **Geist** (display sans), **JetBrains Mono**
(monospace). Either can be swapped — the layout doesn't depend on
particular font metrics.

## Project layout

```
website/
├── index.html                   # Vite entry, meta tags, noscript fallback
├── package.json                 # npm scripts + deps
├── vite.config.js               # base path, chunking, target
├── public/                      # passed through verbatim into dist/
│   ├── favicon.svg
│   ├── og-image.svg
│   ├── robots.txt
│   └── sitemap.xml
├── scripts/
│   └── check-html.mjs           # post-build smoke test
└── src/
    ├── main.jsx                 # React entry
    ├── App.jsx                  # hash router + theme
    ├── styles.css               # all CSS, tokenised
    ├── data.js                  # COMPAT_ROWS + BUILD_META + content
    ├── components/
    │   ├── Nav.jsx
    │   ├── Footer.jsx
    │   ├── TierBadge.jsx
    │   ├── StatusPill.jsx
    │   ├── CodeBlock.jsx
    │   ├── Callout.jsx
    │   ├── Section.jsx
    │   ├── Card.jsx
    │   ├── CmdGrid.jsx
    │   ├── AdviceStrip.jsx
    │   └── Tabs.jsx
    └── pages/
        ├── Home.jsx
        ├── Why.jsx
        ├── WatchOut.jsx
        ├── Compatibility.jsx
        ├── Install.jsx
        ├── Security.jsx
        └── NotFound.jsx
```

## Where the content comes from

Every page maps to one or more files in the repo root. If you change one
of those, update the corresponding page so they stay in sync.

| Page              | Sources                                                    |
|-------------------|------------------------------------------------------------|
| Home              | `README.md`                                                |
| Why               | `MIGRATION.md` § "Why switch?"                             |
| Watch out         | `MIGRATION.md`, `BETA.md`, `SECURITY.md`, `NON_GOALS.md`, `COMPAT.md` § "Output divergences" |
| Compatibility     | `COMPAT.md` (the whole file)                               |
| Install & migrate | `README.md`, `MIGRATION.md`, `BETA.md` § "Acknowledging beta" |
| Security          | `SECURITY.md`                                              |

The original design pack — wireframes, sample screenshots, source JSX
before the Vite port — lives in `docs/design/`. The brief that drove the
design lives at `docs/WEBSITE-BRIEF.md`.

## Accessibility

- **Keyboard navigation**: every interactive element is focusable and
  has a visible focus ring (added in `styles.css` under `:focus-visible`).
- **Reduced motion**: `prefers-reduced-motion: reduce` flattens scroll
  behaviour and transitions.
- **Colour contrast**: dark + light themes both pass WCAG AA at body
  copy size. Tier badges (`T1`/`T2`/`T3`/`OUT`) use both colour and a
  text label, so colour-blind users don't lose information.
- **Screen readers**: TOC, table headers, callouts, and the burger
  menu all have ARIA roles + labels.
- **No JS** fallback: `<noscript>` in `index.html` directs readers to
  the repo Markdown.

## Anchors used elsewhere

These deep links exist and are quoted from rustygit's binary error
messages — **don't rename them without finding the matching string
in the rustygit source**:

| Anchor                                       | Used by                                            |
|----------------------------------------------|----------------------------------------------------|
| `#/watch-out#beta-status`                    | beta banner suggestion                              |
| `#/watch-out#refused-by-design`              | `doctor --import-config` output                    |
| `#/watch-out#data-loss`                      | `SECURITY.md`, the security advisory CTA           |
| `#/compatibility#out-of-scope`               | `explain_unsupported_subcommand()` error text      |

## Bumping dependencies

```sh
npm outdated
npm update
npm run build && npm run preview   # sanity check
```

Pin Vite + plugin-react to the same minor in `package.json` — they're
the only dev deps. React itself we keep on 18 until 19 ships a stable
LTS.

## Troubleshooting

**"npm: command not found"** — install Node from <https://nodejs.org/>
or your distro's package manager. The site needs Node 18.18+.

**"Cannot find module 'react'"** after `npm install`** — your
`node_modules/` got into a weird state. Nuke + retry:

```sh
rm -rf node_modules package-lock.json
npm install
```

**Theme stuck after toggle** — open DevTools → Application →
Local Storage → clear `rustygit-theme`. Bug: file an issue.

**Assets 404 on GitHub Pages** — your fork's repo isn't `rustygit`, so
`VITE_BASE=/rustygit/` in `.github/workflows/website.yml` doesn't match.
Change the env var to `/<your-repo-name>/`.

**`npm run build` fails with "ERR_REQUIRE_ESM"** — you're on Node 16 or
earlier. Upgrade to 18.18+.

## Reporting bugs in the website itself

- **Content wrong / out of date**: open a PR against the source Markdown
  in the repo root, then update the matching page in `src/pages/`.
- **Visual / interaction bug**: open an issue at
  <https://github.com/bsadashi/rustygit/issues> with a screenshot.
- **Security-relevant**: same disclosure channel as rustygit itself —
  see <https://github.com/bsadashi/rustygit/security/advisories/new>.

## Licence

Same as rustygit — Apache-2.0 or MIT at your option.
