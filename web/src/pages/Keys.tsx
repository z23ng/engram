import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Copy, KeyRound, Plus, ShieldCheck } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { Empty, PageHeader } from "@/components/ui/empty";
import { Input, Label } from "@/components/ui/input";
import { Table, TD, TH, THead, TR } from "@/components/ui/table";
import type { Key, Role } from "@/lib/api";
import { can, errorText, useApp } from "@/lib/app";

const ROLES: [Role, string][] = [
  ["dev", "dev - read and write memory, branches"],
  ["pm", "pm - read and write memory, branches"],
  ["bot", "bot - read only"],
  ["admin", "admin - everything, including audit"],
];
const roleTone = { admin: "danger", dev: "accent", pm: "success", bot: "neutral" } as const;

function status(k: Key) {
  if (k.revoked) return <Badge tone="danger">revoked</Badge>;
  if (k.expires_at_ms && k.expires_at_ms < Date.now()) return <Badge tone="warning">expired</Badge>;
  return <Badge tone="success">active</Badge>;
}

function CreateKey({ onDone }: { onDone: () => void }) {
  const { client } = useApp();
  const queries = useQueryClient();
  const projects = useQuery({ queryKey: ["projects"], queryFn: client.projects });
  const [name, setName] = useState("");
  const [role, setRole] = useState<Role>("dev");
  const [scope, setScope] = useState<string[]>([]);
  const [days, setDays] = useState("90");
  const [secret, setSecret] = useState<string | null>(null);
  const create = useMutation({
    mutationFn: () =>
      client.createKey({
        name: name.trim(),
        role,
        projects: scope.length ? scope : null,
        expires_days: days ? Number(days) : undefined,
      }),
    onSuccess: (r) => {
      setSecret(r.secret);
      queries.invalidateQueries({ queryKey: ["keys"] });
    },
    onError: (e) => toast.error("Create failed", { description: errorText(e) }),
  });

  if (secret) {
    return (
      <DialogContent title="Key created" description="Copy the key now. The server stores only its hash and cannot show it again.">
        <div className="flex items-center gap-1 rounded-md border bg-muted/40 py-1 pr-1 pl-2.5">
          <code className="flex-1 font-mono text-xs break-all">{secret}</code>
          <Button variant="ghost" size="iconSm" aria-label="Copy key" onClick={() => navigator.clipboard.writeText(secret).then(() => toast.success("Copied"))}>
            <Copy />
          </Button>
        </div>
        <div className="flex justify-end">
          <Button onClick={onDone}>Done</Button>
        </div>
      </DialogContent>
    );
  }
  const select = "h-8 w-full rounded-md border border-input bg-background px-2 text-[13px]";
  return (
    <DialogContent title="Create API key" description="Use one key per person, agent, or bot so the audit log stays clear.">
      <form
        className="space-y-3"
        onSubmit={(e) => {
          e.preventDefault();
          if (name.trim()) create.mutate();
        }}
      >
        <div className="space-y-1.5">
          <Label htmlFor="key-name">Name</Label>
          <Input id="key-name" placeholder="pm-alice, claude-code-ci, docs-bot" value={name} onChange={(e) => setName(e.target.value)} autoFocus />
        </div>
        <div className="grid grid-cols-2 gap-3">
          <div className="space-y-1.5">
            <Label htmlFor="key-role">Role</Label>
            <select id="key-role" className={select} value={role} onChange={(e) => setRole(e.target.value as Role)}>
              {ROLES.map(([r, label]) => (
                <option key={r} value={r}>
                  {label}
                </option>
              ))}
            </select>
          </div>
          <div className="space-y-1.5">
            <Label htmlFor="key-days">Expires after (days, empty = never)</Label>
            <Input id="key-days" inputMode="numeric" value={days} onChange={(e) => setDays(e.target.value.replace(/\D/g, ""))} />
          </div>
        </div>
        <div className="space-y-1.5">
          <Label>Projects (none selected = all projects)</Label>
          <div className="flex flex-wrap gap-1.5">
            {(projects.data ?? []).map((p) => {
              const on = scope.includes(p);
              return (
                <button
                  type="button"
                  key={p}
                  onClick={() => setScope(on ? scope.filter((x) => x !== p) : [...scope, p])}
                  className={`rounded-md border px-2 py-1 font-mono text-xs ${on ? "border-primary bg-primary/10 text-primary" : "hover:bg-accent"}`}
                >
                  {p}
                </button>
              );
            })}
          </div>
        </div>
        <div className="flex justify-end">
          <Button type="submit" disabled={!name.trim() || create.isPending}>
            Create key
          </Button>
        </div>
      </form>
    </DialogContent>
  );
}

export function Keys() {
  const { client, me } = useApp();
  const queries = useQueryClient();
  const [open, setOpen] = useState(false);
  const keys = useQuery({ queryKey: ["keys"], queryFn: client.keys, enabled: can(me).globalAdmin });
  const revoke = useMutation({
    mutationFn: (id: number) => client.revokeKey(id),
    onSuccess: () => {
      queries.invalidateQueries({ queryKey: ["keys"] });
      toast.success("Key revoked");
    },
    onError: (e) => toast.error("Revoke failed", { description: errorText(e) }),
  });
  if (!can(me).globalAdmin) {
    return (
      <div className="p-6">
        <Empty title="Admin only">API keys are managed with a global admin key.</Empty>
      </div>
    );
  }
  return (
    <div className="mx-auto max-w-6xl p-6">
      <PageHeader
        title="API keys"
        description="Keys authenticate MCP agents, WebDAV clients, and this console. Roles: dev and pm write, bot reads."
        actions={
          <Button onClick={() => setOpen(true)}>
            <Plus /> Create key
          </Button>
        }
      />
      <Table>
        <THead>
          <tr>
            <TH className="w-14">ID</TH>
            <TH>Name</TH>
            <TH className="w-24">Role</TH>
            <TH>Projects</TH>
            <TH className="w-32">Expires</TH>
            <TH className="w-24">Status</TH>
            <TH className="w-24" />
          </tr>
        </THead>
        <tbody>
          {(keys.data ?? []).map((k) => (
            <TR key={k.id}>
              <TD className="font-mono text-xs text-muted-foreground">{k.id}</TD>
              <TD>
                <span className="inline-flex items-center gap-1.5 font-mono text-xs">
                  {k.role === "admin" ? <ShieldCheck className="size-3.5" /> : <KeyRound className="size-3.5 text-muted-foreground" />}
                  {k.name}
                </span>
              </TD>
              <TD>
                <Badge tone={roleTone[k.role]}>{k.role}</Badge>
              </TD>
              <TD className="font-mono text-xs">{k.projects ? k.projects.join(", ") : <span className="text-muted-foreground">all</span>}</TD>
              <TD className="text-xs text-muted-foreground">{k.expires_at_ms ? new Date(k.expires_at_ms).toISOString().slice(0, 10) : "never"}</TD>
              <TD>{status(k)}</TD>
              <TD className="text-right">
                {!k.revoked ? (
                  <Button
                    size="sm"
                    variant="ghost"
                    className="text-destructive"
                    onClick={() => confirm(`Revoke key '${k.name}'? Clients using it stop working at once.`) && revoke.mutate(k.id)}
                  >
                    Revoke
                  </Button>
                ) : null}
              </TD>
            </TR>
          ))}
        </tbody>
      </Table>
      <Dialog open={open} onOpenChange={setOpen}>
        {open ? <CreateKey onDone={() => setOpen(false)} /> : null}
      </Dialog>
    </div>
  );
}
