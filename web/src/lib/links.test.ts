import { describe, expect, it } from "vitest";
import { attachmentPath, relativeLink, resolveLink } from "./links";

const now = new Date(Date.UTC(2026, 10, 19, 10, 15, 0));

describe("attachment links", () => {
  it("stores images below the document tier, or semantic for core", () => {
    expect(attachmentPath("/episodic/2026/day.md", { name: "My Diagram.PNG", type: "image/png" }, now)).toBe(
      "/episodic/_attachments/20261119-101500-my-diagram.png",
    );
    expect(attachmentPath("/core/rules.md", { name: "x.jpg", type: "image/jpeg" }, now)).toBe(
      "/semantic/_attachments/20261119-101500-x.jpg",
    );
    expect(attachmentPath("/semantic/a.md", { name: "x.svg", type: "image/svg+xml" }, now)).toBeNull();
  });

  it("writes relative Markdown links and resolves them back", () => {
    const doc = "/semantic/notes/a.md";
    const img = "/semantic/_attachments/x y.png";
    const link = relativeLink(doc, img);
    expect(link).toBe("../_attachments/x%20y.png");
    expect(resolveLink(doc, link)).toBe(img);
    expect(resolveLink(doc, "/procedural/_attachments/p.png")).toBe("/procedural/_attachments/p.png");
    expect(resolveLink(doc, "https://example.com/a.png")).toBeNull();
    expect(resolveLink(doc, "data:image/png;base64,AA")).toBeNull();
    expect(resolveLink(doc, "../../../etc/passwd")).toBeNull();
  });
});
