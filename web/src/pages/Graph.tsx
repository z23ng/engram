import { useQuery } from "@tanstack/react-query";
import { RotateCcw } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Empty, PageHeader } from "@/components/ui/empty";
import type { GraphEdge, GraphNode } from "@/lib/api";
import { errorText, useApp } from "@/lib/app";

const WIDTH = 840;
const HEIGHT = 520;
const TIERS = ["all", "core", "semantic", "procedural", "episodic"] as const;

type PosMap = Record<string, { x: number; y: number }>;

function tierStroke(tier: string): string {
  switch (tier) {
    case "core":
      return "#f59e0b";
    case "procedural":
      return "#0284c7";
    case "episodic":
      return "#8b5cf6";
    default:
      return "#10b981";
  }
}

function tierTone(tier: string): "warning" | "success" | "accent" | "neutral" {
  switch (tier) {
    case "core":
      return "warning";
    case "semantic":
      return "success";
    case "procedural":
      return "accent";
    default:
      return "neutral";
  }
}

function computeInitialPositions(nodes: GraphNode[], edges: GraphEdge[]): PosMap {
  const pos: PosMap = {};
  if (nodes.length === 0) return pos;
  const cx = WIDTH / 2;
  const cy = HEIGHT / 2;
  const r = Math.min(WIDTH, HEIGHT) * 0.34;
  nodes.forEach((n, i) => {
    const angle = (2 * Math.PI * i) / nodes.length - Math.PI / 2;
    pos[n.id] = { x: cx + r * Math.cos(angle), y: cy + r * Math.sin(angle) };
  });
  if (nodes.length === 1) return pos;

  const k = Math.sqrt((WIDTH * HEIGHT) / Math.max(4, nodes.length)) * 0.65;
  const disp: PosMap = {};
  for (let iter = 0; iter < 140; iter++) {
    for (const n of nodes) disp[n.id] = { x: 0, y: 0 };
    for (let i = 0; i < nodes.length; i++) {
      for (let j = i + 1; j < nodes.length; j++) {
        const a = pos[nodes[i].id];
        const b = pos[nodes[j].id];
        let dx = a.x - b.x;
        let dy = a.y - b.y;
        let d2 = dx * dx + dy * dy;
        if (d2 < 1) {
          dx = 1;
          dy = 1;
          d2 = 2;
        }
        const d = Math.sqrt(d2);
        const force = ((k * k) / d) * 0.45;
        const fx = (dx / d) * force;
        const fy = (dy / d) * force;
        disp[nodes[i].id].x += fx;
        disp[nodes[i].id].y += fy;
        disp[nodes[j].id].x -= fx;
        disp[nodes[j].id].y -= fy;
      }
    }
    for (const e of edges) {
      const a = pos[e.source];
      const b = pos[e.target];
      if (!a || !b) continue;
      const dx = b.x - a.x;
      const dy = b.y - a.y;
      const d = Math.max(1, Math.sqrt(dx * dx + dy * dy));
      const force = ((d * d) / k) * 0.14;
      const fx = (dx / d) * force;
      const fy = (dy / d) * force;
      disp[e.source].x += fx;
      disp[e.source].y += fy;
      disp[e.target].x -= fx;
      disp[e.target].y -= fy;
    }
    const stepCap = Math.max(2, 18 * (1 - iter / 145));
    for (const n of nodes) {
      const p = pos[n.id];
      const v = disp[n.id];
      v.x += (cx - p.x) * 0.04;
      v.y += (cy - p.y) * 0.04;
      const mag = Math.max(0.001, Math.sqrt(v.x * v.x + v.y * v.y));
      const step = Math.min(mag, stepCap);
      p.x = Math.max(48, Math.min(WIDTH - 48, p.x + (v.x / mag) * step));
      p.y = Math.max(36, Math.min(HEIGHT - 36, p.y + (v.y / mag) * step));
    }
  }
  return pos;
}

