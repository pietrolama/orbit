//! In-memory relational graph and level-of-detail views.
//!
//! [`Graph`] is a read model built from the store: present (and unobserved) entities, active
//! edges, persistent positions and constellations. It answers the three zoom levels:
//!
//! * **Galaxy** ([`Graph::overview`]): a bounded number of clusters (directories) with aggregated
//!   relation flows between them — never thousands of individual lines;
//! * **System** ([`Graph::region`]): nodes inside a viewport whose on-screen size is above a pixel
//!   threshold, plus the strongest edges among them, both capped;
//! * **File** ([`Graph::entity_detail`]): one entity with metadata, history, and explained relations.

use crate::identity::is_within_raw;
use crate::kinship::{project_kinship, ProjectKin};
use crate::layout::{NodePosition, PositionSource};
use crate::model::{
    Edge, EdgeOrigin, EdgeState, EntityId, EntityState, EntryType, GraphNode, RelationKind,
};
use crate::relations::{combined_score, kind_label, RelationConfig};
use crate::store::{Constellation, DbError, Observation, OrbitStore, StoredIdentityCandidate};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};

/// Read model of the relational space.
#[derive(Debug, Clone)]
pub struct Graph {
    nodes: Vec<GraphNode>,
    index: HashMap<EntityId, usize>,
    children: HashMap<EntityId, Vec<EntityId>>,
    edges: Vec<Edge>,
    adjacency: HashMap<EntityId, Vec<usize>>,
    positions: HashMap<EntityId, NodePosition>,
    constellations: Vec<Constellation>,
    membership: HashMap<EntityId, Vec<i64>>,
    /// Latest mtime in each subtree (directories) or own mtime (files).
    recent: HashMap<EntityId, i64>,
    /// Project of every node: its ancestor directly under a scan root (the root itself for roots
    /// and for nodes outside any project).
    project_of: HashMap<EntityId, EntityId>,
    cfg: RelationConfig,
}

/// A related entity with its combined score and the edges that explain it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Neighbor {
    pub entity_id: EntityId,
    pub score: f64,
    pub kinds: Vec<RelationKind>,
}

/// Compact node for rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeView {
    pub id: EntityId,
    pub name: String,
    pub kind: EntryType,
    pub state: EntityState,
    pub x: f64,
    pub y: f64,
    pub r: f64,
    pub parent: Option<EntityId>,
    pub depth: u32,
    pub bytes: u64,
    pub mtime: Option<i64>,
    pub ext: String,
    pub pinned: bool,
    pub manual: bool,
    pub child_count: usize,
    pub degree: usize,
}

/// Compact edge for rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeView {
    pub id: Option<i64>,
    pub src: EntityId,
    pub dst: EntityId,
    pub kind: RelationKind,
    pub origin: EdgeOrigin,
    pub strength: f64,
    pub confidence: f64,
}

/// Galaxy-level cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterView {
    pub id: EntityId,
    pub name: String,
    pub path: String,
    pub x: f64,
    pub y: f64,
    pub r: f64,
    pub files: u64,
    pub bytes: u64,
    pub recent_mtime: Option<i64>,
    pub relation_count: usize,
}

/// Aggregated relations between two clusters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowView {
    pub a: EntityId,
    pub b: EntityId,
    pub count: usize,
    pub strength: f64,
    pub by_kind: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstellationView {
    pub id: i64,
    pub name: String,
    pub color: Option<String>,
    pub members: Vec<EntityId>,
    /// Positions of (at most 500) members, for drawing the constellation's halo.
    pub member_positions: Vec<[f64; 2]>,
    pub x: Option<f64>,
    pub y: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverviewView {
    pub clusters: Vec<ClusterView>,
    pub flows: Vec<FlowView>,
    pub constellations: Vec<ConstellationView>,
    pub bounds: Option<[f64; 4]>,
    pub total_nodes: usize,
    pub total_edges: usize,
    pub edges_by_kind: BTreeMap<String, usize>,
}

/// Viewport query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionQuery {
    /// Only nodes inside this entity's subtree (project mode).
    #[serde(default)]
    pub within: Option<EntityId>,
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
    /// Pixels per world unit.
    pub scale: f64,
    #[serde(default = "default_min_px")]
    pub min_px: f64,
    #[serde(default = "default_max_nodes")]
    pub max_nodes: usize,
    #[serde(default = "default_max_edges")]
    pub max_edges: usize,
    #[serde(default)]
    pub min_strength: f64,
    /// Restrict overlay edges to these kinds (`None` = all).
    #[serde(default)]
    pub kinds: Option<Vec<RelationKind>>,
    /// Entity whose neighbourhood is always included.
    #[serde(default)]
    pub focus: Option<EntityId>,
}

