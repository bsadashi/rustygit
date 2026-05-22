// scripts/check-html.mjs
//
// Post-build smoke test. Asserts that dist/index.html is structurally sane —
// hashed JS bundle linked, CSS bundled, meta tags present, public assets
// copied over. Runs in <50 ms; meant to catch the everyday mistakes a Vite
// upgrade or a wrong --base flag would surface.
//
// Usage: `npm run check:html` (assumes `npm run build` already ran).

import { readFile, stat } from "node:fs/promises";
import { existsSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const root = resolve(dirname(__filename), "..");
const dist = resolve(root, "dist");

let failures = 0;
const check = (label, ok, hint) => {
  if (ok) {
    console.log(`  ok    ${label}`);
  } else {
    console.error(`  FAIL  ${label}${hint ? `\n        ${hint}` : ""}`);
    failures++;
  }
};

console.log(`smoke-test ${dist}`);

if (!existsSync(dist)) {
  console.error(`error: dist/ not found. Run \`npm run build\` first.`);
  process.exit(2);
}

const html = await readFile(resolve(dist, "index.html"), "utf8");

check("index.html exists and is non-empty", html.length > 500, `actual size ${html.length}`);
check("page title set", /<title>rustygit — git/.test(html));
check("description meta set", /<meta name="description"/.test(html));
check("Open Graph title set", /<meta property="og:title"/.test(html));
check("favicon link present", /<link rel="icon"/.test(html));
check("hashed JS bundle linked", /<script[^>]+src="[^"]*\/assets\/[^"]*\.js"/.test(html));
check("hashed CSS bundle linked", /<link[^>]+href="[^"]*\/assets\/[^"]*\.css"/.test(html));
check("noscript fallback present", /<noscript>/.test(html));
check("root container present", /<div id="root">/.test(html));

const expectAsset = async (rel) => {
  try {
    const s = await stat(resolve(dist, rel));
    check(`asset ${rel} exists (${s.size} bytes)`, s.size > 0);
  } catch {
    check(`asset ${rel} exists`, false, `missing: ${rel}`);
  }
};

await expectAsset("favicon.svg");
await expectAsset("og-image.svg");
await expectAsset("robots.txt");
await expectAsset("sitemap.xml");

console.log("");
if (failures === 0) {
  console.log("PASS · dist/ looks sane");
  process.exit(0);
} else {
  console.error(`FAIL · ${failures} check(s) failed`);
  process.exit(1);
}
