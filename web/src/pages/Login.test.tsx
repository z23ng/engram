import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Login } from "./Login";

afterEach(() => vi.unstubAllGlobals());

function stub(status: number, body: unknown) {
  vi.stubGlobal("fetch", vi.fn(async () => new Response(JSON.stringify(body), { status })));
}

describe("Login", () => {
  it("verifies the key with /me and reports the identity", async () => {
    stub(200, { name: "pm-alice", role: "pm", projects: ["alpha"] });
    const onLogin = vi.fn();
    render(<Login onLogin={onLogin} />);
    await userEvent.type(screen.getByLabelText("API key"), "egm_secret");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    expect(onLogin).toHaveBeenCalledWith("egm_secret", { name: "pm-alice", role: "pm", projects: ["alpha"] });
  });

  it("shows an error for a rejected key", async () => {
    stub(401, { error: "unauthorized" });
    const onLogin = vi.fn();
    render(<Login onLogin={onLogin} />);
    await userEvent.type(screen.getByLabelText("API key"), "egm_bad");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Invalid, expired, or revoked key");
    expect(onLogin).not.toHaveBeenCalled();
  });
});
