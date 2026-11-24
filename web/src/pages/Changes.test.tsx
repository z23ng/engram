import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { renderWithApp } from "@/test/render";
import { CommitDialog } from "./Changes";

describe("CommitDialog", () => {
  it("commits the current branch with the typed message", async () => {
    const commit = vi.fn(async () => ({ commit_id: "c0ffee0000" }));
    const onOpenChange = vi.fn();
    renderWithApp(<CommitDialog open onOpenChange={onOpenChange} count={2} />, {
      route: { project: "alpha", view: "changes", branch: "draft/pm" },
      client: { commit },
    });
    expect(screen.getByRole("button", { name: "Commit" })).toBeDisabled();
    await userEvent.type(screen.getByLabelText("Message"), "save notes");
    await userEvent.click(screen.getByRole("button", { name: "Commit" }));
    expect(commit).toHaveBeenCalledWith("alpha", "draft/pm", "save notes");
    await vi.waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
  });
});
