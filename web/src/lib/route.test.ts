import { describe, expect, it } from "vitest";
import { formatRoute, parseRoute } from "./route";

describe("routes", () => {
  it("round-trips project, view, branch and file", () => {
    const route = { project: "alpha", view: "files" as const, branch: "draft/x", file: "/semantic/a b.md" };
    const url = formatRoute(route);
    expect(url).toBe("/projects/alpha/files?branch=draft%2Fx&file=%2Fsemantic%2Fa+b.md");
    const [path, search] = url.split("?");
    expect(parseRoute(path, `?${search}`)).toEqual(route);
  });

  it("defaults to main and the files view, and rejects unknown views", () => {
    expect(parseRoute("/projects/alpha", "")).toEqual({ project: "alpha", view: "files", branch: "main" });
    expect(parseRoute("/projects/alpha/graph", "?branch=main")).toEqual({
      project: "alpha",
      view: "graph",
      branch: "main",
    });
    expect(parseRoute("/projects/alpha/nope", "").view).toBe("files");
    expect(parseRoute("/", "")).toEqual({ view: "projects", branch: "main" });
    expect(parseRoute("/keys", "").view).toBe("keys");
  });
});
