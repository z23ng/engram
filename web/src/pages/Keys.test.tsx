import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { renderWithApp } from "@/test/render";
import { Keys } from "./Keys";

describe("Keys", () => {
  it("shows a new secret once and not after the dialog is reopened", async () => {
    const createKey = vi.fn(async () => ({
      key: { id: 7, name: "ci-bot", role: "dev" as const, projects: null, expires_at_ms: null, revoked: false },
      secret: "egm_shown_once",
    }));
    renderWithApp(<Keys />, {
      me: { name: "admin", role: "admin", projects: null },
      client: { keys: async () => [], projects: async () => ["alpha"], createKey },
    });
    await userEvent.click(screen.getByRole("button", { name: "Create key" }));
    const dialog = screen.getByRole("dialog");
    await userEvent.type(within(dialog).getByLabelText("Name"), "ci-bot");
    await userEvent.click(within(dialog).getByRole("button", { name: "Create key" }));
    expect(createKey).toHaveBeenCalledWith({ name: "ci-bot", role: "dev", projects: null, expires_days: 90 });
    expect(await screen.findByText("egm_shown_once")).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: "Done" }));
    await userEvent.click(screen.getByRole("button", { name: "Create key" }));
    expect(within(screen.getByRole("dialog")).getByLabelText("Name")).toHaveValue("");
    expect(screen.queryByText("egm_shown_once")).not.toBeInTheDocument();
  });
});
