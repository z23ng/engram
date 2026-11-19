import { useQuery } from "@tanstack/react-query";
import { Badge } from "@/components/ui/badge";
import { Empty, PageHeader } from "@/components/ui/empty";
import { Table, TD, TH, THead, TR } from "@/components/ui/table";
import { useApp } from "@/lib/app";
import { shortId } from "@/lib/utils";

export function Audit() {
  const { client, route } = useApp();
  const project = route.project!;
  const audit = useQuery({ queryKey: ["audit", project], queryFn: () => client.audit(project) });
  return (
    <div className="mx-auto max-w-6xl p-6">
      <PageHeader title="Audit log" description="Write and branch operations by key, newest first. Single WebDAV saves are recorded by their auto-commit." />
      {!audit.data?.length ? (
        <Empty title="No audit entries" />
      ) : (
        <Table>
          <THead>
            <tr>
              <TH className="w-48">Time (UTC)</TH>
              <TH>Key</TH>
              <TH className="w-20">Role</TH>
              <TH className="w-40">Operation</TH>
              <TH className="w-32">Commit</TH>
            </tr>
          </THead>
          <tbody>
            {audit.data.map((a, i) => (
              <TR key={`${a.at_ms}-${i}`}>
                <TD className="font-mono text-xs text-muted-foreground">{new Date(a.at_ms).toISOString().replace("T", " ").slice(0, 19)}</TD>
                <TD className="font-mono text-xs">{a.key_name}</TD>
                <TD>
                  <Badge>{a.role}</Badge>
                </TD>
                <TD>
                  <Badge tone="accent" className="font-mono">
                    {a.operation}
                  </Badge>
                </TD>
                <TD className="font-mono text-xs text-muted-foreground">{a.commit_id ? shortId(a.commit_id) : "-"}</TD>
              </TR>
            ))}
          </tbody>
        </Table>
      )}
    </div>
  );
}