fn default_min_px() -> f64 {
    0.6
}
fn default_max_nodes() -> usize {
    2500
}
fn default_max_edges() -> usize {
    1200
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionView {
    pub nodes: Vec<NodeView>,
    pub edges: Vec<EdgeView>,
    /// Number of nodes in the viewport that were omitted (too small or over the cap).
    pub omitted_nodes: usize,
    pub omitted_edges: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationDetail {
    pub edge_id: Option<i64>,
    pub other_id: EntityId,
    pub other_name: String,
    pub other_path: String,
    pub other_state: Option<EntityState>,
    /// `outgoing` (this → other), `incoming` (other → this) or `undirected`.
    pub direction: String,
    pub kind: RelationKind,
    pub kind_label: String,
    pub origin: EdgeOrigin,
    pub weight: f64,
    pub confidence: f64,
    pub strength: f64,
    pub provenance: String,
    pub reasons: Vec<String>,
    pub state: EdgeState,
    /// `true` when the other endpoint is outside this entity's physical parent (a "satellite").
    pub elsewhere: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Breadcrumb {
    pub id: EntityId,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Satellite {
    pub entity_id: EntityId,
    pub name: String,
    pub path: String,
    pub score: f64,
    pub kinds: Vec<RelationKind>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    /// Entity inside the focused subtree that carries the strongest link.
    pub via: EntityId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityDetail {
    pub node: GraphNode,
    pub breadcrumb: Vec<Breadcrumb>,
    pub position: Option<NodePosition>,
    pub child_count: usize,
    pub relations: Vec<RelationDetail>,
    pub neighbors: Vec<Neighbor>,
    pub satellites: Vec<Satellite>,
    pub observations: Vec<Observation>,
    pub identity_candidates: Vec<StoredIdentityCandidate>,
    pub constellations: Vec<ConstellationView>,
    pub fingerprint: Option<FingerprintInfo>,
    pub preview: PreviewKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FingerprintInfo {
    pub sample_hash: String,
    pub sample_covers_full_file: bool,
    pub full_hash: Option<String>,
    pub valid_for_current_version: bool,
}

/// A project: a directory directly under a scan root (e.g. `~/projects/<name>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub id: EntityId,
    pub name: String,
    pub path: String,
    pub files: u64,
    pub bytes: u64,
    pub recent_mtime: Option<i64>,
    /// Entities outside the project related to something inside it (relations above the
    /// threshold of the query).
    pub external_links: usize,
    /// Distinct other projects reached by those relations.
    pub linked_projects: usize,
    /// Projects that look derived from, or built like, this one (see [`crate::kinship`]).
    pub relatives: Vec<Relative>,
}

/// A related project, from the point of view of another one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relative {
    pub id: EntityId,
    pub name: String,
    pub path: String,
    pub score: f64,
    pub vocabulary: f64,
    pub shared: Vec<String>,
    pub name_root: Option<String>,
    /// `Some(true)` if the relative came first, `Some(false)` if it came after, `None` if unclear.
    pub came_first: Option<bool>,
}

/// Default threshold of project kinship (see [`crate::kinship`]).
pub const KINSHIP_MIN_SCORE: f64 = 0.5;

/// An entity outside a project that is related to entities inside it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectLink {
    pub entity_id: EntityId,
    pub name: String,
    pub path: String,
    pub kind: EntryType,
    /// Strongest combined score over the inside entities it is related to.
    pub score: f64,
    pub kinds: Vec<RelationKind>,
    /// Inside entities it is related to, strongest first (at most 32).
    pub via: Vec<EntityId>,
}

/// External links of a project grouped by where they live: another project, or a directory
/// outside every project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkGroup {
    pub id: EntityId,
    pub name: String,
    pub path: String,
    pub is_project: bool,
    pub score: f64,
    pub links: Vec<ProjectLink>,
}

/// Project mode: the project's root and its relations with the rest of the space.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectView {
    pub root: NodeView,
    pub breadcrumb: Vec<Breadcrumb>,
    pub groups: Vec<LinkGroup>,
    /// External entities above the threshold, before the `limit` cap.
    pub total_links: usize,
    /// Pairs dropped because their score is below `min_score` (shown with a lower threshold).
    pub weak_links: usize,
    /// Projects that look derived from, or built like, this one.
    pub relatives: Vec<Relative>,
}

/// Combined score and relation kinds of each (inside, outside) pair of a project.
type PairScores = BTreeMap<(EntityId, EntityId), (f64, Vec<RelationKind>)>;

/// Kind of local preview the UI may request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewKind {
    None,
    Text,
    Image,
}

/// Maximum size of a file previewed as an image.
pub const PREVIEW_MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
/// Bytes read for a text preview.
pub const PREVIEW_TEXT_BYTES: usize = 16 * 1024;

pub fn preview_kind(node: &GraphNode) -> PreviewKind {
    if node.kind != EntryType::File || node.state != EntityState::Present {
        return PreviewKind::None;
    }
    match node.extension().as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
            if node.apparent_bytes <= PREVIEW_MAX_IMAGE_BYTES =>
        {
            PreviewKind::Image
        }
        "txt" | "md" | "markdown" | "rs" | "py" | "js" | "ts" | "json" | "toml" | "yaml"
        | "yml" | "csv" | "log" | "html" | "css" | "c" | "h" | "cpp" | "hpp" | "java" | "go"
        | "sh" | "ini" | "cfg" | "conf" | "xml" | "sql" | "tex" | "rst" | "org" => {
            PreviewKind::Text
        }
        _ => PreviewKind::None,
    }
}

impl Graph {
    /// Builds the read model from the store for one canvas.
    pub fn load(store: &OrbitStore, canvas_id: i64, cfg: RelationConfig) -> Result<Self, DbError> {
        let nodes: Vec<GraphNode> = store
            .load_nodes(true)?
            .into_iter()
            .filter(|n| n.state != EntityState::ProbablyDeleted)
            .collect();
        let edges = store.load_edges(Some(&[EdgeState::Active]))?;
        let positions = store.load_positions(canvas_id)?;
        let constellations = store.list_constellations(false)?;
        Ok(Self::from_parts(
            nodes,
            edges,
            positions,
            constellations,
            cfg,
        ))
    }

