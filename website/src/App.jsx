import React, { useCallback, useEffect, useState } from "react";
import { Nav } from "./components/Nav.jsx";
import { Footer } from "./components/Footer.jsx";
import { HomePage } from "./pages/Home.jsx";
import { WhyPage } from "./pages/Why.jsx";
import { WatchOutPage } from "./pages/WatchOut.jsx";
import { CompatibilityPage } from "./pages/Compatibility.jsx";
import { InstallPage } from "./pages/Install.jsx";
import { SecurityPage } from "./pages/Security.jsx";
import { NotFoundPage } from "./pages/NotFound.jsx";

const THEME_KEY = "rustygit-theme";

// Parse #/path#anchor → { path, anchor }
function parseHash() {
  const h = window.location.hash || "#/";
  let path = h.replace(/^#/, "");
  let anchor = "";
  const idx = path.indexOf("#");
  if (idx > -1) {
    anchor = path.slice(idx + 1);
    path = path.slice(0, idx);
  }
  if (!path.startsWith("/")) path = "/" + path;
  return { path, anchor };
}

// Initial theme: persisted choice → OS preference → dark.
function initialTheme() {
  try {
    const saved = localStorage.getItem(THEME_KEY);
    if (saved === "dark" || saved === "light") return saved;
  } catch {
    // localStorage unavailable (sandboxed iframe / disabled storage); fall
    // through to OS preference.
  }
  if (typeof window !== "undefined" && window.matchMedia?.("(prefers-color-scheme: light)").matches) {
    return "light";
  }
  return "dark";
}

export default function App() {
  const [{ path, anchor }, setRoute] = useState(parseHash);
  const [theme, setTheme] = useState(initialTheme);

  // Routing
  useEffect(() => {
    const onHash = () => setRoute(parseHash());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  // Scroll to anchor (or top) on route change.
  useEffect(() => {
    if (anchor) {
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

  const navigate = useCallback((to) => {
    window.location.hash = to === "/" ? "/" : to;
  }, []);

  // Theme → data attribute + persistence.
  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    document.documentElement.dataset.accent = "amber";
    document.documentElement.dataset.density = "regular";
    document.documentElement.dataset.heroTerm = "on";
    try {
      localStorage.setItem(THEME_KEY, theme);
    } catch {
      // Best-effort persistence; quota / sandboxed storage can fail. Theme
      // still applies for the current session via the dataset attribute.
    }
  }, [theme]);

  const onThemeToggle = () => setTheme((t) => (t === "dark" ? "light" : "dark"));

  // Page resolution
  let page;
  if (path === "/" || path === "") {
    page = <HomePage navigate={navigate} />;
  } else if (path.startsWith("/why")) {
    page = <WhyPage navigate={navigate} />;
  } else if (path.startsWith("/watch-out")) {
    page = <WatchOutPage navigate={navigate} />;
  } else if (path.startsWith("/compatibility")) {
    page = <CompatibilityPage navigate={navigate} />;
  } else if (path.startsWith("/install")) {
    page = <InstallPage navigate={navigate} />;
  } else if (path.startsWith("/security")) {
    page = <SecurityPage navigate={navigate} />;
  } else {
    page = <NotFoundPage navigate={navigate} />;
  }

  return (
    <div className="app">
      <Nav route={path || "/"} navigate={navigate} theme={theme} onThemeToggle={onThemeToggle} />
      <div className="app-body">{page}</div>
      <Footer navigate={navigate} />
    </div>
  );
}
