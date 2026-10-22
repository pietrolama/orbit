//! Kinship between projects: which projects look derived from, or built like, one another.
//!
//! Relations between single files miss a project rewritten from another (its code changed, so no
//! duplicate, and nothing references the original). What survives a rewrite is the *vocabulary*:
//! module and folder names (`memory/episodic`, `policy`, `registry`) and, often, the project's own
//! name (`ghostFrame` → `ghostMobile`, `pizzaLAB` → `pizzaLAB2`).
//!
//! * **Vocabulary**: each project is the set of its file and directory name stems plus
//!   `folder/name` pairs, skipping generated trees (build output, tool caches, editor metadata,
//!   excluded directories). Two projects are compared by the cosine of their sets weighted by
//!   `idf²`, where `idf = ln(projects / projects using the name)`: sharing `readme` or `index`
//!   says little, sharing `memory/episodic` says a lot.
//! * **Name**: a word of at least four letters shared by the two project names and used by at
//!   most three projects. It only amplifies a vocabulary signal (adds up to `min(0.5, 2·v)`),
//!   never creates kinship alone: `hackThis` and `neuronHACK` share a word, not a structure.
//! * **Time**: the project whose files are older (10th percentile of mtimes) came first, stated
//!   only when the two differ by at least three days.

use crate::model::{AccessStatus, EntityId, EntryType, GraphNode};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Directory names whose content is generated, vendored or tool metadata: never vocabulary.
const GENERATED: &[&str] = &[
    "build",
    "cmakefiles",
    ".godot",
    ".import",
    ".cxx",
    ".chroma",
    "dist",
    "vendor",
    "third_party",
    "site-packages",
    ".claude",
    ".idea",
    ".vscode",
];

/// Minimum distance between the first activity of two projects to state which came first.
const ORDER_MIN_SECS: i64 = 3 * 86_400;

/// Kinship of two projects, with what explains it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectKin {
    pub a: EntityId,
    pub b: EntityId,
    /// Combined score in `[0, 1]`.
    pub score: f64,
    /// Vocabulary similarity alone.
    pub vocabulary: f64,
    /// Most specific shared names (rarest first, at most 10).
    pub shared: Vec<String>,
    /// Word shared by the two project names, when it contributed.
    pub name_root: Option<String>,
    /// The project that came first, when their first activity is clearly apart.
    pub older: Option<EntityId>,
}

fn is_generated_name(name: &str) -> bool {
    let n = name.to_lowercase();
    GENERATED.contains(&n.as_str()) || n.ends_with(".egg-info")
}

/// Vocabulary token of a file or directory name: the lowercase stem without leading dots, or
/// `None` for short, numeric or hash-like names.
fn token(name: &str) -> Option<String> {
    let n = name.to_lowercase();
    let stem = match n.rfind('.') {
        Some(i) if i > 0 => &n[..i],
        _ => n.as_str(),
    };
    let stem = stem.trim_start_matches('.');
    let hashy = stem.len() >= 12 && stem.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    let numeric = stem.chars().all(|c| c.is_ascii_digit() || c == '.');
    if stem.chars().count() < 3 || hashy || numeric {
        return None;
    }
    Some(stem.to_string())
}

/// Words of a project name: camelCase and separators split, lowercase, at least four letters.
fn name_words(name: &str) -> BTreeSet<String> {
    let mut words = BTreeSet::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        let boundary = !c.is_alphabetic() || (c.is_uppercase() && prev_lower);
        if boundary && !cur.is_empty() {
            words.insert(std::mem::take(&mut cur));
        }
        if c.is_alphabetic() {
            cur.extend(c.to_lowercase());
        }
        prev_lower = c.is_lowercase();
    }
    if !cur.is_empty() {
        words.insert(cur);
    }
    words.retain(|w| w.chars().count() >= 4);
    words
}

