import { cva, type VariantProps } from "class-variance-authority";
import type { ComponentProps } from "react";
import { cn } from "@/lib/utils";

const badgeVariants = cva("inline-flex items-center gap-1 rounded px-1.5 py-px text-[11px] font-medium", {
  variants: {
    tone: {
      neutral: "bg-muted text-muted-foreground",
      accent: "bg-primary/12 text-primary",
      success: "bg-success/15 text-success",
      warning: "bg-warning/18 text-warning",
      danger: "bg-destructive/12 text-destructive",
    },
  },
  defaultVariants: { tone: "neutral" },
});

export function Badge({ className, tone, ...props }: ComponentProps<"span"> & VariantProps<typeof badgeVariants>) {
  return <span className={cn(badgeVariants({ tone }), className)} {...props} />;
}