    pub fn from_parts(
        nodes: Vec<GraphNode>,
        edges: Vec<Edge>,
        positions: HashMap<EntityId, NodePosition>,
        constellations: Vec<Constellation>,
        cfg: RelationConfig,
    ) -> Self {
        let index: HashMap<EntityId, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.entity_id, i))
            .collect();
        let mut children: HashMap<EntityId, Vec<EntityId>> = HashMap::new();
        for n in &nodes {
            if let Some(p) = n.parent_entity_id {
                if index.contains_key(&p) {
                    children.entry(p).or_default().push(n.entity_id);
                }
            }
        }
        for v in children.values_mut() {
            v.sort_unstable();
        }
        let edges: Vec<Edge> = edges
            .into_iter()
            .filter(|e| {
                e.state == EdgeState::Active
                    && index.contains_key(&e.src)
                    && index.contains_key(&e.dst)
            })
            .collect();
        let mut adjacency: HashMap<EntityId, Vec<usize>> = HashMap::new();
        for (i, e) in edges.iter().enumerate() {
            adjacency.entry(e.src).or_default().push(i);
            adjacency.entry(e.dst).or_default().push(i);
        }
        let mut membership: HashMap<EntityId, Vec<i64>> = HashMap::new();
        for c in &constellations {
            for m in &c.members {
                membership.entry(*m).or_default().push(c.id);
            }
        }
        // Bottom-up recent activity: process deeper nodes first.
        let mut order: Vec<usize> = (0..nodes.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(nodes[i].depth));
        let mut recent: HashMap<EntityId, i64> = HashMap::new();
        for i in order {
            let n = &nodes[i];
            let own = if n.kind == EntryType::File {
                n.mtime_sec.unwrap_or(0)
            } else {
                0
            };
            let cur = recent.get(&n.entity_id).copied().unwrap_or(0).max(own);
            recent.insert(n.entity_id, cur);
            if let Some(p) = n.parent_entity_id {
                let e = recent.entry(p).or_insert(0);
                *e = (*e).max(cur);
            }
        }
        // Project of every node (memoised walk up to the child of a root).
        let mut project_of: HashMap<EntityId, EntityId> = HashMap::new();
        for n in &nodes {
            let mut path = Vec::new();
            let mut cur = n.entity_id;
            let project = loop {
                if let Some(&p) = project_of.get(&cur) {
                    break p;
                }
                let parent = nodes[index[&cur]]
                    .parent_entity_id
                    .filter(|p| index.contains_key(p) && *p != cur);
                match parent {
                    None => break cur,
                    Some(p) => {
                        let grand = nodes[index[&p]]
                            .parent_entity_id
                            .filter(|g| index.contains_key(g) && *g != p);
                        if grand.is_none() {
                            break cur;
                        }
                        path.push(cur);
                        cur = p;
                        if path.len() > 4096 {
                            break cur;
                        }
                    }
                }
            };
            project_of.insert(cur, project);
            for c in path {
                project_of.insert(c, project);
            }
        }
        Self {
            nodes,
            index,
            children,
            edges,
            adjacency,
            positions,
            constellations,
            membership,
            recent,
            project_of,
            cfg,
        }
    }

    pub fn nodes(&self) -> &[GraphNode] {
        &self.nodes
    }

    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    pub fn config(&self) -> &RelationConfig {
        &self.cfg
    }

    pub fn node(&self, id: EntityId) -> Option<&GraphNode> {
        self.index.get(&id).map(|&i| &self.nodes[i])
    }

    pub fn position(&self, id: EntityId) -> Option<&NodePosition> {
        self.positions.get(&id)
    }

    pub fn positions(&self) -> &HashMap<EntityId, NodePosition> {
        &self.positions
    }

    /// Updates positions in place after a save (avoids a full reload).
    pub fn apply_positions(&mut self, updates: &[NodePosition]) {
        for p in updates {
            self.positions.insert(p.entity_id, p.clone());
        }
    }

    /// Applies a stored edge change in place (insert, update or removal when no longer active)
    /// without reloading the whole graph from the store.
    pub fn upsert_edge(&mut self, e: Edge) {
        if let Some(id) = e.id {
            self.edges.retain(|x| x.id != Some(id));
        }
        if e.state == EdgeState::Active
            && self.index.contains_key(&e.src)
            && self.index.contains_key(&e.dst)
        {
            self.edges.push(e);
        }
        self.adjacency.clear();
        for (i, e) in self.edges.iter().enumerate() {
            self.adjacency.entry(e.src).or_default().push(i);
            self.adjacency.entry(e.dst).or_default().push(i);
        }
    }

    pub fn set_pinned_local(&mut self, ids: &[EntityId], pinned: bool) {
        for id in ids {
            if let Some(p) = self.positions.get_mut(id) {
                p.pinned = pinned;
            }
        }
    }

    pub fn children(&self, id: EntityId) -> &[EntityId] {
        self.children.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn degree(&self, id: EntityId) -> usize {
        self.adjacency.get(&id).map(Vec::len).unwrap_or(0)
    }

    /// Ancestors from the top-most known one down to `id` (inclusive).
    pub fn breadcrumb(&self, id: EntityId) -> Vec<Breadcrumb> {
        let mut out = Vec::new();
        let mut cur = Some(id);
        let mut guard = 0;
        while let Some(c) = cur {
            let Some(n) = self.node(c) else { break };
            out.push(Breadcrumb {
                id: c,
                name: n.name.clone(),
            });
            cur = n.parent_entity_id;
            guard += 1;
            if guard > 4096 {
                break;
            }
        }
        out.reverse();
        out
    }

    /// Edges incident to `id`.
    pub fn incident(&self, id: EntityId) -> impl Iterator<Item = &Edge> {
        self.adjacency
            .get(&id)
            .into_iter()
            .flatten()
            .map(move |&i| &self.edges[i])
    }

    /// Related entities ranked by combined score (descending), ties by id. ParentChild is not
    /// included: physical neighbours are reachable through the hierarchy.
    pub fn neighbors(&self, id: EntityId, limit: usize) -> Vec<Neighbor> {
        let mut grouped: BTreeMap<EntityId, Vec<&Edge>> = BTreeMap::new();
        for e in self.incident(id) {
            let other = if e.src == id { e.dst } else { e.src };
            grouped.entry(other).or_default().push(e);
        }
        let mut out: Vec<Neighbor> = grouped
            .into_iter()
            .map(|(other, es)| {
                let mut kinds: Vec<RelationKind> = es.iter().map(|e| e.kind).collect();
                kinds.sort_unstable();
                kinds.dedup();
                Neighbor {
                    entity_id: other,
                    score: combined_score(es.iter().copied(), &self.cfg),
                    kinds,
                }
            })
            .collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.entity_id.cmp(&b.entity_id))
        });
        out.truncate(limit);
        out
    }

    fn node_view(&self, n: &GraphNode) -> Option<NodeView> {
        let p = self.positions.get(&n.entity_id)?;
        Some(NodeView {
            id: n.entity_id,
            name: n.name.clone(),
            kind: n.kind,
            state: n.state,
            x: p.x,
            y: p.y,
            r: p.radius,
            parent: n.parent_entity_id,
            depth: n.depth,
            bytes: n.display_bytes(),
            mtime: if n.kind == EntryType::Directory {
                self.recent.get(&n.entity_id).copied().filter(|&t| t > 0)
            } else {
                n.mtime_sec
            },
            ext: n.extension(),
            pinned: p.pinned,
            manual: p.source == PositionSource::Manual,
            child_count: self.children(n.entity_id).len(),
            degree: self.degree(n.entity_id),
        })
    }

    /// Render views for the given ids (unknown or unpositioned ids are skipped).
    pub fn node_views(&self, ids: &[EntityId]) -> Vec<NodeView> {
        ids.iter()
            .filter_map(|id| self.node(*id))
            .filter_map(|n| self.node_view(n))
            .collect()
    }

    fn edge_view(e: &Edge) -> EdgeView {
        EdgeView {
            id: e.id,
            src: e.src,
            dst: e.dst,
            kind: e.kind,
            origin: e.origin,
            strength: e.strength(),
            confidence: e.confidence,
        }
    }

    fn constellation_views(&self) -> Vec<ConstellationView> {
        self.constellations
            .iter()
            .map(|c| ConstellationView {
                id: c.id,
                name: c.name.clone(),
                color: c.color.clone(),
                members: c.members.clone(),
                member_positions: c
                    .members
                    .iter()
                    .filter_map(|m| self.positions.get(m))
                    .take(500)
                    .map(|p| [p.x, p.y])
                    .collect(),
                x: c.x,
                y: c.y,
            })
            .collect()
    }

    /// Galaxy view: splits the largest clusters (by file count) until `max_clusters` is reached,
    /// then aggregates relations between clusters into at most `max_flows` flows.
    pub fn overview(&self, max_clusters: usize, max_flows: usize) -> OverviewView {
        self.overview_within(max_clusters, max_flows, None)
    }

    /// [`Graph::overview`] restricted to the subtree of `within` (project mode): clusters, flows,
    /// bounds and totals only cover that subtree.
    pub fn overview_within(
        &self,
        max_clusters: usize,
        max_flows: usize,
        within: Option<EntityId>,
    ) -> OverviewView {
        let max_clusters = max_clusters.clamp(1, 2000);
        let within = within.and_then(|w| self.node(w));
        let roots: Vec<EntityId> = if let Some(w) = within {
            vec![w.entity_id]
        } else {
            let mut r: Vec<EntityId> = self
                .nodes
                .iter()
                .filter(|n| {
                    n.parent_entity_id
                        .map(|p| !self.index.contains_key(&p))
                        .unwrap_or(true)
                })
                .map(|n| n.entity_id)
                .collect();
            r.sort_unstable();
            r
        };
        let weight = |id: EntityId| -> u64 {
            self.node(id)
                .map(|n| n.subtree_files_count.max(1))
                .unwrap_or(1)
        };
        let mut heap: BinaryHeap<(u64, std::cmp::Reverse<EntityId>)> = BinaryHeap::new();
        let mut leaves: Vec<EntityId> = Vec::new();
        for r in roots {
            heap.push((weight(r), std::cmp::Reverse(r)));
        }
        while let Some((w, std::cmp::Reverse(id))) = heap.pop() {
            let dirs: Vec<EntityId> = self
                .children(id)
                .iter()
                .copied()
                .filter(|c| {
                    self.node(*c)
                        .map(|n| n.kind == EntryType::Directory)
                        .unwrap_or(false)
                })
                .collect();
            let projected = heap.len() + leaves.len() + dirs.len() + 1;
            if dirs.is_empty() || projected > max_clusters || w < 2 {
                leaves.push(id);
                continue;
            }
            // Keep the parent as a cluster for its direct files, expand its subdirectories.
            leaves.push(id);
            for d in dirs {
                heap.push((weight(d), std::cmp::Reverse(d)));
            }
        }
        leaves.sort_unstable();
        let cluster_set: HashSet<EntityId> = leaves.iter().copied().collect();

        // Map every node to its nearest cluster ancestor (memoised walk).
        let mut cluster_of: HashMap<EntityId, EntityId> = HashMap::new();
        for n in &self.nodes {
            let mut path = Vec::new();
            let mut cur = Some(n.entity_id);
            let mut found = None;
            while let Some(c) = cur {
                if let Some(&cl) = cluster_of.get(&c) {
                    found = Some(cl);
                    break;
                }
                if cluster_set.contains(&c) {
                    found = Some(c);
                    break;
                }
                path.push(c);
                cur = self.node(c).and_then(|x| x.parent_entity_id);
            }
            if let Some(cl) = found {
                for p in path {
                    cluster_of.insert(p, cl);
                }
                cluster_of.insert(n.entity_id, cl);
            }
        }

        let mut flows: HashMap<(EntityId, EntityId), FlowView> = HashMap::new();
        let mut rel_count: HashMap<EntityId, usize> = HashMap::new();
        let mut edges_by_kind: BTreeMap<String, usize> = BTreeMap::new();
        for e in &self.edges {
            let (ca, cb) = (cluster_of.get(&e.src), cluster_of.get(&e.dst));
            if within.is_none() || (ca.is_some() && cb.is_some()) {
                *edges_by_kind.entry(e.kind.as_db().to_string()).or_default() += 1;
            }
            let (Some(&ca), Some(&cb)) = (ca, cb) else {
                continue;
            };
            *rel_count.entry(ca).or_default() += 1;
            if ca != cb {
                *rel_count.entry(cb).or_default() += 1;
                let key = if ca < cb { (ca, cb) } else { (cb, ca) };
                let f = flows.entry(key).or_insert_with(|| FlowView {
                    a: key.0,
                    b: key.1,
                    count: 0,
                    strength: 0.0,
                    by_kind: BTreeMap::new(),
                });
                f.count += 1;
                f.strength += e.strength();
                *f.by_kind.entry(e.kind.as_db().to_string()).or_default() += 1;
            }
        }
        let mut flows: Vec<FlowView> = flows.into_values().collect();
        flows.sort_by(|a, b| {
            b.strength
                .partial_cmp(&a.strength)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then((a.a, a.b).cmp(&(b.a, b.b)))
        });
        flows.truncate(max_flows);

        let clusters: Vec<ClusterView> = leaves
            .iter()
            .filter_map(|id| {
                let n = self.node(*id)?;
                let p = self.positions.get(id)?;
                Some(ClusterView {
                    id: *id,
                    name: n.name.clone(),
                    path: n.path_display.clone(),
                    x: p.x,
                    y: p.y,
                    r: p.radius,
                    files: n.subtree_files_count,
                    bytes: n.display_bytes(),
                    recent_mtime: self.recent.get(id).copied().filter(|&t| t > 0),
                    relation_count: rel_count.get(id).copied().unwrap_or(0),
                })
            })
            .collect();

        let bounds = self
            .positions
            .values()
            .filter(|p| {
                within.map_or(true, |w| {
                    self.node(p.entity_id)
                        .is_some_and(|n| is_within_raw(&n.path_raw, &w.path_raw))
                })
            })
            .fold(None, |acc: Option<[f64; 4]>, p| {
                let b = [
                    p.x - p.radius,
                    p.y - p.radius,
                    p.x + p.radius,
                    p.y + p.radius,
                ];
                Some(match acc {
                    None => b,
                    Some(a) => [
                        a[0].min(b[0]),
                        a[1].min(b[1]),
                        a[2].max(b[2]),
                        a[3].max(b[3]),
                    ],
                })
            });

        OverviewView {
            clusters,
            flows,
            constellations: self.constellation_views(),
            bounds,
            total_nodes: match within {
                Some(w) => self
                    .nodes
                    .iter()
                    .filter(|n| is_within_raw(&n.path_raw, &w.path_raw))
                    .count(),
                None => self.nodes.len(),
            },
            total_edges: if within.is_some() {
                edges_by_kind.values().sum()
            } else {
                self.edges.len()
            },
            edges_by_kind,
        }
    }

    /// System view: nodes in a viewport whose on-screen radius is at least `min_px`, capped, plus
    /// the strongest overlay edges among them and every edge incident to the focus.
    pub fn region(&self, q: &RegionQuery) -> RegionView {
        let max_nodes = q.max_nodes.clamp(1, 20_000);
        let max_edges = q.max_edges.min(20_000);
        let scale = if q.scale.is_finite() && q.scale > 0.0 {
            q.scale
        } else {
            1.0
        };
        let (x0, x1) = (q.x0.min(q.x1), q.x0.max(q.x1));
        let (y0, y1) = (q.y0.min(q.y1), q.y0.max(q.y1));
        let mut visible: Vec<(&GraphNode, &NodePosition)> = Vec::new();
        let mut omitted = 0usize;
        let within = q.within.and_then(|w| self.node(w));
        for n in &self.nodes {
            let Some(p) = self.positions.get(&n.entity_id) else {
                continue;
            };
            if within.is_some_and(|w| !is_within_raw(&n.path_raw, &w.path_raw)) {
                continue;
            }
            if p.x + p.radius < x0
                || p.x - p.radius > x1
                || p.y + p.radius < y0
                || p.y - p.radius > y1
            {
                continue;
            }
            if p.radius * scale < q.min_px {
                omitted += 1;
                continue;
            }
            visible.push((n, p));
        }
        visible.sort_by(|a, b| {
            b.1.radius
                .partial_cmp(&a.1.radius)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.entity_id.cmp(&b.0.entity_id))
        });
        if visible.len() > max_nodes {
            omitted += visible.len() - max_nodes;
            visible.truncate(max_nodes);
        }
        let mut selected: HashSet<EntityId> = visible.iter().map(|(n, _)| n.entity_id).collect();
        let mut extra: Vec<EntityId> = Vec::new();
        if let Some(f) = q.focus {
            if self.index.contains_key(&f) {
                if selected.insert(f) {
                    extra.push(f);
                }
                for nb in self.neighbors(f, 64) {
                    // In project mode outside entities are shown as satellites, not in place.
                    let outside = within.is_some_and(|w| {
                        self.node(nb.entity_id)
                            .is_some_and(|n| !is_within_raw(&n.path_raw, &w.path_raw))
                    });
                    if !outside && selected.insert(nb.entity_id) {
                        extra.push(nb.entity_id);
                    }
                }
            }
        }
        let kind_ok = |k: RelationKind| q.kinds.as_ref().map(|ks| ks.contains(&k)).unwrap_or(true);
        let mut overlay: Vec<&Edge> = self
            .edges
            .iter()
            .filter(|e| selected.contains(&e.src) && selected.contains(&e.dst))
            .filter(|e| kind_ok(e.kind))
            .filter(|e| {
                e.strength() >= q.min_strength
                    || Some(e.src) == q.focus
                    || Some(e.dst) == q.focus
                    || e.origin == EdgeOrigin::Manual
            })
            .collect();
        overlay.sort_by(|a, b| {
            let fa =
                Some(a.src) == q.focus || Some(a.dst) == q.focus || a.origin == EdgeOrigin::Manual;
            let fb =
                Some(b.src) == q.focus || Some(b.dst) == q.focus || b.origin == EdgeOrigin::Manual;
            fb.cmp(&fa)
                .then(
                    b.strength()
                        .partial_cmp(&a.strength())
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
                .then((a.src, a.dst, a.kind).cmp(&(b.src, b.dst, b.kind)))
        });
        let omitted_edges = overlay.len().saturating_sub(max_edges);
        overlay.truncate(max_edges);

        let mut nodes: Vec<NodeView> = visible
            .iter()
            .filter_map(|(n, _)| self.node_view(n))
            .collect();
        for id in extra {
            if let Some(v) = self.node(id).and_then(|n| self.node_view(n)) {
                nodes.push(v);
            }
        }
        RegionView {
            nodes,
            edges: overlay.into_iter().map(Self::edge_view).collect(),
            omitted_nodes: omitted,
            omitted_edges,
        }
    }

    /// Entities outside the subtree of `id` that are related to something inside it, ranked by the
    /// strongest combined score. For a file, the subtree is the file itself.
    pub fn satellites(&self, id: EntityId, limit: usize) -> Vec<Satellite> {
        let Some(root) = self.node(id) else {
            return Vec::new();
        };
        let inside = |n: &GraphNode| is_within_raw(&n.path_raw, &root.path_raw);
        let mut best: HashMap<EntityId, (f64, EntityId, Vec<RelationKind>)> = HashMap::new();
        let mut pair_edges: BTreeMap<(EntityId, EntityId), Vec<&Edge>> = BTreeMap::new();
        for e in &self.edges {
            let (Some(a), Some(b)) = (self.node(e.src), self.node(e.dst)) else {
                continue;
            };
            let (ia, ib) = (inside(a), inside(b));
            if ia == ib {
                continue;
            }
            let (inner, outer) = if ia {
                (a.entity_id, b.entity_id)
            } else {
                (b.entity_id, a.entity_id)
            };
            pair_edges.entry((inner, outer)).or_default().push(e);
        }
        for ((inner, outer), es) in pair_edges {
            let score = combined_score(es.iter().copied(), &self.cfg);
            let entry = best.entry(outer).or_insert((0.0, inner, Vec::new()));
            for e in &es {
                if !entry.2.contains(&e.kind) {
                    entry.2.push(e.kind);
                }
            }
            if score > entry.0 {
                entry.0 = score;
                entry.1 = inner;
            }
        }
        let mut out: Vec<Satellite> = best
            .into_iter()
            .filter_map(|(outer, (score, via, mut kinds))| {
                let n = self.node(outer)?;
                kinds.sort_unstable();
                let p = self.positions.get(&outer);
                Some(Satellite {
                    entity_id: outer,
                    name: n.name.clone(),
                    path: n.path_display.clone(),
                    score,
                    kinds,
                    x: p.map(|p| p.x),
                    y: p.map(|p| p.y),
                    via,
                })
            })
            .collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.entity_id.cmp(&b.entity_id))
        });
        out.truncate(limit);
        out
    }

    /// The project of `id`: its ancestor directly under a scan root.
    pub fn project_of(&self, id: EntityId) -> Option<EntityId> {
        self.project_of.get(&id).copied()
    }

    /// Kinship of every pair of projects scoring at least `min_score` (see [`crate::kinship`]).
    pub fn kinship(&self, min_score: f64) -> Vec<ProjectKin> {
        let roots: HashSet<EntityId> = self
            .nodes
            .iter()
            .map(|n| n.entity_id)
            .filter(|&id| self.is_root(id))
            .collect();
        project_kinship(&self.nodes, &self.project_of, &roots, min_score)
    }

    /// Relatives of `project` among `kin`, strongest first.
    fn relatives_of(&self, project: EntityId, kin: &[ProjectKin]) -> Vec<Relative> {
        kin.iter()
            .filter(|k| k.a == project || k.b == project)
            .filter_map(|k| {
                let other = if k.a == project { k.b } else { k.a };
                let n = self.node(other)?;
                Some(Relative {
                    id: other,
                    name: n.name.clone(),
                    path: n.path_display.clone(),
                    score: k.score,
                    vocabulary: k.vocabulary,
                    shared: k.shared.clone(),
                    name_root: k.name_root.clone(),
                    came_first: k.older.map(|o| o == other),
                })
            })
            .collect()
    }

    fn is_root(&self, id: EntityId) -> bool {
        self.node(id)
            .map(|n| {
                n.parent_entity_id
                    .map(|p| !self.index.contains_key(&p))
                    .unwrap_or(true)
            })
            .unwrap_or(false)
    }

    /// Whether a pair of entities in different projects, related by `es`, is shown with threshold
    /// `min_score`. Manual associations always are. Otherwise the combined score must reach the
    /// threshold and, unless the threshold is zero, at least one relation must rest on content or
    /// an explicit link: across projects, name genealogy and temporal proximity are circumstantial
    /// and mostly pair generic names (`test_x.py` ↔ `x.py`, `CLAUDE.md`, `.gitignore`).
    fn strong_external(&self, es: &[&Edge], min_score: f64) -> (bool, f64) {
        let score = combined_score(es.iter().copied(), &self.cfg);
        if es.iter().any(|e| e.origin == EdgeOrigin::Manual) {
            return (true, score);
        }
        let evidence = min_score <= 0.0
            || es.iter().any(|e| {
                !matches!(
                    e.kind,
                    RelationKind::NameSimilarity
                        | RelationKind::TemporalAffinity
                        | RelationKind::ParentChild
                )
            });
        (evidence && score >= min_score, score)
    }

    /// Combined score of every (inside, outside) pair of `project` shown with threshold
    /// `min_score` (see [`Graph::strong_external`]), plus the number of pairs left out.
    fn external_pairs(&self, project: EntityId, min_score: f64) -> (PairScores, usize) {
        let Some(root) = self.node(project) else {
            return (BTreeMap::new(), 0);
        };
        let mut pairs: BTreeMap<(EntityId, EntityId), Vec<&Edge>> = BTreeMap::new();
        for e in &self.edges {
            let (Some(a), Some(b)) = (self.node(e.src), self.node(e.dst)) else {
                continue;
            };
            let (ia, ib) = (
                is_within_raw(&a.path_raw, &root.path_raw),
                is_within_raw(&b.path_raw, &root.path_raw),
            );
            if ia == ib {
                continue;
            }
            let key = if ia { (e.src, e.dst) } else { (e.dst, e.src) };
            pairs.entry(key).or_default().push(e);
        }
        let mut weak = 0;
        let mut out = BTreeMap::new();
        for (key, es) in pairs {
            let (strong, score) = self.strong_external(&es, min_score);
            if !strong {
                weak += 1;
                continue;
            }
            let mut kinds: Vec<RelationKind> = es.iter().map(|e| e.kind).collect();
            kinds.sort_unstable();
            kinds.dedup();
            out.insert(key, (score, kinds));
        }
        (out, weak)
    }

    /// Projects (directories directly under each scan root), most recently active first, with
    /// how many outside entities and other projects their relations reach.
    pub fn projects(&self, min_score: f64) -> Vec<ProjectSummary> {
        let mut projects: Vec<EntityId> = self
            .nodes
            .iter()
            .filter(|n| n.kind == EntryType::Directory)
            .filter(|n| {
                n.parent_entity_id
                    .is_some_and(|p| self.index.contains_key(&p) && self.is_root(p))
            })
            .map(|n| n.entity_id)
            .collect();
        projects.sort_unstable();
        let set: HashSet<EntityId> = projects.iter().copied().collect();
        let mut outside: HashMap<EntityId, HashSet<EntityId>> = HashMap::new();
        let mut reached: HashMap<EntityId, HashSet<EntityId>> = HashMap::new();
        let mut pairs: HashMap<(EntityId, EntityId), Vec<&Edge>> = HashMap::new();
        for e in &self.edges {
            let (Some(&pa), Some(&pb)) = (self.project_of.get(&e.src), self.project_of.get(&e.dst))
            else {
                continue;
            };
            if pa == pb {
                continue;
            }
            let key = if e.src < e.dst {
                (e.src, e.dst)
            } else {
                (e.dst, e.src)
            };
            pairs.entry(key).or_default().push(e);
        }
        for ((a, b), es) in pairs {
            if !self.strong_external(&es, min_score).0 {
                continue;
            }
            let (pa, pb) = (self.project_of[&a], self.project_of[&b]);
            for (p, other, other_p) in [(pa, b, pb), (pb, a, pa)] {
                if set.contains(&p) {
                    outside.entry(p).or_default().insert(other);
                    if set.contains(&other_p) {
                        reached.entry(p).or_default().insert(other_p);
                    }
                }
            }
        }
        let kin = self.kinship(KINSHIP_MIN_SCORE);
        let mut out: Vec<ProjectSummary> = projects
            .into_iter()
            .filter_map(|id| {
                let n = self.node(id)?;
                Some(ProjectSummary {
                    id,
                    name: n.name.clone(),
                    path: n.path_display.clone(),
                    files: n.subtree_files_count,
                    bytes: n.display_bytes(),
                    recent_mtime: self.recent.get(&id).copied().filter(|&t| t > 0),
                    external_links: outside.get(&id).map(HashSet::len).unwrap_or(0),
                    linked_projects: reached.get(&id).map(HashSet::len).unwrap_or(0),
                    relatives: self.relatives_of(id, &kin),
                })
            })
            .collect();
        out.sort_by(|a, b| {
            b.recent_mtime
                .cmp(&a.recent_mtime)
                .then(a.name.cmp(&b.name))
        });
        out
    }

    /// Project mode: outside entities related to `project` (pairs scoring at least `min_score`),
    /// grouped by the project (or top-level directory) they live in, strongest first, at most
    /// `limit` entities overall.
    pub fn project_view(
        &self,
        project: EntityId,
        min_score: f64,
        limit: usize,
    ) -> Option<ProjectView> {
        let root = self.node(project).and_then(|n| self.node_view(n))?;
        let (pairs, weak_links) = self.external_pairs(project, min_score);
        // Per outside entity: best score, kinds, and (score, inside entity) pairs.
        type Acc = (f64, Vec<RelationKind>, Vec<(f64, EntityId)>);
        let mut by_outer: BTreeMap<EntityId, Acc> = BTreeMap::new();
        for ((inner, outer), (score, kinds)) in pairs {
            let entry = by_outer
                .entry(outer)
                .or_insert((0.0, Vec::new(), Vec::new()));
            entry.0 = entry.0.max(score);
            for k in kinds {
                if !entry.1.contains(&k) {
                    entry.1.push(k);
                }
            }
            entry.2.push((score, inner));
        }
        let mut links: Vec<ProjectLink> = by_outer
            .into_iter()
            .filter_map(|(outer, (score, mut kinds, mut via))| {
                let n = self.node(outer)?;
                kinds.sort_unstable();
                via.sort_by(|a, b| {
                    b.0.partial_cmp(&a.0)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(a.1.cmp(&b.1))
                });
                via.truncate(32);
                Some(ProjectLink {
                    entity_id: outer,
                    name: n.name.clone(),
                    path: n.path_display.clone(),
                    kind: n.kind,
                    score,
                    kinds,
                    via: via.into_iter().map(|(_, id)| id).collect(),
                })
            })
            .collect();
        let total_links = links.len();
        links.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.entity_id.cmp(&b.entity_id))
        });
        links.truncate(limit);
        let mut groups: BTreeMap<EntityId, LinkGroup> = BTreeMap::new();
        for l in links {
            // Its project directory, or the scan root for entities outside every project.
            let gid = match self.project_of(l.entity_id) {
                Some(p) if self.is_root(p) => p,
                Some(p) if self.node(p).is_some_and(|n| n.kind == EntryType::Directory) => p,
                Some(p) => self
                    .node(p)
                    .and_then(|n| n.parent_entity_id)
                    .filter(|r| self.index.contains_key(r))
                    .unwrap_or(p),
                None => l.entity_id,
            };
            let g = groups.entry(gid).or_insert_with(|| {
                let n = self.node(gid);
                LinkGroup {
                    id: gid,
                    name: n.map(|n| n.name.clone()).unwrap_or_default(),
                    path: n.map(|n| n.path_display.clone()).unwrap_or_default(),
                    is_project: !self.is_root(gid),
                    score: 0.0,
                    links: Vec::new(),
                }
            });
            g.score = g.score.max(l.score);
            g.links.push(l);
        }
        let mut groups: Vec<LinkGroup> = groups.into_values().collect();
        groups.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.id.cmp(&b.id))
        });
        let relatives = self.relatives_of(project, &self.kinship(KINSHIP_MIN_SCORE));
        Some(ProjectView {
            root,
            breadcrumb: self.breadcrumb(project),
            groups,
            total_links,
            weak_links,
            relatives,
        })
    }

    fn relation_detail(
        &self,
        id: EntityId,
        e: &Edge,
        store_node: Option<&GraphNode>,
    ) -> RelationDetail {
        let other = if e.src == id { e.dst } else { e.src };
        let me = self.node(id);
        let on = store_node.or_else(|| self.node(other));
        let direction = if !e.kind.is_directed() {
            "undirected"
        } else if e.src == id {
            "outgoing"
        } else {
            "incoming"
        };
        RelationDetail {
            edge_id: e.id,
            other_id: other,
            other_name: on.map(|n| n.name.clone()).unwrap_or_default(),
            other_path: on.map(|n| n.path_display.clone()).unwrap_or_default(),
            other_state: on.map(|n| n.state),
            direction: direction.to_string(),
            kind: e.kind,
            kind_label: kind_label(e.kind).to_string(),
            origin: e.origin,
            weight: e.weight,
            confidence: e.confidence,
            strength: e.strength(),
            provenance: e.provenance.clone(),
            reasons: e.reasons.clone(),
            state: e.state,
            elsewhere: match (me, on) {
                (Some(a), Some(b)) => a.parent_entity_id != b.parent_entity_id,
                _ => true,
            },
        }
    }

    /// File-level detail. Reads history, candidates, fingerprints and non-active edges from the store.
    pub fn entity_detail(&self, store: &OrbitStore, id: EntityId) -> Result<EntityDetail, DbError> {
        let all_nodes;
        let node = match self.node(id) {
            Some(n) => n.clone(),
            None => {
                all_nodes = store.load_nodes(true)?;
                all_nodes
                    .into_iter()
                    .find(|n| n.entity_id == id)
                    .ok_or_else(|| DbError::NotFound(format!("entità {}", id)))?
            }
        };
        let mut relations: Vec<RelationDetail> = self
            .incident(id)
            .map(|e| self.relation_detail(id, e, None))
            .collect();
        // Hidden/deleted edges stay visible in the detail so the user can restore them.
        let others: Vec<Edge> = store
            .load_edges(Some(&[EdgeState::Hidden, EdgeState::Deleted]))?
            .into_iter()
            .filter(|e| e.src == id || e.dst == id)
            .collect();
        for e in &others {
            relations.push(self.relation_detail(id, e, None));
        }
        relations.sort_by(|a, b| {
            (a.state != EdgeState::Active)
                .cmp(&(b.state != EdgeState::Active))
                .then(
                    b.strength
                        .partial_cmp(&a.strength)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
                .then(a.other_id.cmp(&b.other_id))
        });
        let fingerprint = store
            .load_fingerprints()?
            .remove(&id)
            .map(|fp| FingerprintInfo {
                valid_for_current_version: fp.matches_version(&node),
                sample_hash: fp.sample_hash,
                sample_covers_full_file: fp.sample_covers_full_file,
                full_hash: fp.full_hash,
            });
        let constellations = self
            .constellation_views()
            .into_iter()
            .filter(|c| {
                self.membership
                    .get(&id)
                    .map(|m| m.contains(&c.id))
                    .unwrap_or(false)
            })
            .collect();
        Ok(EntityDetail {
            breadcrumb: self.breadcrumb(id),
            position: self.positions.get(&id).cloned(),
            child_count: self.children(id).len(),
            neighbors: self.neighbors(id, 24),
            satellites: self.satellites(id, 16),
            relations,
            observations: store.entity_observations(id)?,
            identity_candidates: store.identity_candidates(id)?,
            constellations,
            fingerprint,
            preview: preview_kind(&node),
            node,
        })
    }

    /// Full JSON export of nodes, positions and active edges (ParentChild included as derived
    /// edges with `origin = filesystem`).
    pub fn export(&self) -> serde_json::Value {
        let nodes: Vec<serde_json::Value> = self
            .nodes
            .iter()
            .map(|n| {
                let p = self.positions.get(&n.entity_id);
                serde_json::json!({
                    "id": n.entity_id,
                    "kind": n.kind,
                    "state": n.state,
                    "name": n.name,
                    "path": n.path_display,
                    "parent": n.parent_entity_id,
                    "bytes": n.display_bytes(),
                    "mtime": n.mtime_sec,
                    "x": p.map(|p| p.x),
                    "y": p.map(|p| p.y),
                    "pinned": p.map(|p| p.pinned).unwrap_or(false),
                })
            })
            .collect();
        let mut edges: Vec<serde_json::Value> = self
            .nodes
            .iter()
            .filter_map(|n| {
                let p = n.parent_entity_id.filter(|p| self.index.contains_key(p))?;
                Some(serde_json::json!({
                    "src": p, "dst": n.entity_id, "kind": RelationKind::ParentChild,
                    "origin": EdgeOrigin::Filesystem, "weight": 1.0, "confidence": 1.0,
                    "provenance": "filesystem", "reasons": ["struttura fisica del filesystem"],
                }))
            })
            .collect();
        edges.extend(self.edges.iter().map(|e| {
            serde_json::json!({
                "id": e.id, "src": e.src, "dst": e.dst, "kind": e.kind, "origin": e.origin,
                "weight": e.weight, "confidence": e.confidence, "provenance": e.provenance,
                "reasons": e.reasons,
            })
        }));
        serde_json::json!({
            "format": "orbit-graph",
            "version": 1,
            "nodes": nodes,
            "edges": edges,
            "constellations": self.constellation_views(),
        })
    }
}