/// Kinship of every pair of projects scoring at least `min_score`, strongest first. `projects`
/// maps every node to its project (see `Graph::project_of`); only directories that are their own
/// project's root and not scan roots themselves (`roots`) are projects.
pub fn project_kinship(
    nodes: &[GraphNode],
    project_of: &HashMap<EntityId, EntityId>,
    roots: &HashSet<EntityId>,
    min_score: f64,
) -> Vec<ProjectKin> {
    let by_id: HashMap<EntityId, &GraphNode> = nodes.iter().map(|n| (n.entity_id, n)).collect();
    let is_project = |id: EntityId| {
        !roots.contains(&id)
            && project_of.get(&id) == Some(&id)
            && by_id
                .get(&id)
                .is_some_and(|n| n.kind == EntryType::Directory)
    };

    // Generated subtrees, parents first.
    let mut order: Vec<&GraphNode> = nodes.iter().collect();
    order.sort_by_key(|n| (n.depth, n.entity_id));
    let mut generated: HashMap<EntityId, bool> = HashMap::new();
    for n in &order {
        let inherited = n
            .parent_entity_id
            .and_then(|p| generated.get(&p).copied())
            .unwrap_or(false);
        let own = n.access_status == AccessStatus::ExcludedMount || is_generated_name(&n.name);
        generated.insert(n.entity_id, inherited || own);
    }

    let mut vocab: BTreeMap<EntityId, HashSet<String>> = BTreeMap::new();
    let mut mtimes: HashMap<EntityId, Vec<i64>> = HashMap::new();
    for n in nodes {
        let Some(&p) = project_of.get(&n.entity_id) else {
            continue;
        };
        if p == n.entity_id || !is_project(p) || generated[&n.entity_id] {
            continue;
        }
        if n.kind == EntryType::File {
            if let Some(m) = n.mtime_sec {
                mtimes.entry(p).or_default().push(m);
            }
        }
        let Some(t) = token(&n.name) else {
            continue;
        };
        let set = vocab.entry(p).or_default();
        if let Some(parent) = n
            .parent_entity_id
            .filter(|&q| q != p && !generated.get(&q).copied().unwrap_or(true))
            .and_then(|q| by_id.get(&q))
            .and_then(|q| token(&q.name))
        {
            set.insert(format!("{}/{}", parent, t));
        }
        set.insert(t);
    }
    let count = vocab.len();
    if count < 2 {
        return Vec::new();
    }

    // Document frequency over projects; names used by more than half of them carry no signal.
    let mut users: HashMap<&str, Vec<EntityId>> = HashMap::new();
    for (&p, set) in &vocab {
        for t in set {
            users.entry(t.as_str()).or_default().push(p);
        }
    }
    let idf = |df: usize| (count as f64 / df as f64).ln();
    let useful = |df: usize| df > 1 && df * 2 <= count.max(4);
    let mut norm: HashMap<EntityId, f64> = HashMap::new();
    for ps in users.values() {
        if useful(ps.len()) {
            let w = idf(ps.len()).powi(2);
            for p in ps {
                *norm.entry(*p).or_default() += w;
            }
        }
    }
    type Pair = (f64, Vec<(f64, String)>);
    let mut pairs: BTreeMap<(EntityId, EntityId), Pair> = BTreeMap::new();
    for (t, ps) in &users {
        if !useful(ps.len()) {
            continue;
        }
        let w = idf(ps.len()).powi(2);
        for (i, &a) in ps.iter().enumerate() {
            for &b in &ps[i + 1..] {
                let key = if a < b { (a, b) } else { (b, a) };
                let e = pairs.entry(key).or_insert((0.0, Vec::new()));
                e.0 += w;
                e.1.push((w, t.to_string()));
            }
        }
    }

    let words: HashMap<EntityId, BTreeSet<String>> = vocab
        .keys()
        .map(|&p| (p, name_words(&by_id[&p].name)))
        .collect();
    let mut word_df: HashMap<&str, usize> = HashMap::new();
    for ws in words.values() {
        for w in ws {
            *word_df.entry(w.as_str()).or_default() += 1;
        }
    }
    let first_activity = |p: EntityId| -> Option<i64> {
        let mut v = mtimes.get(&p)?.clone();
        v.sort_unstable();
        v.get(v.len() / 10).copied()
    };

    let mut out: Vec<ProjectKin> = pairs
        .into_iter()
        .filter_map(|((a, b), (sum, mut shared))| {
            let denom = (norm.get(&a)? * norm.get(&b)?).sqrt();
            if denom <= 0.0 {
                return None;
            }
            let vocabulary = (sum / denom).clamp(0.0, 1.0);
            let name_root = words[&a]
                .intersection(&words[&b])
                .find(|w| word_df[w.as_str()] <= 3)
                .cloned();
            let boost = if name_root.is_some() {
                (2.0 * vocabulary).min(0.5)
            } else {
                0.0
            };
            let score = 1.0 - (1.0 - vocabulary) * (1.0 - boost);
            if score < min_score {
                return None;
            }
            shared.sort_by(|x, y| {
                y.0.partial_cmp(&x.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(x.1.cmp(&y.1))
            });
            let older = match (first_activity(a), first_activity(b)) {
                (Some(fa), Some(fb)) if (fa - fb).abs() >= ORDER_MIN_SECS => {
                    Some(if fa < fb { a } else { b })
                }
                _ => None,
            };
            Some(ProjectKin {
                a,
                b,
                score,
                vocabulary,
                shared: shared.into_iter().take(10).map(|(_, t)| t).collect(),
                name_root: name_root.filter(|_| boost > 0.0),
                older,
            })
        })
        .collect();
    out.sort_by(|x, y| {
        y.score
            .partial_cmp(&x.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then((x.a, x.b).cmp(&(y.a, y.b)))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_drop_extensions_dots_and_noise() {
        assert_eq!(token("Episodic.py").as_deref(), Some("episodic"));
        assert_eq!(token(".gitignore").as_deref(), Some("gitignore"));
        assert_eq!(token("main.cpp.o").as_deref(), Some("main.cpp"));
        assert_eq!(token("a.rs"), None);
        assert_eq!(token("3f672054-f746-410e"), None);
        assert_eq!(token("2026"), None);
    }

    #[test]
    fn project_names_split_into_words() {
        let w = |s: &str| name_words(s).into_iter().collect::<Vec<_>>();
        assert_eq!(w("ghostFrame2.0"), vec!["frame", "ghost"]);
        assert_eq!(w("pizzaLAB2"), vec!["pizza"]);
        assert_eq!(w("neuronHACK"), vec!["hack", "neuron"]);
        assert_eq!(
            w("michelin-star-recepy"),
            vec!["michelin", "recepy", "star"]
        );
    }
}
