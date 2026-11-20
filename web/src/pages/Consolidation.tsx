import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Sparkles } from "lucide-react";
import { toast } from "sonner";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Table, TD, TH, THead, TR } from "@/components/ui/table";
import type { ConsolidationRun } from "@/lib/api";
import { errorText, useApp } from "@/lib/app";

const SCHEDULES = [
  { label: "Off", value: "" },
  { label: "Every 6 h", value: "6" },
  { label: "Every 24 h", value: "24" },
  { label: "Every 7 d", value: "168" },
];

const TONE = { running: "accent", succeeded: "success", no_changes: "neutral", skipped: "neutral", failed: "danger" } as const;

/** Admin-only: start an LLM consolidation run, set the schedule, list recent runs (S6). */
export function ConsolidationPanel() {
  const { client, route, go } = useApp();
  const project = route.project!;
  const queries = useQueryClient();
  const runs = useQuery({
    queryKey: ["consolidation", project],
    queryFn: () => client.consolidationRuns(project),
    refetchInterval: (q) => (q.state.data?.runs.some((r) => r.status === "running") ? 2000 : false),
  });
  const refresh = () => {
    for (const k of ["consolidation", "branches"]) queries.invalidateQueries({ queryKey: [k, project] });
  };
  const start = useMutation({
    mutationFn: () => client.consolidate(project),
    onSuccess: (r) => {
      refresh();
      toast.success(`Consolidation run ${r.run_id} started`, { description: "The result appears as a consolidate/* branch to review." });
    },
    onError: (e) => toast.error("Consolidation failed to start", { description: errorText(e) }),
  });
  const schedule = useMutation({
    mutationFn: (every: number | null) => client.setConsolidateSchedule(project, every),
    onSuccess: refresh,
    onError: (e) => toast.error("Schedule not saved", { description: errorText(e) }),
  });
  const every = runs.data?.every_hours;

  return (
    <section className="mt-8">
      <div className="mb-3 flex items-end justify-between gap-4">
        <div>
          <h2 className="text-sm font-semibold">Consolidation</h2>
          <p className="mt-0.5 text-xs text-muted-foreground">
            An LLM turns new /episodic/ notes on main into /semantic/ and /procedural/ notes on a consolidate/* branch. Review the branch, then merge it.
          </p>
        </div>
        <div className="flex items-center gap-2">
          <label htmlFor="consolidate-schedule" className="text-xs text-muted-foreground">
            Schedule
          </label>
          <select
            id="consolidate-schedule"
            className="h-7 rounded-md border bg-background px-2 text-xs"
            value={every == null ? "" : String(every)}
            onChange={(e) => schedule.mutate(e.target.value ? Number(e.target.value) : null)}
          >
            {SCHEDULES.map((s) => (
              <option key={s.value} value={s.value}>
                {s.label}
              </option>
            ))}
            {every != null && !SCHEDULES.some((s) => s.value === String(every)) ? <option value={String(every)}>Every {every} h</option> : null}
          </select>
          <Button size="sm" disabled={start.isPending} onClick={() => start.mutate()}>
            <Sparkles /> Consolidate now
          </Button>
        </div>
      </div>
      <Table>
        <THead>
          <tr>
            <TH className="w-40">Started</TH>
            <TH className="w-28">Status</TH>
            <TH>Result</TH>
            <TH className="w-32">By</TH>
          </tr>
        </THead>
        <tbody>
          {(runs.data?.runs ?? []).map((r: ConsolidationRun) => (
            <TR key={r.id}>
              <TD className="font-mono text-xs text-muted-foreground">{new Date(r.started_at_ms).toISOString().replace("T", " ").slice(0, 16)}</TD>
              <TD>
                <Badge tone={TONE[r.status]}>{r.status.replace("_", " ")}</Badge>
              </TD>
              <TD className="text-xs">
                {r.branch ? (
                  <button className="font-mono text-primary hover:underline" onClick={() => go({ project, view: "files", branch: r.branch! })}>
                    {r.branch}
                  </button>
                ) : null}
                {r.error ? <span className="text-destructive">{r.error}</span> : null}
                {r.status !== "failed" && r.status !== "running" ? (
                  <span className="ml-2 text-muted-foreground">
                    {r.files_in} episodic file(s), {r.notes_written} note(s){r.notes_dropped ? `, ${r.notes_dropped} dropped` : ""}
                  </span>
                ) : null}
              </TD>
              <TD className="text-xs text-muted-foreground">{r.requested_by}</TD>
            </TR>
          ))}
        </tbody>
      </Table>
    </section>
  );
}
