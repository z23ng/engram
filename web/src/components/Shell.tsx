import { useQuery } from "@tanstack/react-query";
import {
  Boxes,
  ChevronDown,
  Clock,
  Copy,
  Database,
  FileDiff,
  FolderTree,
  GitBranch,
  KeyRound,
  LogOut,
  Moon,
  Network,
  Plug,
  ScrollText,
  Search,
  Sun,
} from "lucide-react";
import { lazy, Suspense, useEffect, useState, type ReactNode } from "react";
import { toast } from "sonner";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { Menu, MenuContent, MenuItem, MenuLabel, MenuSeparator, MenuTrigger } from "@/components/ui/menu";
import { can, davBranch, useApp } from "@/lib/app";
import type { View } from "@/lib/route";
import { cn } from "@/lib/utils";
import { Audit } from "@/pages/Audit";
import { Branches } from "@/pages/Branches";
import { Changes } from "@/pages/Changes";
import { History } from "@/pages/History";
import { Keys } from "@/pages/Keys";
import { Projects } from "@/pages/Projects";
import { SearchPage } from "@/pages/Search";
import { Palette } from "./Palette";

// CodeMirror and the Markdown renderer are most of the bundle; load them with the Files view.
const Files = lazy(() => import("@/pages/Files").then((m) => ({ default: m.Files })));
const Graph = lazy(() => import("@/pages/Graph").then((m) => ({ default: m.Graph })));

function NavItem({ icon, label, active, count, onClick }: {
  icon: ReactNode;
  label: string;
  active: boolean;
  count?: number;
  onClick: () => void;
}) {
  return (
    <button
      onClick={onClick}
      className={cn(
        "flex h-8 w-full items-center gap-2.5 rounded-md px-2.5 text-[13px] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground [&_svg]:size-4",
        active && "bg-accent font-medium text-foreground",
      )}
    >
      {icon}
      <span className="flex-1 text-left">{label}</span>
      {count ? <Badge tone="accent">{count}</Badge> : null}
    </button>
  );
}

