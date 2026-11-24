import { Database, LoaderCircle } from "lucide-react";
import { useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import { Input, Label } from "@/components/ui/input";
import { ApiError, createClient, type Me } from "@/lib/api";

export function Login({ onLogin }: { onLogin: (key: string, me: Me) => void }) {
  const [key, setKey] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    const trimmed = key.trim();
    try {
      const me = await createClient(() => trimmed, () => {}).me();
      onLogin(trimmed, me);
    } catch (err) {
      setError(err instanceof ApiError && err.status === 401 ? "Invalid, expired, or revoked key" : String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="grid min-h-screen place-items-center bg-muted/40 p-4">
      <form onSubmit={submit} className="w-full max-w-sm space-y-5 rounded-xl border bg-card p-6 shadow-sm">
        <div className="flex items-center gap-2.5">
          <div className="grid size-8 place-items-center rounded-md bg-primary text-primary-foreground">
            <Database className="size-4" />
          </div>
          <div>
            <div className="text-sm font-semibold">Engram Console</div>
            <div className="text-xs text-muted-foreground">Versioned memory for agents and teams</div>
          </div>
        </div>
        <div className="space-y-1.5">
          <Label htmlFor="key">API key</Label>
          <Input
            id="key"
            type="password"
            autoComplete="off"
            placeholder="egm_..."
            className="font-mono"
            value={key}
            onChange={(e) => setKey(e.target.value)}
          />
          <p className="text-[11px] text-muted-foreground">
            The key stays in this browser tab only (session storage). Ask an admin for a dev, pm, or bot key.
          </p>
        </div>
        {error ? (
          <div role="alert" className="rounded-md bg-destructive/10 px-3 py-2 text-xs text-destructive">
            {error}
          </div>
        ) : null}
        <Button type="submit" className="w-full" disabled={busy || !key.trim()}>
          {busy ? <LoaderCircle className="animate-spin" /> : null}
          Sign in
        </Button>
      </form>
    </div>
  );
}
