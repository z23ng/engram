import { useQuery } from "@tanstack/react-query";
import { RotateCcw, Search, Tags, ZoomIn, ZoomOut } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Empty, PageHeader } from "@/components/ui/empty";
import { Input } from "@/components/ui/input";
import type { GraphEdge, GraphNode } from "@/lib/api";
import { errorText, useApp } from "@/lib/app";

const TIERS = ["all", "core", "semantic", "procedural", "episodic"] as const;
const TIER_ORDER = ["core", "semantic", "procedural", "episodic"] as const;

type Point = { x: number; y: number };
type PosMap = Record<string, Point>;
type Transform = { x: number; y: number; k: number };
type LabelsMode = "auto" | "all" | "off";

function tierColor(tier: string): string {
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

function parentFolder(path: string): string {
  const idx = path.lastIndexOf("/");
  return idx > 0 ? path.slice(0, idx) : "/";
}

function truncateDisplay(text: string, maxCols = 20): string {
  let cols = 0;
  let out = "";
  for (const ch of text) {
    const w = ch.charCodeAt(0) > 0x2e7f ? 2 : 1;
    if (cols + w > maxCols) {
      return `${out}…`;
    }
    cols += w;
    out += ch;
  }
  return out;
}

/**
 * Computes a collision-free, cluster-aware graph layout without fixed viewport wall clamping.
 * Nodes are grouped by tier (with `core` centered) on a landscape ellipse and sorted by
 * directory + title so sparse or zero-edge vaults form clean, readable constellations.
 */
function computeLayout(nodes: GraphNode[], edges: GraphEdge[]): PosMap {
  const pos: PosMap = {};
  if (nodes.length === 0) return pos;

  const byTier: Record<string, GraphNode[]> = { core: [], semantic: [], procedural: [], episodic: [] };
  for (const n of nodes) {
    (byTier[n.tier] ?? byTier.semantic).push(n);
  }
  for (const tier of TIER_ORDER) {
    byTier[tier].sort((a, b) => {
      const da = parentFolder(a.path);
      const db = parentFolder(b.path);
      if (da !== db) return da.localeCompare(db);
      return a.title.localeCompare(b.title);
    });
  }

  // Place `core` around the origin `(0, 0)` and distribute remaining tiers on a landscape ellipse.
  const coreNodes = byTier.core;
  const golden = Math.PI * (3 - Math.sqrt(5));
  coreNodes.forEach((n, idx) => {
    if (coreNodes.length === 1) {
      pos[n.id] = { x: 0, y: 0 };
    } else {
      const r = 34 * Math.sqrt(idx + 0.5);
      const ang = idx * golden;
      pos[n.id] = { x: r * Math.cos(ang), y: r * Math.sin(ang) };
    }
  });

  const outerTiers = (["semantic", "procedural", "episodic"] as const).filter((t) => byTier[t].length > 0);
  const totalOuter = outerTiers.reduce((acc, t) => acc + byTier[t].length, 0);
  const baseRing = coreNodes.length > 0 ? 96 : 60;
  const ringRadius = Math.max(baseRing, Math.sqrt(Math.max(1, totalOuter)) * 27);

  outerTiers.forEach((tier, sectorIdx) => {
    const group = byTier[tier];
    const angle = (2 * Math.PI * sectorIdx) / outerTiers.length - Math.PI / 2;
    const spreadRadius = Math.max(54, Math.sqrt(group.length) * 26);
    const centerDist = outerTiers.length === 1 && coreNodes.length === 0 ? 0 : ringRadius + spreadRadius * 0.4;
    const cx = centerDist * 1.45 * Math.cos(angle);
    const cy = centerDist * 0.78 * Math.sin(angle);

    group.forEach((n, i) => {
      if (group.length === 1) {
        pos[n.id] = { x: cx, y: cy };
      } else {
        const rr = 28 * Math.sqrt(i + 0.6);
        const theta = i * golden + angle;
        pos[n.id] = {
          x: cx + rr * Math.cos(theta),
          y: cy + rr * Math.sin(theta),
        };
      }
    });
  });

  // Gentle relaxation: pull linked nodes lightly closer and resolve close collisions.
  const minSep = 42;
  for (let iter = 0; iter < 90; iter++) {
    for (let i = 0; i < nodes.length; i++) {
      for (let j = i + 1; j < nodes.length; j++) {
        const a = pos[nodes[i].id];
        const b = pos[nodes[j].id];
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        let d = Math.sqrt(dx * dx + dy * dy);
        if (d < 0.01) {
          dx = 1;
          dy = 0.5;
          d = 1.12;
        }
        if (d < minSep) {
          const push = ((minSep - d) / d) * 0.48;
          a.x -= dx * push;
          a.y -= dy * push;
          b.x += dx * push;
          b.y += dy * push;
        }
      }
    }

    for (const e of edges) {
      const a = pos[e.source];
      const b = pos[e.target];
      if (!a || !b) continue;
      const dx = b.x - a.x;
      const dy = b.y - a.y;
      const d = Math.max(1, Math.sqrt(dx * dx + dy * dy));
      const targetLen = 96;
      if (d > targetLen) {
        const pull = ((d - targetLen) / d) * 0.08;
        a.x += dx * pull;
        a.y += dy * pull;
        b.x -= dx * pull;
        b.y -= dy * pull;
      }
    }
  }

  return pos;
}

type ClusterCircle = { key: string; label: string; tier: string; x: number; y: number; r: number };

function computeClusterCircles(nodes: GraphNode[], pos: PosMap): ClusterCircle[] {
  const byTier: Record<string, GraphNode[]> = { core: [], semantic: [], procedural: [], episodic: [] };
  for (const n of nodes) {
    (byTier[n.tier] ?? byTier.semantic).push(n);
  }
  const clusterCircles: ClusterCircle[] = [];
  for (const tier of TIER_ORDER) {
    const members = byTier[tier];
    if (members.length < 2) continue;
    let sx = 0;
    let sy = 0;
    let count = 0;
    for (const m of members) {
      const p = pos[m.id];
      if (!p) continue;
      sx += p.x;
      sy += p.y;
      count += 1;
    }
    if (count < 2) continue;
    const cx = sx / count;
    const cy = sy / count;
    let maxD = 0;
    for (const m of members) {
      const p = pos[m.id];
      if (!p) continue;
      const dx = p.x - cx;
      const dy = p.y - cy;
      maxD = Math.max(maxD, Math.sqrt(dx * dx + dy * dy));
    }
    clusterCircles.push({
      key: tier,
      label: `${tier} · ${members.length}`,
      tier,
      x: cx,
      y: cy,
      r: maxD + 26,
    });
  }
  return clusterCircles;
}

function fitTransform(nodes: GraphNode[], pos: PosMap, viewW: number, viewH: number): Transform {
  if (nodes.length === 0) return { x: viewW / 2, y: viewH / 2, k: 1 };
  let minX = Infinity;
  let maxX = -Infinity;
  let minY = Infinity;
  let maxY = -Infinity;
  for (const n of nodes) {
    const p = pos[n.id];
    if (!p) continue;
    minX = Math.min(minX, p.x);
    maxX = Math.max(maxX, p.x);
    minY = Math.min(minY, p.y);
    maxY = Math.max(maxY, p.y);
  }
  if (!Number.isFinite(minX)) return { x: viewW / 2, y: viewH / 2, k: 1 };
  const pad = 76;
  const spanX = Math.max(120, maxX - minX + pad * 2);
  const spanY = Math.max(120, maxY - minY + pad * 2);
  const k = Math.max(0.25, Math.min(1.65, Math.min(viewW / spanX, viewH / spanY)));
  const cx = (minX + maxX) / 2;
  const cy = (minY + maxY) / 2;
  return { x: viewW / 2 - cx * k, y: viewH / 2 - cy * k, k };
}

export function Graph() {
  const { client, route, go } = useApp();
  const project = route.project!;
  const stageRef = useRef<HTMLDivElement>(null);
  const svgRef = useRef<SVGSVGElement>(null);

  const [viewport, setViewport] = useState({ w: 920, h: 560 });
  const viewportRef = useRef({ w: 920, h: 560 });
  const [tierFilter, setTierFilter] = useState<(typeof TIERS)[number]>("all");
  const [searchTerm, setSearchTerm] = useState("");
  const [showClusters, setShowClusters] = useState(true);
  const [labelsMode, setLabelsMode] = useState<LabelsMode>("auto");
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [hoveredId, setHoveredId] = useState<string | null>(null);
  const [positions, setPositions] = useState<PosMap>({});
  const [transform, setTransform] = useState<Transform>({ x: 460, y: 280, k: 1 });

  const userMovedRef = useRef(false);
  const suppressNextClickRef = useRef(false);
  const interactionRef = useRef<
    | { kind: "node"; id: string; startClient: Point; startNode: Point; moved: boolean }
    | { kind: "pan"; startClient: Point; startTx: Point; moved: boolean }
    | null
  >(null);

  const query = useQuery({
    queryKey: ["graph", project, route.branch],
    queryFn: () => client.graph(project, route.branch),
    refetchOnWindowFocus: false,
  });

  useEffect(() => {
    const el = stageRef.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const update = () => {
      const rect = el.getBoundingClientRect();
      const w = Math.round(rect.width);
      const h = Math.round(rect.height);
      if (w > 50 && h > 50 && (w !== viewportRef.current.w || h !== viewportRef.current.h)) {
        viewportRef.current = { w, h };
        setViewport({ w, h });
      }
    };
    update();
    const ro = new ResizeObserver(update);
    ro.observe(el);
    return () => ro.disconnect();
  }, [query.isLoading, query.data]);

  const allNodes = useMemo(() => query.data?.nodes ?? [], [query.data]);
  const allEdges = useMemo(() => query.data?.edges ?? [], [query.data]);

  const tierCounts = useMemo(() => {
    const counts: Record<string, number> = { all: allNodes.length, core: 0, semantic: 0, procedural: 0, episodic: 0 };
    for (const n of allNodes) counts[n.tier] = (counts[n.tier] ?? 0) + 1;
    return counts;
  }, [allNodes]);

  const visibleNodes = useMemo(
    () => allNodes.filter((n) => tierFilter === "all" || n.tier === tierFilter),
    [allNodes, tierFilter],
  );
  const visibleSet = useMemo(() => new Set(visibleNodes.map((n) => n.id)), [visibleNodes]);
  const visibleEdges = useMemo(
    () => allEdges.filter((e) => visibleSet.has(e.source) && visibleSet.has(e.target)),
    [allEdges, visibleSet],
  );

  const layoutSignature = useMemo(() => {
    const nodeSig = visibleNodes.map((n) => n.id).join("\0");
    const edgeSig = visibleEdges.map((e) => `${e.source}->${e.target}`).join("\0");
    return `${project}|${route.branch}|${tierFilter}|${nodeSig}|${edgeSig}`;
  }, [project, route.branch, tierFilter, visibleNodes, visibleEdges]);

  const latestDataRef = useRef({ nodes: visibleNodes, edges: visibleEdges });
  latestDataRef.current = { nodes: visibleNodes, edges: visibleEdges };

  const recomputeAndFit = useCallback(() => {
    const { nodes, edges } = latestDataRef.current;
    userMovedRef.current = false;
    setSelectedId(null);
    setHoveredId(null);
    const pos = computeLayout(nodes, edges);
    setPositions(pos);
    setTransform(fitTransform(nodes, pos, viewportRef.current.w, viewportRef.current.h));
  }, []);

  // Re-run layout ONLY when graph structure, project, branch, or tier filter actually changes.
  useEffect(() => {
    recomputeAndFit();
  }, [layoutSignature, recomputeAndFit]);

  // Refit initial viewport measurement or window resizes unless user has moved/zoomed manually.
  useEffect(() => {
    if (userMovedRef.current || latestDataRef.current.nodes.length === 0) return;
    setTransform((prev) => {
      const next = fitTransform(latestDataRef.current.nodes, positions, viewport.w, viewport.h);
      if (Math.abs(prev.x - next.x) < 0.5 && Math.abs(prev.y - next.y) < 0.5 && Math.abs(prev.k - next.k) < 0.002) {
        return prev;
      }
      return next;
    });
  }, [viewport.w, viewport.h, positions]);

  const clusters = useMemo(
    () => (showClusters ? computeClusterCircles(visibleNodes, positions) : []),
    [showClusters, visibleNodes, positions],
  );

  const searchLower = searchTerm.trim().toLowerCase();
  const matchedIds = useMemo(() => {
    if (!searchLower) return new Set<string>();
    const s = new Set<string>();
    for (const n of visibleNodes) {
      if (
        n.title.toLowerCase().includes(searchLower) ||
        n.path.toLowerCase().includes(searchLower) ||
        n.tags.some((t) => t.toLowerCase().includes(searchLower))
      ) {
        s.add(n.id);
      }
    }
    return s;
  }, [visibleNodes, searchLower]);

  const focusId = hoveredId ?? selectedId;
  const connected = useMemo(() => {
    const set = new Set<string>();
    if (!focusId) return set;
    set.add(focusId);
    for (const e of visibleEdges) {
      if (e.source === focusId) set.add(e.target);
      if (e.target === focusId) set.add(e.source);
    }
    return set;
  }, [focusId, visibleEdges]);

  const topHubIds = useMemo(() => {
    const sorted = [...visibleNodes]
      .filter((n) => n.in_degree + n.out_degree > 0 || n.tier === "core")
      .sort((a, b) => b.in_degree + b.out_degree - (a.in_degree + a.out_degree));
    return new Set(sorted.slice(0, 12).map((n) => n.id));
  }, [visibleNodes]);

  const shouldShowLabel = (node: GraphNode) => {
    if (labelsMode === "off") return focusId === node.id || matchedIds.has(node.id);
    if (labelsMode === "all") return true;
    if (visibleNodes.length <= 24 || transform.k >= 1.15) return true;
    return connected.has(node.id) || matchedIds.has(node.id) || topHubIds.has(node.id);
  };

  const selectedNode = visibleNodes.find((n) => n.id === selectedId) ?? null;
  const inspectedNode = selectedNode ?? visibleNodes.find((n) => n.id === hoveredId) ?? null;
  const inspectId = inspectedNode?.id ?? null;
  const selectedOutlinks = useMemo(
    () => (inspectId ? visibleEdges.filter((e) => e.source === inspectId).map((e) => e.target) : []),
    [inspectId, visibleEdges],
  );
  const selectedBacklinks = useMemo(
    () => (inspectId ? visibleEdges.filter((e) => e.target === inspectId).map((e) => e.source) : []),
    [inspectId, visibleEdges],
  );

  const labelFont = Math.min(20, Math.max(9.5, 11.5 / Math.pow(transform.k, 0.8)));
  const labelStroke = 3.4 / Math.pow(transform.k, 0.8);
  const clusterFont = Math.min(21, Math.max(10.5, 12 / Math.pow(transform.k, 0.78)));

  const zoomBy = (factor: number, clientPt?: Point) => {
    userMovedRef.current = true;
    setTransform((prev) => {
      const nextK = Math.max(0.2, Math.min(4.0, prev.k * factor));
      const rect = svgRef.current?.getBoundingClientRect();
      const px = clientPt && rect ? clientPt.x - rect.left : viewport.w / 2;
      const py = clientPt && rect ? clientPt.y - rect.top : viewport.h / 2;
      const ratio = nextK / prev.k;
      return {
        k: nextK,
        x: px - (px - prev.x) * ratio,
        y: py - (py - prev.y) * ratio,
      };
    });
  };

  return (
    <div className="mx-auto flex h-full max-w-7xl flex-col gap-3 p-6">
      <PageHeader
        title="Knowledge Graph"
        description={`Branch-isolated graph for ${route.branch} (${visibleNodes.length} notes, ${visibleEdges.length} note links).`}
        actions={
          <div className="flex flex-wrap items-center gap-1.5">
            {TIERS.map((tier) => (
              <Button
                key={tier}
                size="sm"
                variant={tierFilter === tier ? "default" : "outline"}
                onClick={() => setTierFilter(tier)}
              >
                {tier === "all" ? `All (${tierCounts.all})` : `${tier} (${tierCounts[tier] ?? 0})`}
              </Button>
            ))}
          </div>
        }
      />

      <div className="flex flex-wrap items-center gap-2">
        <div className="relative min-w-[220px] flex-1">
          <Search className="absolute top-2.5 left-2.5 size-3.5 text-muted-foreground" />
          <Input
            className="h-8 pl-8 text-xs"
            placeholder="Filter or highlight nodes by title, path, or #tag (Enter to focus)..."
            value={searchTerm}
            onChange={(e) => setSearchTerm(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && matchedIds.size > 0) {
                const first = visibleNodes.find((n) => matchedIds.has(n.id));
                if (first && positions[first.id]) {
                  userMovedRef.current = true;
                  setSelectedId(first.id);
                  const p = positions[first.id];
                  const k = Math.max(transform.k, 1.15);
                  setTransform({ k, x: viewport.w / 2 - p.x * k, y: viewport.h / 2 - p.y * k });
                }
              }
            }}
          />
        </div>
        <Button
          size="sm"
          variant={showClusters ? "default" : "outline"}
          onClick={() => setShowClusters((v) => !v)}
        >
          <Tags /> Clusters
        </Button>
        <Button
          size="sm"
          variant="outline"
          onClick={() => setLabelsMode((m) => (m === "auto" ? "all" : m === "all" ? "off" : "auto"))}
        >
          Labels: {labelsMode}
        </Button>
        <div className="flex items-center gap-1">
          <Button size="sm" variant="outline" aria-label="Zoom out" onClick={() => zoomBy(0.8)}>
            <ZoomOut />
          </Button>
          <Button
            size="sm"
            variant="outline"
            onClick={() => {
              userMovedRef.current = false;
              setTransform(fitTransform(visibleNodes, positions, viewport.w, viewport.h));
            }}
          >
            Fit
          </Button>
          <Button size="sm" variant="outline" aria-label="Zoom in" onClick={() => zoomBy(1.25)}>
            <ZoomIn />
          </Button>
        </div>
        <Button
          size="sm"
          variant="outline"
          onClick={recomputeAndFit}
          disabled={!visibleNodes.length}
        >
          <RotateCcw /> Reset layout
        </Button>
      </div>

      {query.isLoading ? (
        <div className="p-6 text-xs text-muted-foreground">Loading graph...</div>
      ) : query.error ? (
        <div className="rounded-lg border border-destructive/40 bg-destructive/10 p-4 text-xs text-destructive">
          {errorText(query.error)}
        </div>
      ) : visibleNodes.length === 0 ? (
        <Empty title="No notes to graph">Create or stage Markdown notes on this branch to inspect the graph.</Empty>
      ) : (
        <div className="flex min-h-0 flex-1 flex-col overflow-hidden rounded-lg border bg-card">
          <div ref={stageRef} className="relative min-h-[420px] flex-1 overflow-hidden bg-background/50">
            <svg
              ref={svgRef}
              width="100%"
              height="100%"
              viewBox={`0 0 ${viewport.w} ${viewport.h}`}
              className="h-full w-full cursor-grab select-none active:cursor-grabbing"
              onWheel={(e) => {
                e.preventDefault();
                zoomBy(e.deltaY < 0 ? 1.14 : 0.88, { x: e.clientX, y: e.clientY });
              }}
              onPointerDown={(e) => {
                interactionRef.current = {
                  kind: "pan",
                  startClient: { x: e.clientX, y: e.clientY },
                  startTx: { x: transform.x, y: transform.y },
                  moved: false,
                };
              }}
              onPointerMove={(e) => {
                const cur = interactionRef.current;
                if (!cur) return;
                const dx = e.clientX - cur.startClient.x;
                const dy = e.clientY - cur.startClient.y;
                if (Math.abs(dx) > 3 || Math.abs(dy) > 3) {
                  cur.moved = true;
                  userMovedRef.current = true;
                }
                if (cur.kind === "node") {
                  setPositions((prev) => ({
                    ...prev,
                    [cur.id]: {
                      x: cur.startNode.x + dx / transform.k,
                      y: cur.startNode.y + dy / transform.k,
                    },
                  }));
                } else if (cur.moved) {
                  setTransform((prev) => ({
                    ...prev,
                    x: cur.startTx.x + dx,
                    y: cur.startTx.y + dy,
                  }));
                }
              }}
              onPointerUp={() => {
                const cur = interactionRef.current;
                if (cur?.kind === "node") {
                  suppressNextClickRef.current = true;
                  setTimeout(() => {
                    suppressNextClickRef.current = false;
                  }, 0);
                  if (!cur.moved) {
                    setSelectedId((prev) => (prev === cur.id ? null : cur.id));
                  }
                } else if (cur?.kind === "pan" && !cur.moved) {
                  setSelectedId(null);
                }
                interactionRef.current = null;
              }}
              onPointerLeave={() => {
                interactionRef.current = null;
              }}
            >
              <defs>
                <marker id="wiki-arrow" markerWidth="7" markerHeight="6" refX="6" refY="3" orient="auto">
                  <polygon points="0 0, 7 3, 0 6" className="fill-muted-foreground/75" />
                </marker>
                <marker id="wiki-arrow-hot" markerWidth="8" markerHeight="6" refX="7" refY="3" orient="auto">
                  <polygon points="0 0, 8 3, 0 6" className="fill-primary" />
                </marker>
              </defs>
              <g transform={`translate(${transform.x.toFixed(1)}, ${transform.y.toFixed(1)}) scale(${transform.k.toFixed(3)})`}>
                {showClusters &&
                  clusters.map((c) => (
                    <g key={c.key} style={{ pointerEvents: "none" }}>
                      <circle
                        cx={c.x}
                        cy={c.y}
                        r={c.r}
                        fill={tierColor(c.tier)}
                        fillOpacity={0.05}
                        stroke={tierColor(c.tier)}
                        strokeOpacity={0.28}
                        strokeWidth={1.25 / Math.sqrt(transform.k)}
                        strokeDasharray="5 4"
                      />
                      <text
                        x={c.x}
                        y={c.y - c.r - 8 / Math.pow(transform.k, 0.65)}
                        textAnchor="middle"
                        fontSize={clusterFont}
                        className="fill-muted-foreground font-mono font-medium"
                        style={{
                          opacity: 0.88,
                          paintOrder: "stroke",
                          stroke: "var(--background)",
                          strokeWidth: `${labelStroke}px`,
                          strokeLinejoin: "round",
                        }}
                      >
                        {c.label}
                      </text>
                    </g>
                  ))}

                <g style={{ pointerEvents: "none" }}>
                  {visibleEdges.map((edge) => {
                    const p1 = positions[edge.source];
                    const p2 = positions[edge.target];
                    if (!p1 || !p2) return null;
                    const dstNode = visibleNodes.find((n) => n.id === edge.target);
                    const dstDeg = dstNode ? dstNode.in_degree + dstNode.out_degree : 0;
                    const dstR = 7 + Math.min(8, dstDeg * 1.4) + 3;
                    const dx = p2.x - p1.x;
                    const dy = p2.y - p1.y;
                    const dist = Math.max(1, Math.sqrt(dx * dx + dy * dy));
                    const x2 = p2.x - (dx / dist) * dstR;
                    const y2 = p2.y - (dy / dist) * dstR;
                    const hot = !!focusId && (edge.source === focusId || edge.target === focusId);
                    return (
                      <line
                        key={`${edge.source}->${edge.target}`}
                        x1={p1.x}
                        y1={p1.y}
                        x2={x2}
                        y2={y2}
                        className={hot ? "stroke-primary" : "stroke-border"}
                        strokeWidth={hot ? 2.1 / Math.sqrt(transform.k) : 1.25 / Math.sqrt(transform.k)}
                        strokeOpacity={focusId ? (hot ? 0.95 : 0.16) : 0.75}
                        markerEnd={hot ? "url(#wiki-arrow-hot)" : "url(#wiki-arrow)"}
                      />
                    );
                  })}
                </g>

                <g>
                  {visibleNodes.map((node) => {
                    const p = positions[node.id] ?? { x: 0, y: 0 };
                    const deg = node.in_degree + node.out_degree;
                    const r = 7 + Math.min(8, deg * 1.4);
                    const hitR = Math.max(r + 5, 13);
                    const isSel = selectedId === node.id;
                    const isMatch = matchedIds.has(node.id);
                    const dimmed =
                      (!!focusId && !connected.has(node.id)) ||
                      (searchLower.length > 0 && !isMatch && !focusId);
                    const color = tierColor(node.tier);
                    const showText = shouldShowLabel(node);
                    return (
                      <g
                        key={node.id}
                        data-testid={`graph-node-${node.path}`}
                        transform={`translate(${p.x.toFixed(1)}, ${p.y.toFixed(1)})`}
                        style={{ opacity: dimmed ? 0.2 : 1, cursor: "pointer" }}
                        onPointerEnter={() => setHoveredId(node.id)}
                        onPointerLeave={() => setHoveredId((cur) => (cur === node.id ? null : cur))}
                        onPointerDown={(e) => {
                          e.stopPropagation();
                          interactionRef.current = {
                            kind: "node",
                            id: node.id,
                            startClient: { x: e.clientX, y: e.clientY },
                            startNode: { ...p },
                            moved: false,
                          };
                        }}
                        onClick={(e) => {
                          e.stopPropagation();
                          if (suppressNextClickRef.current) {
                            suppressNextClickRef.current = false;
                            return;
                          }
                          setSelectedId((cur) => (cur === node.id ? null : node.id));
                        }}
                        onDoubleClick={(e) => {
                          e.stopPropagation();
                          go({ project, view: "files", branch: route.branch, file: node.path });
                        }}
                      >
                        <circle r={hitR} fill="transparent" />
                        <circle
                          r={r}
                          fill={color}
                          fillOpacity={isSel || isMatch ? 0.42 : 0.24}
                          stroke={isSel || isMatch ? "currentColor" : color}
                          strokeWidth={(isSel || isMatch ? 2.5 : 1.7) / Math.sqrt(transform.k)}
                        />
                        {showText && (
                          <text
                            y={r + 13 / Math.pow(transform.k, 0.65)}
                            textAnchor="middle"
                            fontSize={labelFont}
                            className="fill-foreground font-medium"
                            style={{
                              pointerEvents: "none",
                              paintOrder: "stroke",
                              stroke: "var(--background)",
                              strokeWidth: `${labelStroke}px`,
                              strokeLinejoin: "round",
                            }}
                          >
                            {truncateDisplay(node.title, 22)}
                          </text>
                        )}
                      </g>
                    );
                  })}
                </g>
              </g>
            </svg>

            <div className="pointer-events-none absolute top-2.5 left-3 flex flex-wrap items-center gap-2 rounded-md border bg-background/85 px-2.5 py-1 text-[11px] backdrop-blur-xs">
              <span className="inline-flex items-center gap-1"><span className="size-2 rounded-full" style={{ background: "#f59e0b" }} /> core</span>
              <span className="inline-flex items-center gap-1"><span className="size-2 rounded-full" style={{ background: "#10b981" }} /> semantic</span>
              <span className="inline-flex items-center gap-1"><span className="size-2 rounded-full" style={{ background: "#0284c7" }} /> procedural</span>
              <span className="inline-flex items-center gap-1"><span className="size-2 rounded-full" style={{ background: "#8b5cf6" }} /> episodic</span>
              {visibleEdges.length === 0 ? (
                <span className="text-muted-foreground">· No note links yet (clustered by tier &amp; folder; add [[Wikilinks]] or [text](note.md) for edges)</span>
              ) : null}
            </div>
          </div>

          <div className="flex h-[72px] shrink-0 items-center overflow-hidden border-t bg-muted/20 px-4 text-xs">
            {inspectedNode ? (
              <div className="flex w-full min-w-0 flex-col gap-1.5">
                <div className="flex min-w-0 items-center justify-between gap-2">
                  <div className="flex min-w-0 items-center gap-2">
                    <Badge tone={tierTone(inspectedNode.tier)}>{inspectedNode.tier}</Badge>
                    <span className="truncate font-medium">{inspectedNode.title}</span>
                    <span className="truncate font-mono text-muted-foreground">{inspectedNode.path}</span>
                    <span className="shrink-0 text-muted-foreground">
                      in: {inspectedNode.in_degree} · out: {inspectedNode.out_degree}
                    </span>
                  </div>
                  <Button
                    size="sm"
                    className="shrink-0"
                    onClick={() => go({ project, view: "files", branch: route.branch, file: inspectedNode.path })}
                  >
                    Open in Files
                  </Button>
                </div>
                {(selectedOutlinks.length > 0 || selectedBacklinks.length > 0) && (
                  <div className="flex items-center gap-1.5 overflow-x-auto whitespace-nowrap text-[11px]">
                    {selectedOutlinks.map((target) => (
                      <button
                        key={`out-${target}`}
                        onClick={() => setSelectedId(target)}
                        className="shrink-0 rounded border bg-background px-1.5 py-0.5 font-mono text-muted-foreground hover:border-primary hover:text-foreground"
                      >
                        → {target}
                      </button>
                    ))}
                    {selectedBacklinks.map((source) => (
                      <button
                        key={`in-${source}`}
                        onClick={() => setSelectedId(source)}
                        className="shrink-0 rounded border bg-background px-1.5 py-0.5 font-mono text-muted-foreground hover:border-primary hover:text-foreground"
                      >
                        ← {source}
                      </button>
                    ))}
                  </div>
                )}
              </div>
            ) : (
              <span className="text-muted-foreground">
                Scroll to zoom, drag background to pan, drag nodes to move, click to inspect links, double-click to open in Files.
              </span>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