export function Graph() {
  const { client, route, go } = useApp();
  const project = route.project!;
  const svgRef = useRef<SVGSVGElement>(null);
  const [tierFilter, setTierFilter] = useState<(typeof TIERS)[number]>("all");
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [positions, setPositions] = useState<PosMap>({});
  const dragRef = useRef<{ id: string; moved: boolean } | null>(null);

  const query = useQuery({
    queryKey: ["graph", project, route.branch],
    queryFn: () => client.graph(project, route.branch),
  });

  useEffect(() => {
    setSelectedId(null);
    if (query.data) {
      setPositions(computeInitialPositions(query.data.nodes, query.data.edges));
    }
  }, [query.data, project, route.branch]);

  const visibleNodes = useMemo(
    () => (query.data?.nodes ?? []).filter((n) => tierFilter === "all" || n.tier === tierFilter),
    [query.data, tierFilter],
  );
  const visibleSet = useMemo(() => new Set(visibleNodes.map((n) => n.id)), [visibleNodes]);
  const visibleEdges = useMemo(
    () => (query.data?.edges ?? []).filter((e) => visibleSet.has(e.source) && visibleSet.has(e.target)),
    [query.data, visibleSet],
  );

  const connected = useMemo(() => {
    const set = new Set<string>();
    if (!selectedId) return set;
    set.add(selectedId);
    for (const e of visibleEdges) {
      if (e.source === selectedId) set.add(e.target);
      if (e.target === selectedId) set.add(e.source);
    }
    return set;
  }, [selectedId, visibleEdges]);

  const selectedNode = visibleNodes.find((n) => n.id === selectedId) ?? null;

  const clientToSvg = (clientX: number, clientY: number) => {
    const svg = svgRef.current;
    if (!svg || typeof svg.createSVGPoint !== "function") return { x: WIDTH / 2, y: HEIGHT / 2 };
    const pt = svg.createSVGPoint();
    pt.x = clientX;
    pt.y = clientY;
    const ctm = svg.getScreenCTM?.();
    if (!ctm) return { x: WIDTH / 2, y: HEIGHT / 2 };
    const mapped = pt.matrixTransform(ctm.inverse());
    return {
      x: Math.max(36, Math.min(WIDTH - 36, mapped.x)),
      y: Math.max(28, Math.min(HEIGHT - 28, mapped.y)),
    };
  };

  return (
    <div className="mx-auto flex h-full max-w-6xl flex-col gap-4 p-6">
      <PageHeader
        title="Knowledge Graph"
        description={`Branch-isolated [[Wikilink]] graph for ${route.branch} (${visibleNodes.length} nodes, ${visibleEdges.length} edges).`}
        actions={
          <div className="flex flex-wrap items-center gap-1.5">
            {TIERS.map((tier) => (
              <Button
                key={tier}
                size="sm"
                variant={tierFilter === tier ? "default" : "outline"}
                onClick={() => setTierFilter(tier)}
              >
                {tier === "all" ? "All tiers" : tier}
              </Button>
            ))}
            <Button
              size="sm"
              variant="outline"
              onClick={() => query.data && setPositions(computeInitialPositions(query.data.nodes, query.data.edges))}
              disabled={!query.data?.nodes.length}
            >
              <RotateCcw /> Reset layout
            </Button>
          </div>
        }
      />

      {query.isLoading ? (
        <div className="p-6 text-xs text-muted-foreground">Loading graph...</div>
      ) : query.error ? (
        <div className="rounded-lg border border-destructive/40 bg-destructive/10 p-4 text-xs text-destructive">
          {errorText(query.error)}
        </div>
      ) : visibleNodes.length === 0 ? (
        <Empty title="No notes to graph">Create or stage Markdown notes with [[Wikilinks]] on this branch.</Empty>
      ) : (
        <div className="flex min-h-0 flex-1 flex-col overflow-hidden rounded-lg border bg-card">
          <div className="relative min-h-[380px] flex-1 bg-background/40">
            <svg
              ref={svgRef}
              viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
              className="h-full w-full select-none"
              onPointerMove={(e) => {
                if (!dragRef.current) return;
                dragRef.current.moved = true;
                const pt = clientToSvg(e.clientX, e.clientY);
                const id = dragRef.current.id;
                setPositions((prev) => ({ ...prev, [id]: pt }));
              }}
              onPointerUp={() => {
                if (dragRef.current && !dragRef.current.moved) {
                  const clicked = dragRef.current.id;
                  setSelectedId((cur) => (cur === clicked ? null : clicked));
                }
                dragRef.current = null;
              }}
            >
              <defs>
                <marker id="wiki-arrow" markerWidth="8" markerHeight="6" refX="15" refY="3" orient="auto">
                  <polygon points="0 0, 8 3, 0 6" className="fill-muted-foreground/70" />
                </marker>
                <marker id="wiki-arrow-hot" markerWidth="8" markerHeight="6" refX="15" refY="3" orient="auto">
                  <polygon points="0 0, 8 3, 0 6" className="fill-primary" />
                </marker>
              </defs>
              <g>
                {visibleEdges.map((edge) => {
                  const p1 = positions[edge.source];
                  const p2 = positions[edge.target];
                  if (!p1 || !p2) return null;
                  const hot = !!selectedId && (edge.source === selectedId || edge.target === selectedId);
                  return (
                    <line
                      key={`${edge.source}->${edge.target}`}
                      x1={p1.x}
                      y1={p1.y}
                      x2={p2.x}
                      y2={p2.y}
                      className={hot ? "stroke-primary" : "stroke-border"}
                      strokeWidth={hot ? 2 : 1.25}
                      strokeOpacity={selectedId ? (hot ? 0.95 : 0.25) : 0.8}
                      markerEnd={hot ? "url(#wiki-arrow-hot)" : "url(#wiki-arrow)"}
                    />
                  );
                })}
              </g>
              <g>
                {visibleNodes.map((node) => {
                  const p = positions[node.id] ?? { x: WIDTH / 2, y: HEIGHT / 2 };
                  const deg = node.in_degree + node.out_degree;
                  const r = 8 + Math.min(8, deg * 1.5);
                  const isSel = selectedId === node.id;
                  const dimmed = !!selectedId && !connected.has(node.id);
                  const color = tierStroke(node.tier);
                  return (
                    <g
                      key={node.id}
                      data-testid={`graph-node-${node.path}`}
                      transform={`translate(${p.x}, ${p.y})`}
                      style={{ opacity: dimmed ? 0.3 : 1, cursor: "pointer" }}
                      onPointerDown={(e) => {
                        e.stopPropagation();
                        dragRef.current = { id: node.id, moved: false };
                      }}
                      onClick={(e) => {
                        e.stopPropagation();
                        if (!dragRef.current) setSelectedId((cur) => (cur === node.id ? null : node.id));
                      }}
                      onDoubleClick={() => go({ project, view: "files", branch: route.branch, file: node.path })}
                    >
                      <circle
                        r={r}
                        fill={color}
                        fillOpacity={0.22}
                        stroke={isSel ? "currentColor" : color}
                        strokeWidth={isSel ? 2.6 : 1.8}
                      />
                      <text y={r + 14} textAnchor="middle" className="fill-foreground text-[11px] font-medium">
                        {node.title.slice(0, 24)}
                      </text>
                    </g>
                  );
                })}
              </g>
            </svg>
          </div>
          <div className="flex flex-wrap items-center justify-between gap-2 border-t bg-muted/20 px-4 py-2.5 text-xs">
            {selectedNode ? (
              <>
                <div className="flex flex-wrap items-center gap-2">
                  <Badge tone={tierTone(selectedNode.tier)}>{selectedNode.tier}</Badge>
                  <span className="font-medium">{selectedNode.title}</span>
                  <span className="font-mono text-muted-foreground">{selectedNode.path}</span>
                  <span className="text-muted-foreground">
                    in: {selectedNode.in_degree} · out: {selectedNode.out_degree}
                  </span>
                </div>
                <Button
                  size="sm"
                  onClick={() => go({ project, view: "files", branch: route.branch, file: selectedNode.path })}
                >
                  Open in Files
                </Button>
              </>
            ) : (
              <span className="text-muted-foreground">
                Click a node to inspect incoming/outgoing [[Wikilinks]]; drag to rearrange; double-click to open in Files.
              </span>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
