// app.jsx — top-level router + theme + tweaks wiring for the rustygit site.

const TWEAK_DEFAULTS = /*EDITMODE-BEGIN*/{
  "theme": "dark",
  "accent": "amber",
  "monoFont": "JetBrains Mono",
  "density": "regular",
  "showHeroTerminal": true
}/*EDITMODE-END*/;

// hash route: #/, #/why, #/watch-out#beta-status, etc.
function parseHash() {
  const h = window.location.hash || "#/";
  // strip leading "#"
  let path = h.replace(/^#/, "");
  // an inner "#anchor" after the path
  let anchor = "";
  const idx = path.indexOf("#");
  if (idx > -1) {
    anchor = path.slice(idx + 1);
    path = path.slice(0, idx);
  }
  if (!path.startsWith("/")) path = "/" + path;
  return { path, anchor };
}

function App() {
  const [{ path, anchor }, setRoute] = React.useState(parseHash());
  const [t, setTweak] = useTweaks(TWEAK_DEFAULTS);

  // ── routing ─────────────────────────────────────────────────────
  React.useEffect(() => {
    const onHash = () => setRoute(parseHash());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  React.useEffect(() => {
    // scroll on route change, but honour anchors on watch-out
    if (anchor) {
      // give the page a tick to mount, then jump
      setTimeout(() => {
        const el = document.getElementById(anchor);
        if (el) {
          const y = el.getBoundingClientRect().top + window.scrollY - 90;
          window.scrollTo({ top: y, behavior: "auto" });
        } else {
          window.scrollTo({ top: 0, behavior: "auto" });
        }
      }, 60);
    } else {
      window.scrollTo({ top: 0, behavior: "auto" });
    }
  }, [path, anchor]);

  const navigate = React.useCallback((to) => {
    window.location.hash = to === "/" ? "/" : to;
  }, []);

  // ── theme ───────────────────────────────────────────────────────
  React.useEffect(() => {
    document.documentElement.dataset.theme = t.theme;
    document.documentElement.dataset.accent = t.accent;
    document.documentElement.dataset.density = t.density;
    document.documentElement.dataset.heroTerm = t.showHeroTerminal ? "on" : "off";
    document.documentElement.style.setProperty("--font-mono", `"${t.monoFont}", ui-monospace, SFMono-Regular, Menlo, monospace`);
  }, [t.theme, t.accent, t.density, t.monoFont, t.showHeroTerminal]);

  const onThemeToggle = () => setTweak("theme", t.theme === "dark" ? "light" : "dark");

  // ── page resolution ─────────────────────────────────────────────
  let page;
  let screenLabel;
  if (path === "/" || path === "") {
    page = <HomePage navigate={navigate} />;
    screenLabel = "01 Home";
  } else if (path.startsWith("/why")) {
    page = <WhyPage navigate={navigate} />;
    screenLabel = "02 Why";
  } else if (path.startsWith("/watch-out")) {
    page = <WatchOutPage navigate={navigate} />;
    screenLabel = "03 Watch out";
  } else if (path.startsWith("/compatibility")) {
    page = <CompatibilityPage navigate={navigate} />;
    screenLabel = "04 Compatibility";
  } else if (path.startsWith("/install")) {
    page = <InstallPage navigate={navigate} />;
    screenLabel = "05 Install";
  } else {
    page = (
      <main className="page page-404">
        <Section
          eyebrow="404"
          title="Page not found"
          lede="That route doesn't exist on this site. Try one of the five below."
        >
          <div className="next-grid">
            {[
              ["/", "Home"], ["/why", "Why rustygit"], ["/watch-out", "Watch out"],
              ["/compatibility", "Compatibility"], ["/install", "Install & migrate"],
            ].map(([to, label]) => (
              <a key={to} className="next" href={`#${to}`} onClick={(e) => { e.preventDefault(); navigate(to); }}>
                <div className="next-eyebrow mono">{to}</div>
                <div className="next-title">{label}</div>
                <div className="next-arrow mono">→</div>
              </a>
            ))}
          </div>
        </Section>
      </main>
    );
    screenLabel = "404";
  }

  return (
    <div className="app" data-screen-label={screenLabel}>
      <Nav route={path || "/"} navigate={navigate} theme={t.theme} onThemeToggle={onThemeToggle} />
      <div className="app-body">{page}</div>
      <Footer navigate={navigate} />

      <TweaksPanel title="Tweaks">
        <TweakSection label="Theme" />
        <TweakRadio
          label="Mode"
          value={t.theme}
          options={["dark", "light"]}
          onChange={(v) => setTweak("theme", v)}
        />
        <TweakRadio
          label="Accent"
          value={t.accent}
          options={["amber", "rust", "lime", "violet"]}
          onChange={(v) => setTweak("accent", v)}
        />
        <TweakSection label="Type" />
        <TweakSelect
          label="Mono"
          value={t.monoFont}
          options={["JetBrains Mono", "IBM Plex Mono", "Fira Code", "ui-monospace"]}
          onChange={(v) => setTweak("monoFont", v)}
        />
        <TweakRadio
          label="Density"
          value={t.density}
          options={["compact", "regular", "comfy"]}
          onChange={(v) => setTweak("density", v)}
        />
        <TweakSection label="Home" />
        <TweakToggle
          label="Hero terminal"
          value={t.showHeroTerminal}
          onChange={(v) => setTweak("showHeroTerminal", v)}
        />
      </TweaksPanel>
    </div>
  );
}

ReactDOM.createRoot(document.getElementById("root")).render(<App />);
