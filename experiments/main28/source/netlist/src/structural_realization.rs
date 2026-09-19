//! Unified structural-realization cone enumeration for topology-changing V1.
//!
//! Rewrite rules only populate a local logical e-graph.  Enumeration and
//! scoring consume complete selected-enode cones and are intentionally
//! independent of the rule family that happened to create them.

use crate::language::{StdCellLanguage, StdCellType};
use crate::netlist::Netlist;
use crate::physical_topology_egraph::TopologyExpr;
use crate::rule::JsonRules;
use egg::{EGraph, Id, Runner, Symbol};
use petgraph::graph::NodeIndex;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;
use std::path::Path;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::time::Duration;

const MAX_REALIZATIONS_PER_ROOT: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleFamily {
    Inv,
    Dmg,
    Comm,
    Expand,
    Mixed,
}

#[derive(Clone, Debug, Serialize)]
pub struct StructuralRealization {
    pub root_anchor: usize,
    pub root_eclass: String,
    pub root_enode: String,
    #[serde(skip)]
    pub expression: TopologyExpr,
    /// Canonical, ordered expression used for manifests and debugging.  Pin
    /// order is intentionally retained because it can change physical PPA.
    pub expression_text: String,
    pub boundary_anchors: Vec<usize>,
    pub created_logical_nodes: usize,
    pub logic_depth: usize,
    pub structural_hash: String,
    /// Debug metadata only.  It must never participate in scoring.
    pub provenance: Vec<String>,
    pub first_seen_family: RuleFamily,
}

#[derive(Clone, Debug, Serialize)]
pub struct DedupStats {
    /// Aggregate size of the per-anchor bounded local e-graphs.  These are
    /// intentionally not global logical-egraph counts because physical
    /// occurrence anchors remain independent.
    pub local_eclasses: usize,
    pub local_enodes: usize,
    pub raw_realizations: usize,
    pub unique_realizations: usize,
    pub duplicate_realizations: usize,
    pub cycle_pruned: usize,
    pub gate_bound_pruned: usize,
    /// JSON rules removed because they explicitly equate drive variants.
    pub excluded_phase1_drive_rewrites: usize,
    /// Forward-only aliases which preserve structural reachability from every
    /// incumbent family member without creating sibling drive alternatives.
    pub added_direct_structural_seed_rewrites: usize,
}

fn strip_drive_strength(op: &str) -> String {
    let bytes = op.as_bytes();
    for index in 0..bytes.len().saturating_sub(1) {
        if bytes[index] == b'x' && bytes[index + 1].is_ascii_digit() {
            let mut end = index + 1;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            return format!("{}{}", &op[..index], &op[end..]);
        }
    }
    op.to_owned()
}

impl StructuralRealization {
    /// True only for a one-cell, same-pin-order drive-strength replacement.
    /// Such choices belong to the existing A2 inner sizing space and are not
    /// structural candidates.  COMM remains structural because the ordered
    /// boundary list changes.
    pub fn is_scale_only_alias(&self, source: &Netlist<StdCellType, ()>) -> bool {
        let anchor = NodeIndex::new(self.root_anchor);
        let original_children: Vec<_> = source.inputs(anchor).collect();
        match &self.expression {
            TopologyExpr::Cell { op, children } => {
                children.len() == original_children.len()
                    && strip_drive_strength(op)
                        == strip_drive_strength(&source.graph[anchor].to_string())
                    && children.iter().zip(original_children).all(|(child, original)| {
                        matches!(child, TopologyExpr::Anchor(anchor) if *anchor == original)
                    })
            }
            TopologyExpr::Anchor(_) => false,
        }
    }
}

#[derive(Clone, Debug)]
struct EnumeratedExpr {
    expression: TopologyExpr,
    gates: usize,
    depth: usize,
    root_enode: String,
}

fn boundary_symbol(anchor: NodeIndex) -> Symbol {
    format!("__egg_boundary_{}", anchor.index()).into()
}

