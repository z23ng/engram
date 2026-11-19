import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { GitBranch, GitMerge, Plus } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { PageHeader } from "@/components/ui/empty";
import { Input, Label } from "@/components/ui/input";
import { Table, TD, TH, THead, TR } from "@/components/ui/table";
import type { MergeResult } from "@/lib/api";
import { can, errorText, useApp } from "@/lib/app";
import { shortId } from "@/lib/utils";

export function Branches() {
  const { client, me, route, go } = useApp();
  const project = route.project!;
  const queries = useQueryClient();
  const allowed = can(me).write;
  const [name, setName] = useState("");
  const [createOpen, setCreateOpen] = useState(false);
  const [conflict, setConflict] = useState<{ source: string; paths: string[] } | null>(null);
  const branches = useQuery({ queryKey: ["branches", project], queryFn: () => client.branches(project) });
  const refresh = () => {
    for (const k of ["branches", "log", "tree", "status"]) queries.invalidateQueries({ queryKey: [k, project] });
  };

  const create = useMutation({
    mutationFn: () => client.createBranch(project, name.trim(), route.branch),
    onSuccess: () => {
      setCreateOpen(false);
      refresh();
      toast.success(`Created ${name.trim()}`, { description: `from ${route.branch}` });
      setName("");
    },
    onError: (e) => toast.error("Create failed", { description: errorText(e) }),
  });
  const merge = useMutation({
    mutationFn: (source: string) => client.merge(project, source, "main"),
    onSuccess: (r: MergeResult, source) => {
      if (r.outcome === "conflict") {
        setConflict({ source, paths: r.conflicts.map((c) => c.path) });
        return;
      }
      refresh();
      toast.success(`Merged ${source} into main`, { description: `${r.outcome} ${shortId(r.commit_id)}` });
    },
    onError: (e) => toast.error("Merge failed", { description: errorText(e) }),
  });

  return (
    <div className="mx-auto max-w-5xl p-6">
      <PageHeader
        title="Branches"
        description="Branches share content; creating one copies nothing. Merging into main needs a dev, pm, or admin key."
        actions={
          allowed ? (
            <Button onClick={() => setCreateOpen(true)}>
              <Plus /> New branch
            </Button>
          ) : null
        }
      />
      <Table>
        <THead>
          <tr>
            <TH>Branch</TH>
            <TH className="w-36">Head</TH>
            <TH className="w-32">Staged</TH>
            <TH className="w-56 text-right">Actions</TH>
          </tr>
        </THead>
        <tbody>
          {(branches.data ?? []).map((b) => (
            <TR key={b.name}>
              <TD>
                <span className="inline-flex items-center gap-1.5 font-mono text-xs">
                  <GitBranch className="size-3.5 text-muted-foreground" />
                  {b.name}
                  {b.name === "main" ? <Badge tone="accent">default</Badge> : null}
                  {b.name === route.branch ? <Badge>current</Badge> : null}
                </span>
              </TD>
              <TD className="font-mono text-xs text-muted-foreground">{shortId(b.head)}</TD>
              <TD>{b.changes ? <Badge tone="warning">{b.changes} staged</Badge> : <span className="text-xs text-muted-foreground">clean</span>}</TD>
              <TD className="space-x-1 text-right">
                <Button size="sm" variant="ghost" onClick={() => go({ project, view: "files", branch: b.name })}>
                  Open
                </Button>
                {allowed && b.name !== "main" ? (
                  <Button size="sm" variant="outline" disabled={merge.isPending} onClick={() => merge.mutate(b.name)}>
                    <GitMerge /> Merge into main
                  </Button>
                ) : null}
              </TD>
            </TR>
          ))}
        </tbody>
      </Table>

      <Dialog open={createOpen} onOpenChange={setCreateOpen}>
        <DialogContent title="New branch" description={`Starts at the head of ${route.branch}.`}>
          <form
            className="space-y-3"
            onSubmit={(e) => {
              e.preventDefault();
              if (name.trim()) create.mutate();
            }}
          >
            <div className="space-y-1.5">
              <Label htmlFor="branch-name">Name</Label>
              <Input id="branch-name" className="font-mono" placeholder="draft/pm-inbox" autoFocus value={name} onChange={(e) => setName(e.target.value)} />
            </div>
            <div className="flex justify-end">
              <Button type="submit" disabled={!name.trim() || name.includes("~") || create.isPending}>
                Create
              </Button>
            </div>
          </form>
        </DialogContent>
      </Dialog>

      <Dialog open={!!conflict} onOpenChange={(v) => !v && setConflict(null)}>
        <DialogContent
          title={`Merge conflict: ${conflict?.source ?? ""} into main`}
          description="Nothing was changed. Edit these files on one branch so they match, commit, and merge again."
        >
          <ul className="space-y-1">
            {conflict?.paths.map((p) => (
              <li key={p} className="font-mono text-xs">
                {p}
              </li>
            ))}
          </ul>
        </DialogContent>
      </Dialog>
    </div>
  );
}
