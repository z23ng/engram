import { useQuery } from "@tanstack/react-query";
import { useEffect, useMemo } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { useApp } from "@/lib/app";
import { resolveLink } from "@/lib/links";

/** Images need the bearer key, so they load through `/raw` as `blob:` URLs. */
function VaultImage({ doc, src, alt }: { doc: string; src: string; alt: string }) {
  const { client, route } = useApp();
  const path = resolveLink(doc, src);
  const blob = useQuery({
    queryKey: ["raw", route.project, route.branch, path],
    queryFn: () => client.rawBlob(route.project!, route.branch, path!),
    enabled: !!path,
  });
  const url = useMemo(() => (blob.data ? URL.createObjectURL(blob.data) : null), [blob.data]);
  useEffect(() => () => (url ? URL.revokeObjectURL(url) : undefined), [url]);
  if (!path) return <span className="text-xs text-muted-foreground">[external image blocked: {alt || src}]</span>;
  if (blob.isError) return <span className="text-xs text-destructive">[missing image: {path}]</span>;
  return url ? <img src={url} alt={alt} className="rounded border" /> : null;
}

export function Markdown({ doc, text }: { doc: string; text: string }) {
  return (
    <article className="prose prose-sm max-w-none dark:prose-invert prose-headings:tracking-tight prose-code:before:content-none prose-code:after:content-none prose-pre:bg-muted prose-pre:text-foreground">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{
          img: ({ src, alt }) => <VaultImage doc={doc} src={typeof src === "string" ? src : ""} alt={alt ?? ""} />,
          a: ({ href, children }) => (
            <a href={href} target="_blank" rel="noreferrer noopener">
              {children}
            </a>
          ),
        }}
      >
        {text}
      </ReactMarkdown>
    </article>
  );
}
