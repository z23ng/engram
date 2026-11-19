import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { Role } from "@/lib/api";
import { renderWithApp } from "@/test/render";
import { Files } from "./Files";

function renderAs(role: Role) {
  renderWithApp(<Files />, {
    me: { name: role, role, projects: ["alpha"] },
    route: { project: "alpha", view: "files", branch: "main", file: "/semantic/a.md" },
    client: {
      tree: async () => [{ path: "/semantic/a.md", blob_id: "b", content_type: "text/markdown", updated_at_ms: 0 }],
      read: async () => ({ path: "/semantic/a.md", blob_id: "b", content: "# Alpha" }),
      status: async () => [],
    },
  });
}

describe("Files", () => {
  it("hides every write action for a bot key", async () => {
    renderAs("bot");
    expect(await screen.findByText("read-only")).toBeInTheDocument();
    for (const name of ["Save", /^Commit/, "Delete file", "New file"]) {
      expect(screen.queryByRole("button", { name })).not.toBeInTheDocument();
    }
  });

  it("offers save, commit, and delete to a pm key", async () => {
    renderAs("pm");
    expect(await screen.findByRole("button", { name: "Save" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /^Commit/ })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Delete file" })).toBeInTheDocument();
    expect(screen.queryByText("read-only")).not.toBeInTheDocument();
  });
});
