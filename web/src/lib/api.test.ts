import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError, createClient } from "./api";

function mockFetch(status: number, body: unknown) {
  const fn = vi.fn(async (_url: string, _init?: RequestInit) =>
    new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } }),
  );
  vi.stubGlobal("fetch", fn);
  return fn;
}

afterEach(() => vi.unstubAllGlobals());

describe("api client", () => {
  it("sends the bearer key and encodes branch names in the query", async () => {
    const fetch = mockFetch(200, { files: [] });
    await createClient(() => "egm_k", () => {}).tree("alpha", "draft/x");
    const [url, init] = fetch.mock.calls[0];
    expect(url).toBe("/api/v1/projects/alpha/tree?branch=draft%2Fx");
    expect(new Headers(init?.headers).get("authorization")).toBe("Bearer egm_k");
  });

  it("reports 401 to the session and throws ApiError with the server message", async () => {
    mockFetch(401, { error: "unauthorized" });
    const onUnauthorized = vi.fn();
    const err = await createClient(() => "bad", onUnauthorized).me().catch((e) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect(err.status).toBe(401);
    expect(err.message).toBe("unauthorized");
    expect(onUnauthorized).toHaveBeenCalledOnce();
  });

  it("returns a merge conflict as data instead of throwing", async () => {
    mockFetch(409, { outcome: "conflict", conflicts: [{ path: "/semantic/a.md" }] });
    const result = await createClient(() => "k", () => {}).merge("alpha", "draft", "main");
    expect(result.outcome).toBe("conflict");
  });

  it("writes JSON bodies with content type and method", async () => {
    const fetch = mockFetch(200, { commit_id: "c" });
    await createClient(() => "k", () => {}).commit("alpha", "main", "save");
    const [url, init] = fetch.mock.calls[0];
    expect(url).toBe("/api/v1/projects/alpha/commit");
    expect(init?.method).toBe("POST");
    expect(new Headers(init?.headers).get("content-type")).toBe("application/json");
    expect(JSON.parse(String(init?.body))).toEqual({ branch: "main", message: "save" });
  });
});