export function Shell() {
  const { client, me, route, go, theme, setTheme, signOut } = useApp();
  const perms = can(me);
  const [palette, setPalette] = useState(false);
  const [connect, setConnect] = useState(false);
  const project = route.project;
  const projects = useQuery({ queryKey: ["projects"], queryFn: client.projects });
  const branches = useQuery({
    queryKey: ["branches", project],
    queryFn: () => client.branches(project!),
    enabled: !!project,
  });
  const current = branches.data?.find((b) => b.name === route.branch);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setPalette((v) => !v);
      }
    };
    addEventListener("keydown", onKey);
    return () => removeEventListener("keydown", onKey);
  }, []);

  const view = (v: View) => go({ project, view: v, branch: route.branch });
  const projectNav: [View, string, ReactNode, number?][] = [
    ["files", "Files", <FolderTree />],
    ["changes", "Changes", <FileDiff />, current?.changes],
    ["history", "History", <Clock />],
    ["branches", "Branches", <GitBranch />],
    ["search", "Search", <Search />],
    ["graph", "Graph", <Network />],
  ];
  if (perms.manage) projectNav.push(["audit", "Audit log", <ScrollText />]);

  const page = (() => {
    if (route.view === "keys") return <Keys />;
    if (!project || route.view === "projects") return <Projects />;
    switch (route.view) {
      case "changes":
        return <Changes />;
      case "history":
        return <History />;
      case "branches":
        return <Branches />;
      case "search":
        return <SearchPage />;
      case "graph":
        return <Graph />;
      case "audit":
        return <Audit />;
      default:
        return <Files />;
    }
  })();

  return (
    <div className="flex h-full">
      <aside className="flex w-56 shrink-0 flex-col border-r bg-sidebar">
        <div className="flex h-12 items-center gap-2 border-b px-4">
          <div className="grid size-6 place-items-center rounded bg-primary text-primary-foreground">
            <Database className="size-3.5" />
          </div>
          <span className="text-sm font-semibold tracking-tight">Engram</span>
          <Badge className="ml-auto font-mono">console</Badge>
        </div>
        <nav className="flex-1 space-y-4 overflow-y-auto p-2">
          {project ? (
            <div className="space-y-0.5">
              <div className="px-2.5 pb-1 text-[11px] font-medium text-muted-foreground uppercase">Project</div>
              {projectNav.map(([v, label, icon, count]) => (
                <NavItem key={v} icon={icon} label={label} count={count} active={route.view === v} onClick={() => view(v)} />
              ))}
            </div>
          ) : null}
          <div className="space-y-0.5">
            <div className="px-2.5 pb-1 text-[11px] font-medium text-muted-foreground uppercase">Organization</div>
            <NavItem
              icon={<Boxes />}
              label="Projects"
              active={route.view === "projects"}
              onClick={() => go({ view: "projects", branch: "main" })}
            />
            {perms.globalAdmin ? (
              <NavItem
                icon={<KeyRound />}
                label="API keys"
                active={route.view === "keys"}
                onClick={() => go({ view: "keys", branch: "main" })}
              />
            ) : null}
          </div>
        </nav>
        <div className="border-t p-2">
          <Menu>
            <MenuTrigger className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left hover:bg-accent">
              <div className="grid size-7 place-items-center rounded-full bg-muted text-xs font-semibold uppercase">
                {me.name.slice(0, 2)}
              </div>
              <div className="min-w-0 flex-1">
                <div className="truncate text-[13px] font-medium">{me.name}</div>
                <div className="text-[11px] text-muted-foreground">{me.role}</div>
              </div>
            </MenuTrigger>
            <MenuContent side="top">
              <MenuItem onSelect={() => setTheme(theme === "dark" ? "light" : "dark")}>
                {theme === "dark" ? <Sun /> : <Moon />} {theme === "dark" ? "Light theme" : "Dark theme"}
              </MenuItem>
              <MenuSeparator />
              <MenuItem onSelect={signOut} className="text-destructive">
                <LogOut /> Sign out
              </MenuItem>
            </MenuContent>
          </Menu>
        </div>
      </aside>

      <div className="flex min-w-0 flex-1 flex-col">
        <header className="flex h-12 shrink-0 items-center gap-2 border-b px-4">
          <Menu>
            <MenuTrigger asChild>
              <Button variant="ghost" size="sm" className="font-mono">
                <Boxes /> {project ?? "Select project"} <ChevronDown className="text-muted-foreground" />
              </Button>
            </MenuTrigger>
            <MenuContent>
              <MenuLabel>Projects</MenuLabel>
              {(projects.data ?? []).map((p) => (
                <MenuItem key={p} className="font-mono" onSelect={() => go({ project: p, view: "files", branch: "main" })}>
                  {p}
                </MenuItem>
              ))}
            </MenuContent>
          </Menu>
          {project ? (
            <>
              <span className="text-muted-foreground">/</span>
              <Menu>
                <MenuTrigger asChild>
                  <Button variant="ghost" size="sm" className="font-mono">
                    <GitBranch /> {route.branch}
                    {current?.changes ? <Badge tone="warning">{current.changes} staged</Badge> : null}
                    <ChevronDown className="text-muted-foreground" />
                  </Button>
                </MenuTrigger>
                <MenuContent>
                  <MenuLabel>Branches</MenuLabel>
                  {(branches.data ?? []).map((b) => (
                    <MenuItem
                      key={b.name}
                      className="font-mono"
                      onSelect={() => go({ project, view: route.view, branch: b.name })}
                    >
                      <span className="flex-1">{b.name}</span>
                      {b.changes ? <Badge tone="warning">{b.changes}</Badge> : null}
                    </MenuItem>
                  ))}
                </MenuContent>
              </Menu>
            </>
          ) : null}
          <button
            onClick={() => setPalette(true)}
            className="ml-auto flex h-8 w-72 items-center gap-2 rounded-md border bg-muted/40 px-2.5 text-xs text-muted-foreground hover:bg-muted"
          >
            <Search className="size-3.5" />
            <span className="flex-1 text-left">Search files and commands</span>
            <kbd className="rounded border bg-background px-1 font-mono text-[10px]">Ctrl K</kbd>
          </button>
          {project ? (
            <Button variant="outline" size="sm" onClick={() => setConnect(true)}>
              <Plug /> Connect
            </Button>
          ) : null}
        </header>
        <main className="min-h-0 flex-1 overflow-auto">
          <Suspense fallback={<div className="p-6 text-xs text-muted-foreground">Loading editor...</div>}>{page}</Suspense>
        </main>
      </div>

      <Palette open={palette} onOpenChange={setPalette} />
      {project ? (
        <Dialog open={connect} onOpenChange={setConnect}>
          <ConnectDialog project={project} branch={route.branch} />
        </Dialog>
      ) : null}
    </div>
  );
}

function CopyRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="space-y-1">
      <div className="text-xs font-medium text-muted-foreground">{label}</div>
      <div className="flex items-center gap-1 rounded-md border bg-muted/40 py-1 pr-1 pl-2.5">
        <code className="flex-1 truncate font-mono text-xs">{value}</code>
        <Button
          variant="ghost"
          size="iconSm"
          aria-label={`Copy ${label}`}
          onClick={() => navigator.clipboard.writeText(value).then(() => toast.success("Copied"))}
        >
          <Copy />
        </Button>
      </div>
    </div>
  );
}

function ConnectDialog({ project, branch }: { project: string; branch: string }) {
  const origin = location.origin;
  return (
    <DialogContent
      title="Connect clients"
      description="Every client authenticates with an API key. The key role decides read or write access."
      className="w-[min(92vw,36rem)]"
    >
      <CopyRow label="MCP endpoint (Streamable HTTP, header Authorization: Bearer <key>)" value={`${origin}/p/${project}/mcp`} />
      <CopyRow
        label="WebDAV vault for Obsidian Remotely Save (any username, password = API key)"
        value={`${origin}/p/${project}/dav/${davBranch(branch)}/`}
      />
      <CopyRow label="REST API base" value={`${origin}/api/v1/projects/${project}`} />
      <p className="text-[11px] text-muted-foreground">
        WebDAV changes are committed automatically after about 10 seconds without new changes.
      </p>
    </DialogContent>
  );
}
