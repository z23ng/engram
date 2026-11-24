import { useCallback, useEffect, useMemo, useState } from "react";

export const PROJECT_VIEWS = ["files", "changes", "history", "branches", "search", "graph", "audit"] as const;
export type ProjectView = (typeof PROJECT_VIEWS)[number];
export type View = ProjectView | "projects" | "keys";
export type Route = { project?: string; view: View; branch: string; file?: string };

export function parseRoute(pathname: string, search: string): Route {
  const q = new URLSearchParams(search);
  const branch = q.get("branch") || "main";
  const parts = pathname.split("/").filter(Boolean).map(decodeURIComponent);
  if (parts[0] === "projects" && parts[1]) {
    const known = (PROJECT_VIEWS as readonly string[]).includes(parts[2]);
    const route: Route = { project: parts[1], view: known ? (parts[2] as ProjectView) : "files", branch };
    const file = q.get("file");
    if (file) route.file = file;
    return route;
  }
  return { view: parts[0] === "keys" ? "keys" : "projects", branch };
}

export function formatRoute(r: Route): string {
  const q = new URLSearchParams();
  if (r.branch && r.branch !== "main") q.set("branch", r.branch);
  if (r.file) q.set("file", r.file);
  const path = r.project ? `/projects/${encodeURIComponent(r.project)}/${r.view}` : r.view === "keys" ? "/keys" : "/";
  const qs = q.toString();
  return qs ? `${path}?${qs}` : path;
}

/** Minimal pushState router: the URL is the only route state. */
export function useRoute(): [Route, (r: Route) => void] {
  const [url, setUrl] = useState(() => location.pathname + location.search);
  useEffect(() => {
    const onPop = () => setUrl(location.pathname + location.search);
    addEventListener("popstate", onPop);
    return () => removeEventListener("popstate", onPop);
  }, []);
  const route = useMemo(() => {
    const [path, search = ""] = url.split("?");
    return parseRoute(path, search ? `?${search}` : "");
  }, [url]);
  const go = useCallback((r: Route) => {
    const next = formatRoute(r);
    history.pushState(null, "", next);
    setUrl(next);
  }, []);
  return [route, go];
}
