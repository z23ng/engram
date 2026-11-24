import type { ReactNode } from "react";

export function Empty({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <div className="flex flex-col items-center justify-center gap-1 rounded-lg border border-dashed py-12 text-center">
      <div className="text-sm font-medium">{title}</div>
      {children ? <div className="text-xs text-muted-foreground">{children}</div> : null}
    </div>
  );
}

export function PageHeader({ title, description, actions }: { title: string; description?: string; actions?: ReactNode }) {
  return (
    <div className="mb-4 flex items-end justify-between gap-4">
      <div>
        <h1 className="text-base font-semibold tracking-tight">{title}</h1>
        {description ? <p className="mt-0.5 text-xs text-muted-foreground">{description}</p> : null}
      </div>
      <div className="flex items-center gap-2">{actions}</div>
    </div>
  );
}