fn add_local_cone(
    egraph: &mut EGraph<StdCellLanguage, ()>,
    source: &Netlist<StdCellType, ()>,
    anchor: NodeIndex,
    remaining_depth: usize,
    anchor_ids: &mut FxHashMap<NodeIndex, Id>,
    boundaries: &mut FxHashMap<Symbol, NodeIndex>,
) -> Id {
    if remaining_depth == 0 || source.leaves.contains(&anchor) {
        let symbol = boundary_symbol(anchor);
        boundaries.insert(symbol, anchor);
        return egraph.add(StdCellLanguage::Symbol(symbol));
    }
    if let Some(existing) = anchor_ids.get(&anchor) {
        return *existing;
    }
    let children: Vec<_> = source
        .inputs(anchor)
        .map(|child| {
            add_local_cone(
                egraph,
                source,
                child,
                remaining_depth - 1,
                anchor_ids,
                boundaries,
            )
        })
        .collect();
    let id = egraph.add(StdCellLanguage::Gate(
        source.graph[anchor].to_string().into(),
        children.into(),
    ));
    anchor_ids.insert(anchor, id);
    id
}

pub(crate) fn expression_text(expression: &TopologyExpr) -> String {
    match expression {
        TopologyExpr::Anchor(anchor) => format!("@{}", anchor.index()),
        TopologyExpr::Cell { op, children } => format!(
            "{}({})",
            op,
            children
                .iter()
                .map(expression_text)
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

pub(crate) fn expr_hash(expression: &TopologyExpr) -> String {
    match expression {
        TopologyExpr::Anchor(anchor) => format!("@{}", anchor.index()),
        TopologyExpr::Cell { op, children } => format!(
            "{}({})",
            strip_drive_strength(op),
            children.iter().map(expr_hash).collect::<Vec<_>>().join(",")
        ),
    }
}

fn collect_boundaries(expression: &TopologyExpr, output: &mut FxHashSet<usize>) {
    match expression {
        TopologyExpr::Anchor(anchor) => {
            output.insert(anchor.index());
        }
        TopologyExpr::Cell { children, .. } => {
            for child in children {
                collect_boundaries(child, output);
            }
        }
    }
}

fn inherit_preserved_root_size(expression: &mut TopologyExpr, original_op: &str) {
    if let TopologyExpr::Cell { op, .. } = expression {
        if strip_drive_strength(op) == strip_drive_strength(original_op) {
            *op = original_op.to_owned();
        }
    }
}

struct ConeEnumerator<'a> {
    egraph: &'a mut EGraph<StdCellLanguage, ()>,
    root: Id,
    collapse: FxHashMap<Id, NodeIndex>,
    boundaries: &'a FxHashMap<Symbol, NodeIndex>,
    max_gates: usize,
    stats: DedupStats,
    /// Exact dynamic-programming cache.  Bidirectional rewrites make the
    /// e-graph cyclic, so the active ancestor set is part of the key; two
    /// calls are shared only when cycle legality is identical.
    memo: FxHashMap<(usize, Vec<usize>), Vec<EnumeratedExpr>>,
    memo_enabled: bool,
    /// Opt-in: prune paths already deeper than the allowed gate count before
    /// recursing. Legacy prefix-cap behavior is retained when disabled.
    depth_bound: bool,
}

impl<'a> ConeEnumerator<'a> {
    fn enumerate(&mut self, class: Id, stack: &mut FxHashSet<Id>) -> Vec<EnumeratedExpr> {
        let canonical = self.egraph.find(class);
        if canonical != self.root {
            if let Some(anchor) = self.collapse.get(&canonical) {
                return vec![EnumeratedExpr {
                    expression: TopologyExpr::Anchor(*anchor),
                    gates: 0,
                    depth: 0,
                    root_enode: format!("boundary:{}", anchor.index()),
                }];
            }
        }
        if !stack.insert(canonical) {
            self.stats.cycle_pruned += 1;
            return Vec::new();
        }
        let mut ancestors: Vec<_> = stack
            .iter()
            .filter(|ancestor| **ancestor != canonical)
            .map(|ancestor| usize::from(*ancestor))
            .collect();
        ancestors.sort_unstable();
        let memo_key = (usize::from(canonical), ancestors);
        if self.memo_enabled {
            if let Some(cached) = self.memo.get(&memo_key) {
                stack.remove(&canonical);
                return cached.clone();
            }
        }
        let mut nodes = self.egraph[canonical].nodes.clone();
        nodes.sort_by_key(|node| format!("{node:?}"));
        let mut result = Vec::new();
        for node in nodes {
            match node {
                StdCellLanguage::Symbol(symbol) | StdCellLanguage::Input(symbol) => {
                    if let Some(anchor) = self.boundaries.get(&symbol) {
                        result.push(EnumeratedExpr {
                            expression: TopologyExpr::Anchor(*anchor),
                            gates: 0,
                            depth: 0,
                            root_enode: symbol.to_string(),
                        });
                    }
                }
                StdCellLanguage::Gate(op, children) => {
                    if self.depth_bound && stack.len() > self.max_gates {
                        self.stats.gate_bound_pruned += 1;
                        continue;
                    }
                    let root_enode = format!("{}:{:?}", op, children);
                    let mut combinations = vec![(Vec::new(), 0usize, 0usize)];
                    for child in children.iter() {
                        let alternatives = self.enumerate(*child, stack);
                        let mut next = Vec::new();
                        for (prefix, gates, depth) in &combinations {
                            for alternative in &alternatives {
                                let total_gates = gates + alternative.gates;
                                if total_gates + 1 > self.max_gates {
                                    self.stats.gate_bound_pruned += 1;
                                    continue;
                                }
                                let mut expressions = prefix.clone();
                                expressions.push(alternative.expression.clone());
                                next.push((
                                    expressions,
                                    total_gates,
                                    (*depth).max(alternative.depth),
                                ));
                                if next.len() >= MAX_REALIZATIONS_PER_ROOT {
                                    break;
                                }
                            }
                            if next.len() >= MAX_REALIZATIONS_PER_ROOT {
                                break;
                            }
                        }
                        combinations = next;
                    }
                    for (expressions, child_gates, child_depth) in combinations {
                        result.push(EnumeratedExpr {
                            expression: TopologyExpr::cell(op.to_string(), expressions),
                            gates: child_gates + 1,
                            depth: child_depth + 1,
                            root_enode: root_enode.clone(),
                        });
                        if result.len() >= MAX_REALIZATIONS_PER_ROOT {
                            break;
                        }
                    }
                }
                StdCellLanguage::Bool(value) => {
                    result.push(EnumeratedExpr {
                        expression: TopologyExpr::cell(value.to_string(), Vec::new()),
                        gates: 0,
                        depth: 0,
                        root_enode: value.to_string(),
                    });
                }
                StdCellLanguage::Output(_, _) => {}
            }
            if result.len() >= MAX_REALIZATIONS_PER_ROOT {
                break;
            }
        }
        stack.remove(&canonical);
        if self.memo_enabled {
            self.memo.insert(memo_key, result.clone());
        }
        result
    }
}

fn enumerate_root_realizations(
    source: &Netlist<StdCellType, ()>,
    rules: &[egg::Rewrite<StdCellLanguage, ()>],
    root_anchor: NodeIndex,
    provenance: &[String],
    first_seen_family: RuleFamily,
    local_cone_depth: usize,
    max_realization_gates: usize,
) -> (Vec<StructuralRealization>, DedupStats) {
    let mut egraph = EGraph::<StdCellLanguage, ()>::default();
    let mut anchor_ids = FxHashMap::default();
    let mut boundaries = FxHashMap::default();
    let root_id = add_local_cone(
        &mut egraph,
        source,
        root_anchor,
        local_cone_depth,
        &mut anchor_ids,
        &mut boundaries,
    );
    egraph.rebuild();
    let mut runner = Runner::default()
        .with_egraph(egraph)
        .with_iter_limit(
            std::env::var("EGG_STRUCTURAL_REWRITE_ITER_LIMIT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(12),
        )
        .with_node_limit(
            std::env::var("EGG_STRUCTURAL_REWRITE_NODE_LIMIT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(20_000),
        )
        .with_time_limit(Duration::from_secs(2))
        .run(rules);
    let mut stats = DedupStats {
        local_eclasses: runner.egraph.number_of_classes(),
        local_enodes: runner.egraph.total_size(),
        raw_realizations: 0,
        unique_realizations: 0,
        duplicate_realizations: 0,
        cycle_pruned: 0,
        gate_bound_pruned: 0,
        excluded_phase1_drive_rewrites: 0,
        added_direct_structural_seed_rewrites: 0,
    };
    let root = runner.egraph.find(root_id);
    let mut collapse = FxHashMap::default();
    for (anchor, id) in anchor_ids {
        if anchor != root_anchor {
            collapse.entry(runner.egraph.find(id)).or_insert(anchor);
        }
    }
    let current = TopologyExpr::cell(
        source.graph[root_anchor].to_string(),
        source
            .inputs(root_anchor)
            .map(TopologyExpr::Anchor)
            .collect(),
    );
    let current_hash = expr_hash(&current);
    let mut enumerator = ConeEnumerator {
        egraph: &mut runner.egraph,
        root,
        collapse,
        boundaries: &boundaries,
        max_gates: max_realization_gates,
        stats: DedupStats {
            local_eclasses: 0,
            local_enodes: 0,
            raw_realizations: 0,
            unique_realizations: 0,
            duplicate_realizations: 0,
            cycle_pruned: 0,
            gate_bound_pruned: 0,
            excluded_phase1_drive_rewrites: 0,
            added_direct_structural_seed_rewrites: 0,
        },
        memo: FxHashMap::default(),
        memo_enabled: std::env::var("EGG_CONE_ENUM_MEMO")
            .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
            .unwrap_or(true),
        depth_bound: std::env::var("EGG_CONE_ENUM_DEPTH_BOUND")
            .map(|value| value == "1")
            .unwrap_or(false),
    };
    let expressions = enumerator.enumerate(root, &mut FxHashSet::default());
    stats.cycle_pruned = enumerator.stats.cycle_pruned;
    stats.gate_bound_pruned = enumerator.stats.gate_bound_pruned;
    let mut raw = Vec::new();
    for mut expression in expressions {
        inherit_preserved_root_size(
            &mut expression.expression,
            &source.graph[root_anchor].to_string(),
        );
        let hash = expr_hash(&expression.expression);
        if hash == current_hash {
            continue;
        }
        let mut boundary_set = FxHashSet::default();
        collect_boundaries(&expression.expression, &mut boundary_set);
        let mut boundary_anchors: Vec<_> = boundary_set.into_iter().collect();
        boundary_anchors.sort_unstable();
        raw.push(StructuralRealization {
            root_anchor: root_anchor.index(),
            root_eclass: root.to_string(),
            root_enode: expression.root_enode,
            expression_text: expression_text(&expression.expression),
            expression: expression.expression,
            boundary_anchors,
            created_logical_nodes: expression.gates,
            logic_depth: expression.depth,
            structural_hash: format!("root={}|{}", root_anchor.index(), hash),
            provenance: provenance.to_vec(),
            first_seen_family,
        });
    }
    (raw, stats)
}

/// Enumerate complete, bounded selected-enode cones for every physical anchor.
/// Rule provenance is carried only as metadata; expression generation and
/// structural hashing are common to every rewrite family.
fn enumerate_structural_realizations_with_policy(
    source: &Netlist<StdCellType, ()>,
    rule_paths: &[&Path],
    first_seen_family: RuleFamily,
    local_cone_depth: usize,
    max_realization_gates: usize,
    excluded_drive_rule_path: Option<&Path>,
) -> Result<(Vec<StructuralRealization>, DedupStats), String> {
    let mut rules = Vec::new();
    let excluded_drive_rules = excluded_drive_rule_path
        .map(JsonRules::from_path)
        .transpose()
        .map_err(|error| error.to_string())?;
    let mut excluded_phase1_drive_rewrites = 0;
    let mut added_direct_structural_seed_rewrites = 0;
    for path in rule_paths {
        let json_rules = JsonRules::from_path(path).map_err(|error| error.to_string())?;
        if let Some(scale_rules) = &excluded_drive_rules {
            let (projected, audit) = json_rules
                .into_phase1_structural_seed_rules::<StdCellLanguage>(scale_rules)
                .map_err(|error| error.to_string())?;
            excluded_phase1_drive_rewrites += audit.excluded_drive_rewrites;
            added_direct_structural_seed_rewrites += audit.added_direct_seed_rewrites;
            rules.extend(projected);
        } else {
            rules.extend(
                json_rules
                    .into_egg_rules::<StdCellLanguage>()
                    .map_err(|error| error.to_string())?,
            );
        }
    }
    let provenance: Vec<_> = rule_paths
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    let mut total_stats = DedupStats {
        local_eclasses: 0,
        local_enodes: 0,
        raw_realizations: 0,
        unique_realizations: 0,
        duplicate_realizations: 0,
        cycle_pruned: 0,
        gate_bound_pruned: 0,
        excluded_phase1_drive_rewrites,
        added_direct_structural_seed_rewrites,
    };
    let roots: Vec<_> = source
        .graph
        .node_indices()
        .filter(|anchor| !source.leaves.contains(anchor) && !source.roots.contains(anchor))
        .collect();
    let requested_jobs = std::env::var("EGG_REALIZATION_JOBS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    let jobs = requested_jobs.min(roots.len().max(1));
    let mut per_root: Vec<Option<(Vec<StructuralRealization>, DedupStats)>> =
        (0..roots.len()).map(|_| None).collect();
    if jobs == 1 {
        for (index, root_anchor) in roots.iter().copied().enumerate() {
            per_root[index] = Some(enumerate_root_realizations(
                source,
                &rules,
                root_anchor,
                &provenance,
                first_seen_family,
                local_cone_depth,
                max_realization_gates,
            ));
        }
    } else {
        let next = AtomicUsize::new(0);
        let (sender, receiver) = mpsc::channel();
        std::thread::scope(|scope| {
            for _ in 0..jobs {
                let sender = sender.clone();
                let next = &next;
                let roots = &roots;
                let rules = &rules;
                let provenance = &provenance;
                scope.spawn(move || {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(root_anchor) = roots.get(index).copied() else {
                            break;
                        };
                        let result = enumerate_root_realizations(
                            source,
                            rules,
                            root_anchor,
                            provenance,
                            first_seen_family,
                            local_cone_depth,
                            max_realization_gates,
                        );
                        if sender.send((index, result)).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        drop(sender);
        for (index, result) in receiver {
            per_root[index] = Some(result);
        }
    }
    let mut raw = Vec::new();
    for result in per_root {
        let (mut realizations, stats) =
            result.expect("missing structural realization worker result");
        total_stats.local_eclasses += stats.local_eclasses;
        total_stats.local_enodes += stats.local_enodes;
        total_stats.cycle_pruned += stats.cycle_pruned;
        total_stats.gate_bound_pruned += stats.gate_bound_pruned;
        raw.append(&mut realizations);
    }
    total_stats.raw_realizations = raw.len();
    let mut by_hash: FxHashMap<String, StructuralRealization> = FxHashMap::default();
    for realization in raw {
        match by_hash.get_mut(&realization.structural_hash) {
            Some(existing) => {
                for item in realization.provenance {
                    if !existing.provenance.contains(&item) {
                        existing.provenance.push(item);
                    }
                }
            }
            None => {
                by_hash.insert(realization.structural_hash.clone(), realization);
            }
        }
    }
    let mut unique: Vec<_> = by_hash.into_values().collect();
    unique.sort_by_key(|item| (item.root_anchor, item.structural_hash.clone()));
    total_stats.unique_realizations = unique.len();
    total_stats.duplicate_realizations =
        total_stats.raw_realizations - total_stats.unique_realizations;
    Ok((unique, total_stats))
}

pub fn enumerate_structural_realizations(
    source: &Netlist<StdCellType, ()>,
    rule_paths: &[&Path],
    first_seen_family: RuleFamily,
    local_cone_depth: usize,
    max_realization_gates: usize,
) -> Result<(Vec<StructuralRealization>, DedupStats), String> {
    enumerate_structural_realizations_with_policy(
        source,
        rule_paths,
        first_seen_family,
        local_cone_depth,
        max_realization_gates,
        None,
    )
}

pub fn enumerate_structural_realizations_without_drive_expansion(
    source: &Netlist<StdCellType, ()>,
    rule_paths: &[&Path],
    scale_rule_path: &Path,
    first_seen_family: RuleFamily,
    local_cone_depth: usize,
    max_realization_gates: usize,
) -> Result<(Vec<StructuralRealization>, DedupStats), String> {
    enumerate_structural_realizations_with_policy(
        source,
        rule_paths,
        first_seen_family,
        local_cone_depth,
        max_realization_gates,
        Some(scale_rule_path),
    )
}

/// Rule-agnostic adapter for extraction code.  The legacy family field is
/// populated with `Mixed` only for backward-compatible serialization; callers
/// cannot provide, inspect, or dispatch on a rule family through this API.
pub fn enumerate_structural_realizations_unattributed(
    source: &Netlist<StdCellType, ()>,
    rule_paths: &[&Path],
    local_cone_depth: usize,
    max_realization_gates: usize,
) -> Result<(Vec<StructuralRealization>, DedupStats), String> {
    enumerate_structural_realizations(
        source,
        rule_paths,
        RuleFamily::Mixed,
        local_cone_depth,
        max_realization_gates,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::liberty::{get_direction_of_pins, read_liberty};
    use crate::io::stdcell::{
        read_verilog_with_lib_to_netlist, write_verilog_from_netlist_with_lib,
    };
    use crate::language::LanguageType;
    use crate::netlist_to_egg_roots_with_provenance;
    use crate::physical_scale_egraph::build_occurrence_preserving_original_space;
    use crate::physical_topology_egraph::{TopologySelection, physicalize_topology};
    use extraction_gym::ExtendedEGraph;
    use serde_json::Value;

    #[test]
    fn depth_bound_retains_short_realizations_and_prunes_impossible_long_paths() {
        let mut graph = EGraph::<StdCellLanguage, ()>::default();
        let boundary = Symbol::from("__egg_boundary_0");
        let leaf = graph.add(StdCellLanguage::Input(boundary));
        let inv = Symbol::from("INVx1_ASAP7_75t_R");
        let root = graph.add(StdCellLanguage::Gate(inv, vec![leaf].into()));
        let mut long = leaf;
        for _ in 0..5 {
            long = graph.add(StdCellLanguage::Gate(inv, vec![long].into()));
        }
        graph.union(root, long);
        graph.rebuild();
        let root = graph.find(root);
        let boundaries = FxHashMap::from_iter([(boundary, NodeIndex::new(0))]);
        let run = |depth_bound: bool| {
            let mut graph = graph.clone();
            let mut enumerator = ConeEnumerator {
                egraph: &mut graph,
                root,
                collapse: FxHashMap::default(),
                boundaries: &boundaries,
                max_gates: 3,
                stats: DedupStats {
                    local_eclasses: 0,
                    local_enodes: 0,
                    raw_realizations: 0,
                    unique_realizations: 0,
                    duplicate_realizations: 0,
                    cycle_pruned: 0,
                    gate_bound_pruned: 0,
                    excluded_phase1_drive_rewrites: 0,
                    added_direct_structural_seed_rewrites: 0,
                },
                memo: FxHashMap::default(),
                memo_enabled: true,
                depth_bound,
            };
            enumerator
                .enumerate(root, &mut FxHashSet::default())
                .into_iter()
                .map(|value| (expression_text(&value.expression), value.gates))
                .collect::<Vec<_>>()
        };
        let legacy = run(false);
        let bounded = run(true);
        assert!(!bounded.is_empty());
        assert!(bounded.iter().all(|(_, gates)| *gates <= 3));
        assert_eq!(bounded, legacy);
    }

    fn add_node(netlist: &mut Netlist<StdCellType, ()>, op: &str) -> NodeIndex {
        netlist.graph.add_node(StdCellType::from_op(op))
    }

    #[test]
    fn no_drive_expansion_preserves_structure_from_noncanonical_incumbent() {
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let pins = get_direction_of_pins(&liberty).unwrap();
        let (mut source, _) =
            read_verilog_with_lib_to_netlist("test/topology_inv_and.v", pins).unwrap();
        let anchor = source
            .graph
            .node_indices()
            .find(|node| source.graph[*node].to_string() == "AND2x2_ASAP7_6t_L")
            .unwrap();
        // NAND2x1 is the structural rule's canonical seed; x2 exercises the
        // direct family-member match without inserting a scale equality.
        source.graph[anchor] = StdCellType::from_op("NAND2x2_ASAP7_6t_L");
        let (realizations, audit) = enumerate_structural_realizations_without_drive_expansion(
            &source,
            &[Path::new("../optimizer/test/runtime_full_comb_rules.json")],
            Path::new("../optimizer/test/6t_full_comb_scale_rules.json"),
            RuleFamily::Mixed,
            2,
            3,
        )
        .unwrap();
        assert_eq!(audit.excluded_phase1_drive_rewrites, 102);
        assert!(audit.added_direct_structural_seed_rewrites > 0);
        assert!(
            realizations
                .iter()
                .any(|item| item.root_anchor == anchor.index())
        );
        assert!(
            realizations
                .iter()
                .all(|item| !item.is_scale_only_alias(&source))
        );
    }

    fn connect(netlist: &mut Netlist<StdCellType, ()>, parent: NodeIndex, children: &[NodeIndex]) {
        for child in children {
            netlist.graph.add_edge(parent, *child, ());
        }
    }

    fn with_output(
        mut netlist: Netlist<StdCellType, ()>,
        driver: NodeIndex,
    ) -> Netlist<StdCellType, ()> {
        let output = add_node(&mut netlist, "y");
        connect(&mut netlist, output, &[driver]);
        netlist.roots.push(output);
        netlist
    }

    fn ppa(netlist: &Netlist<StdCellType, ()>) -> [f64; 3] {
        let provenance = netlist_to_egg_roots_with_provenance::<_, ()>(netlist).unwrap();
        let space = build_occurrence_preserving_original_space(netlist, &provenance).unwrap();
        let json = serde_json::to_value(&space.egraph).unwrap();
        let ext = ExtendedEGraph::from_base_to_extention(
            space.egraph,
            &Value::String("test/asap7sc6t_SELECT_LVT_TT_nldm.lib".into()),
            &json,
        );
        let cost = space.original_extraction.dag_cost_nldm_v2_on_pruned(&ext);
        [
            cost.components[0].into_inner(),
            cost.components[1].into_inner(),
            cost.components[2].into_inner(),
        ]
    }

    fn materialize(
        source: &Netlist<StdCellType, ()>,
        realization: &StructuralRealization,
    ) -> crate::physical_topology_egraph::PhysicalizedTopology {
        let mut selection = TopologySelection::default();
        selection.choose(
            NodeIndex::new(realization.root_anchor),
            realization.expression.clone(),
        );
        physicalize_topology(source, &selection).unwrap()
    }

    fn assert_roundtrip_v2(netlist: Netlist<StdCellType, ()>, module: &str) {
        let before = ppa(&netlist);
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let pins = get_direction_of_pins(&liberty).unwrap();
        let output = std::env::temp_dir().join(format!(
            "egg_topology_v1_{}_{}.v",
            module,
            std::process::id()
        ));
        write_verilog_from_netlist_with_lib(&output, netlist, module, pins.clone()).unwrap();
        let (reparsed, reparsed_module) = read_verilog_with_lib_to_netlist(&output, pins).unwrap();
        assert_eq!(reparsed_module, module);
        let after = ppa(&reparsed);
        for index in 0..3 {
            assert!((before[index] - after[index]).abs() < 1e-12);
        }
        std::fs::remove_file(output).unwrap();
    }

    fn find_expr<'a>(
        realizations: &'a [StructuralRealization],
        root: NodeIndex,
        needle: &str,
    ) -> &'a StructuralRealization {
        realizations
            .iter()
            .find(|item| item.root_anchor == root.index() && item.expression_text == needle)
            .unwrap_or_else(|| {
                panic!(
                    "missing {needle}; available: {:?}",
                    realizations
                        .iter()
                        .filter(|item| item.root_anchor == root.index())
                        .map(|item| &item.expression_text)
                        .collect::<Vec<_>>()
                )
            })
    }

    #[test]
    fn topology_v1_dmg_unified_cone_materializes_and_roundtrips() {
        // OR(!a, !b) == NAND(a, b), using the repository's actual DMG rule.
        let mut source = Netlist::default();
        let a = add_node(&mut source, "a");
        let b = add_node(&mut source, "b");
        source.leaves.extend([a, b]);
        let ia = add_node(&mut source, "INVx1_ASAP7_6t_L");
        let ib = add_node(&mut source, "INVx1_ASAP7_6t_L");
        connect(&mut source, ia, &[a]);
        connect(&mut source, ib, &[b]);
        let root = add_node(&mut source, "OR2x2_ASAP7_6t_L");
        connect(&mut source, root, &[ia, ib]);
        let source = with_output(source, root);

        let (realizations, _) = enumerate_structural_realizations(
            &source,
            &[Path::new("test/6t_dmg_rules.json")],
            RuleFamily::Dmg,
            2,
            3,
        )
        .unwrap();
        let expected = format!("NAND2x1_ASAP7_6t_L(@{},@{})", a.index(), b.index());
        let realization = find_expr(&realizations, root, &expected);
        assert_eq!(realization.first_seen_family, RuleFamily::Dmg);
        assert_eq!(realization.created_logical_nodes, 1);
        assert_eq!(realization.logic_depth, 1);
        let physical = materialize(&source, realization);
        assert!(physical.deleted_originals.contains(&ia));
        assert!(physical.deleted_originals.contains(&ib));
        assert_eq!(physical.created.len(), 0);
        assert!(ppa(&physical.netlist).iter().all(|value| value.is_finite()));
        assert_roundtrip_v2(physical.netlist, "topology_v1_dmg");
    }

    #[test]
    fn topology_v1_dmg_shared_fanout_materializes_new_driver_once() {
        let mut source = Netlist::default();
        let a = add_node(&mut source, "a");
        let b = add_node(&mut source, "b");
        source.leaves.extend([a, b]);
        let ia = add_node(&mut source, "INVx1_ASAP7_6t_L");
        let ib = add_node(&mut source, "INVx1_ASAP7_6t_L");
        connect(&mut source, ia, &[a]);
        connect(&mut source, ib, &[b]);
        let root = add_node(&mut source, "OR2x2_ASAP7_6t_L");
        connect(&mut source, root, &[ia, ib]);
        for name in ["y0", "y1"] {
            let consumer = add_node(&mut source, "INVx1_ASAP7_6t_L");
            connect(&mut source, consumer, &[root]);
            let output = add_node(&mut source, name);
            connect(&mut source, output, &[consumer]);
            source.roots.push(output);
        }
        let (realizations, _) = enumerate_structural_realizations(
            &source,
            &[Path::new("test/6t_dmg_rules.json")],
            RuleFamily::Dmg,
            2,
            3,
        )
        .unwrap();
        let expected = format!("NAND2x1_ASAP7_6t_L(@{},@{})", a.index(), b.index());
        let physical = materialize(&source, find_expr(&realizations, root, &expected));
        let driver = physical.original_to_materialized[&root];
        assert_eq!(physical.netlist.outputs(driver).count(), 2);
        assert_eq!(
            physical
                .netlist
                .graph
                .node_weights()
                .filter(|cell| cell.to_string() == "NAND2x1_ASAP7_6t_L")
                .count(),
            1
        );
    }

    #[test]
    fn topology_v1_comm_preserves_ordered_pin_mapping_without_duplication() {
        let mut source = Netlist::default();
        let a = add_node(&mut source, "a");
        let b = add_node(&mut source, "b");
        source.leaves.extend([a, b]);
        let root = add_node(&mut source, "AND2x4_ASAP7_6t_L");
        connect(&mut source, root, &[a, b]);
        let source = with_output(source, root);
        let (realizations, stats) = enumerate_structural_realizations(
            &source,
            &[Path::new("test/6t_comm_rules.json")],
            RuleFamily::Comm,
            1,
            1,
        )
        .unwrap();
        let swapped = format!("AND2x4_ASAP7_6t_L(@{},@{})", b.index(), a.index());
        let realization = find_expr(&realizations, root, &swapped);
        assert!(stats.unique_realizations <= stats.raw_realizations);
        let physical = materialize(&source, realization);
        let materialized_root = physical.original_to_materialized[&root];
        let inputs: Vec<_> = physical.netlist.inputs(materialized_root).collect();
        assert_eq!(inputs[0], physical.original_to_materialized[&b]);
        assert_eq!(inputs[1], physical.original_to_materialized[&a]);
        assert_eq!(physical.created.len(), 0);
        assert_eq!(
            physical
                .netlist
                .graph
                .node_weights()
                .filter(|cell| cell.to_string() == "AND2x4_ASAP7_6t_L")
                .count(),
            1
        );
        assert!(ppa(&physical.netlist).iter().all(|value| value.is_finite()));
    }

    #[test]
    fn topology_v1_expand_is_bounded_materialized_and_roundtrips() {
        let mut source = Netlist::default();
        let a = add_node(&mut source, "a");
        let b = add_node(&mut source, "b");
        let c = add_node(&mut source, "c");
        source.leaves.extend([a, b, c]);
        let root = add_node(&mut source, "AND3x1_ASAP7_6t_L");
        connect(&mut source, root, &[a, b, c]);
        let source = with_output(source, root);
        let (realizations, stats) = enumerate_structural_realizations(
            &source,
            &[Path::new("test/6t_expand_rules.json")],
            RuleFamily::Expand,
            1,
            3,
        )
        .unwrap();
        let expected = format!(
            "AND2x2_ASAP7_6t_L(AND2x2_ASAP7_6t_L(@{},@{}),@{})",
            a.index(),
            b.index(),
            c.index()
        );
        let realization = find_expr(&realizations, root, &expected);
        assert_eq!(realization.created_logical_nodes, 2);
        assert_eq!(realization.logic_depth, 2);
        assert!(
            realizations
                .iter()
                .all(|item| item.created_logical_nodes <= 3)
        );
        assert!(stats.gate_bound_pruned > 0 || realizations.len() <= MAX_REALIZATIONS_PER_ROOT);
        let physical = materialize(&source, realization);
        assert_eq!(physical.created.len(), 1);
        assert_eq!(
            physical.netlist.graph.node_count(),
            source.graph.node_count() + 1
        );
        assert!(ppa(&physical.netlist).iter().all(|value| value.is_finite()));
        assert_roundtrip_v2(physical.netlist, "topology_v1_expand");
    }
}
