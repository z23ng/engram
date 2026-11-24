import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { toast } from "sonner";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { Empty, PageHeader } from "@/components/ui/empty";
import { Input, Label } from "@/components/ui/input";
import { Table, TD, TH, THead, TR } from "@/components/ui/table";
import { can, errorText, useApp } from "@/lib/app";
import { shortId } from "@/lib/utils";

/** Commits every staged change of the current branch; the server sets author = key name. */
export function CommitDialog({ open, onOpenChange, count }: { open: boolean; onOpenChange: (v: boolean) => void; count: number }) {
  const { client, me, route } = useApp();
  const project = route.project!;
  const queries = useQueryClient();
  const [message, setMessage] = useState("");
  const commit = useMutation({
    mutationFn: () => client.commit(project, route.branch, message.trim()),
    onSuccess: (r) => {
      onOpenChange(false);
      setMessage("");
      for (const k of ["status", "branches", "log", "tree"]) queries.invalidateQueries({ queryKey: [k, project] });
      toast.success("Committed", { description: shortId(r.commit_id) });
    },
    onError: (e) => toast.error("Commit failed", { description: errorText(e) }),
  });
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        title={`Commit ${count} change(s) to ${route.branch}`}
        description={`Author: ${me.name}. The commit includes every staged change on this branch.`}
      >
        <form
          className="space-y-3"
          onSubmit={(e) => {
            e.preventDefault();
            if (message.trim()) commit.mutate();
          }}
        >
          <div className="space-y-1.5">
            <Label htmlFor="commit-message">Message</Label>
            <Input id="commit-message" autoFocus value={message} onChange={(e) => setMessage(e.target.value)} />
          </div>
          <div className="flex justify-end">
            <Button type="submit" disabled={!message.trim() || commit.isPending}>
              Commit
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

export function Changes() {
  const { client, me, route, go } = useApp();
  const project = route.project!;
  const [open, setOpen] = useState(false);
  const status = useQuery({ queryKey: ["status", project, route.branch], queryFn: () => client.status(project, route.branch) });
  const changes = status.data ?? [];
  return (
    <div className="mx-auto max-w-5xl p-6">
      <PageHeader
        title="Changes"
        description={`Staged workspace changes on ${route.branch}. WebDAV changes commit automatically after about 10 seconds idle.`}
        actions={
          can(me).write ? (
            <Button disabled={!changes.length} onClick={() => setOpen(true)}>
              Commit {changes.length || ""}
            </Button>
          ) : null
        }
      />
      {!changes.length ? (
        <Empty title="Working tree clean">No staged changes on this branch.</Empty>
      ) : (
        <Table>
          <THead>
            <tr>
              <TH className="w-28">Change</TH>
              <TH>Path</TH>
            </tr>
          </THead>
          <tbody>
            {changes.map((c) => (
              <TR key={c.path}>
                <TD>
                  <Badge tone={c.op === "delete" ? "danger" : "success"}>{c.op === "delete" ? "deleted" : "modified"}</Badge>
                </TD>
                <TD>
                  {c.op === "delete" ? (
                    <span className="font-mono text-xs text-muted-foreground line-through">{c.path}</span>
                  ) : (
                    <button
                      className="font-mono text-xs hover:text-primary hover:underline"
                      onClick={() => go({ project, view: "files", branch: route.branch, file: c.path })}
                    >
                      {c.path}
                    </button>
                  )}
                </TD>
              </TR>
            ))}
          </tbody>
        </Table>
      )}
      <CommitDialog open={open} onOpenChange={setOpen} count={changes.length} />
    </div>
  );
}
