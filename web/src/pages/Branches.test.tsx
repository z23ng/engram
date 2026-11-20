import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { ConsolidationRun } from "@/lib/api";
import { renderWithApp } from "@/test/render";
import { Branches } from "./Branches";

const route = { project: "alpha", view: "branches", branch: "main" } as const;
const admin = { name: "root", role: "admin", projects: null } as const;
const branches = vi.fn(async () => [
  { name: "main", head: "a".repeat(64), changes: 0 },
  { name: "consolidate/20261120-0900", head: "b".repeat(64), changes: 0 },
]);
const run: ConsolidationRun = {
  id: 7, project: "alpha", trigger: "manual", requested_by: "root", status: "failed", source_commit: null,
  branch: null, files_in: 2, notes_written: 0, notes_dropped: 0, error: "LLM unreachable", started_at_ms: 1, finished_at_ms: 2, batches_failed: 0, failed_paths: [],
};
const partial: ConsolidationRun = {
  ...run, id: 9, status: "succeeded", branch: "consolidate/20261120-0900", notes_written: 3, batches_failed: 1,
  error: "batch 3 of 4: output cut",
};

describe("Branches consolidation", () => {
  it("lets an admin start a run, set the schedule, and see past runs", async () => {
    const consolidate = vi.fn(async () => ({ run_id: 8 }));
    const setConsolidateSchedule = vi.fn(async () => ({ every_hours: 24 }));
    const consolidationRuns = vi.fn(async () => ({ runs: [run, partial], every_hours: null }));
    renderWithApp(<Branches />, { me: admin, route, client: { branches, consolidate, consolidationRuns, setConsolidateSchedule } });

    expect(await screen.findByText("LLM unreachable")).toBeTruthy();
    expect(screen.getByText(/1 batch\(es\) skipped/)).toBeTruthy();
    expect(screen.getByText("LLM")).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: "Consolidate now" }));
    expect(consolidate).toHaveBeenCalledWith("alpha");
    await userEvent.selectOptions(screen.getByLabelText("Schedule"), "24");
    expect(setConsolidateSchedule).toHaveBeenCalledWith("alpha", 24);
  });

  it("hides consolidation from non-admin keys", async () => {
    renderWithApp(<Branches />, { route, client: { branches } });
    expect(await screen.findByText("LLM")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Consolidate now" })).toBeNull();
  });
});
