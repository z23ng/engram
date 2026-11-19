import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Boxes, Plus } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { Empty, PageHeader } from "@/components/ui/empty";
import { Input, Label } from "@/components/ui/input";
import { can, errorText, useApp } from "@/lib/app";

export function Projects() {
  const { client, me, go } = useApp();
  const queries = useQueryClient();
  const [open, setOpen] = useState(false);
  const [slug, setSlug] = useState("");
  const projects = useQuery({ queryKey: ["projects"], queryFn: client.projects });
  const create = useMutation({
    mutationFn: () => client.createProject(slug),
    onSuccess: () => {
      setOpen(false);
      queries.invalidateQueries({ queryKey: ["projects"] });
      go({ project: slug, view: "files", branch: "main" });
      setSlug("");
    },
    onError: (e) => toast.error("Create failed", { description: errorText(e) }),
  });
  const valid = /^[a-z0-9][a-z0-9-]{0,62}$/.test(slug);
  return (
    <div className="mx-auto max-w-5xl p-6">
      <PageHeader
        title="Projects"
        description="Each project is one versioned memory repository with its own branches."
        actions={
          can(me).globalAdmin ? (
            <Button onClick={() => setOpen(true)}>
              <Plus /> New project
            </Button>
          ) : null
        }
      />
      {projects.data?.length === 0 ? <Empty title="No projects">Ask an admin to create one.</Empty> : null}
      <div className="grid grid-cols-[repeat(auto-fill,minmax(15rem,1fr))] gap-3">
        {(projects.data ?? []).map((p) => (
          <button
            key={p}
            onClick={() => go({ project: p, view: "files", branch: "main" })}
            className="flex items-center gap-3 rounded-lg border bg-card p-4 text-left transition-colors hover:border-primary/50"
          >
            <div className="grid size-8 place-items-center rounded-md bg-muted">
              <Boxes className="size-4 text-muted-foreground" />
            </div>
            <div>
              <div className="font-mono text-[13px] font-medium">{p}</div>
              <div className="text-[11px] text-muted-foreground">Open files on main</div>
            </div>
          </button>
        ))}
      </div>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent title="New project" description="Lowercase letters, digits, and '-'. The slug is part of MCP and WebDAV URLs.">
          <form
            className="space-y-3"
            onSubmit={(e) => {
              e.preventDefault();
              if (valid) create.mutate();
            }}
          >
            <div className="space-y-1.5">
              <Label htmlFor="slug">Slug</Label>
              <Input id="slug" className="font-mono" placeholder="payments-team" value={slug} onChange={(e) => setSlug(e.target.value)} autoFocus />
            </div>
            <div className="flex justify-end">
              <Button type="submit" disabled={!valid || create.isPending}>
                Create
              </Button>
            </div>
          </form>
        </DialogContent>
      </Dialog>
    </div>
  );
}
