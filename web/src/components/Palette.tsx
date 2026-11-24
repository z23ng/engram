import { useQuery } from "@tanstack/react-query";
import { Command } from "cmdk";
import { Boxes, FileText, FolderTree, GitBranch, KeyRound, Network, Search } from "lucide-react";
import { Dialog as D } from "radix-ui";
import type { ReactNode } from "react";
import { can, useApp } from "@/lib/app";

const itemClass =
  "flex cursor-default items-center gap-2 rounded px-2 py-1.5 text-[13px] data-[selected=true]:bg-accent [&_svg]:size-3.5 [&_svg]:text-muted-foreground";

export function Palette({ open, onOpenChange }: { open: boolean; onOpenChange: (v: boolean) => void }) {
  const { client, me, route, go } = useApp();
  const project = route.project;
  const files = useQuery({
    queryKey: ["tree", project, route.branch],
    queryFn: () => client.tree(project!, route.branch),
    enabled: open && !!project,
  });
  const projects = useQuery({ queryKey: ["projects"], queryFn: client.projects, enabled: open });
  const run = (f: () => void) => () => {
    onOpenChange(false);
    f();
  };
  const item = (value: string, icon: ReactNode, label: ReactNode, f: () => void) => (
    <Command.Item key={value} value={value} onSelect={run(f)} className={itemClass}>
      {icon}
      {label}
    </Command.Item>
  );

  return (
    <D.Root open={open} onOpenChange={onOpenChange}>
      <D.Portal>
        <D.Overlay className="fixed inset-0 z-50 bg-black/40" />
        <D.Content className="fixed top-[15vh] left-1/2 z-50 w-[min(92vw,36rem)] -translate-x-1/2 overflow-hidden rounded-lg border bg-popover shadow-2xl">
          <D.Title className="sr-only">Command palette</D.Title>
          <D.Description className="sr-only">Jump to a file, view, or project</D.Description>
          <Command loop>
            <div className="flex items-center gap-2 border-b px-3">
              <Search className="size-4 text-muted-foreground" />
              <Command.Input
                autoFocus
                placeholder="Type a file path, view, or project..."
                className="h-11 flex-1 bg-transparent text-sm outline-none placeholder:text-muted-foreground"
              />
            </div>
            <Command.List className="max-h-80 overflow-y-auto p-1.5">
              <Command.Empty className="py-6 text-center text-xs text-muted-foreground">No results</Command.Empty>
              {project ? (
                <Command.Group heading="Views" className="text-[11px] text-muted-foreground [&_[cmdk-group-heading]]:px-2 [&_[cmdk-group-heading]]:py-1">
                  {item("view files", <FolderTree />, "Files", () => go({ project, view: "files", branch: route.branch }))}
                  {item("view changes", <FileText />, "Changes", () => go({ project, view: "changes", branch: route.branch }))}
                  {item("view history", <FileText />, "History", () => go({ project, view: "history", branch: route.branch }))}
                  {item("view branches", <GitBranch />, "Branches", () => go({ project, view: "branches", branch: route.branch }))}
                  {item("view search", <Search />, "Search memory", () => go({ project, view: "search", branch: route.branch }))}
                  {item("view graph", <Network />, "Knowledge graph", () => go({ project, view: "graph", branch: route.branch }))}
                </Command.Group>
              ) : null}
              {project && files.data?.length ? (
                <Command.Group heading="Files" className="text-[11px] text-muted-foreground [&_[cmdk-group-heading]]:px-2 [&_[cmdk-group-heading]]:py-1">
                  {files.data.map((f) =>
                    item(
                      `file ${f.path}`,
                      <FileText />,
                      <span className="font-mono text-xs text-foreground">{f.path}</span>,
                      () => go({ project, view: "files", branch: route.branch, file: f.path }),
                    ),
                  )}
                </Command.Group>
              ) : null}
              <Command.Group heading="Projects" className="text-[11px] text-muted-foreground [&_[cmdk-group-heading]]:px-2 [&_[cmdk-group-heading]]:py-1">
                {(projects.data ?? []).map((p) =>
                  item(`project ${p}`, <Boxes />, <span className="font-mono text-foreground">{p}</span>, () =>
                    go({ project: p, view: "files", branch: "main" }),
                  ),
                )}
                {can(me).globalAdmin ? item("api keys", <KeyRound />, "API keys", () => go({ view: "keys", branch: "main" })) : null}
              </Command.Group>
            </Command.List>
          </Command>
        </D.Content>
      </D.Portal>
    </D.Root>
  );
}
