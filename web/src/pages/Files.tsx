import { markdown } from "@codemirror/lang-markdown";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import CodeMirror, { EditorView } from "@uiw/react-codemirror";
import { ChevronDown, ChevronRight, Columns2, Eye, FilePen, FileText, Folder, Plus, Save, Trash } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import { Markdown } from "@/components/Markdown";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { Empty } from "@/components/ui/empty";
import { Input, Label } from "@/components/ui/input";
import { can, errorText, useApp } from "@/lib/app";
import { attachmentPath, relativeLink } from "@/lib/links";
import { buildTree, TIERS, type TreeNode } from "@/lib/tree";
import { cn } from "@/lib/utils";
import { CommitDialog } from "./Changes";

type Mode = "edit" | "split" | "preview";

function TreeItem({ node, depth, selected, onOpen }: {
  node: TreeNode;
  depth: number;
  selected?: string;
  onOpen: (path: string) => void;
}) {
  const [open, setOpen] = useState(depth === 0 || !!selected?.startsWith(`${node.path}/`));
  const pad = { paddingLeft: 8 + depth * 12 };
  if (!node.dir) {
    return (
      <button
        style={pad}
        onClick={() => onOpen(node.path)}
        className={cn(
          "flex h-7 w-full items-center gap-1.5 truncate rounded pr-2 text-left text-[13px] hover:bg-accent",
          selected === node.path && "bg-primary/10 text-primary",
        )}
      >
        <FileText className="size-3.5 shrink-0 text-muted-foreground" />
        <span className="truncate">{node.name}</span>
      </button>
    );
  }
  return (
    <div>
      <button
        style={pad}
        onClick={() => setOpen(!open)}
        className="flex h-7 w-full items-center gap-1 rounded pr-2 text-left text-[13px] font-medium hover:bg-accent"
      >
        {open ? <ChevronDown className="size-3.5" /> : <ChevronRight className="size-3.5" />}
        <Folder className="size-3.5 text-muted-foreground" />
        <span className={cn("truncate", depth === 0 && "font-mono text-xs uppercase")}>{node.name}</span>
      </button>
      {open
        ? node.children.map((c) => <TreeItem key={c.path} node={c} depth={depth + 1} selected={selected} onOpen={onOpen} />)
        : null}
    </div>
  );
}

function NewFileDialog({ onCreate }: { onCreate: (path: string) => void }) {
  const [path, setPath] = useState("/semantic/");
  const tier = path.split("/")[1];
  const valid = (TIERS as readonly string[]).includes(tier) && /^\/[a-z]+\/[^.][^]*\.md$/.test(path) && !path.includes("/.");
  return (
    <DialogContent title="New Markdown file" description="Files live below one of the four memory tiers.">
      <div className="space-y-1.5">
        <Label htmlFor="new-path">Path</Label>
        <Input id="new-path" className="font-mono" value={path} onChange={(e) => setPath(e.target.value)} autoFocus />
        <p className="text-[11px] text-muted-foreground">Example: /semantic/architecture/storage.md</p>
      </div>
      <div className="flex justify-end">
        <Button disabled={!valid} onClick={() => onCreate(path)}>
          Create
        </Button>
      </div>
    </DialogContent>
  );
}

