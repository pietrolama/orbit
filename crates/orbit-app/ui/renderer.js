/* ORBIT renderer: camera, level-of-detail drawing, hit testing and the gravitational search
 * session. Pure functions (camera math, gravity targets, GravitySession) have no DOM dependency
 * and are unit-tested with Node (crates/orbit-app/tests/ui_gravity.test.js).
 *
 * Coordinates: world units are those stored by the layout engine. The camera maps world to screen
 * with sx = (wx - cx) * scale + width / 2. Temporary search positions live only in GravitySession
 * and are never written back. */
(function (root) {
  "use strict";

  const KIND_COLORS = {
    parent_child: "rgba(150,160,190,0.35)",
    temporal_affinity: "#b48cff",
    name_similarity: "#ffcf6e",
    fingerprint_match: "#ff9d5c",
    verified_duplicate: "#ff6b6b",
    explicit_reference: "#5cd6ff",
    manual_association: "#ffffff",
    project_membership: "#7ee2a8",
  };
  const KIND_LABELS = {
    temporal_affinity: "prossimità temporale",
    name_similarity: "genealogia del nome",
    fingerprint_match: "fingerprint simile",
    verified_duplicate: "duplicato verificato",
    explicit_reference: "riferimento esplicito",
    manual_association: "associazione manuale",
    project_membership: "costellazione",
  };
  const EXT_GROUPS = [
    ["doc", "#9ec5ff", ["pdf", "doc", "docx", "odt", "rtf", "txt", "md", "tex", "pages", "epub"]],
    ["sheet", "#8ee6b8", ["xls", "xlsx", "ods", "csv", "tsv", "json", "xml", "yaml", "yml", "toml", "sql", "db"]],
    ["image", "#ffb3d9", ["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "tif", "tiff", "heic", "raw", "psd", "xcf", "kra"]],
    ["code", "#ffd27f", ["rs", "py", "js", "ts", "c", "h", "cpp", "hpp", "go", "java", "kt", "rb", "php", "sh", "html", "css", "lua", "pd"]],
    ["media", "#c9a6ff", ["mp3", "wav", "flac", "ogg", "m4a", "mp4", "mkv", "mov", "avi", "webm"]],
    ["archive", "#ff9f80", ["zip", "gz", "xz", "zst", "bz2", "7z", "rar", "tar", "iso", "deb", "rpm"]],
  ];
  const EXT_COLOR = {};
  for (const [, color, exts] of EXT_GROUPS) for (const e of exts) EXT_COLOR[e] = color;

  function extColor(ext) {
    return EXT_COLOR[ext] || "#c8cfdf";
  }

  // ---------------------------------------------------------------------------------------------
  // Camera
  // ---------------------------------------------------------------------------------------------

  function makeCamera() {
    return { cx: 0, cy: 0, scale: 1, width: 1, height: 1 };
  }

  function toScreen(cam, x, y) {
    return [(x - cam.cx) * cam.scale + cam.width / 2, (y - cam.cy) * cam.scale + cam.height / 2];
  }

  function toWorld(cam, sx, sy) {
    return [(sx - cam.width / 2) / cam.scale + cam.cx, (sy - cam.height / 2) / cam.scale + cam.cy];
  }

  function viewBounds(cam, margin) {
    const m = margin || 0;
    const [x0, y0] = toWorld(cam, -m * cam.width, -m * cam.height);
    const [x1, y1] = toWorld(cam, cam.width * (1 + m), cam.height * (1 + m));
    return { x0, y0, x1, y1 };
  }

  function zoomAt(cam, sx, sy, factor, minScale, maxScale) {
    const [wx, wy] = toWorld(cam, sx, sy);
    const next = Math.min(maxScale, Math.max(minScale, cam.scale * factor));
    cam.scale = next;
    // Keep the world point under the cursor fixed.
    cam.cx = wx - (sx - cam.width / 2) / next;
    cam.cy = wy - (sy - cam.height / 2) / next;
  }

  function fitBounds(cam, b, padding) {
    const w = Math.max(1e-6, b[2] - b[0]);
    const h = Math.max(1e-6, b[3] - b[1]);
    const p = padding || 0.9;
    cam.cx = (b[0] + b[2]) / 2;
    cam.cy = (b[1] + b[3]) / 2;
    cam.scale = Math.min((cam.width * p) / w, (cam.height * p) / h);
    return cam.scale;
  }

  // ---------------------------------------------------------------------------------------------
  // Gravitational search (pure)
  // ---------------------------------------------------------------------------------------------

  const GOLDEN = 2.399963229728653;

  /** Temporary target positions for search results and their neighbours around `focal`.
   *  `results`: [{entity_id, relation_neighbors: [ids]}] ordered by rank.
   *  `original`: Map id -> {x, y, pinned}. `pxToWorld`: world units per screen pixel.
   *  Pinned nodes keep their original position. Deterministic for identical inputs. */
  function gravityTargets(results, original, focal, pxToWorld, opts) {
    const o = Object.assign({ spacingPx: 34, neighborPx: 16, maxNeighbors: 5 }, opts || {});
    const targets = new Map();
    const placed = new Set();
    const resultIds = new Set(results.map((r) => r.entity_id));
    let slot = 0;
    for (const r of results) {
      const id = r.entity_id;
      const orig = original.get(id);
      if (!orig || placed.has(id)) continue;
      placed.add(id);
      if (orig.pinned) {
        targets.set(id, { x: orig.x, y: orig.y });
        continue;
      }
      const d = o.spacingPx * pxToWorld * Math.sqrt(slot + 0.5);
      const a = slot * GOLDEN;
      const t = { x: focal.x + d * Math.cos(a), y: focal.y + d * Math.sin(a) };
      targets.set(id, t);
      slot += 1;
    }
    for (const r of results) {
      const base = targets.get(r.entity_id);
      if (!base) continue;
      const nbs = (r.relation_neighbors || []).slice(0, o.maxNeighbors);
      nbs.forEach((nid, k) => {
        if (placed.has(nid) || resultIds.has(nid)) return;
        const orig = original.get(nid);
        if (!orig) return;
        placed.add(nid);
        if (orig.pinned) {
          targets.set(nid, { x: orig.x, y: orig.y });
          return;
        }
        const a = (k / Math.max(1, nbs.length)) * Math.PI * 2 + r.entity_id * 0.37;
        const d = o.neighborPx * pxToWorld * (1 + 0.15 * k);
        targets.set(nid, { x: base.x + d * Math.cos(a), y: base.y + d * Math.sin(a) });
      });
    }
    return targets;
  }

  function ease(t) {
    return t < 0.5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2;
  }

  /** Animation between persistent and temporary positions. `original` is copied at creation and
   *  is the only source used when restoring, so the final positions equal it exactly. */
  class GravitySession {
    constructor(original, targets, now, durationMs) {
      this.original = new Map();
      for (const [id, p] of original) this.original.set(id, { x: p.x, y: p.y });
      this.targets = targets;
      this.duration = durationMs || 650;
      this.phase = "in";
      this.t0 = now;
      this.finished = false;
    }

    retarget(targets, now) {
      const current = this.positions(now);
      this.from = current;
      this.targets = targets;
      this.phase = "in";
      this.t0 = now;
    }

    release(now) {
      this.from = this.positions(now);
      this.phase = "out";
      this.t0 = now;
    }

    positions(now) {
      const p = Math.min(1, Math.max(0, (now - this.t0) / this.duration));
      const e = ease(p);
      const out = new Map();
      if (this.phase === "out" && p >= 1) {
        this.finished = true;
        for (const [id, o] of this.original) out.set(id, { x: o.x, y: o.y });
        return out;
      }
      for (const [id, o] of this.original) {
        const from = (this.from && this.from.get(id)) || o;
        const to = this.phase === "out" ? o : this.targets.get(id) || o;
        out.set(id, { x: from.x + (to.x - from.x) * e, y: from.y + (to.y - from.y) * e });
      }
      return out;
    }

    animating(now) {
      return now - this.t0 < this.duration;
    }
  }

  // ---------------------------------------------------------------------------------------------
  // Drawing (DOM)
  // ---------------------------------------------------------------------------------------------

  function makeStars(n, seed) {
    let x = seed || 12345;
    const rnd = () => {
      x ^= x << 13;
      x ^= x >>> 17;
      x ^= x << 5;
      return ((x >>> 0) % 100000) / 100000;
    };
    const stars = [];
    for (let i = 0; i < n; i++) stars.push([rnd(), rnd(), rnd() * 1.2 + 0.2, rnd() * 0.5 + 0.1]);
    return stars;
  }
  const STARS = makeStars(260, 987654);

  function truncate(ctx, text, maxPx) {
    if (ctx.measureText(text).width <= maxPx) return text;
    let lo = 0;
    let hi = text.length;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (ctx.measureText(text.slice(0, mid) + "…").width <= maxPx) lo = mid;
      else hi = mid - 1;
    }
    return text.slice(0, lo) + "…";
  }

  function hash01(n) {
    let x = (n * 2654435761) >>> 0;
    x ^= x >>> 13;
    return (x % 1000) / 1000;
  }

  class Renderer {
    constructor(canvas) {
      this.canvas = canvas;
      this.ctx = canvas.getContext("2d");
      this.cam = makeCamera();
      this.dpr = 1;
      this.labelBoxes = [];
    }

    resize() {
      const r = this.canvas.getBoundingClientRect();
      this.dpr = window.devicePixelRatio || 1;
      this.canvas.width = Math.max(1, Math.round(r.width * this.dpr));
      this.canvas.height = Math.max(1, Math.round(r.height * this.dpr));
      this.cam.width = r.width;
      this.cam.height = r.height;
    }

    /** Draws a frame. `scene` is assembled by app.js:
     *  { overview, nodes: Map id->NodeView, edges: [], lod, selected:Set, focus, neighbors:Set,
     *    search: {positions: Map, ids: Set} | null, satellites: [], kindsOn: Set, linkFrom,
     *    dragging: {id, dx, dy, ids:Set} | null, hover } */
    draw(scene) {
      const ctx = this.ctx;
      const cam = this.cam;
      ctx.setTransform(this.dpr, 0, 0, this.dpr, 0, 0);
      ctx.clearRect(0, 0, cam.width, cam.height);
      this.drawBackground();
      this.labelBoxes = [];
      this.hitList = [];
      const search = scene.search;
      const dim = !!(search && search.ids.size) || (scene.focus != null && scene.neighbors && scene.neighbors.size);
      const posOf = (n) => {
        if (search && search.positions.has(n.id)) return search.positions.get(n.id);
        if (scene.dragging && scene.dragging.ids.has(n.id)) return { x: n.x + scene.dragging.dx, y: n.y + scene.dragging.dy };
        return n;
      };

      let galaxyAlpha = scene.lod === "galaxy" ? 1 : Math.max(0, 1 - (scene.zoomRatio - 3) / 6);
      if (search && search.ids.size) galaxyAlpha *= 0.3;
      if (scene.overview && galaxyAlpha > 0.02) this.drawGalaxy(scene, galaxyAlpha);
      this.drawConstellations(scene);
      if (scene.lod !== "galaxy" || scene.zoomRatio > 1.5) {
        this.drawEdges(scene, posOf, dim);
        this.drawNodes(scene, posOf, dim);
      }
      if (search) this.drawSearchNodes(scene, posOf);
      if (scene.project) this.drawProjectLinks(scene, posOf);
      else this.drawSatellites(scene, posOf);
    }

    drawBackground() {
      const ctx = this.ctx;
      const { width, height, cx, cy } = this.cam;
      const g = ctx.createRadialGradient(width * 0.5, height * 0.45, 0, width * 0.5, height * 0.5, Math.max(width, height) * 0.8);
      g.addColorStop(0, "#0b1222");
      g.addColorStop(1, "#05070d");
      ctx.fillStyle = g;
      ctx.fillRect(0, 0, width, height);
      ctx.fillStyle = "#ffffff";
      for (const [sx, sy, r, a] of STARS) {
        // Very slow parallax gives depth without suggesting meaning.
        const px = ((sx * width - cx * 0.02 * r) % width + width) % width;
        const py = ((sy * height - cy * 0.02 * r) % height + height) % height;
        ctx.globalAlpha = a;
        ctx.fillRect(px, py, r, r);
      }
      ctx.globalAlpha = 1;
    }

    drawGalaxy(scene, alpha) {
      const ctx = this.ctx;
      const cam = this.cam;
      const ov = scene.overview;
      const byId = new Map(ov.clusters.map((c) => [c.id, c]));
      const now = Date.now() / 1000;
      // Flows: aggregated relations between clusters.
      for (const f of ov.flows) {
        const a = byId.get(f.a);
        const b = byId.get(f.b);
        if (!a || !b) continue;
        let kind = null;
        let best = -1;
        for (const k in f.by_kind) {
          if (!scene.kindsOn.has(k)) continue;
          if (f.by_kind[k] > best) {
            best = f.by_kind[k];
            kind = k;
          }
        }
        if (!kind) continue;
        const [x1, y1] = toScreen(cam, a.x, a.y);
        const [x2, y2] = toScreen(cam, b.x, b.y);
        const mx = (x1 + x2) / 2 - (y2 - y1) * 0.15;
        const my = (y1 + y2) / 2 + (x2 - x1) * 0.15;
        ctx.strokeStyle = KIND_COLORS[kind];
        ctx.globalAlpha = alpha * Math.min(0.7, 0.12 + Math.log2(1 + f.count) * 0.08);
        ctx.lineWidth = Math.min(6, 0.6 + Math.log2(1 + f.count) * 0.7);
        ctx.beginPath();
        ctx.moveTo(x1, y1);
        ctx.quadraticCurveTo(mx, my, x2, y2);
        ctx.stroke();
      }
      // Clusters.
      const sorted = ov.clusters.slice().sort((p, q) => q.files - p.files);
      for (const c of sorted) {
        const [sx, sy] = toScreen(cam, c.x, c.y);
        const r = Math.max(2.5, c.r * cam.scale);
        if (sx < -r || sy < -r || sx > cam.width + r || sy > cam.height + r) continue;
        const age = c.recent_mtime ? (now - c.recent_mtime) / 86400 : 9999;
        const warmth = Math.max(0, 1 - Math.log10(1 + age) / 3); // recent activity → brighter core
        const hue = 205 + hash01(c.id) * 60 - warmth * 170;
        // Glow: bounded on screen so that large parent systems do not wash out their children;
        // parents get a faint outline that still shows their extent.
        const gr = Math.min(r, 70 + 6 * Math.log2(1 + c.files));
        const g = ctx.createRadialGradient(sx, sy, 0, sx, sy, gr);
        g.addColorStop(0, `hsla(${hue}, 85%, ${55 + warmth * 20}%, ${0.5 * alpha})`);
        g.addColorStop(0.4, `hsla(${hue}, 70%, 45%, ${0.14 * alpha})`);
        g.addColorStop(1, `hsla(${hue}, 60%, 30%, 0)`);
        ctx.globalAlpha = 1;
        ctx.fillStyle = g;
        ctx.beginPath();
        ctx.arc(sx, sy, gr, 0, Math.PI * 2);
        ctx.fill();
        if (r > gr * 1.2) {
          ctx.strokeStyle = `hsla(${hue}, 60%, 60%, ${0.12 * alpha})`;
          ctx.lineWidth = 1;
          ctx.beginPath();
          ctx.arc(sx, sy, r, 0, Math.PI * 2);
          ctx.stroke();
        }
        ctx.fillStyle = `hsla(${hue}, 90%, 85%, ${alpha})`;
        ctx.beginPath();
        ctx.arc(sx, sy, Math.max(1.2, Math.min(4, Math.log10(2 + c.files))), 0, Math.PI * 2);
        ctx.fill();
        this.hitList.push({ id: c.id, sx, sy, r: Math.max(6, r * 0.5), cluster: true });
        if (r > 14) this.label(c.name, sx, sy + Math.min(r * 0.5, 40) + 12, alpha * 0.9, "#dfe7ff", 600, 12, true);
      }
      ctx.globalAlpha = 1;
    }

    drawConstellations(scene) {
      const ov = scene.overview;
      if (!ov || !ov.constellations) return;
      const ctx = this.ctx;
      const cam = this.cam;
      for (const c of ov.constellations) {
        const pts = c.member_positions;
        if (!pts || !pts.length) continue;
        let mx = 0;
        let my = 0;
        for (const [x, y] of pts) {
          mx += x;
          my += y;
        }
        mx /= pts.length;
        my /= pts.length;
        let rr = 0;
        for (const [x, y] of pts) rr = Math.max(rr, Math.hypot(x - mx, y - my));
        const [sx, sy] = toScreen(cam, mx, my);
        const r = Math.max(18, rr * cam.scale + 18);
        const color = c.color || "#7ee2a8";
        ctx.save();
        ctx.strokeStyle = color;
        ctx.globalAlpha = 0.55;
        ctx.setLineDash([2, 6]);
        ctx.lineWidth = 1.2;
        ctx.beginPath();
        ctx.arc(sx, sy, r, 0, Math.PI * 2);
        ctx.stroke();
        ctx.setLineDash([]);
        ctx.globalAlpha = 0.25;
        for (const [x, y] of pts) {
          const [px, py] = toScreen(cam, x, y);
          ctx.beginPath();
          ctx.moveTo(sx, sy);
          ctx.lineTo(px, py);
          ctx.stroke();
        }
        ctx.restore();
        this.label("✦ " + c.name, sx, sy - r - 6, 0.95, color, 600, 12, true);
      }
    }

    drawEdges(scene, posOf, dim) {
      const ctx = this.ctx;
      const cam = this.cam;
      const nodes = scene.nodes;
      // Physical hierarchy: thin straight lines, only when children are visible enough.
      ctx.lineWidth = 1;
      for (const n of nodes.values()) {
        if (n.parent == null) continue;
        const p = nodes.get(n.parent);
        if (!p) continue;
        if (n.r * cam.scale < 2.2) continue;
        const a = posOf(p);
        const b = posOf(n);
        const [x1, y1] = toScreen(cam, a.x, a.y);
        const [x2, y2] = toScreen(cam, b.x, b.y);
        ctx.strokeStyle = KIND_COLORS.parent_child;
        ctx.globalAlpha = dim ? 0.12 : 0.45;
        ctx.beginPath();
        ctx.moveTo(x1, y1);
        ctx.lineTo(x2, y2);
        ctx.stroke();
      }
      // Overlay relations.
      for (const e of scene.edges) {
        if (!scene.kindsOn.has(e.kind)) continue;
        const a = nodes.get(e.src);
        const b = nodes.get(e.dst);
        if (!a || !b) continue;
        const pa = posOf(a);
        const pb = posOf(b);
        const [x1, y1] = toScreen(cam, pa.x, pa.y);
        const [x2, y2] = toScreen(cam, pb.x, pb.y);
        const focused = scene.focus != null && (e.src === scene.focus || e.dst === scene.focus);
        const manual = e.origin === "manual";
        let alpha = 0.15 + 0.6 * e.confidence;
        if (dim && !focused) alpha *= 0.25;
        if (focused) alpha = Math.max(alpha, 0.85);
        ctx.globalAlpha = alpha;
        ctx.strokeStyle = KIND_COLORS[e.kind] || "#aaa";
        ctx.lineWidth = manual ? 2.2 : focused ? 1.8 : 1.1;
        ctx.setLineDash(!manual && e.confidence < 0.35 ? [3, 4] : []);
        const mx = (x1 + x2) / 2 - (y2 - y1) * 0.12;
        const my = (y1 + y2) / 2 + (x2 - x1) * 0.12;
        ctx.beginPath();
        ctx.moveTo(x1, y1);
        if (manual) ctx.lineTo(x2, y2);
        else ctx.quadraticCurveTo(mx, my, x2, y2);
        ctx.stroke();
        ctx.setLineDash([]);
        if (manual) {
          const cx = (x1 + x2) / 2;
          const cy = (y1 + y2) / 2;
          ctx.fillStyle = "#fff";
          ctx.beginPath();
          ctx.moveTo(cx, cy - 4);
          ctx.lineTo(cx + 4, cy);
          ctx.lineTo(cx, cy + 4);
          ctx.lineTo(cx - 4, cy);
          ctx.closePath();
          ctx.fill();
        }
        if (e.kind === "explicit_reference" && (focused || cam.scale * a.r > 6)) {
          // Arrow head: references are directed.
          const ang = Math.atan2(y2 - my, x2 - mx);
          const rr = Math.max(3, b.r * cam.scale);
          const hx = x2 - Math.cos(ang) * rr;
          const hy = y2 - Math.sin(ang) * rr;
          ctx.fillStyle = KIND_COLORS.explicit_reference;
          ctx.beginPath();
          ctx.moveTo(hx, hy);
          ctx.lineTo(hx - 7 * Math.cos(ang - 0.4), hy - 7 * Math.sin(ang - 0.4));
          ctx.lineTo(hx - 7 * Math.cos(ang + 0.4), hy - 7 * Math.sin(ang + 0.4));
          ctx.closePath();
          ctx.fill();
        }
      }
      ctx.globalAlpha = 1;
    }

    drawNodes(scene, posOf, dim) {
      const ctx = this.ctx;
      const cam = this.cam;
      const list = Array.from(scene.nodes.values()).sort((a, b) => b.r - a.r);
      for (const n of list) {
        if (scene.search && scene.search.positions.has(n.id)) continue;
        const p = posOf(n);
        const [sx, sy] = toScreen(cam, p.x, p.y);
        const r = n.r * cam.scale;
        if (sx < -r - 50 || sy < -r - 50 || sx > cam.width + r + 50 || sy > cam.height + r + 50) continue;
        const selected = scene.selected.has(n.id);
        const related = scene.neighbors && scene.neighbors.has(n.id);
        let alpha = n.state === "unobserved" ? 0.45 : 1;
        if (dim && !selected && !related && n.id !== scene.focus) alpha *= scene.search ? 0.12 : 0.35;
        ctx.globalAlpha = alpha;
        // Anchor of the node's glyph, hit area and selection ring.
        let gx = sx;
        let gy = sy;
        let gr = r;
        if (n.kind === "directory") {
          const rr = Math.max(1.5, r);
          ctx.strokeStyle = "rgba(140,170,230,0.55)";
          ctx.fillStyle = "rgba(60,90,160,0.06)";
          ctx.lineWidth = 1;
          ctx.beginPath();
          ctx.arc(sx, sy, rr, 0, Math.PI * 2);
          ctx.fill();
          if (rr > 3) ctx.stroke();
          // Glyph of the directory: the centre of a large disc belongs to its largest child, so
          // there the glyph sits on the top of the rim.
          const onRim = rr > 20;
          const gs = onRim ? 4 : Math.max(1.5, Math.min(rr * 0.12, 6));
          if (onRim) gy = sy - rr;
          gr = onRim ? 8 : Math.min(rr * 0.25, 18);
          ctx.fillStyle = "#a9c4ff";
          ctx.beginPath();
          ctx.arc(gx, gy, gs, 0, Math.PI * 2);
          ctx.fill();
          this.hitList.push({ id: n.id, sx: gx, sy: gy, r: Math.max(6, gr), dir: true });
          if (rr > 22) this.label(n.name + "/", gx, gy - gs - 6, alpha, "#c9d8ff", 600, 12);
        } else {
          const rr = Math.max(0.9, r);
          ctx.fillStyle = extColor(n.ext);
          ctx.beginPath();
          ctx.arc(sx, sy, rr, 0, Math.PI * 2);
          ctx.fill();
          if (rr > 4) {
            ctx.strokeStyle = "rgba(255,255,255,0.25)";
            ctx.lineWidth = 1;
            ctx.stroke();
          }
          this.hitList.push({ id: n.id, sx, sy, r: Math.max(5, rr + 2) });
          if (rr > 6 || selected) this.label(n.name, sx, sy + rr + 11, alpha, "#e6ebf7", 400, 11);
        }
        if (n.pinned) {
          ctx.fillStyle = "#ffcf6e";
          ctx.fillRect(gx + Math.max(3, gr * 0.7), gy - Math.max(3, gr * 0.7) - 3, 3, 3);
        }
        if (selected || n.id === scene.focus) {
          ctx.globalAlpha = 1;
          ctx.strokeStyle = "#ffcf6e";
          ctx.lineWidth = 2;
          ctx.beginPath();
          ctx.arc(gx, gy, Math.max(5, gr + 4), 0, Math.PI * 2);
          ctx.stroke();
          if (n.kind === "directory" && r > 20) {
            // Also outline the whole disc, which is what the selection covers.
            ctx.globalAlpha = 0.5;
            ctx.lineWidth = 1;
            ctx.beginPath();
            ctx.arc(sx, sy, r, 0, Math.PI * 2);
            ctx.stroke();
          }
        }
        if (scene.hover === n.id && !selected) {
          ctx.globalAlpha = 1;
          ctx.strokeStyle = "rgba(124,196,255,0.9)";
          ctx.lineWidth = 1;
          ctx.beginPath();
          ctx.arc(gx, gy, Math.max(4, gr + 2), 0, Math.PI * 2);
          ctx.stroke();
          this.label(n.name, gx, gy - Math.max(8, gr) - 8, 1, "#ffffff", 600, 12, true);
        }
      }
      ctx.globalAlpha = 1;
    }

    drawSearchNodes(scene, posOf) {
      const ctx = this.ctx;
      const cam = this.cam;
      const s = scene.search;
      // Dark backdrop separates the attracted group from the (dimmed) scene behind it.
      let bx = 0;
      let by = 0;
      let count = 0;
      for (const p of s.positions.values()) {
        bx += p.x;
        by += p.y;
        count++;
      }
      if (count) {
        bx /= count;
        by /= count;
        let br = 0;
        for (const p of s.positions.values()) br = Math.max(br, Math.hypot(p.x - bx, p.y - by));
        const [cx, cy] = toScreen(cam, bx, by);
        const rr = Math.min(Math.max(br * cam.scale + 60, 80), Math.max(cam.width, cam.height));
        const g = ctx.createRadialGradient(cx, cy, rr * 0.4, cx, cy, rr);
        g.addColorStop(0, "rgba(5,7,13,0.78)");
        g.addColorStop(1, "rgba(5,7,13,0)");
        ctx.globalAlpha = 1;
        ctx.fillStyle = g;
        ctx.beginPath();
        ctx.arc(cx, cy, rr, 0, Math.PI * 2);
        ctx.fill();
      }
      // Tethers from temporary to persistent positions remind where things really live.
      for (const [id, p] of s.positions) {
        const n = s.nodes.get(id);
        if (!n) continue;
        const [sx, sy] = toScreen(cam, p.x, p.y);
        const [ox, oy] = toScreen(cam, n.x, n.y);
        ctx.globalAlpha = 0.18;
        ctx.strokeStyle = "#7cc4ff";
        ctx.setLineDash([2, 5]);
        ctx.beginPath();
        ctx.moveTo(ox, oy);
        ctx.lineTo(sx, sy);
        ctx.stroke();
        ctx.setLineDash([]);
      }
      // Result → neighbour lines.
      for (const r of s.results) {
        const a = s.positions.get(r.entity_id);
        if (!a) continue;
        const [x1, y1] = toScreen(cam, a.x, a.y);
        r.relation_neighbors.forEach((nid, k) => {
          const b = s.positions.get(nid);
          if (!b) return;
          const [x2, y2] = toScreen(cam, b.x, b.y);
          const kinds = r.neighbor_kinds[k] || [];
          ctx.strokeStyle = KIND_COLORS[kinds[0]] || "#888";
          ctx.globalAlpha = 0.25 + 0.6 * (r.neighbor_scores[k] || 0);
          ctx.lineWidth = 1.2;
          ctx.beginPath();
          ctx.moveTo(x1, y1);
          ctx.lineTo(x2, y2);
          ctx.stroke();
        });
      }
      for (const [id, p] of s.positions) {
        const n = s.nodes.get(id);
        if (!n) continue;
        const isResult = s.ids.has(id);
        const [sx, sy] = toScreen(cam, p.x, p.y);
        const rr = isResult ? 6 + 6 * (s.score.get(id) || 0) : 4;
        ctx.globalAlpha = 1;
        ctx.fillStyle = n.kind === "directory" ? "#a9c4ff" : extColor(n.ext);
        ctx.beginPath();
        ctx.arc(sx, sy, rr, 0, Math.PI * 2);
        ctx.fill();
        if (isResult) {
          ctx.strokeStyle = "#ffcf6e";
          ctx.lineWidth = 1.5;
          ctx.stroke();
        }
        if (n.pinned) {
          ctx.fillStyle = "#ffcf6e";
          ctx.fillRect(sx + rr, sy - rr - 3, 3, 3);
        }
        this.hitList.push({ id, sx, sy, r: rr + 3 });
        this.label(n.name, sx, sy + rr + 11, isResult ? 1 : 0.7, isResult ? "#ffffff" : "#b8c2da", isResult ? 600 : 400, 11);
      }
      ctx.globalAlpha = 1;
    }

    /** Project mode: each group of outside entities (another project, or a folder outside every
     * project) is a planet on an orbit around the project's disc, kept inside the viewport, with
     * one line per relation from the inside entity that carries it. */
    drawProjectLinks(scene, posOf) {
      const pv = scene.project;
      if (!pv || !pv.groups.length || scene.search) return;
      const ctx = this.ctx;
      const cam = this.cam;
      const [rx, ry] = toScreen(cam, pv.root.x, pv.root.y);
      const rr = pv.root.r * cam.scale;
      const orbit = rr + 70;
      const m = 48;
      const groups = pv.groups;
      const hl = pv.highlight;
      ctx.save();
      groups.forEach((g, i) => {
        const a = -Math.PI / 2 + (i / groups.length) * Math.PI * 2;
        const gx = Math.min(cam.width - m, Math.max(m, rx + orbit * Math.cos(a)));
        const gy = Math.min(cam.height - m - 40, Math.max(m + 10, ry + orbit * Math.sin(a)));
        const lit = !hl || g.links.some((l) => l.via.some((v) => hl.has(v)));
        const size = 7 + 2.5 * Math.sqrt(g.links.length + (g.relative ? 3 : 0));
        // Lines from the inside entities (when loaded) to the planet.
        for (const l of g.links) {
          const kind = l.kinds.find((k) => k !== "name_similarity" && k !== "temporal_affinity") || l.kinds[0];
          for (const v of l.via.slice(0, 4)) {
            const n = scene.nodes.get(v);
            if (!n) continue;
            const on = !hl || hl.has(v);
            const p = posOf(n);
            const [sx, sy] = toScreen(cam, p.x, p.y);
            ctx.globalAlpha = on ? 0.25 + 0.6 * l.score : 0.06;
            ctx.strokeStyle = KIND_COLORS[kind] || "#ccc";
            ctx.lineWidth = on && hl ? 2 : 1.2;
            ctx.setLineDash(l.kinds.includes("explicit_reference") || l.kinds.includes("verified_duplicate") ? [] : [4, 4]);
            ctx.beginPath();
            ctx.moveTo(sx, sy);
            ctx.quadraticCurveTo((sx + gx) / 2 + (gy - sy) * 0.12, (sy + gy) / 2 - (gx - sx) * 0.12, gx, gy);
            ctx.stroke();
          }
        }
        ctx.setLineDash([]);
        const first = g.links[0];
        const color = first ? KIND_COLORS[first.kinds.find((k) => k !== "name_similarity" && k !== "temporal_affinity") || first.kinds[0]] || "#ccc" : KIND_COLORS.project_membership;
        if (g.relative) {
          // Kinship: a dashed green halo and a thread to the project's disc.
          ctx.globalAlpha = lit ? 0.5 : 0.15;
          ctx.strokeStyle = KIND_COLORS.project_membership;
          ctx.lineWidth = 1.5;
          ctx.setLineDash([3, 4]);
          ctx.beginPath();
          ctx.moveTo(gx, gy);
          const d = Math.hypot(gx - rx, gy - ry) || 1;
          ctx.lineTo(rx + ((gx - rx) * Math.min(rr, d)) / d, ry + ((gy - ry) * Math.min(rr, d)) / d);
          ctx.stroke();
          ctx.globalAlpha = lit ? 0.9 : 0.3;
          ctx.beginPath();
          ctx.arc(gx, gy, size + 5, 0, Math.PI * 2);
          ctx.stroke();
          ctx.setLineDash([]);
        }
        ctx.globalAlpha = lit ? 0.95 : 0.3;
        ctx.fillStyle = "rgba(5,7,13,0.9)";
        ctx.strokeStyle = color;
        ctx.lineWidth = 2;
        ctx.beginPath();
        ctx.arc(gx, gy, size, 0, Math.PI * 2);
        ctx.fill();
        ctx.stroke();
        this.hitList.push({ id: g.id, sx: gx, sy: gy, r: size + 4, group: g, satellite: true });
        const name = g.is_project ? g.name : g.name + " (fuori dai progetti)";
        const kin = g.relative
          ? g.relative.came_first === true ? "↑ deriva da " : g.relative.came_first === false ? "↓ origine di " : "~ "
          : "";
        const count = g.links.length ? ` · ${g.links.length}` : "";
        this.label(`${kin}${name}${count}`, gx, gy + size + (g.relative ? 19 : 14), lit ? 0.95 : 0.4, g.relative ? "#bff3d4" : g.is_project ? "#e6ebf7" : "#b8c2da", 600, 12, true);
      });
      ctx.restore();
    }

    drawSatellites(scene, posOf) {
      if (!scene.satellites || !scene.satellites.length || scene.focus == null || scene.search) return;
      const host = scene.nodes.get(scene.focus);
      if (!host) return;
      const ctx = this.ctx;
      const cam = this.cam;
      const hp = posOf(host);
      const [hx, hy] = toScreen(cam, hp.x, hp.y);
      const hostR = host.kind === "directory" ? host.r * cam.scale : Math.max(6, host.r * cam.scale);
      const orbit = Math.max(70, Math.min(hostR + 60, 260));
      const sats = scene.satellites.slice(0, 12);
      ctx.save();
      ctx.strokeStyle = "rgba(255,207,110,0.18)";
      ctx.setLineDash([1, 5]);
      ctx.beginPath();
      ctx.arc(hx, hy, orbit, 0, Math.PI * 2);
      ctx.stroke();
      sats.forEach((s, i) => {
        const a = -Math.PI / 2 + (i / sats.length) * Math.PI * 2;
        const gx = hx + orbit * Math.cos(a);
        const gy = hy + orbit * Math.sin(a);
        // Tether to the real location: satellites are not children of this directory.
        if (s.x != null) {
          const [rx, ry] = toScreen(cam, s.x, s.y);
          ctx.globalAlpha = 0.35;
          ctx.strokeStyle = KIND_COLORS[s.kinds[0]] || "#999";
          ctx.setLineDash([4, 4]);
          ctx.beginPath();
          ctx.moveTo(gx, gy);
          ctx.lineTo(rx, ry);
          ctx.stroke();
        }
        ctx.setLineDash([]);
        ctx.globalAlpha = 0.95;
        ctx.strokeStyle = KIND_COLORS[s.kinds[0]] || "#ccc";
        ctx.lineWidth = 1.5;
        ctx.fillStyle = "rgba(5,7,13,0.85)";
        ctx.beginPath();
        ctx.arc(gx, gy, 6 + 5 * s.score, 0, Math.PI * 2);
        ctx.fill();
        ctx.stroke();
        this.hitList.push({ id: s.entity_id, sx: gx, sy: gy, r: 10, satellite: true });
        this.label("↗ " + s.name, gx, gy + 18, 0.95, "#ffe6ad", 400, 11, true);
      });
      ctx.restore();
    }

    /** Draws a label unless it overlaps an already drawn one (zero-clutter). */
    label(text, x, y, alpha, color, weight, size, force) {
      const ctx = this.ctx;
      ctx.font = `${weight || 400} ${size || 11}px Inter, "Noto Sans", sans-serif`;
      const t = truncate(ctx, text, 180);
      const w = ctx.measureText(t).width;
      const box = [x - w / 2 - 2, y - (size || 11), x + w / 2 + 2, y + 3];
      if (!force) {
        for (const b of this.labelBoxes) {
          if (box[0] < b[2] && box[2] > b[0] && box[1] < b[3] && box[3] > b[1]) return;
        }
      }
      this.labelBoxes.push(box);
      ctx.globalAlpha = alpha;
      ctx.fillStyle = "rgba(5,7,13,0.65)";
      ctx.fillRect(box[0], box[1], box[2] - box[0], box[3] - box[1]);
      ctx.fillStyle = color;
      ctx.textAlign = "center";
      ctx.fillText(t, x, y);
      ctx.globalAlpha = 1;
    }

    /** Topmost hit at screen point (satellites and search nodes first). */
    hit(sx, sy) {
      if (!this.hitList) return null;
      let best = null;
      for (let i = this.hitList.length - 1; i >= 0; i--) {
        const h = this.hitList[i];
        const d = Math.hypot(h.sx - sx, h.sy - sy);
        if (d <= h.r && (!best || h.satellite || (!best.satellite && d < best.d))) {
          best = Object.assign({ d }, h);
          if (h.satellite) break;
        }
      }
      return best;
    }
  }

  const api = {
    KIND_COLORS,
    KIND_LABELS,
    EXT_GROUPS,
    extColor,
    makeCamera,
    toScreen,
    toWorld,
    viewBounds,
    zoomAt,
    fitBounds,
    gravityTargets,
    GravitySession,
    Renderer,
  };
  root.OrbitRender = api;
  if (typeof module !== "undefined" && module.exports) module.exports = api;
})(typeof window !== "undefined" ? window : globalThis);
