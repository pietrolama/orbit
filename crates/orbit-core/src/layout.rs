//! Deterministic, persistent, incremental spatial layout.
//!
//! The layout is a hierarchical circle packing: every directory is the disc enclosing its
//! children, which are placed largest first on a golden-angle spiral from the centre, skipping
//! positions that would overlap an already placed sibling. There is no reserved core: a directory
//! with one dominant child is only slightly larger than that child, so nesting depth adds to the
//! radius instead of multiplying it (the UI draws the directory's own glyph on its rim). Packing is computed bottom-up for subtrees that have no position yet,
//! then the subtree is placed next to its positioned parent.
//!
//! Stability rules (see `docs/PRODUCT_DECISIONS.md`):
//! * existing positions are never recomputed by [`update_layout`]; only nodes without a position
//!   are placed, near their physical parent;
//! * pinned nodes and nodes placed manually (`source = Manual`) are never moved automatically;
//! * an automatically placed node whose physical parent changed (the file or directory was moved)
//!   is re-placed near its new parent, and its auto-placed descendants are translated with it;
//! * the angular phase of each directory derives from `hash(seed, entity id)`, never from
//!   iteration order, so identical inputs and seed give identical coordinates.
//!
//! The physics/animation of the UI is a presentation layer on top of these coordinates; it never
//! writes back temporary positions.

use crate::model::{EntityId, EntryType, GraphNode};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

const GOLDEN_ANGLE: f64 = 2.399_963_229_728_653;

/// Who decided a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionSource {
    Auto,
    Manual,
}

impl PositionSource {
    pub fn as_db(self) -> &'static str {
        match self {
            PositionSource::Auto => "auto",
            PositionSource::Manual => "manual",
        }
    }

    pub fn from_db(s: &str) -> Self {
        if s == "manual" {
            PositionSource::Manual
        } else {
            PositionSource::Auto
        }
    }
}

/// Persistent position of a file entity on a canvas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodePosition {
    pub entity_id: EntityId,
    pub x: f64,
    pub y: f64,
    pub radius: f64,
    pub pinned: bool,
    pub source: PositionSource,
    /// Physical parent at the time the node was placed (used to detect moves).
    pub cluster_id: Option<EntityId>,
    pub layout_version: u32,
}

impl NodePosition {
    /// `true` if automatic layout must not move this node.
    pub fn is_locked(&self) -> bool {
        self.pinned || self.source == PositionSource::Manual
    }
}

/// Layout parameters. `seed` and `layout_version` are explicit and persisted per canvas.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutConfig {
    pub seed: u64,
    pub layout_version: u32,
    /// Gap between sibling discs, in world units.
    pub padding: f64,
    /// Radius of an empty directory disc, and the minimum radius of any directory.
    pub dir_core_radius: f64,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            seed: 1,
            layout_version: 1,
            padding: 0.35,
            dir_core_radius: 1.6,
        }
    }
}

/// Statistics of a layout pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutReport {
    pub placed_new: usize,
    pub replaced_after_move: usize,
    pub translated_descendants: usize,
    pub kept_existing: usize,
    pub kept_locked: usize,
}

/// Result of a layout pass: only positions that must be written.
#[derive(Debug, Clone, Default)]
pub struct LayoutResult {
    pub updates: Vec<NodePosition>,
    pub report: LayoutReport,
}

