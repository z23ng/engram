import type { ComponentProps } from "react";
import { cn } from "@/lib/utils";

export const Table = ({ className, ...p }: ComponentProps<"table">) => (
  <div className="overflow-x-auto rounded-lg border">
    <table className={cn("w-full text-[13px]", className)} {...p} />
  </div>
);
export const THead = (p: ComponentProps<"thead">) => <thead className="bg-muted/50 text-left" {...p} />;
export const TH = ({ className, ...p }: ComponentProps<"th">) => (
  <th className={cn("h-8 px-3 text-xs font-medium text-muted-foreground", className)} {...p} />
);
export const TR = ({ className, ...p }: ComponentProps<"tr">) => (
  <tr className={cn("border-t hover:bg-muted/40", className)} {...p} />
);
export const TD = ({ className, ...p }: ComponentProps<"td">) => (
  <td className={cn("h-9 px-3 align-middle", className)} {...p} />
);
