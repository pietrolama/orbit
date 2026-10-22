/* ORBIT UI controller. All user-controlled strings (file names, paths, reasons) are inserted with
 * textContent, never as HTML. The UI never writes temporary (search/animation) coordinates. */
(function () {
  "use strict";
  const R = window.OrbitRender;
  const $ = (id) => document.getElementById(id);

  // -------------------------------------------------------------------------------------------
  // Session bootstrap: single-use launch code (URL fragment) → session token (sessionStorage).
  // -------------------------------------------------------------------------------------------
  let TOKEN = null;
  try {
    TOKEN = sessionStorage.getItem("orbit-token");
  } catch (_) {
    TOKEN = null;
  }

  async function bootstrap() {
    const m = /launch=([0-9a-f]{64})/.exec(location.hash || "");
    if (m) {
      history.replaceState(null, "", "/");
      const res = await fetch("/api/session", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ launch_code: m[1] }),
      });
      if (res.ok) {
        TOKEN = (await res.json()).token;
        try {
          sessionStorage.setItem("orbit-token", TOKEN);
        } catch (_) {
          /* private mode: the token stays in memory for this page */
        }
      }
    }
    if (!TOKEN) {
      showFatal("Sessione non valida o scaduta. Avvia di nuovo ORBIT (orbit ui) e usa l'indirizzo stampato nel terminale: è valido una sola volta.");
      return false;
    }
    return true;
  }

  function showFatal(msg) {
    const e = $("empty");
    e.classList.remove("hidden");
    setKids(e, el("h2", {}, "ORBIT"), el("p", {}, msg));
  }

  // -------------------------------------------------------------------------------------------
  // API
  // -------------------------------------------------------------------------------------------
  async function api(method, path, body, signal) {
    const opts = { method, headers: { "X-Orbit-Token": TOKEN }, signal };
    if (body !== undefined) {
      opts.headers["Content-Type"] = "application/json";
      opts.body = JSON.stringify(body);
    }
    const res = await fetch(path, opts);
    let data = null;
    try {
      data = await res.json();
    } catch (_) {
      data = null;
    }
    if (!res.ok) throw new Error((data && data.error) || `HTTP ${res.status}`);
    return data;
  }
  const GET = (p, s) => api("GET", p, undefined, s);
  const POST = (p, b) => api("POST", p, b || {});

  // -------------------------------------------------------------------------------------------
  // DOM helpers
  // -------------------------------------------------------------------------------------------
  function el(tag, attrs, ...children) {
    const e = document.createElement(tag);
    for (const k in attrs || {}) {
      const v = attrs[k];
      if (k === "class") e.className = v;
      else if (k === "onclick") e.addEventListener("click", v);
      else if (k === "style") Object.assign(e.style, v);
      else if (k === "title") e.title = v;
      else if (v !== undefined && v !== null && v !== false) e.setAttribute(k, v === true ? "" : v);
    }
    for (const c of children.flat(Infinity)) {
      if (c === null || c === undefined || c === false) continue;
      e.append(c instanceof Node ? c : document.createTextNode(String(c)));
    }
    return e;
  }

  /** Replaces the children of `node`, flattening nested arrays and skipping null/false. */
  function setKids(node, ...items) {
    node.replaceChildren(...items.flat(Infinity).filter((c) => c !== null && c !== undefined && c !== false));
  }

  function bytes(b) {
    if (b == null) return "?";
    const u = ["B", "KiB", "MiB", "GiB", "TiB"];
    let v = b;
    let i = 0;
    while (v >= 1024 && i < u.length - 1) {
      v /= 1024;
      i++;
    }
    return i === 0 ? `${b} B` : `${v.toFixed(1)} ${u[i]}`;
  }

  function date(secs) {
    if (!secs) return "?";
    return new Date(secs * 1000).toLocaleString("it-IT");
  }

  function toast(msg, opts) {
    const o = opts || {};
    const t = el("div", { class: "toast" + (o.error ? " error" : "") }, el("span", {}, msg));
    if (o.action) {
      t.append(
        el("button", {
          onclick: () => {
            o.action.run();
            t.remove();
          },
        }, o.action.label)
      );
    }
    $("toasts").append(t);
    setTimeout(() => t.remove(), o.ms || (o.action ? 8000 : 4000));
  }

  function fail(e) {
    toast(e.message || String(e), { error: true });
  }

  // -------------------------------------------------------------------------------------------
  // State
  // -------------------------------------------------------------------------------------------
  const ALL_KINDS = ["name_similarity", "temporal_affinity", "fingerprint_match", "verified_duplicate", "explicit_reference", "manual_association"];
  const state = {
    mode: "explore",
    overview: null,
    nodes: new Map(),
    edges: [],
    graphVersion: 0,
    selected: new Set(),
    focus: null,
    detail: null,
    neighbors: new Set(),
    satellites: [],
    kindsOn: new Set(ALL_KINDS.concat(["parent_child", "project_membership"])),
    search: null, // {query, results, nodes: Map, ids:Set, score: Map, session, positions}
    hover: null,
    dragging: null,
    linkFrom: null,
    fitScale: 1,
    job: null,
    // Project mode: only this subtree is drawn, its outside relations orbit around it.
    project: null,
    projectView: null,
    showWeak: false,
  };
  const renderer = new R.Renderer($("canvas"));
  let dirty = true;
  const invalidate = () => {
    dirty = true;
  };

  function zoomRatio() {
    return renderer.cam.scale / (state.fitScale || 1);
  }

  function lod() {
    const z = zoomRatio();
    if (z < 3) return "galaxy";
    if (state.focus != null) {
      const n = state.nodes.get(state.focus);
      if (n && n.kind !== "directory" && n.r * renderer.cam.scale > 14) return "file";
    }
    return "system";
  }

  const LOD_LABEL = { galaxy: "Livello 1 · Galassia", system: "Livello 2 · Sistema", file: "Livello 3 · File" };

  function frame() {
    const now = performance.now();
    if (state.search && state.search.session) {
      const s = state.search;
      s.positions = s.session.positions(now);
      if (s.session.animating(now)) dirty = true;
      if (s.session.finished) {
        state.search = null;
        $("search-hint").classList.add("hidden");
        dirty = true;
      }
    }
    if (dirty) {
      dirty = false;
      const l = lod();
      $("lod").textContent = LOD_LABEL[l];
      renderer.draw({
        overview: state.overview,
        nodes: state.nodes,
        edges: state.edges,
        lod: l,
        zoomRatio: zoomRatio(),
        selected: state.selected,
        focus: state.focus,
        neighbors: state.neighbors,
        satellites: state.satellites,
        kindsOn: state.kindsOn,
        search: state.search && state.search.positions ? state.search : null,
        dragging: state.dragging,
        hover: state.hover,
        project: state.project != null && state.projectView ? Object.assign({ highlight: projectHighlight() }, state.projectView) : null,
      });
    }
    requestAnimationFrame(frame);
  }

  // -------------------------------------------------------------------------------------------
  // Data loading
  // -------------------------------------------------------------------------------------------
  // Leaves room around a project's disc for the orbit of its outside relations.
  const fitPadding = () => (state.project != null ? 0.62 : 0.9);

  async function loadOverview(fit) {
    const within = state.project != null ? `&within=${state.project}` : "";
    state.overview = await GET("/api/graph/overview?max_clusters=300&max_flows=250" + within);
    const b = state.overview.bounds;
    $("empty").classList.toggle("hidden", state.overview.total_nodes > 0);
    if (b) {
      const cam = renderer.cam;
      const saved = { cx: cam.cx, cy: cam.cy, scale: cam.scale };
      state.fitScale = R.fitBounds(cam, b, fitPadding());
      if (!fit) Object.assign(cam, saved);
    }
    renderLegend();
    invalidate();
    scheduleRegion();
  }

  let regionTimer = null;
  let regionAbort = null;
  function scheduleRegion() {
    clearTimeout(regionTimer);
    regionTimer = setTimeout(loadRegion, 110);
  }

  async function loadRegion() {
    if (!state.overview || !state.overview.total_nodes) return;
    if (regionAbort) regionAbort.abort();
    regionAbort = new AbortController();
    const cam = renderer.cam;
    const b = R.viewBounds(cam, 0.25);
    const kinds = ALL_KINDS.filter((k) => state.kindsOn.has(k)).join(",");
    const q = new URLSearchParams({
      x0: b.x0, y0: b.y0, x1: b.x1, y1: b.y1, scale: cam.scale,
      min_px: zoomRatio() < 3 ? 2.5 : 0.7, max_nodes: 2500, max_edges: 1500, kinds,
    });
    if (state.focus != null) q.set("focus", state.focus);
    if (state.project != null) q.set("within", state.project);
    try {
      const r = await GET("/api/graph/region?" + q.toString(), regionAbort.signal);
      state.nodes = new Map(r.nodes.map((n) => [n.id, n]));
      state.edges = r.edges;
      invalidate();
    } catch (e) {
      if (e.name !== "AbortError") fail(e);
    }
  }

  async function select(id, opts) {
    const o = opts || {};
    if (o.additive) {
      if (state.selected.has(id)) state.selected.delete(id);
      else state.selected.add(id);
      updateSelectionBar();
      invalidate();
      return;
    }
    state.selected = new Set([id]);
    state.focus = id;
    updateSelectionBar();
    try {
      const d = await GET("/api/graph/entity?id=" + id);
      if (state.focus !== id) return;
      state.detail = d;
      state.neighbors = new Set(d.neighbors.map((n) => n.entity_id));
      state.satellites = state.project != null ? [] : d.satellites;
      // In project mode an outside entity is shown in the panel only (it is not drawn).
      const outside = state.project != null && !d.breadcrumb.some((b) => b.id === state.project);
      renderPanel(d, outside ? d.breadcrumb[1] : null);
      renderBreadcrumb(d.breadcrumb);
      if (o.fly && d.position && !outside) flyTo(d.position.x, d.position.y, o.zoomTo ? d.position.radius : null);
      scheduleRegion();
    } catch (e) {
      fail(e);
    }
    invalidate();
  }

  function clearSelection() {
    state.selected.clear();
    state.focus = null;
    state.detail = null;
    state.neighbors = new Set();
    state.satellites = [];
    $("panel").classList.add("hidden");
    document.body.classList.remove("panel-open");
    if (state.projectView) renderBreadcrumb(state.projectView.breadcrumb);
    else setKids($("breadcrumb"));
    updateSelectionBar();
    invalidate();
  }

  function flyTo(x, y, radius) {
    const cam = renderer.cam;
    const from = { cx: cam.cx, cy: cam.cy, scale: cam.scale };
    let target = cam.scale;
    if (radius) target = Math.min(cam.width, cam.height) * 0.35 / Math.max(radius, 0.5);
    const t0 = performance.now();
    const dur = 550;
    function step() {
      const p = Math.min(1, (performance.now() - t0) / dur);
      const e = p < 0.5 ? 2 * p * p : 1 - Math.pow(-2 * p + 2, 2) / 2;
      cam.cx = from.cx + (x - from.cx) * e;
      cam.cy = from.cy + (y - from.cy) * e;
      cam.scale = Math.exp(Math.log(from.scale) + (Math.log(target) - Math.log(from.scale)) * e);
      invalidate();
      if (p < 1) requestAnimationFrame(step);
      else scheduleRegion();
    }
    step();
  }

  // -------------------------------------------------------------------------------------------
  // Panel (File level)
  // -------------------------------------------------------------------------------------------
  function kindChip(kind, label) {
    return el("span", { class: "rel-kind", style: { background: R.KIND_COLORS[kind] || "#aaa" } }, label);
  }

  function renderPanel(d, otherProject) {
    const n = d.node;
    const body = $("panel-body");
    const kindLabel = { directory: "directory", file: "file", symlink: "link simbolico", other: "altro" }[n.kind] || n.kind;
    const stateBadge =
      n.state === "present" ? null : el("span", { class: "badge warn" }, n.state === "unobserved" ? "non osservato" : n.state);
    const pinned = d.position && d.position.pinned;
    const actions = el(
      "div",
      { class: "actions" },
      n.kind === "file" ? el("button", { class: "primary", onclick: () => openEntity(n.entity_id, "open") }, "Apri") : null,
      el("button", { onclick: () => openEntity(n.entity_id, "reveal") }, n.kind === "directory" ? "Apri nel file manager" : "Mostra nella cartella"),
      d.position ? el("button", { onclick: () => setPinned([n.entity_id], !pinned) }, pinned ? "Sblocca" : "Fissa") : null,
      d.position && (pinned || d.position.source === "manual")
        ? el("button", { title: "Restituisce il nodo al layout automatico", onclick: () => releasePositions([n.entity_id]) }, "Layout automatico")
        : null,
      el("button", { onclick: () => startLink(n.entity_id) }, "Collega a…"),
      el("button", { onclick: () => searchRelated(n.entity_id) }, "Attira i correlati")
    );
    const kv = el(
      "dl",
      { class: "kv" },
      el("dt", {}, "tipo"), el("dd", {}, kindLabel),
      el("dt", {}, n.kind === "directory" ? "dimensione (albero)" : "dimensione"), el("dd", {}, bytes(n.kind === "directory" ? n.subtree_apparent_bytes : n.apparent_bytes)),
      n.kind === "directory" ? [el("dt", {}, "file contenuti"), el("dd", {}, String(n.subtree_files_count))] : null,
      el("dt", {}, "modificato"), el("dd", {}, date(n.mtime_sec)),
      el("dt", {}, "entità"), el("dd", { class: "mono" }, "#" + n.entity_id),
      n.nlink > 1 ? [el("dt", {}, "hard link"), el("dd", {}, String(n.nlink))] : null,
      d.fingerprint
        ? [
            el("dt", {}, "fingerprint"),
            el("dd", { class: "mono" }, d.fingerprint.sample_hash.slice(0, 16) + "… ",
              el("span", { class: "badge" + (d.fingerprint.valid_for_current_version ? "" : " bad") },
                d.fingerprint.full_hash || d.fingerprint.sample_covers_full_file ? "contenuto verificato" : "solo campione")),
          ]
        : null
    );

    const relEls = d.relations.map((r) => {
      const active = r.state === "active";
      const buttons = [];
      if (active && r.origin === "auto") buttons.push(el("button", { onclick: () => edgeDelete(r.edge_id, false) }, "Nascondi"));
      if (active && r.origin === "manual") buttons.push(el("button", { class: "danger", onclick: () => edgeDelete(r.edge_id, true) }, "Elimina"));
      if (!active && (r.state === "hidden" || r.state === "deleted")) buttons.push(el("button", { onclick: () => edgeRestore(r.edge_id) }, "Ripristina"));
      return el(
        "div",
        { class: "rel" + (active ? "" : " inactive") },
        el(
          "div",
          { class: "rel-head" },
          kindChip(r.kind, r.kind_label),
          el("span", { class: "rel-other", title: r.other_path, onclick: () => select(r.other_id, { fly: true }) },
            (r.direction === "incoming" ? "← " : r.direction === "outgoing" ? "→ " : "") + r.other_name),
          r.elsewhere ? el("span", { class: "badge", title: "In un'altra directory" }, "altrove") : null,
          r.origin === "manual" ? el("span", { class: "badge" }, "manuale") : null,
          !active ? el("span", { class: "badge warn" }, r.state === "hidden" ? "nascosta" : r.state === "deleted" ? "eliminata" : r.state) : null
        ),
        el("ul", {}, r.reasons.map((x) => el("li", {}, x))),
        el("div", { class: "bar", title: `peso ${r.weight.toFixed(2)} · confidenza ${r.confidence.toFixed(2)}` }, el("span", { style: { width: Math.round(r.strength * 100) + "%" } })),
        el("div", { class: "muted mono", style: { fontSize: "11px" } }, `peso ${r.weight.toFixed(2)} · confidenza ${r.confidence.toFixed(2)} · ${r.provenance}`),
        buttons.length ? el("div", { class: "rel-actions" }, buttons) : null
      );
    });

    const sats = d.satellites.length
      ? el("div", {}, el("h3", {}, `Satelliti (${d.satellites.length}) — correlati che vivono altrove`),
          d.satellites.map((s) => el("div", { class: "rel" },
            el("div", { class: "rel-head" }, kindChip(s.kinds[0], R.KIND_LABELS[s.kinds[0]] || s.kinds[0]),
              el("span", { class: "rel-other", title: s.path, onclick: () => select(s.entity_id, { fly: true }) }, s.name),
              el("span", { class: "badge" }, s.score.toFixed(2))),
            el("div", { class: "path" }, s.path))))
      : null;

    const history = el("div", {}, el("h3", {}, `Cronologia (${d.observations.length} osservazioni)`),
      el("ul", { class: "timeline" }, d.observations.slice(-12).reverse().map((o) =>
        el("li", {}, el("span", { class: "muted" }, new Date(o.snapshot_created_at_unix_ms).toLocaleString("it-IT") + " · "),
          matchLabel(o.identity_match), ` (${o.identity_confidence.toFixed(2)}) `, el("div", { class: "path" }, o.path_display)))),
      d.identity_candidates.length
        ? el("div", {}, el("h3", {}, "Identità incerte"), d.identity_candidates.map((c) =>
            el("div", { class: "note" }, `Possibile stesso file di #${c.candidate_entity_id} (${c.confidence.toFixed(2)}): ${c.reason} — `,
              el("span", { class: "path" }, c.candidate_path_display))))
        : null);

    const consts = d.constellations.length
      ? el("div", {}, el("h3", {}, "Costellazioni"), d.constellations.map((c) =>
          el("div", { class: "rel" }, el("div", { class: "rel-head" }, kindChip("project_membership", "costellazione"),
            el("span", { class: "rel-other" }, c.name),
            el("button", { onclick: () => removeFromConstellation(c.id, n.entity_id) }, "Rimuovi")))))
      : null;

    const preview = d.preview !== "none" ? el("div", { class: "preview", id: "preview" }, el("span", { class: "muted" }, "Anteprima in caricamento…")) : null;

    setKids(body, 
      el("h2", {}, n.name || "/", stateBadge),
      el("div", { class: "path" }, n.path_display),
      otherProject
        ? el("div", { class: "note" }, "Fuori dal progetto corrente. ",
            otherProject.id !== d.node.entity_id
              ? el("button", { onclick: () => enterProject(otherProject.id) }, `Apri il progetto «${otherProject.name}»`)
              : null)
        : null,
      actions,
      el("h3", {}, "Metadati"),
      kv,
      preview ? [el("h3", {}, "Anteprima locale"), preview] : null,
      el("h3", {}, `Relazioni (${d.relations.length})`),
      relEls.length ? relEls : el("div", { class: "note" }, "Nessuna relazione oltre alla struttura fisica."),
      sats,
      consts,
      history
    );
    $("panel").classList.remove("hidden");
    document.body.classList.add("panel-open");
    if (preview) loadPreview(n.entity_id, d.preview);
  }

  function matchLabel(m) {
    return {
      new: "nuova entità",
      same_path: "stesso percorso",
      same_path_replaced: "stesso percorso, file sostituito",
      moved: "spostato",
      moved_modified: "spostato e modificato",
      moved_by_content_metadata: "spostato (stessi metadati, inode diverso)",
    }[m] || m;
  }

  async function loadPreview(id, kind) {
    const box = $("preview");
    if (!box) return;
    const url = `/api/preview?id=${id}&token=${encodeURIComponent(TOKEN)}`;
    if (kind === "image") {
      const img = el("img", { alt: "anteprima" });
      img.addEventListener("error", () => setKids(box, el("span", { class: "muted" }, "Anteprima non disponibile.")));
      img.src = url;
      setKids(box, img);
      return;
    }
    try {
      const r = await GET(url);
      setKids(box, el("pre", {}, r.text), r.truncated ? el("div", { class: "note" }, "(prime righe)") : null);
    } catch (e) {
      setKids(box, el("span", { class: "muted" }, e.message));
    }
  }

  function renderBreadcrumb(items) {
    const nav = $("breadcrumb");
    const parts = [];
    items.forEach((b, i) => {
      if (i) parts.push(el("span", { class: "sep" }, "/"));
      parts.push(el("button", { onclick: () => select(b.id, { fly: true, zoomTo: true }) }, b.name || "/"));
    });
    setKids(nav, ...parts);
  }

  function renderLegend() {
    const lg = $("legend");
    const dots = R.EXT_GROUPS.map(([name, color]) => el("span", {}, el("span", { class: "dot", style: { background: color } }), name));
    setKids(lg, 
      el("div", { class: "row" }, el("span", {}, el("span", { class: "dot", style: { background: "transparent", border: "1px solid #8cf" } }), "directory"), ...dots),
      el("div", { class: "row" },
        el("span", {}, el("span", { class: "ln", style: { borderColor: "rgba(150,160,190,0.6)" } }), "struttura fisica"),
        el("span", {}, el("span", { class: "ln", style: { borderColor: "#b48cff", borderTopStyle: "dashed" } }), "automatica (tratteggio = debole)"),
        el("span", {}, el("span", { class: "ln", style: { borderColor: "#fff", borderTopWidth: "3px" } }), "manuale ◆")),
      el("div", { class: "note" }, "Galassia: dimensione = numero di file, luminosità = attività recente, flussi = relazioni aggregate.")
    );
  }

  function renderFilters() {
    const box = $("filters");
    setKids(box, 
      state.project != null
        ? el("button", {
            class: state.showWeak ? "on" : "",
            title: "Collegamenti con altri progetti basati solo su nomi simili o modifiche ravvicinate",
            onclick: () => {
              state.showWeak = !state.showWeak;
              renderFilters();
              loadProjectView().catch(fail);
            },
          }, el("span", { class: "swatch", style: { background: "#888", opacity: state.showWeak ? 1 : 0.25 } }),
            state.projectView && !state.showWeak ? `deboli (${state.projectView.weak_links})` : "deboli")
        : null,
      ...ALL_KINDS.map((k) =>
        el("button", {
          class: state.kindsOn.has(k) ? "on" : "",
          title: "Mostra/nascondi " + R.KIND_LABELS[k],
          onclick: () => {
            if (state.kindsOn.has(k)) state.kindsOn.delete(k);
            else state.kindsOn.add(k);
            renderFilters();
            scheduleRegion();
            invalidate();
          },
        }, el("span", { class: "swatch", style: { background: R.KIND_COLORS[k], opacity: state.kindsOn.has(k) ? 1 : 0.25 } }), R.KIND_LABELS[k])
      )
    );
  }

  // -------------------------------------------------------------------------------------------
  // Actions
  // -------------------------------------------------------------------------------------------
  async function openEntity(id, mode) {
    try {
      const r = await POST("/api/open-file", { entity_id: id, mode });
      if (r.opened) toast(mode === "open" ? "Aperto con l'applicazione predefinita" : "Cartella aperta");
      else toast(r.reason, { action: r.can_reveal ? { label: "Mostra nella cartella", run: () => openEntity(id, "reveal") } : null });
    } catch (e) {
      fail(e);
    }
  }

  async function setPinned(ids, pinned) {
    try {
      await POST("/api/positions/pin", { entity_ids: ids, pinned });
      toast(pinned ? "Fissato: il layout automatico non lo sposterà" : "Sbloccato");
      await refreshAfterChange(false);
    } catch (e) {
      fail(e);
    }
  }

  async function releasePositions(ids) {
    try {
      await POST("/api/positions/release", { entity_ids: ids });
      toast("Restituito al layout automatico (resta dov'è fino a un ricalcolo)");
      await refreshAfterChange(false);
    } catch (e) {
      fail(e);
    }
  }

  function startLink(id) {
    state.linkFrom = id;
    $("link-hint").classList.remove("hidden");
    $("canvas").classList.add("linking");
  }

  function stopLink() {
    state.linkFrom = null;
    $("link-hint").classList.add("hidden");
    $("canvas").classList.remove("linking");
  }

  async function finishLink(target) {
    const a = state.linkFrom;
    stopLink();
    if (a == null || a === target) return;
    try {
      const e = await POST("/api/relations/create", { a, b: target });
      toast("Associazione manuale creata", { action: { label: "Annulla", run: () => edgeDelete(e.id, true, true) } });
      await refreshAfterChange(false);
    } catch (err) {
      fail(err);
    }
  }

  async function edgeDelete(edgeId, manual, silent) {
    try {
      await POST("/api/relations/delete", { edge_id: edgeId });
      if (!silent) {
        toast(manual ? "Associazione eliminata" : "Relazione nascosta dalla vista", { action: { label: "Annulla", run: () => edgeRestore(edgeId) } });
      }
      await refreshAfterChange(false);
    } catch (e) {
      fail(e);
    }
  }

  async function edgeRestore(edgeId) {
    try {
      await POST("/api/relations/restore", { edge_id: edgeId });
      toast("Relazione ripristinata");
      await refreshAfterChange(false);
    } catch (e) {
      fail(e);
    }
  }

  async function removeFromConstellation(cid, id) {
    try {
      await POST("/api/constellations/update", { id: cid, remove: [id] });
      toast("Rimosso dalla costellazione", { action: { label: "Annulla", run: () => POST("/api/constellations/update", { id: cid, add: [id] }).then(() => refreshAfterChange(false)) } });
      await refreshAfterChange(false);
    } catch (e) {
      fail(e);
    }
  }

  async function refreshAfterChange(fit) {
    await loadOverview(fit);
    if (state.focus != null) await select(state.focus);
  }

  function updateSelectionBar() {
    const n = state.selected.size;
    $("selection-bar").classList.toggle("hidden", n < 2);
    $("selection-count").textContent = `${n} selezionati`;
  }

  $("constellation-create").addEventListener("click", async () => {
    const name = $("constellation-name").value.trim();
    if (!name) {
      $("constellation-name").focus();
      return;
    }
    try {
      const c = await POST("/api/constellations/create", { name, members: Array.from(state.selected) });
      $("constellation-name").value = "";
      toast(`Costellazione «${c.name}» creata`, { action: { label: "Annulla", run: () => POST("/api/constellations/update", { id: c.id, deleted: true }).then(() => refreshAfterChange(false)) } });
      await refreshAfterChange(false);
    } catch (e) {
      fail(e);
    }
  });
  $("selection-clear").addEventListener("click", clearSelection);
  $("panel-close").addEventListener("click", clearSelection);

  // -------------------------------------------------------------------------------------------
  // Gravitational search
  // -------------------------------------------------------------------------------------------
  let searchTimer = null;
  let searchSeq = 0;
  $("search").addEventListener("input", () => {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => runSearch($("search").value), 180);
  });
  $("search").addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      $("search").value = "";
      endSearch();
      $("search").blur();
    }
  });

  function searchRelated(id) {
    $("search").value = `related-to:${id}`;
    runSearch($("search").value);
  }

  async function runSearch(q) {
    const seq = ++searchSeq;
    if (!q.trim()) {
      endSearch();
      return;
    }
    let r;
    try {
      r = await POST("/api/search", { query: q, limit: 60 });
    } catch (e) {
      fail(e);
      return;
    }
    if (seq !== searchSeq) return; // superseded by a newer query
    const s = r.search;
    $("search-count").textContent = s.total_matches ? `${s.total_matches}${s.truncated ? "+" : ""} risultati` : "nessun risultato";
    for (const w of s.parsed.warnings) toast(w);
    const nodes = new Map(r.nodes.map((n) => [n.id, n]));
    // Persistent coordinates: the only source used to restore.
    const original = new Map();
    for (const n of r.nodes) original.set(n.id, { x: n.x, y: n.y, pinned: n.pinned });
    const cam = renderer.cam;
    const focal = { x: cam.cx, y: cam.cy };
    const targets = R.gravityTargets(s.results, original, focal, 1 / cam.scale);
    const now = performance.now();
    if (state.search && state.search.session && !state.search.session.finished) {
      // Keep the snapshot of persistent coordinates already taken; add new ids.
      for (const [id, p] of original) if (!state.search.session.original.has(id)) state.search.session.original.set(id, { x: p.x, y: p.y });
      state.search.session.retarget(targets, now);
    } else {
      state.search = { session: new R.GravitySession(original, targets, now, 650) };
    }
    Object.assign(state.search, {
      query: q,
      results: s.results,
      nodes,
      ids: new Set(s.results.map((x) => x.entity_id)),
      score: new Map(s.results.map((x) => [x.entity_id, x.match_score])),
    });
    // Ids no longer part of the result set animate back to their persistent position.
    for (const id of state.search.session.original.keys()) {
      if (!targets.has(id)) targets.set(id, state.search.session.original.get(id));
      if (!nodes.has(id)) {
        const n = state.nodes.get(id);
        if (n) nodes.set(id, n);
      }
    }
    $("search-hint").classList.remove("hidden");
    invalidate();
  }

  function endSearch() {
    searchSeq++;
    $("search-count").textContent = "";
    if (state.search && state.search.session) state.search.session.release(performance.now());
    invalidate();
  }

  // -------------------------------------------------------------------------------------------
  // Canvas interaction
  // -------------------------------------------------------------------------------------------
  const canvas = $("canvas");
  let pointer = null;

  canvas.addEventListener("wheel", (e) => {
    e.preventDefault();
    const rect = canvas.getBoundingClientRect();
    const f = Math.exp(-e.deltaY * 0.0015);
    R.zoomAt(renderer.cam, e.clientX - rect.left, e.clientY - rect.top, f, (state.fitScale || 1) * 0.2, (state.fitScale || 1) * 5e4);
    invalidate();
    scheduleRegion();
  }, { passive: false });

  canvas.addEventListener("pointerdown", (e) => {
    const rect = canvas.getBoundingClientRect();
    const sx = e.clientX - rect.left;
    const sy = e.clientY - rect.top;
    const hit = renderer.hit(sx, sy);
    pointer = { sx, sy, startX: sx, startY: sy, hit, moved: false, cam: { cx: renderer.cam.cx, cy: renderer.cam.cy } };
    canvas.setPointerCapture(e.pointerId);
  });

  canvas.addEventListener("pointermove", (e) => {
    const rect = canvas.getBoundingClientRect();
    const sx = e.clientX - rect.left;
    const sy = e.clientY - rect.top;
    if (!pointer) {
      const h = renderer.hit(sx, sy);
      const id = h && !h.cluster ? h.id : null;
      if (id !== state.hover) {
        state.hover = id;
        invalidate();
      }
      return;
    }
    const dx = sx - pointer.startX;
    const dy = sy - pointer.startY;
    if (!pointer.moved && Math.hypot(dx, dy) < 4) return;
    pointer.moved = true;
    const cam = renderer.cam;
    const h = pointer.hit;
    const draggable = h && !h.cluster && !h.satellite && !state.search && state.nodes.has(h.id);
    if (draggable) {
      if (!state.dragging) {
        const ids = new Set([h.id]);
        const node = state.nodes.get(h.id);
        if (node.kind === "directory") {
          // Visually carry the loaded unlocked descendants; the server applies the same rule.
          for (const n of state.nodes.values()) {
            let p = n.parent;
            let guard = 0;
            while (p != null && guard++ < 512) {
              if (p === h.id) {
                if (!n.pinned && !n.manual) ids.add(n.id);
                break;
              }
              const pn = state.nodes.get(p);
              p = pn ? pn.parent : null;
            }
          }
        }
        state.dragging = { id: h.id, ids, dx: 0, dy: 0 };
        canvas.classList.add("dragging");
      }
      state.dragging.dx = dx / cam.scale;
      state.dragging.dy = dy / cam.scale;
    } else {
      cam.cx = pointer.cam.cx - dx / cam.scale;
      cam.cy = pointer.cam.cy - dy / cam.scale;
      canvas.classList.add("dragging");
      scheduleRegion();
    }
    invalidate();
  });

  canvas.addEventListener("pointerup", async (e) => {
    const p = pointer;
    pointer = null;
    canvas.classList.remove("dragging");
    if (!p) return;
    if (state.dragging) {
      const d = state.dragging;
      state.dragging = null;
      const n = state.nodes.get(d.id);
      if (n && (d.dx || d.dy)) {
        n.x += d.dx;
        n.y += d.dy;
        try {
          await POST("/api/positions/save", { positions: [{ entity_id: d.id, x: n.x, y: n.y }], with_descendants: n.kind === "directory" });
          await loadOverview(false);
        } catch (err) {
          fail(err);
          await loadRegion();
        }
      }
      invalidate();
      return;
    }
    if (p.moved) return;
    const h = p.hit;
    if (state.linkFrom != null) {
      if (h && !h.cluster) finishLink(h.id);
      else stopLink();
      return;
    }
    if (!h) {
      if (!e.shiftKey) clearSelection();
      return;
    }
    if (h.cluster) {
      select(h.id, { fly: true, zoomTo: true });
      return;
    }
    if (h.group) {
      renderGroupPanel(h.group);
      return;
    }
    select(h.id, { additive: e.shiftKey, fly: !!h.satellite });
  });

  canvas.addEventListener("dblclick", (e) => {
    const rect = canvas.getBoundingClientRect();
    const h = renderer.hit(e.clientX - rect.left, e.clientY - rect.top);
    if (h && h.group) {
      if (h.group.is_project) enterProject(h.group.id);
    } else if (h) select(h.id, { fly: true, zoomTo: true });
  });

  window.addEventListener("keydown", (e) => {
    if (e.target.tagName === "INPUT") return;
    if (e.key === "Escape") {
      if (state.linkFrom != null) stopLink();
      else if (state.search) {
        $("search").value = "";
        endSearch();
      } else clearSelection();
    } else if (e.key === "/") {
      e.preventDefault();
      $("search").focus();
    } else if (e.key === "f" || e.key === "F") {
      if (state.overview && state.overview.bounds) {
        state.fitScale = R.fitBounds(renderer.cam, state.overview.bounds, fitPadding());
        invalidate();
        scheduleRegion();
      }
    }
  });

  window.addEventListener("resize", () => {
    renderer.resize();
    invalidate();
    scheduleRegion();
  });

  // -------------------------------------------------------------------------------------------
  // Modes: History and Recover
  // -------------------------------------------------------------------------------------------
  document.querySelectorAll(".modes button").forEach((b) =>
    b.addEventListener("click", () => setMode(b.dataset.mode))
  );

  function setMode(mode) {
    state.mode = mode;
    document.querySelectorAll(".modes button").forEach((b) => {
      b.classList.toggle("active", b.dataset.mode === mode);
      b.setAttribute("aria-selected", String(b.dataset.mode === mode));
    });
    $("sheet").classList.toggle("hidden", mode === "explore");
    if (mode === "projects") renderProjects();
    if (mode === "history") renderHistory();
    if (mode === "recover") renderRecover();
  }

  function goToEntity(id) {
    setMode("explore");
    select(id, { fly: true, zoomTo: true });
  }

  // -------------------------------------------------------------------------------------------
  // Projects: pick a project, see only its tree and what elsewhere relates to it
  // -------------------------------------------------------------------------------------------
  function savedProject() {
    try {
      const v = localStorage.getItem("orbit-project");
      return v ? Number(v) : null;
    } catch (_) {
      return null;
    }
  }

  function saveProject(id) {
    try {
      if (id == null) localStorage.removeItem("orbit-project");
      else localStorage.setItem("orbit-project", String(id));
    } catch (_) {
      /* per-viewer convenience only */
    }
  }

  /** Inside entities whose outside relations stand out: the focus and, for a directory, the
   * loaded nodes under it. `null` when nothing inside the project is focused. */
  function projectHighlight() {
    const f = state.focus;
    if (f == null || f === state.project || !state.nodes.has(f)) return null;
    const ids = new Set([f]);
    for (const n of state.nodes.values()) {
      let p = n.parent;
      let guard = 0;
      while (p != null && guard++ < 512) {
        if (p === f) {
          ids.add(n.id);
          break;
        }
        const pn = state.nodes.get(p);
        p = pn ? pn.parent : null;
      }
    }
    return ids;
  }

  async function loadProjectView() {
    if (state.project == null) return;
    const id = state.project;
    const v = await GET(`/api/graph/project?id=${id}&limit=120${state.showWeak ? "&min_score=0" : ""}`);
    if (state.project !== id) return;
    // Related projects join the planets: on the group of the same project, or as their own.
    for (const r of v.relatives) {
      const g = v.groups.find((x) => x.id === r.id);
      if (g) g.relative = r;
      else v.groups.push({ id: r.id, name: r.name, path: r.path, is_project: true, score: r.score, links: [], relative: r });
    }
    v.groups.sort((a, b) => Number(!!b.relative) - Number(!!a.relative) || b.score - a.score);
    state.projectView = v;
    renderFilters();
    if (state.focus == null) renderBreadcrumb(v.breadcrumb);
    invalidate();
  }

  async function enterProject(id) {
    state.project = id;
    state.projectView = null;
    saveProject(id);
    clearSelection();
    endSearch();
    setMode("explore");
    try {
      await loadProjectView();
      await loadOverview(true);
    } catch (e) {
      // The saved project may no longer exist (other database, rescan): back to the list.
      state.project = null;
      saveProject(null);
      fail(e);
      setMode("projects");
    }
    renderFilters();
  }

  async function leaveProject() {
    state.project = null;
    state.projectView = null;
    saveProject(null);
    clearSelection();
    setMode("explore");
    renderFilters();
    await loadOverview(true);
  }

  async function renderProjects() {
    const body = $("sheet-body");
    setKids(body, el("h2", {}, "Progetti"), el("p", { class: "note" }, "Caricamento…"));
    let ps;
    try {
      ps = await GET("/api/projects");
    } catch (e) {
      fail(e);
      return;
    }
    const linked = ps.filter((p) => p.external_links > 0).length;
    setKids(body,
      el("h2", {}, "Progetti"),
      el("p", { class: "note" }, ps.length
        ? `Scegli un progetto: vedrai solo il suo albero e, attorno, i progetti da cui deriva o che ne derivano e ciò che altrove è collegato ad esso (riferimenti, duplicati, file identici). ${ps.filter((p) => p.relatives.length).length} progetti hanno una famiglia, ${linked} su ${ps.length} hanno file collegati con il resto.`
        : "Nessun progetto: scansiona la cartella che contiene i tuoi progetti (es. ~/projects). Ogni sua sottocartella diventa un progetto."),
      el("div", { class: "controls" },
        state.project != null ? el("button", { onclick: () => setMode("explore") }, "Torna al progetto") : null,
        el("button", { onclick: leaveProject }, "Mappa completa")),
      el("table", { class: "projects" },
        el("tr", {}, ["progetto", "file", "dimensione", "ultima attività", "famiglia", "collegamenti esterni"].map((h) => el("th", {}, h))),
        ps.map((p) => el("tr", { class: "row-click" + (p.id === state.project ? " current" : ""), onclick: () => enterProject(p.id) },
          el("td", {}, el("b", {}, p.name)),
          el("td", { class: "num" }, String(p.files)),
          el("td", { class: "num" }, bytes(p.bytes)),
          el("td", {}, p.recent_mtime ? new Date(p.recent_mtime * 1000).toLocaleDateString("it-IT") : "—"),
          el("td", {}, p.relatives.length ? p.relatives.map((r, i) => [i ? el("br") : null, kinLabel(r)]) : el("span", { class: "muted" }, "—")),
          el("td", {}, p.external_links ? `${p.external_links} con ${p.linked_projects} progett${p.linked_projects === 1 ? "o" : "i"}` : el("span", { class: "muted" }, "—"))))));
  }

  /** "deriva da X" / "origine di X" / "imparentato con X", from the current project's side. */
  function kinLabel(r) {
    if (r.came_first === true) return `deriva da ${r.name}`;
    if (r.came_first === false) return `origine di ${r.name}`;
    return `imparentato con ${r.name}`;
  }

  function kinExplanation(r) {
    const current = state.projectView ? state.projectView.root.name : "questo progetto";
    return el("div", { class: "rel" },
      el("div", { class: "rel-head" }, kindChip("project_membership", "parentela"), el("span", { class: "badge" }, r.score.toFixed(2))),
      el("ul", {},
        el("li", {}, `Struttura e nomi in comune (somiglianza ${r.vocabulary.toFixed(2)}): `, el("span", { class: "mono" }, r.shared.join(", "))),
        r.name_root ? el("li", {}, `Stessa radice nel nome: «${r.name_root}»`) : null,
        el("li", {}, r.came_first === true ? `${r.name} è nato prima di ${current}`
          : r.came_first === false ? `${r.name} è nato dopo ${current}`
          : "Iniziati nello stesso periodo: non si sa chi venga prima")),
      el("div", { class: "note" }, "La parentela non richiede file identici: rileva progetti riscritti a partire da un altro o costruiti con la stessa architettura."));
  }

  /** Panel for a planet: a related project and/or the outside entities of one group. */
  function renderGroupPanel(g) {
    const name = (id) => {
      const n = state.nodes.get(id);
      return n ? n.name : "#" + id;
    };
    setKids($("panel-body"),
      el("h2", {}, g.name),
      el("div", { class: "path" }, g.path),
      el("div", { class: "actions" },
        g.is_project ? el("button", { class: "primary", onclick: () => enterProject(g.id) }, "Apri questo progetto") : null,
        el("button", { onclick: () => openEntity(g.id, "reveal") }, "Apri nel file manager")),
      g.relative ? [el("h3", {}, kinLabel(g.relative)), kinExplanation(g.relative)] : null,
      g.links.length ? el("h3", {}, `${g.links.length} file collegat${g.links.length === 1 ? "o" : "i"} al progetto corrente`) : null,
      g.links.map((l) => el("div", { class: "rel" },
        el("div", { class: "rel-head" },
          l.kinds.map((k) => kindChip(k, R.KIND_LABELS[k] || k)),
          el("span", { class: "badge" }, l.score.toFixed(2))),
        el("div", {}, el("span", { class: "rel-other", title: l.path, onclick: () => select(l.entity_id) }, l.name)),
        el("div", { class: "path" }, l.path),
        el("div", { class: "note" }, "↔ ", l.via.slice(0, 6).map((v, i) => [i ? ", " : "",
          el("span", { class: "rel-other", onclick: () => select(v, { fly: true }) }, name(v))]),
          l.via.length > 6 ? ` e altri ${l.via.length - 6}` : ""))));
    $("panel").classList.remove("hidden");
    document.body.classList.add("panel-open");
  }

  async function renderHistory() {
    const body = $("sheet-body");
    setKids(body, el("h2", {}, "History"), el("p", { class: "note" }, "Caricamento…"));
    let snaps;
    try {
      snaps = await GET("/api/history/snapshots");
    } catch (e) {
      fail(e);
      return;
    }
    const usable = snaps.filter((s) => s.status === "completed" || s.status === "partial");
    const opt = (s) => el("option", { value: s.id }, `#${s.id} · ${new Date(s.created_at_unix_ms).toLocaleString("it-IT")} · ${s.root_path_display}${s.status === "partial" ? " (parziale)" : ""}`);
    const base = el("select", {}, usable.map(opt));
    const target = el("select", {}, usable.map(opt));
    // Default: latest snapshot vs the previous one of the same root.
    const sameRoot = usable.findIndex((x, i) => i > 0 && x.root_path_display === (usable[0] && usable[0].root_path_display));
    if (usable.length > 1) base.selectedIndex = sameRoot > 0 ? sameRoot : 1;
    const out = el("div", {});
    const run = async () => {
      setKids(out, el("p", { class: "note" }, "Confronto…"));
      try {
        const d = await POST("/api/history/diff", { base: Number(base.value), target: Number(target.value), limit: 300 });
        setKids(out, renderDiff(d));
      } catch (e) {
        setKids(out, el("p", { class: "note" }, e.message));
      }
    };
    const table = el("table", {},
      el("tr", {}, ["ID", "data", "stato", "file", "nuove", "spostate", "non oss.", "prob. eliminate", "radice"].map((h) => el("th", {}, h))),
      snaps.map((s) => el("tr", {},
        el("td", {}, "#" + s.id), el("td", {}, new Date(s.created_at_unix_ms).toLocaleString("it-IT")),
        el("td", {}, s.status), el("td", { class: "num" }, String(s.total_files_count)),
        el("td", { class: "num" }, String(s.new_entities_count)), el("td", { class: "num" }, String(s.moved_entities_count)),
        el("td", { class: "num" }, String(s.unobserved_entities_count)), el("td", { class: "num" }, String(s.probably_deleted_entities_count)),
        el("td", {}, s.root_path_display))));
    setKids(body, 
      el("h2", {}, "History — memoria temporale"),
      el("p", { class: "note" }, "Gli snapshot sono osservazioni: l'identità delle entità sopravvive a spostamenti e rinomine. Un elemento non visto per un errore di permesso è «non osservato», mai «eliminato»."),
      usable.length >= 2
        ? el("div", { class: "controls" }, "Da", base, "a", target, el("button", { class: "primary", onclick: run }, "Confronta"))
        : el("p", { class: "note" }, "Servono almeno due snapshot per un confronto: esegui un'altra scansione più tardi."),
      out,
      el("h3", {}, `Snapshot (${snaps.length})`),
      table
    );
    if (usable.length >= 2) run();
  }

  function renderDiff(d) {
    const t = d.totals;
    const differentRoots = d.base.root_path_display !== d.target.root_path_display;
    const rows = (items, cols) => el("table", {}, items.map((it) => el("tr", { class: "row-click", onclick: () => goToEntity(it.entity_id) }, cols(it))));
    return el("div", {},
      differentRoots ? el("p", { class: "note" }, `Attenzione: i due snapshot hanno radici diverse (${d.base.root_path_display} → ${d.target.root_path_display}). Gli elementi fuori dalla radice di destinazione non sono stati osservati, non eliminati.`) : null,
      el("p", {}, `Spostati ${t.moved} · modificati ${t.modified} · nuovi ${t.appeared} · probabilmente eliminati ${t.disappeared_probably_deleted} · non osservati ${t.disappeared_unobserved} · fuori radice ${t.disappeared_outside_root}`),
      d.moved.length ? [el("h3", {}, "Spostati o rinominati"), rows(d.moved, (m) => [el("td", {}, m.from_path), el("td", {}, "→"), el("td", {}, m.to_path), el("td", { class: "muted" }, matchLabel(m.identity_match) + (m.content_changed ? ", modificato" : ""))])] : null,
      d.modified.length ? [el("h3", {}, "Modificati"), rows(d.modified, (m) => [el("td", {}, m.path), el("td", { class: "num" }, `${bytes(m.before_bytes)} → ${bytes(m.after_bytes)}`)])] : null,
      d.appeared.length ? [el("h3", {}, "Nuovi"), rows(d.appeared, (m) => [el("td", {}, m.path), el("td", { class: "num" }, bytes(m.apparent_bytes))])] : null,
      d.disappeared.length ? [el("h3", {}, "Non più osservati"), el("table", {}, d.disappeared.filter((m) => m.verdict !== "outside_target_root").map((m) => el("tr", {}, el("td", {}, m.path), el("td", {}, m.verdict === "probably_deleted" ? "probabilmente eliminato" : m.verdict === "unobserved" ? "non osservato" : "fuori radice"), el("td", { class: "muted" }, m.reason))))] : null,
      d.truncated ? el("p", { class: "note" }, "Elenchi troncati.") : null
    );
  }

  async function renderRecover() {
    const body = $("sheet-body");
    setKids(body, el("h2", {}, "Recover"), el("p", { class: "note" }, "Caricamento…"));
    let r;
    try {
      r = await GET("/api/recover");
    } catch (e) {
      fail(e);
      return;
    }
    const reveal = (id) => el("button", { onclick: (ev) => { ev.stopPropagation(); openEntity(id, "reveal"); } }, "Mostra nella cartella");
    setKids(body, 
      el("h2", {}, "Recover — comprensione dello spazio"),
      r.notes.map((n) => el("p", { class: "note" }, "ℹ " + n)),
      el("p", {}, `Recuperabile stimato da duplicati verificati: ${bytes(r.verified_recoverable_bytes)} · rilevato in duplicati non verificati: ${bytes(r.unverified_duplicate_detected_bytes)}`),
      el("h3", {}, "File più grandi (spazio allocato rilevato)"),
      el("table", {}, r.largest_files.map((f) => el("tr", { class: "row-click", onclick: () => goToEntity(f.entity_id) }, el("td", { class: "num" }, bytes(f.allocated_bytes)), el("td", {}, f.path), el("td", {}, reveal(f.entity_id))))),
      el("h3", {}, "Gruppi di duplicati"),
      r.duplicate_groups.length ? r.duplicate_groups.map((g) => el("div", { class: "group" },
        el("div", {}, el("b", {}, g.verified ? "Contenuto verificato identico" : "Candidati (solo campioni: non verificati)"),
          ` · ${g.members.length} file · rilevato ${bytes(g.detected_bytes)} · recuperabile ${g.recoverable_estimate_bytes == null ? "non stimato" : bytes(g.recoverable_estimate_bytes)}`),
        el("table", {}, g.members.map((m) => el("tr", { class: "row-click", onclick: () => goToEntity(m.entity_id) }, el("td", {}, m.path), el("td", {}, reveal(m.entity_id))))))) : el("p", { class: "note" }, "Nessun gruppo. Suggerimento: «verifica duplicati» durante la scansione calcola gli hash completi."),
      el("h3", {}, "Directory di cache note"),
      el("table", {}, r.caches.map((c) => el("tr", { class: "row-click", onclick: () => goToEntity(c.entity_id) }, el("td", { class: "num" }, bytes(c.detected_bytes)), el("td", {}, c.path), el("td", { class: "muted" }, c.note), el("td", {}, reveal(c.entity_id)))))
    );
  }

  // -------------------------------------------------------------------------------------------
  // Scan control and status polling
  // -------------------------------------------------------------------------------------------
  $("scan-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const root = $("scan-path").value.trim();
    if (!root) return;
    try {
      await POST("/api/scan/start", { root_path: root, verify_duplicates: $("scan-verify").checked });
      pollStatus();
    } catch (err) {
      fail(err);
    }
  });
  $("scan-cancel").addEventListener("click", () => POST("/api/scan/cancel").catch(fail));

  let lastJob = null;
  async function pollStatus() {
    let s;
    try {
      s = await GET("/api/status");
    } catch (e) {
      fail(e);
      return;
    }
    const job = s.job;
    $("scan-start").disabled = !!job;
    $("scan-cancel").classList.toggle("hidden", !job);
    const jobPill = $("job");
    if (job) {
      jobPill.classList.remove("hidden");
      const p = job.progress;
      jobPill.textContent = job.phase === "indexing"
        ? "Indicizzazione: identità, fingerprint, relazioni, layout…"
        : `Scansione: ${p ? p.dirs_scanned : 0} directory, ${p ? p.files_scanned : 0} file${p && p.errors_count ? `, ${p.errors_count} errori` : ""}`;
    } else {
      jobPill.classList.add("hidden");
    }
    if (lastJob && !job) {
      if (s.last_error) toast(s.last_error, { error: true });
      else if (s.last_report && s.last_report.record) {
        const sn = s.last_report.record.snapshot;
        toast(`Snapshot #${sn.id}: ${sn.total_files_count} file · ${sn.new_entities_count} nuove entità · ${sn.moved_entities_count} spostate`);
      }
    }
    if (s.graph_version !== state.graphVersion) {
      const first = state.graphVersion === 0;
      state.graphVersion = s.graph_version;
      if (first) {
        const saved = savedProject();
        if (saved != null) await enterProject(saved);
        else if (s.store && s.store.entities_present) setMode("projects");
      }
      if (state.project != null && !first) await loadProjectView().catch(fail);
      if (!first && state.mode === "projects") renderProjects();
      await loadOverview(first || (lastJob && !job));
    }
    if (!$("scan-path").value && s.home) $("scan-path").value = s.home + "/projects";
    lastJob = job;
    setTimeout(pollStatus, job ? 600 : 4000);
  }

  // -------------------------------------------------------------------------------------------
  // Start
  // -------------------------------------------------------------------------------------------
  renderer.resize();
  renderFilters();
  renderLegend();
  requestAnimationFrame(frame);
  bootstrap().then((ok) => {
    if (ok) pollStatus();
  });
})();
