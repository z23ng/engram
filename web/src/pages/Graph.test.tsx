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
    { source: "/procedural/wal.md", target: "/semantic/arch.md", kind: "mdlink", label: "Architecture" },
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

    fireEvent.click(screen.getByRole("button", { name: /^procedural/i }));
    expect(screen.queryByText("Architecture")).not.toBeInTheDocument();
    expect(screen.getByText("SQLite WAL")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: /^All/i }));
    fireEvent.click(screen.getByTestId("graph-node-/semantic/arch.md"));
    expect(await screen.findByText("in: 1 · out: 1")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "→ /procedural/wal.md" })).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Open in Files" }));
    expect(app.go).toHaveBeenCalledWith({
      project: "alpha",
      view: "files",
      branch: "main",
      file: "/semantic/arch.md",
    });
  });

  it("keeps selection after browser pointerDown -> pointerUp -> click and Cluster toggles", async () => {
    renderWithApp(<Graph />, {
      route: { project: "alpha", view: "graph", branch: "main" },
      client: { graph: async () => SAMPLE_GRAPH },
    });

    const node = await screen.findByTestId("graph-node-/semantic/arch.md");
    fireEvent.pointerDown(node, { clientX: 100, clientY: 100 });
    fireEvent.pointerUp(node, { clientX: 100, clientY: 100 });
    fireEvent.click(node);

    expect(await screen.findByText("in: 1 · out: 1")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Open in Files" })).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: /Clusters/i }));
    expect(screen.getByText("in: 1 · out: 1")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Open in Files" })).toBeInTheDocument();
  });
});
