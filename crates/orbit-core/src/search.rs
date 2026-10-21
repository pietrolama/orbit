//! Search engine for the gravitational search.
//!
//! The engine only *ranks* entities and returns their strongest relational neighbours with
//! reasons; it never changes the graph or the persistent layout. Attraction and restoration are
//! done by the UI on temporary coordinates.
//!
//! Query syntax (all clauses are AND-ed):
//!
//! | clause                         | meaning                                                   |
//! |--------------------------------|-----------------------------------------------------------|
//! | `fattura 2026`                 | every term must appear in the name or the path            |
//! | `"nota spese"`                 | quoted phrase                                             |
//! | `name:contratto`               | term must appear in the name                              |
//! | `path:progetti`                | term must appear in the path                              |
//! | `ext:pdf` / `ext:pdf,docx`     | extension filter                                          |
//! | `type:file` / `type:dir`       | entity kind (`file`, `dir`, `symlink`, `other`)           |
//! | `size:>10MB` `size:<=1k` `size:1MB..1GB` | size filter (B, K/KB/KiB, M/MB/MiB, G/GB/GiB, T…; 1024-based) |
//! | `modified:last-7d` (`h`, `m`, `d`, `w`) | modified within the last period                 |
//! | `modified:>2026-01-01` `modified:2026-01-01..2026-02-01` | modification date range (UTC) |
//! | `related-to:<entity-id>`       | entities related to the given one, ranked by relation score |

use crate::graph::Graph;
use crate::model::{EntityId, EntryType, GraphNode, RelationKind};
use crate::relations::kind_label;
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// A comparison bound.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Range {
    pub min: Option<i64>,
    pub max: Option<i64>,
    pub min_inclusive: bool,
    pub max_inclusive: bool,
}

impl Range {
    fn contains(&self, v: i64) -> bool {
        let lo = match self.min {
            Some(m) if self.min_inclusive => v >= m,
            Some(m) => v > m,
            None => true,
        };
        let hi = match self.max {
            Some(m) if self.max_inclusive => v <= m,
            Some(m) => v < m,
            None => true,
        };
        lo && hi
    }
}

/// Parsed query.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ParsedQuery {
    pub terms: Vec<String>,
    pub name_terms: Vec<String>,
    pub path_terms: Vec<String>,
    pub extensions: Vec<String>,
    pub kinds: Vec<EntryType>,
    pub size: Option<Range>,
    pub modified: Option<Range>,
    pub related_to: Option<EntityId>,
    pub warnings: Vec<String>,
}

impl ParsedQuery {
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
            && self.name_terms.is_empty()
            && self.path_terms.is_empty()
            && self.extensions.is_empty()
            && self.kinds.is_empty()
            && self.size.is_none()
            && self.modified.is_none()
            && self.related_to.is_none()
    }
}

