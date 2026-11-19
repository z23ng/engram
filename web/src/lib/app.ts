import { createContext, useContext } from "react";
import type { Client, Me } from "./api";
import type { Route } from "./route";

export type Theme = "light" | "dark";

export type AppState = {
  client: Client;
  me: Me;
  route: Route;
  go: (r: Route) => void;
  theme: Theme;
  setTheme: (t: Theme) => void;
  signOut: () => void;
};

export const AppContext = createContext<AppState | null>(null);

export function useApp(): AppState {
  const ctx = useContext(AppContext);
  if (!ctx) throw new Error("useApp outside AppContext");
  return ctx;
}

/** Permissions mirror the server role table; the server still enforces them. */
export const can = (me: Me) => ({
  write: me.role !== "bot",
  manage: me.role === "admin",
  globalAdmin: me.role === "admin" && me.projects === null,
});

/** WebDAV URLs write '/' in branch names as '~'. */
export const davBranch = (branch: string) => branch.replaceAll("/", "~");

export const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));
