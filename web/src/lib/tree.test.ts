import { describe, expect, it } from "vitest";
import { buildTree } from "./tree";

describe("buildTree", () => {
  it("always lists the four tiers and puts folders before files", () => {
    const tree = buildTree(["/semantic/z.md", "/semantic/arch/b.md", "/semantic/a.md"]);
    expect(tree.map((n) => n.name)).toEqual(["core", "semantic", "procedural", "episodic"]);
    const semantic = tree[1];
    expect(semantic.children.map((n) => [n.name, n.dir])).toEqual([
      ["arch", true],
      ["a.md", false],
      ["z.md", false],
    ]);
    expect(semantic.children[0].children[0].path).toBe("/semantic/arch/b.md");
    expect(tree[0].children).toEqual([]);
  });
});
