import { Dialog as D } from "radix-ui";
import { X } from "lucide-react";
import type { ReactNode } from "react";
import { cn } from "@/lib/utils";

export const Dialog = D.Root;
export const DialogTrigger = D.Trigger;
export const DialogClose = D.Close;

export function DialogContent({
  title,
  description,
  children,
  className,
}: {
  title: string;
  description?: string;
  children: ReactNode;
  className?: string;
}) {
  return (
    <D.Portal>
      <D.Overlay className="fixed inset-0 z-50 bg-black/40 backdrop-blur-[1px]" />
      <D.Content
        className={cn(
          "fixed top-1/2 left-1/2 z-50 grid w-[min(92vw,30rem)] -translate-x-1/2 -translate-y-1/2 gap-4 rounded-lg border bg-popover p-5 text-popover-foreground shadow-xl",
          className,
        )}
      >
        <div className="space-y-1">
          <D.Title className="text-sm font-semibold">{title}</D.Title>
          {description ? (
            <D.Description className="text-xs text-muted-foreground">{description}</D.Description>
          ) : (
            <D.Description className="sr-only">{title}</D.Description>
          )}
        </div>
        {children}
        <D.Close className="absolute top-3 right-3 rounded p-1 text-muted-foreground hover:bg-accent" aria-label="Close">
          <X className="size-4" />
        </D.Close>
      </D.Content>
    </D.Portal>
  );
}