fn tokenize(q: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for c in q.chars() {
        match c {
            '"' => {
                in_quote = !in_quote;
                if !in_quote && !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Parses a size like `10MB`, `1.5GiB`, `512`, `4k`. 1024-based.
pub fn parse_size(s: &str) -> Option<i64> {
    let s = s.trim().to_lowercase();
    let idx = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(idx);
    let n: f64 = num.parse().ok()?;
    let mult: f64 = match unit.trim() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let v = n * mult;
    (v.is_finite() && v >= 0.0).then_some(v as i64)
}

/// Parses `YYYY-MM-DD` (UTC midnight) into seconds since the epoch.
pub fn parse_date(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.trim().split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i64 = parts[0].parse().ok()?;
    let m: i64 = parts[1].parse().ok()?;
    let d: i64 = parts[2].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(1970..=9999).contains(&y) {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400)
}

fn parse_range(v: &str, parse: impl Fn(&str) -> Option<i64>) -> Option<Range> {
    if let Some((a, b)) = v.split_once("..") {
        return Some(Range {
            min: if a.is_empty() { None } else { Some(parse(a)?) },
            max: if b.is_empty() { None } else { Some(parse(b)?) },
            min_inclusive: true,
            max_inclusive: true,
        });
    }
    let (op, rest) = if let Some(r) = v.strip_prefix(">=") {
        (">=", r)
    } else if let Some(r) = v.strip_prefix("<=") {
        ("<=", r)
    } else if let Some(r) = v.strip_prefix('>') {
        (">", r)
    } else if let Some(r) = v.strip_prefix('<') {
        ("<", r)
    } else if let Some(r) = v.strip_prefix('=') {
        ("=", r)
    } else {
        ("=", v)
    };
    let n = parse(rest)?;
    Some(match op {
        ">=" => Range {
            min: Some(n),
            max: None,
            min_inclusive: true,
            max_inclusive: true,
        },
        ">" => Range {
            min: Some(n),
            max: None,
            min_inclusive: false,
            max_inclusive: true,
        },
        "<=" => Range {
            min: None,
            max: Some(n),
            min_inclusive: true,
            max_inclusive: true,
        },
        "<" => Range {
            min: None,
            max: Some(n),
            min_inclusive: true,
            max_inclusive: false,
        },
        _ => Range {
            min: Some(n),
            max: Some(n),
            min_inclusive: true,
            max_inclusive: true,
        },
    })
}

fn parse_period(v: &str) -> Option<i64> {
    let v = v.strip_prefix("last-")?;
    let (num, unit) = v.split_at(v.len().checked_sub(1)?);
    let n: i64 = num.parse().ok()?;
    let secs = match unit {
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => return None,
    };
    n.checked_mul(secs)
}

/// Parses a query string. `now` is the reference time (seconds) for relative periods.
pub fn parse_query(q: &str, now: i64) -> ParsedQuery {
    let mut pq = ParsedQuery::default();
    for tok in tokenize(q) {
        let Some((key, value)) = tok.split_once(':') else {
            pq.terms.push(tok.to_lowercase());
            continue;
        };
        let value = value.trim();
        let key_l = key.to_lowercase();
        let ok = match key_l.as_str() {
            "name" if !value.is_empty() => {
                pq.name_terms.push(value.to_lowercase());
                true
            }
            "path" if !value.is_empty() => {
                pq.path_terms.push(value.to_lowercase());
                true
            }
            "ext" if !value.is_empty() => {
                pq.extensions.extend(
                    value
                        .split(',')
                        .map(|e| e.trim().trim_start_matches('.').to_lowercase())
                        .filter(|e| !e.is_empty()),
                );
                true
            }
            "type" | "kind" => match value.to_lowercase().as_str() {
                "file" | "f" => {
                    pq.kinds.push(EntryType::File);
                    true
                }
                "dir" | "directory" | "d" | "cartella" => {
                    pq.kinds.push(EntryType::Directory);
                    true
                }
                "symlink" | "link" => {
                    pq.kinds.push(EntryType::Symlink);
                    true
                }
                "other" => {
                    pq.kinds.push(EntryType::Other);
                    true
                }
                _ => false,
            },
            "size" => match parse_range(value, parse_size) {
                Some(r) => {
                    pq.size = Some(r);
                    true
                }
                None => false,
            },
            "modified" | "mtime" => {
                if let Some(p) = parse_period(value) {
                    pq.modified = Some(Range {
                        min: Some(now - p),
                        max: None,
                        min_inclusive: true,
                        max_inclusive: true,
                    });
                    true
                } else if let Some(mut r) = parse_range(value, parse_date) {
                    // A date upper bound includes the whole day.
                    if let Some(m) = r.max {
                        if r.max_inclusive {
                            r.max = Some(m + 86_399);
                        }
                    }
                    pq.modified = Some(r);
                    true
                } else {
                    false
                }
            }
            "related-to" | "related" => match value.parse::<EntityId>() {
                Ok(id) => {
                    pq.related_to = Some(id);
                    true
                }
                Err(_) => false,
            },
            _ => {
                // Not a known filter (e.g. a time "10:30"): treat the whole token as text.
                pq.terms.push(tok.to_lowercase());
                true
            }
        };
        if !ok {
            pq.warnings
                .push(format!("filtro non valido ignorato: «{}»", tok));
        }
    }
    pq
}

/// One ranked result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub entity_id: EntityId,
    pub name: String,
    pub path: String,
    pub kind: EntryType,
    pub match_score: f64,
    pub matched_fields: Vec<String>,
    pub matched_reasons: Vec<String>,
    pub relation_neighbors: Vec<EntityId>,
    pub neighbor_scores: Vec<f64>,
    pub neighbor_kinds: Vec<Vec<RelationKind>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchOptions {
    pub limit: usize,
    pub neighbors_per_result: usize,
    /// Neighbours are computed only for the first `neighbors_for_top` results.
    pub neighbors_for_top: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            limit: 100,
            neighbors_per_result: 5,
            neighbors_for_top: 40,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub query: String,
    pub parsed: ParsedQuery,
    pub results: Vec<SearchResult>,
    pub total_matches: usize,
    pub truncated: bool,
    pub elapsed_us: u64,
}

fn term_score(name: &str, path: &str, term: &str) -> Option<(f64, &'static str)> {
    if name == term {
        Some((1.0, "name"))
    } else if name.starts_with(term) {
        Some((0.8, "name"))
    } else if let Some(pos) = name.find(term) {
        let boundary = name[..pos]
            .chars()
            .last()
            .map(|c| !c.is_alphanumeric())
            .unwrap_or(true);
        Some((if boundary { 0.65 } else { 0.5 }, "name"))
    } else if path.contains(term) {
        Some((0.25, "path"))
    } else {
        None
    }
}

fn human_bytes(b: i64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < units.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} B", b)
    } else {
        format!("{:.1} {}", v, units[u])
    }
}

/// Evaluates filters and text terms on one node. Returns `None` if it does not match.
fn evaluate(n: &GraphNode, pq: &ParsedQuery, now: i64) -> Option<(f64, Vec<String>, Vec<String>)> {
    let mut fields: Vec<String> = Vec::new();
    let mut reasons: Vec<String> = Vec::new();
    if !pq.kinds.is_empty() {
        if !pq.kinds.contains(&n.kind) {
            return None;
        }
        fields.push("type".into());
    }
    let ext = n.extension();
    if !pq.extensions.is_empty() {
        if !pq.extensions.contains(&ext) {
            return None;
        }
        fields.push("ext".into());
        reasons.push(format!("estensione .{}", ext));
    }
    if let Some(r) = &pq.size {
        let size = n.display_bytes() as i64;
        if !r.contains(size) {
            return None;
        }
        fields.push("size".into());
        reasons.push(format!("dimensione {}", human_bytes(size)));
    }
    if let Some(r) = &pq.modified {
        let t = n.mtime_sec?;
        if !r.contains(t) {
            return None;
        }
        fields.push("modified".into());
        let days = (now - t) as f64 / 86_400.0;
        reasons.push(if days < 1.0 {
            "modificato nelle ultime 24 ore".to_string()
        } else {
            format!("modificato {:.0} giorni fa", days)
        });
    }
    let name = n.name.to_lowercase();
    let path = n.path_display.to_lowercase();
    let mut total = 0.0;
    let mut count = 0usize;
    for t in &pq.name_terms {
        let pos = name.find(t.as_str())?;
        total += if name == *t {
            1.0
        } else if pos == 0 {
            0.8
        } else {
            0.6
        };
        count += 1;
        if !fields.contains(&"name".to_string()) {
            fields.push("name".into());
        }
        reasons.push(format!("il nome contiene «{}»", t));
    }
    for t in &pq.path_terms {
        if !path.contains(t.as_str()) {
            return None;
        }
        total += 0.4;
        count += 1;
        if !fields.contains(&"path".to_string()) {
            fields.push("path".into());
        }
        reasons.push(format!("il percorso contiene «{}»", t));
    }
    for t in &pq.terms {
        let (s, field) = term_score(&name, &path, t)?;
        total += s;
        count += 1;
        if !fields.iter().any(|f| f == field) {
            fields.push(field.to_string());
        }
        reasons.push(if field == "name" {
            format!("il nome contiene «{}»", t)
        } else {
            format!("il percorso contiene «{}»", t)
        });
    }
    let mut score = if count > 0 { total / count as f64 } else { 0.5 };
    if count > 0 {
        if n.kind == EntryType::File {
            score += 0.02;
        }
        if let Some(t) = n.mtime_sec {
            if now - t >= 0 && now - t < 7 * 86_400 {
                score += 0.05;
            }
        }
    }
    Some((score.min(1.0), fields, reasons))
}

/// Runs a query on the graph. Ranking is deterministic: score desc, then size or mtime desc
/// (depending on the filters), then entity id asc.
pub fn search(graph: &Graph, query: &str, opts: &SearchOptions, now: i64) -> SearchResponse {
    let started = Instant::now();
    let pq = parse_query(query, now);
    let limit = opts.limit.clamp(1, 2000);
    let mut matches: Vec<(f64, &GraphNode, Vec<String>, Vec<String>)> = Vec::new();

    if pq.is_empty() {
        return SearchResponse {
            query: query.to_string(),
            parsed: pq,
            results: Vec::new(),
            total_matches: 0,
            truncated: false,
            elapsed_us: started.elapsed().as_micros() as u64,
        };
    }

    if let Some(anchor) = pq.related_to {
        for nb in graph.neighbors(anchor, 10_000) {
            let Some(n) = graph.node(nb.entity_id) else {
                continue;
            };
            let text_only = ParsedQuery {
                related_to: None,
                warnings: Vec::new(),
                ..pq.clone()
            };
            let Some((s, mut fields, mut reasons)) = evaluate(n, &text_only, now) else {
                continue;
            };
            let has_text = !text_only.terms.is_empty()
                || !text_only.name_terms.is_empty()
                || !text_only.path_terms.is_empty();
            fields.insert(0, "relation".into());
            for k in &nb.kinds {
                reasons.insert(0, format!("collegato a {} ({})", anchor, kind_label(*k)));
            }
            let score = if has_text {
                0.5 * nb.score + 0.5 * s
            } else {
                nb.score
            };
            matches.push((score, n, fields, reasons));
        }
    } else {
        for n in graph.nodes() {
            if let Some((s, f, r)) = evaluate(n, &pq, now) {
                matches.push((s, n, f, r));
            }
        }
    }

    let by_size = pq.size.is_some();
    matches.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                if by_size {
                    b.1.display_bytes().cmp(&a.1.display_bytes())
                } else {
                    b.1.mtime_sec
                        .unwrap_or(i64::MIN)
                        .cmp(&a.1.mtime_sec.unwrap_or(i64::MIN))
                }
            })
            .then(a.1.entity_id.cmp(&b.1.entity_id))
    });
    let total = matches.len();
    let truncated = total > limit;
    matches.truncate(limit);

    let results = matches
        .into_iter()
        .enumerate()
        .map(|(i, (score, n, fields, reasons))| {
            let nbs = if i < opts.neighbors_for_top {
                graph.neighbors(n.entity_id, opts.neighbors_per_result)
            } else {
                Vec::new()
            };
            SearchResult {
                entity_id: n.entity_id,
                name: n.name.clone(),
                path: n.path_display.clone(),
                kind: n.kind,
                match_score: (score * 1e6).round() / 1e6,
                matched_fields: fields,
                matched_reasons: reasons,
                relation_neighbors: nbs.iter().map(|x| x.entity_id).collect(),
                neighbor_scores: nbs.iter().map(|x| (x.score * 1e6).round() / 1e6).collect(),
                neighbor_kinds: nbs.iter().map(|x| x.kinds.clone()).collect(),
            }
        })
        .collect();

    SearchResponse {
        query: query.to_string(),
        parsed: pq,
        results,
        total_matches: total,
        truncated,
        elapsed_us: started.elapsed().as_micros() as u64,
    }
}
