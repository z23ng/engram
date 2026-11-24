import { describe, expect, it } from "vitest";
import type { ConsolidationRun } from "@/lib/api";
import { pollInterval } from "./Consolidation";

const base: ConsolidationRun = {
  id: 1, project: "alpha", trigger: "manual", requested_by: "root", status: "running", source_commit: null,
  branch: null, files_in: 0, notes_written: 0, notes_dropped: 0, error: null, started_at_ms: 0, finished_at_ms: null,
  batches_failed: 0, failed_paths: [],
};
const now = 10 * 3_600_000;

describe("pollInterval", () => {
  it("polls while a recent run is running", () => {
    expect(pollInterval([{ ...base, started_at_ms: now - 60_000 }])).toBe(2000);
  });
  it("keeps polling a run longer than one hour (the server clears rows of interrupted runs)", () => {
    expect(pollInterval([{ ...base, started_at_ms: now - 2 * 3_600_000 }])).toBe(2000);
  });
  it("stops when no run is running", () => {
    expect(pollInterval([{ ...base, status: "succeeded", started_at_ms: now }])).toBe(false);
    expect(pollInterval(undefined)).toBe(false);
  });
});
