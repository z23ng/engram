import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { renderWithApp } from "@/test/render";
import { SearchPage } from "./Search";

const route = { project: "alpha", view: "search", branch: "main" } as const;

describe("SearchPage rebuild index", () => {
  it("lets an admin rebuild the project index", async () => {
    const reindex = vi.fn(async () => ({ files_indexed: 3, chunks: 9, model: "disabled" }));
    renderWithApp(<SearchPage />, { me: { name: "root", role: "admin", projects: null }, route, client: { reindex } });
    await userEvent.click(screen.getByRole("button", { name: "Rebuild index" }));
    expect(reindex).toHaveBeenCalledWith("alpha");
  });

  it("hides the action from non-admin keys", () => {
    renderWithApp(<SearchPage />, { route, client: {} });
    expect(screen.queryByRole("button", { name: "Rebuild index" })).toBeNull();
  });
});

describe("SearchPage index badge", () => {
  it("says when search runs on BM25 only", async () => {
    const indexInfo = vi.fn(async () => ({ model: "disabled", files: 4, files_with_vectors: 0, chunks: 9 }));
    renderWithApp(<SearchPage />, { route, client: { indexInfo } });
    expect(await screen.findByText("BM25 only")).toBeTruthy();
    expect(indexInfo).toHaveBeenCalledWith("alpha");
  });

  it("names the model and vector coverage in hybrid mode", async () => {
    const indexInfo = vi.fn(async () => ({ model: "ollama:bge-m3", files: 4, files_with_vectors: 3, chunks: 9 }));
    renderWithApp(<SearchPage />, { route, client: { indexInfo } });
    expect(await screen.findByText("Hybrid · ollama:bge-m3 · 3/4 files embedded")).toBeTruthy();
  });
});