/// Visual radius of a leaf: grows slowly with the logarithm of the size.
pub fn leaf_radius(node: &GraphNode) -> f64 {
    match node.kind {
        EntryType::File => {
            let kib = node.apparent_bytes as f64 / 4096.0;
            (0.8 + 0.3 * (1.0 + kib).log10()).clamp(0.8, 3.0)
        }
        _ => 0.8,
    }
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn phase_for(seed: u64, id: EntityId) -> f64 {
    let h = splitmix64(seed ^ splitmix64(id as u64));
    (h as f64 / u64::MAX as f64) * std::f64::consts::TAU
}

/// Spatial hash of placed circles for overlap tests.
struct Grid {
    cell: f64,
    cells: HashMap<(i64, i64), Vec<usize>>,
    circles: Vec<(f64, f64, f64)>,
}

impl Grid {
    fn new(cell: f64) -> Self {
        Self {
            cell: cell.max(0.5),
            cells: HashMap::new(),
            circles: Vec::new(),
        }
    }

    fn cell_range(&self, x: f64, y: f64, r: f64) -> (i64, i64, i64, i64) {
        (
            ((x - r) / self.cell).floor() as i64,
            ((x + r) / self.cell).floor() as i64,
            ((y - r) / self.cell).floor() as i64,
            ((y + r) / self.cell).floor() as i64,
        )
    }

    fn insert(&mut self, x: f64, y: f64, r: f64) {
        let idx = self.circles.len();
        self.circles.push((x, y, r));
        let (x0, x1, y0, y1) = self.cell_range(x, y, r);
        // Very large circles (rare: huge subdirectories) are bounded to a reasonable cell count.
        // Saturating: far-away or huge circles must not wrap around into a giant cell loop.
        if span(x0, x1).saturating_mul(span(y0, y1)) > 4096 {
            self.cells
                .entry((i64::MIN, i64::MIN))
                .or_default()
                .push(idx);
            return;
        }
        for cx in x0..=x1 {
            for cy in y0..=y1 {
                self.cells.entry((cx, cy)).or_default().push(idx);
            }
        }
    }

    fn overlaps(&self, x: f64, y: f64, r: f64, pad: f64) -> bool {
        let check = |idx: &usize| {
            let (cx, cy, cr) = self.circles[*idx];
            let dx = cx - x;
            let dy = cy - y;
            let min = cr + r + pad;
            dx * dx + dy * dy < min * min
        };
        if let Some(big) = self.cells.get(&(i64::MIN, i64::MIN)) {
            if big.iter().any(check) {
                return true;
            }
        }
        let (x0, x1, y0, y1) = self.cell_range(x, y, r + pad);
        // A disc much larger than the cells would visit (r/cell)^2 cells: when that exceeds the
        // number of placed discs, comparing against every disc directly is cheaper.
        let cells = span(x0, x1).saturating_mul(span(y0, y1));
        if cells > self.circles.len() as i64 {
            return (0..self.circles.len()).any(|i| check(&i));
        }
        for cx in x0..=x1 {
            for cy in y0..=y1 {
                if let Some(list) = self.cells.get(&(cx, cy)) {
                    if list.iter().any(check) {
                        return true;
                    }
                }
            }
        }
        false
    }
}

/// Number of grid cells in `lo..=hi`, saturating instead of overflowing.
fn span(lo: i64, hi: i64) -> i64 {
    hi.saturating_sub(lo).saturating_add(1)
}

/// Finds a free spot for a circle of radius `r` around `(cx, cy)` on a golden-angle spiral that
/// starts at the centre, avoiding circles already in `grid`.
///
/// The spiral step is the circle's own size (`step`), and the search begins at distance
/// `d_from` (minus a few spiral indices) rather than at the centre; the distance of the chosen spot is returned. Callers keep a
/// distance cursor between siblings, so placing n children costs O(n) attempts overall instead of
/// rescanning the filled centre for each one (quadratic for a directory with 40 000 files), and a
/// very large child does not crawl outwards with the tiny step of its small siblings.
#[allow(clippy::too_many_arguments)]
fn spiral_place(
    grid: &Grid,
    cx: f64,
    cy: f64,
    r: f64,
    step: f64,
    phase: f64,
    pad: f64,
    d_from: f64,
) -> (f64, f64, f64) {
    // Resume at the cursor, a few spiral turns back so small gaps near the frontier still fill.
    let first = ((d_from.max(0.0) / step).powi(2).floor() as u64).saturating_sub(48);
    let mut k: u64 = first;
    loop {
        let d = step * (k as f64).sqrt();
        let theta = phase + k as f64 * GOLDEN_ANGLE;
        let x = cx + d * theta.cos();
        let y = cy + d * theta.sin();
        if !grid.overlaps(x, y, r, pad) || k > first + 2_000_000 {
            return (x, y, d);
        }
        k += 1;
    }
}

/// Approximately smallest disc enclosing `discs` (`(_, x, y, r)`): Bădoiu–Clarkson iterations
/// pull a centre towards the farthest rim, then the radius is measured exactly from that centre,
/// so the result always encloses every disc. Falls back to the origin if that is tighter.
fn enclosing_disc<T>(discs: &[(T, f64, f64, f64)]) -> (f64, f64, f64) {
    let reach = |cx: f64, cy: f64| -> (usize, f64) {
        let mut best = (0, f64::NEG_INFINITY);
        for (i, &(_, x, y, r)) in discs.iter().enumerate() {
            let d = (x - cx).hypot(y - cy) + r;
            if d > best.1 {
                best = (i, d);
            }
        }
        best
    };
    let (_, at_origin) = reach(0.0, 0.0);
    if discs.len() < 2 {
        return (0.0, 0.0, at_origin.max(0.0));
    }
    let (mut x0, mut x1, mut y0, mut y1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for &(_, x, y, r) in discs {
        x0 = x0.min(x - r);
        x1 = x1.max(x + r);
        y0 = y0.min(y - r);
        y1 = y1.max(y + r);
    }
    let (mut cx, mut cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    for i in 1..=128 {
        let (j, _) = reach(cx, cy);
        let (_, x, y, r) = discs[j];
        let len = (x - cx).hypot(y - cy);
        let (fx, fy) = if len > 1e-12 {
            (x + r * (x - cx) / len, y + r * (y - cy) / len)
        } else {
            (x + r, y)
        };
        let t = 1.0 / (i as f64 + 1.0);
        cx += (fx - cx) * t;
        cy += (fy - cy) * t;
    }
    let (_, rad) = reach(cx, cy);
    if rad < at_origin {
        (cx, cy, rad)
    } else {
        (0.0, 0.0, at_origin)
    }
}

/// Spiral step for a disc of radius `r` among siblings of typical step `base`.
fn step_for(r: f64, base: f64, pad: f64) -> f64 {
    base.max(r * 1.05 + pad)
}

/// Bottom-up packing of a subtree whose nodes have no positions. Returns per-node offsets
/// relative to the subtree root and the subtree's enclosing radius.
fn pack_subtree(
    root: EntityId,
    children: &BTreeMap<EntityId, Vec<EntityId>>,
    nodes: &HashMap<EntityId, &GraphNode>,
    cfg: &LayoutConfig,
) -> (HashMap<EntityId, (f64, f64)>, HashMap<EntityId, f64>) {
    // Iterative post-order (deep trees must not overflow the stack).
    let mut order: Vec<EntityId> = Vec::new();
    let mut stack = vec![root];
    while let Some(v) = stack.pop() {
        order.push(v);
        if let Some(ch) = children.get(&v) {
            stack.extend(ch.iter().copied());
        }
    }
    let mut radius: HashMap<EntityId, f64> = HashMap::new();
    // Offsets of each child relative to its parent centre.
    let mut local: HashMap<EntityId, (f64, f64)> = HashMap::new();

    for &v in order.iter().rev() {
        let kids = children.get(&v).map(Vec::as_slice).unwrap_or(&[]);
        if kids.is_empty() {
            let r = nodes.get(&v).map(|n| {
                if n.kind == EntryType::Directory {
                    cfg.dir_core_radius
                } else {
                    leaf_radius(n)
                }
            });
            radius.insert(v, r.unwrap_or(1.0));
            continue;
        }
        let mut sorted: Vec<EntityId> = kids.to_vec();
        sorted.sort_by(|a, b| {
            radius[b]
                .partial_cmp(&radius[a])
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.cmp(b))
        });
        let mut rs: Vec<f64> = sorted.iter().map(|c| radius[c]).collect();
        rs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = rs[rs.len() / 2];
        let mut grid = Grid::new(median * 2.0 + cfg.padding);
        let phase = phase_for(cfg.seed, v);
        let step = median * 1.05 + cfg.padding;
        let mut cursor = 0.0f64;
        let mut class_step = f64::INFINITY;
        // Area-weighted centroid of the children placed so far: the spiral turns around it, so
        // small children fill the nooks next to the large ones instead of a ring on the far side.
        let (mut sx, mut sy, mut sa) = (0.0f64, 0.0f64, 0.0f64);
        let mut placed: Vec<(EntityId, f64, f64, f64)> = Vec::with_capacity(sorted.len());
        for c in sorted {
            let r = radius[&c];
            let st = step_for(r, step, cfg.padding);
            // Children come largest first: once they get much smaller, restart the search from
            // the centre so they can use the gaps left between the larger ones. Within a size
            // class the cursor keeps placement linear.
            if st < class_step * 0.5 {
                class_step = st;
                cursor = 0.0;
            }
            let (cx, cy) = if sa > 0.0 {
                (sx / sa, sy / sa)
            } else {
                (0.0, 0.0)
            };
            let (x, y, d) = spiral_place(&grid, cx, cy, r, st, phase, cfg.padding, cursor);
            // The largest child takes the centre; the others can only fit outside its rim.
            cursor = cursor.max(d.max(r));
            grid.insert(x, y, r);
            let a = r * r;
            sx += x * a;
            sy += y * a;
            sa += a;
            placed.push((c, x, y, r));
        }
        // The directory sits at the centre of the smallest disc enclosing its children, not at
        // the spiral origin: with two or three large children that saves up to a third of the
        // radius at every level of nesting.
        let (mx, my, enclosing) = enclosing_disc(&placed);
        for &(c, x, y, _) in &placed {
            local.insert(c, (x - mx, y - my));
        }
        radius.insert(v, (enclosing + cfg.padding).max(cfg.dir_core_radius));
    }

    // Accumulate offsets from the subtree root, top-down.
    let mut offsets: HashMap<EntityId, (f64, f64)> = HashMap::new();
    offsets.insert(root, (0.0, 0.0));
    for &v in &order {
        let base = offsets[&v];
        if let Some(ch) = children.get(&v) {
            for c in ch {
                let (lx, ly) = local[c];
                offsets.insert(*c, (base.0 + lx, base.1 + ly));
            }
        }
    }
    (offsets, radius)
}

fn build_children(nodes: &[GraphNode]) -> BTreeMap<EntityId, Vec<EntityId>> {
    let ids: std::collections::HashSet<EntityId> = nodes.iter().map(|n| n.entity_id).collect();
    let mut children: BTreeMap<EntityId, Vec<EntityId>> = BTreeMap::new();
    for n in nodes {
        if let Some(p) = n.parent_entity_id {
            if ids.contains(&p) && p != n.entity_id {
                children.entry(p).or_default().push(n.entity_id);
            }
        }
    }
    for v in children.values_mut() {
        v.sort_unstable();
    }
    children
}

/// Incremental layout: places nodes without a position and re-places auto-positioned nodes whose
/// physical parent changed. Existing positions are otherwise untouched.
pub fn update_layout(
    nodes: &[GraphNode],
    existing: &HashMap<EntityId, NodePosition>,
    cfg: &LayoutConfig,
) -> LayoutResult {
    let by_id: HashMap<EntityId, &GraphNode> = nodes.iter().map(|n| (n.entity_id, n)).collect();
    let children = build_children(nodes);
    let parent_of = |id: EntityId| -> Option<EntityId> {
        by_id
            .get(&id)
            .and_then(|n| n.parent_entity_id)
            .filter(|p| by_id.contains_key(p))
    };

    let mut current: HashMap<EntityId, NodePosition> = existing.clone();
    let mut result = LayoutResult::default();

    let mut changed: BTreeMap<EntityId, NodePosition> = BTreeMap::new();
    // One placer per parent (and one for the top level), reused across all nodes placed in this
    // pass, so that many new files in one directory cost linear time.
    let mut placers: HashMap<Option<EntityId>, Placer> = HashMap::new();

    // 1) New subtree roots: nodes without position whose parent is positioned or absent.
    let mut new_roots: Vec<EntityId> = nodes
        .iter()
        .map(|n| n.entity_id)
        .filter(|id| !current.contains_key(id))
        .filter(|id| match parent_of(*id) {
            Some(p) => current.contains_key(&p),
            None => true,
        })
        .collect();
    new_roots.sort_unstable();

    for root in new_roots {
        let (offsets, radii) = pack_subtree(root, &children, &by_id, cfg);
        let r = radii[&root];
        let parent = parent_of(root);
        let (cx, cy) = placers
            .entry(parent)
            .or_insert_with(|| Placer::new(parent, &children, &current, nodes, &by_id, cfg))
            .place(r, cfg);
        let mut ids: Vec<&EntityId> = offsets.keys().collect();
        ids.sort_unstable();
        for id in ids {
            if current.contains_key(id) {
                continue;
            }
            let (ox, oy) = offsets[id];
            let pos = NodePosition {
                entity_id: *id,
                x: cx + ox,
                y: cy + oy,
                radius: radii[id],
                pinned: false,
                source: PositionSource::Auto,
                cluster_id: parent_of(*id),
                layout_version: cfg.layout_version,
            };
            current.insert(*id, pos.clone());
            changed.insert(*id, pos);
            result.report.placed_new += 1;
        }
    }

    // 2) Detect moved auto nodes: parent at placement time differs from the current parent.
    let mut moved: Vec<EntityId> = Vec::new();
    for n in nodes {
        if let Some(pos) = existing.get(&n.entity_id) {
            if pos.is_locked() {
                result.report.kept_locked += 1;
                continue;
            }
            let parent_now = parent_of(n.entity_id);
            if pos.cluster_id != parent_now
                && parent_now
                    .map(|p| current.contains_key(&p))
                    .unwrap_or(false)
            {
                moved.push(n.entity_id);
            } else {
                result.report.kept_existing += 1;
            }
        }
    }
    moved.sort_unstable();
    // Only the top-most moved nodes are re-placed; their descendants follow.
    let moved_set: std::collections::HashSet<EntityId> = moved.iter().copied().collect();
    let top_moved: Vec<EntityId> = moved
        .iter()
        .copied()
        .filter(|&id| {
            let mut p = parent_of(id);
            while let Some(pp) = p {
                if moved_set.contains(&pp) {
                    return false;
                }
                p = parent_of(pp);
            }
            true
        })
        .collect();

    for id in top_moved {
        let Some(parent) = parent_of(id) else {
            continue;
        };
        let old = current[&id].clone();
        let (nx, ny) = placers
            .entry(Some(parent))
            .or_insert_with(|| Placer::new(Some(parent), &children, &current, nodes, &by_id, cfg))
            .place(old.radius, cfg);
        let (dx, dy) = (nx - old.x, ny - old.y);
        let mut np = old.clone();
        np.x = nx;
        np.y = ny;
        np.cluster_id = Some(parent);
        np.layout_version = cfg.layout_version;
        current.insert(id, np.clone());
        changed.insert(id, np);
        result.report.replaced_after_move += 1;
        // Translate auto-placed descendants.
        let mut stack: Vec<EntityId> = children.get(&id).cloned().unwrap_or_default();
        while let Some(d) = stack.pop() {
            if let Some(dp) = current.get(&d).cloned() {
                if !dp.is_locked() {
                    let mut t = dp.clone();
                    t.x += dx;
                    t.y += dy;
                    t.cluster_id = parent_of(d);
                    current.insert(d, t.clone());
                    changed.insert(d, t);
                    result.report.translated_descendants += 1;
                }
            }
            if let Some(ch) = children.get(&d) {
                stack.extend(ch.iter().copied());
            }
        }
    }

    result.updates = changed.into_values().collect();
    result
}

/// Places successive discs around a parent (or around the origin for top-level trees), keeping
/// the obstacle grid and the spiral cursor between calls.
struct Placer {
    grid: Grid,
    cx: f64,
    cy: f64,
    pad: f64,
    phase: f64,
    cursor: f64,
}

impl Placer {
    fn new(
        parent: Option<EntityId>,
        children: &BTreeMap<EntityId, Vec<EntityId>>,
        current: &HashMap<EntityId, NodePosition>,
        nodes: &[GraphNode],
        by_id: &HashMap<EntityId, &GraphNode>,
        cfg: &LayoutConfig,
    ) -> Self {
        match parent {
            Some(parent) => {
                let p = &current[&parent];
                let siblings: Vec<&NodePosition> = children
                    .get(&parent)
                    .map(|v| v.iter().filter_map(|c| current.get(c)).collect())
                    .unwrap_or_default();
                let mut radii: Vec<f64> = siblings.iter().map(|s| s.radius).collect();
                radii.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let median = radii.get(radii.len() / 2).copied().unwrap_or(1.0);
                let mut grid = Grid::new(median * 2.0 + cfg.padding);
                for s in &siblings {
                    grid.insert(s.x, s.y, s.radius);
                }
                Self {
                    grid,
                    cx: p.x,
                    cy: p.y,
                    pad: cfg.padding,
                    phase: phase_for(cfg.seed, parent),
                    cursor: 0.0,
                }
            }
            None => {
                // Top-level discs (scan roots, unrelated trees) avoid every other top-level disc.
                let mut grid = Grid::new(8.0);
                for n in nodes {
                    let is_top = n
                        .parent_entity_id
                        .map(|p| !by_id.contains_key(&p))
                        .unwrap_or(true);
                    if is_top {
                        if let Some(pos) = current.get(&n.entity_id) {
                            grid.insert(pos.x, pos.y, pos.radius);
                        }
                    }
                }
                Self {
                    grid,
                    cx: 0.0,
                    cy: 0.0,
                    pad: cfg.padding * 4.0,
                    phase: phase_for(cfg.seed, 0),
                    cursor: 0.0,
                }
            }
        }
    }

    fn place(&mut self, r: f64, cfg: &LayoutConfig) -> (f64, f64) {
        let st = step_for(r, 0.0, cfg.padding);
        let (x, y, d) = spiral_place(
            &self.grid,
            self.cx,
            self.cy,
            r,
            st,
            self.phase,
            self.pad,
            self.cursor,
        );
        self.cursor = self.cursor.max(d.max(r));
        self.grid.insert(x, y, r);
        (x, y)
    }
}

/// Full re-layout on explicit user request: every unlocked node is recomputed from scratch;
/// pinned and manual positions are preserved verbatim.
pub fn relayout_all(
    nodes: &[GraphNode],
    existing: &HashMap<EntityId, NodePosition>,
    cfg: &LayoutConfig,
) -> LayoutResult {
    let fresh = update_layout(nodes, &HashMap::new(), cfg);
    let mut result = LayoutResult::default();
    for mut p in fresh.updates {
        match existing.get(&p.entity_id) {
            Some(old) if old.is_locked() => {
                result.report.kept_locked += 1;
            }
            _ => {
                p.layout_version = cfg.layout_version;
                result.updates.push(p);
                result.report.placed_new += 1;
            }
        }
    }
    result
}
