import { useQuery } from "@tanstack/react-query";
import { ChevronDown, ChevronRight, GitCommitHorizontal } from "lucide-react";
import { Fragment, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Empty, PageHeader } from "@/components/ui/empty";
import { Table, TD, TH, THead, TR } from "@/components/ui/table";
import type { Commit } from "@/lib/api";
import { useApp } from "@/lib/app";
import { shortId, timeAgo } from "@/lib/utils";

const tone = { added: "success", modified: "warning", deleted: "danger" } as const;

function CommitDiff({ commit }: { commit: Commit }) {
  const { client, route, go } = useApp();
  const project = route.project!;
  const parent = commit.parent_ids[0];
  const diff = useQuery({
    queryKey: ["diff", project, parent, commit.commit_id],
    queryFn: () => client.diff(project, parent, commit.commit_id),
    enabled: !!parent,
  });
  if (!parent) return <div className="px-3 py-2 text-xs text-muted-foreground">Initial commit.</div>;
  return (
    <div className="space-y-1 px-3 py-2">
      {(diff.data ?? []).map((d) => (
        <div key={d.path} className="flex items-center gap-2">
          <Badge tone={tone[d.kind]} className="w-16 justify-center">
            {d.kind}
          </Badge>
          <button
            className="font-mono text-xs hover:text-primary hover:underline"
            onClick={() => go({ project, view: "files", branch: route.branch, file: d.path })}
          >
            {d.path}
          </button>
        </div>
      ))}
      {diff.data?.length === 0 ? <div className="text-xs text-muted-foreground">No file changes.</div> : null}
    </div>
  );
}

export function History() {
  const { client, route } = useApp();
  const project = route.project!;
  const [open, setOpen] = useState<string | null>(null);
  const log = useQuery({ queryKey: ["log", project, route.branch], queryFn: () => client.log(project, route.branch) });
  return (
    <div className="mx-auto max-w-5xl p-6">
      <PageHeader title="History" description={`Commits reachable from ${route.branch}, newest first.`} />
      {!log.data?.length ? (
        <Empty title="No commits" />
      ) : (
        <Table>
          <THead>
            <tr>
              <TH className="w-8" />
              <TH className="w-32">Commit</TH>
              <TH>Message</TH>
              <TH className="w-44">Author</TH>
              <TH className="w-28">When</TH>
            </tr>
          </THead>
          <tbody>
            {log.data.map((c) => (
              <Fragment key={c.commit_id}>
                <TR className="cursor-pointer" onClick={() => setOpen(open === c.commit_id ? null : c.commit_id)}>
                  <TD className="text-muted-foreground">
                    {open === c.commit_id ? <ChevronDown className="size-3.5" /> : <ChevronRight className="size-3.5" />}
                  </TD>
                  <TD>
                    <span className="inline-flex items-center gap-1 font-mono text-xs text-primary">
                      <GitCommitHorizontal className="size-3.5" />
                      {shortId(c.commit_id)}
                    </span>
                  </TD>
                  <TD className="max-w-0 truncate">{c.message}</TD>
                  <TD className="truncate font-mono text-xs">{c.author}</TD>
                  <TD className="text-xs text-muted-foreground" title={new Date(c.created_at_ms).toISOString()}>
                    {timeAgo(c.created_at_ms)}
                  </TD>
                </TR>
                {open === c.commit_id ? (
                  <tr className="border-t bg-muted/30">
                    <td colSpan={5}>
                      <CommitDiff commit={c} />
                    </td>
                  </tr>
                ) : null}
              </Fragment>
            ))}
          </tbody>
        </Table>
      )}
    </div>
  );
}
