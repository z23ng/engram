import { useQueryClient } from "@tanstack/react-query";
import { useCallback, useEffect, useMemo, useState } from "react";
import { Shell } from "./components/Shell";
import { createClient, type Me } from "./lib/api";
import { AppContext, type Theme } from "./lib/app";
import { useRoute } from "./lib/route";
import { Login } from "./pages/Login";

const KEY = "engram.key";
const THEME = "engram.theme";

function readStore(store: Storage, key: string) {
  try {
    return store.getItem(key);
  } catch {
    return null;
  }
}

function writeStore(store: Storage, key: string, value: string | null) {
  try {
    if (value === null) store.removeItem(key);
    else store.setItem(key, value);
  } catch {
    // Storage blocked: the session lasts until reload.
  }
}

function initialTheme(): Theme {
  const saved = readStore(localStorage, THEME);
  if (saved === "light" || saved === "dark") return saved;
  return matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

export function App() {
  const queries = useQueryClient();
  const [key, setKey] = useState(() => readStore(sessionStorage, KEY));
  const [me, setMe] = useState<Me | null>(null);
  const [theme, setThemeState] = useState<Theme>(initialTheme);
  const [route, go] = useRoute();

  const signOut = useCallback(() => {
    writeStore(sessionStorage, KEY, null);
    setKey(null);
    setMe(null);
    queries.clear();
  }, [queries]);

  const client = useMemo(() => createClient(() => key, signOut), [key, signOut]);

  useEffect(() => {
    if (key && !me) client.me().then(setMe, signOut);
  }, [key, me, client, signOut]);

  useEffect(() => {
    document.documentElement.classList.toggle("dark", theme === "dark");
  }, [theme]);

  const setTheme = useCallback((t: Theme) => {
    writeStore(localStorage, THEME, t);
    setThemeState(t);
  }, []);

  if (!key) {
    return (
      <Login
        onLogin={(k, m) => {
          writeStore(sessionStorage, KEY, k);
          setKey(k);
          setMe(m);
        }}
      />
    );
  }
  if (!me) return <div className="grid h-full place-items-center text-xs text-muted-foreground">Loading...</div>;

  return (
    <AppContext.Provider value={{ client, me, route, go, theme, setTheme, signOut }}>
      <Shell />
    </AppContext.Provider>
  );
}
