import { DropdownMenu as M } from "radix-ui";
import type { ComponentProps } from "react";
import { cn } from "@/lib/utils";

export const Menu = M.Root;
export const MenuTrigger = M.Trigger;

export function MenuContent({ className, ...p }: ComponentProps<typeof M.Content>) {
  return (
    <M.Portal>
      <M.Content
        sideOffset={4}
        align="start"
        className={cn(
          "z-50 max-h-80 min-w-48 overflow-y-auto rounded-md border bg-popover p-1 text-popover-foreground shadow-lg",
          className,
        )}
        {...p}
      />
    </M.Portal>
  );
}

export function MenuItem({ className, ...p }: ComponentProps<typeof M.Item>) {
  return (
    <M.Item
      className={cn(
        "flex cursor-default items-center gap-2 rounded px-2 py-1.5 text-[13px] outline-none select-none data-[highlighted]:bg-accent [&_svg]:size-3.5",
        className,
      )}
      {...p}
    />
  );
}

export const MenuLabel = ({ className, ...p }: ComponentProps<typeof M.Label>) => (
  <M.Label className={cn("px-2 py-1 text-[11px] font-medium text-muted-foreground uppercase", className)} {...p} />
);
export const MenuSeparator = () => <M.Separator className="my-1 h-px bg-border" />;
