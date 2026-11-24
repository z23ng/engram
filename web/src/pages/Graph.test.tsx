import { fireEvent, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { renderWithApp } from "@/test/render";
import { Graph } from "./Graph";

const SAMPLE_GRAPH = {
  branch: "main",
  nodes: [
    {
      id: "/semantic/arch.md",
      path: "/semantic/arch.md",
      title: "Architecture",
      tier: "semantic",
      importance: 0.9,
      tags: ["arch"],
      blob_id: "aaa",
      in_degree: 1,
      out_degree: 1,
    },
    {
      id: "/procedural/wal.md",
      path: "/procedural/wal.md",
      title: "SQLite WAL",
      tier: "procedural",
      importance: 0.8,
      tags: ["sqlite"],
      blob_id: "bbb",
      in_degree: 1,
      out_degree: 1,
    },
  ],
  edges: [
    { source: "/semantic/arch.md", target: "/procedural/wal.md", kind: "wikilink", label: "wal" },
    { source: "/procedural/wal.md", target: "/semantic/arch.md", kind: "wikilink", label: "Architecture" },
  ],
};

describe("Graph", () => {
  it("renders nodes, filters by tier, inspects selection, and navigates to Files", async () => {
    const app = renderWithApp(<Graph />, {
      route: { project: "alpha", view: "graph", branch: "main" },
      client: { graph: async () => SAMPLE_GRAPH },
    });

    expect(await screen.findByText("Architecture")).toBeInTheDocument();
    expect(screen.getByText("SQLite WAL")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "procedural" }));
    expect(screen.queryByText("Architecture")).not.toBeInTheDocument();
    expect(screen.getByText("SQLite WAL")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "All tiers" }));
    fireEvent.click(screen.getByTestId("graph-node-/semantic/arch.md"));
    expect(await screen.findByText("in: 1 · out: 1")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Open in Files" }));
    expect(app.go).toHaveBeenCalledWith({
      project: "alpha",
      view: "files",
      branch: "main",
      file: "/semantic/arch.md",
    });
  });
});
