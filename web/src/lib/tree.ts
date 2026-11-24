export type TreeNode = { name: string; path: string; dir: boolean; children: TreeNode[] };

export const TIERS = ["core", "semantic", "procedural", "episodic"] as const;

/** Builds the folder tree of a flat path list; the 4 tiers always exist, folders sort first. */
export function buildTree(paths: string[]): TreeNode[] {
  const roots: TreeNode[] = TIERS.map((t) => ({ name: t, path: `/${t}`, dir: true, children: [] }));
  for (const full of paths) {
    const parts = full.split("/").filter(Boolean);
    let level = roots;
    let prefix = "";
    parts.forEach((name, i) => {
      prefix += `/${name}`;
      const dir = i < parts.length - 1;
      let node = level.find((n) => n.name === name && n.dir === dir);
      if (!node) {
        node = { name, path: prefix, dir, children: [] };
        level.push(node);
      }
      level = node.children;
    });
  }
  const sort = (nodes: TreeNode[]) => {
    nodes.sort((a, b) => (a.dir === b.dir ? a.name.localeCompare(b.name) : a.dir ? -1 : 1));
    nodes.forEach((n) => sort(n.children));
  };
  roots.forEach((r) => sort(r.children));
  return roots;
}