export function Files() {
  const { client, me, route, go, theme } = useApp();
  const project = route.project!;
  const branch = route.branch;
  const file = route.file;
  const writable = can(me).write;
  const queries = useQueryClient();
  const [mode, setMode] = useState<Mode>("split");
  const [draft, setDraft] = useState<string | null>(null);
  const [newOpen, setNewOpen] = useState(false);
  const [commitOpen, setCommitOpen] = useState(false);
  const [creating, setCreating] = useState<string | null>(null);

  const tree = useQuery({ queryKey: ["tree", project, branch], queryFn: () => client.tree(project, branch) });
  const entry = tree.data?.find((f) => f.path === file);
  const isImage = entry?.content_type.startsWith("image/") ?? false;
  const content = useQuery({
    queryKey: ["read", project, branch, file],
    queryFn: () => client.read(project, branch, file!).then((r) => r.content),
    enabled: !!file && !!entry && !isImage,
  });
  const status = useQuery({ queryKey: ["status", project, branch], queryFn: () => client.status(project, branch) });
  const staged = new Set((status.data ?? []).map((c) => c.path));

  useEffect(() => setDraft(null), [file, branch]);
  const isNew = !!file && creating === file && !entry;
  const text = draft ?? (isNew ? "" : (content.data ?? ""));
  const dirty = draft !== null && (isNew || draft !== content.data);

  const refresh = () => {
    for (const k of ["tree", "status", "branches", "read"]) queries.invalidateQueries({ queryKey: [k, project] });
  };
  const save = useMutation({
    mutationFn: () => client.write(project, branch, file!, text),
    onSuccess: () => {
      setDraft(null);
      setCreating(null);
      refresh();
      toast.success("Saved to workspace", { description: "Commit to record a version." });
    },
    onError: (e) => toast.error("Save failed", { description: errorText(e) }),
  });
  const remove = useMutation({
    mutationFn: () => client.deleteFile(project, branch, file!),
    onSuccess: () => {
      refresh();
      go({ project, view: "files", branch });
      toast.success("Deleted from workspace");
    },
    onError: (e) => toast.error("Delete failed", { description: errorText(e) }),
  });

  const fileRef = useRef(file);
  fileRef.current = file;
  const extensions = useMemo(
    () => [
      markdown(),
      EditorView.lineWrapping,
      EditorView.domEventHandlers({
        paste(event, view) {
          const doc = fileRef.current;
          const image = Array.from(event.clipboardData?.files ?? []).find((f) => f.type.startsWith("image/"));
          if (!doc || !image) return false;
          const target = attachmentPath(doc, image);
          if (!target) {
            toast.error("Only png, jpg, gif, and webp images can be attached");
            return true;
          }
          event.preventDefault();
          client
            .upload(project, branch, target, image)
            .then(() => {
              const link = `![${image.name}](${relativeLink(doc, target)})`;
              view.dispatch(view.state.replaceSelection(link));
              refresh();
              toast.success("Image uploaded", { description: target });
            })
            .catch((e) => toast.error("Upload failed", { description: errorText(e) }));
          return true;
        },
      }),
    ],
    [client, project, branch],
  );

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "s") {
        e.preventDefault();
        if (dirty && writable && !save.isPending) save.mutate();
      }
    };
    addEventListener("keydown", onKey);
    return () => removeEventListener("keydown", onKey);
  }, [dirty, writable, save]);

  const nodes = useMemo(() => buildTree((tree.data ?? []).map((f) => f.path)), [tree.data]);
  const open = (path: string) => go({ project, view: "files", branch, file: path });

  return (
    <div className="flex h-full">
      <div className="flex w-64 shrink-0 flex-col border-r">
        <div className="flex h-10 items-center justify-between border-b px-3">
          <span className="text-xs font-medium text-muted-foreground">Explorer</span>
          {writable ? (
            <Dialog open={newOpen} onOpenChange={setNewOpen}>
              <Button variant="ghost" size="iconSm" aria-label="New file" onClick={() => setNewOpen(true)}>
                <Plus />
              </Button>
              <NewFileDialog
                onCreate={(path) => {
                  setNewOpen(false);
                  setCreating(path);
                  go({ project, view: "files", branch, file: path });
                  setDraft(`# ${path.split("/").pop()!.replace(/\.md$/, "")}\n\n`);
                }}
              />
            </Dialog>
          ) : null}
        </div>
        <div className="flex-1 overflow-y-auto p-1.5">
          {nodes.map((n) => (
            <TreeItem key={n.path} node={n} depth={0} selected={file} onOpen={open} />
          ))}
        </div>
        <div className="border-t px-3 py-2 text-[11px] text-muted-foreground">
          {tree.data?.length ?? 0} files on <span className="font-mono">{branch}</span>
        </div>
      </div>

      <div className="flex min-w-0 flex-1 flex-col">
        {!file ? (
          <div className="p-6">
            <Empty title="Select a file">Pick a file in the explorer, or press Ctrl K to jump to one.</Empty>
          </div>
        ) : !entry && !isNew ? (
          <div className="p-6">
            <Empty title="File not found">{file} does not exist on {branch}.</Empty>
          </div>
        ) : (
          <>
            <div className="flex h-10 shrink-0 items-center gap-2 border-b px-3">
              <FilePen className="size-3.5 text-muted-foreground" />
              <span className="truncate font-mono text-xs">{file}</span>
              {isNew ? <Badge tone="accent">new</Badge> : null}
              {staged.has(file) ? <Badge tone="warning">staged</Badge> : null}
              {dirty ? <Badge tone="danger">unsaved</Badge> : null}
              {!writable ? <Badge>read-only</Badge> : null}
              <div className="ml-auto flex items-center gap-1">
                {!isImage ? (
                  <div className="mr-1 flex rounded-md border p-0.5">
                    {(
                      [
                        ["edit", <FilePen key="e" />],
                        ["split", <Columns2 key="s" />],
                        ["preview", <Eye key="p" />],
                      ] as const
                    ).map(([m, icon]) => (
                      <button
                        key={m}
                        aria-label={`${m} mode`}
                        onClick={() => setMode(m)}
                        className={cn(
                          "grid h-6 w-7 place-items-center rounded text-muted-foreground [&_svg]:size-3.5",
                          mode === m && "bg-accent text-foreground",
                        )}
                      >
                        {icon}
                      </button>
                    ))}
                  </div>
                ) : null}
                {writable ? (
                  <>
                    {!isImage ? (
                      <Button size="sm" variant="outline" disabled={!dirty || save.isPending} onClick={() => save.mutate()}>
                        <Save /> Save
                      </Button>
                    ) : null}
                    <Button size="sm" disabled={!staged.size} onClick={() => setCommitOpen(true)}>
                      Commit{staged.size ? ` (${staged.size})` : ""}
                    </Button>
                    {!isNew ? (
                      <Button
                        size="iconSm"
                        variant="ghost"
                        aria-label="Delete file"
                        onClick={() => confirm(`Delete ${file} from ${branch}?`) && remove.mutate()}
                      >
                        <Trash className="text-destructive" />
                      </Button>
                    ) : null}
                  </>
                ) : null}
              </div>
            </div>
            {isImage ? (
              <div className="overflow-auto p-6">
                <Markdown doc={file} text={`![${file}](${file})`} />
              </div>
            ) : (
              <div className={cn("grid min-h-0 flex-1", mode === "split" && "grid-cols-2")}>
                {mode !== "preview" ? (
                  <div className={cn("min-h-0 overflow-hidden", mode === "split" && "border-r")}>
                    <CodeMirror
                      value={text}
                      height="100%"
                      theme={theme}
                      readOnly={!writable}
                      extensions={extensions}
                      onChange={(v) => setDraft(v)}
                      basicSetup={{ foldGutter: false, highlightActiveLineGutter: false }}
                      className="h-full"
                    />
                  </div>
                ) : null}
                {mode !== "edit" ? (
                  <div className="min-h-0 overflow-auto px-8 py-6">
                    <Markdown doc={file} text={text} />
                  </div>
                ) : null}
              </div>
            )}
          </>
        )}
      </div>
      <CommitDialog open={commitOpen} onOpenChange={setCommitOpen} count={staged.size} />
    </div>
  );
}
