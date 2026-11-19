import { useQuery } from "@tanstack/react-query";
import { Search } from "lucide-react";
import { useState } from "react";
import { Empty, PageHeader } from "@/components/ui/empty";
import { Input } from "@/components/ui/input";
import { useApp } from "@/lib/app";

export function SearchPage() {
  const { client, route, go } = useApp();
  const project = route.project!;
  const [input, setInput] = useState("");
  const [query, setQuery] = useState("");
  const hits = useQuery({
    queryKey: ["search", project, route.branch, query],
    queryFn: () => client.search(project, route.branch, query),
    enabled: !!query,
  });
  return (
    <div className="mx-auto max-w-4xl p-6">
      <PageHeader title="Search" description={`Hybrid search over the ${route.branch} workspace (same results agents get through MCP).`} />
      <form
        className="relative mb-4"
        onSubmit={(e) => {
          e.preventDefault();
          setQuery(input.trim());
        }}
      >
        <Search className="absolute top-2 left-2.5 size-4 text-muted-foreground" />
        <Input className="h-9 pl-8" placeholder="Search memory and press Enter" value={input} onChange={(e) => setInput(e.target.value)} autoFocus />
      </form>
      {query && hits.data?.length === 0 ? <Empty title="No matches">Try other words.</Empty> : null}
      <div className="space-y-2">
        {(hits.data ?? []).map((h) => (
          <button
            key={`${h.path}#${h.heading_path}`}
            onClick={() => go({ project, view: "files", branch: route.branch, file: h.path })}
            className="block w-full rounded-lg border bg-card p-3 text-left hover:border-primary/50"
          >
            <div className="flex items-baseline gap-2">
              <span className="text-[13px] font-medium">{h.title}</span>
              <span className="truncate font-mono text-[11px] text-muted-foreground">{h.path}</span>
            </div>
            {h.heading_path ? <div className="mt-0.5 text-[11px] text-muted-foreground">{h.heading_path}</div> : null}
            <p className="mt-1.5 line-clamp-3 font-mono text-xs whitespace-pre-wrap text-muted-foreground">{h.snippet}</p>
          </button>
        ))}
      </div>
    </div>
  );
}
