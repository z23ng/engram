import { TIERS } from "./tree";

// Same image types as the server attachment rule (no SVG).
const IMAGE_EXT: Record<string, string> = {
  "image/png": "png",
  "image/jpeg": "jpg",
  "image/gif": "gif",
  "image/webp": "webp",
};

const pad = (n: number) => String(n).padStart(2, "0");

/** Upload path for a pasted image: `<tier>/_attachments/<utc stamp>-<name>.<ext>`; core uses semantic. */
export function attachmentPath(docPath: string, file: { name: string; type: string }, now = new Date()) {
  const ext = IMAGE_EXT[file.type];
  if (!ext) return null;
  const tier = docPath.split("/")[1];
  const target = (TIERS as readonly string[]).includes(tier) && tier !== "core" ? tier : "semantic";
  const stem =
    file.name
      .replace(/\.[^.]*$/, "")
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "") || "image";
  const stamp =
    `${now.getUTCFullYear()}${pad(now.getUTCMonth() + 1)}${pad(now.getUTCDate())}-` +
    `${pad(now.getUTCHours())}${pad(now.getUTCMinutes())}${pad(now.getUTCSeconds())}`;
  return `/${target}/_attachments/${stamp}-${stem}.${ext}`;
}

/** Relative Markdown link from a document to a file (works in Obsidian and plain Markdown). */
export function relativeLink(fromDoc: string, target: string) {
  const from = fromDoc.split("/").filter(Boolean).slice(0, -1);
  const to = target.split("/").filter(Boolean);
  let i = 0;
  while (i < from.length && i < to.length - 1 && from[i] === to[i]) i++;
  const parts = [...Array<string>(from.length - i).fill(".."), ...to.slice(i).map(encodeURIComponent)];
  return parts.join("/");
}

/** Vault path of a link in a document; `null` for external URLs or paths above the vault root. */
export function resolveLink(fromDoc: string, href: string): string | null {
  if (/^[a-z][a-z0-9+.-]*:/i.test(href) || href.startsWith("//")) return null;
  let decoded: string;
  try {
    decoded = decodeURIComponent(href.split(/[?#]/)[0]);
  } catch {
    return null;
  }
  const out = decoded.startsWith("/") ? [] : fromDoc.split("/").filter(Boolean).slice(0, -1);
  for (const seg of decoded.split("/")) {
    if (!seg || seg === ".") continue;
    if (seg === "..") {
      if (!out.length) return null;
      out.pop();
    } else {
      out.push(seg);
    }
  }
  return out.length ? `/${out.join("/")}` : null;
}
