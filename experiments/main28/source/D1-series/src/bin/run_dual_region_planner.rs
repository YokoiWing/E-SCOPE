//! Gate-C planner for dual-seeded, boundary-residual adaptive regions.
//!
//! This binary only proposes legal, occurrence-anchored eclass/enode choices.
//! Large regions retain the structural dependency cone of a rewrite as a
//! bounded macro and rank the complete macro with a P2 boundary-dual score.
//! Rule names are not used as a score; the existing Python P1 rerank and
//! A2/O1/Rust-V2 exact guard remain the acceptance authority.

use anyhow::{Context, Result, bail};
use d1_series::phase_i::{
    ImplementationDomain, RegionalScoring, RewrittenEquivalence, ScoredAssignment, extract_phase_i,
    macro_components,
};
use d1_series::shared_load_v2::TimingBoundary;
use d1_series::topology_signature::topology_signature;
use extraction_gym::extract::{
    CircuitAdjointConfig, CircuitObjectiveWeights, SignalEdge, TimingAdjointControl,
    TransitionBackward, circuit_adjoint, circuit_adjoint_weighted,
};
use extraction_gym::{ExtendedEGraph, NLDM};
use mac_egg::adaptive_region::{
    AdaptiveRegionSpace, RegionAssignment, build_rule_agnostic_space,
    build_rule_agnostic_space_without_drive_expansion,
};
use mac_egg::io::liberty::{Library, get_direction_of_pins, read_liberty};
use mac_egg::io::stdcell::{
    read_verilog_with_lib_to_netlist, write_verilog_from_netlist_with_lib_ref,
};
use mac_egg::language::{LanguageType, StdCellType};
use mac_egg::netlist::Netlist;
use mac_egg::physical_scale_egraph::build_occurrence_preserving_original_space_direct;
use mac_egg::physical_topology_egraph::{TopologySelection, physicalize_topology};
use mac_egg::region_local_evaluator::{
    BoundaryDualComponent, LocalRegionResponse, boundary_dual_components_region,
    boundary_dual_score_region, evaluate_region_enode, required_cell_ops,
};
use petgraph::algo::has_path_connecting;
use petgraph::graph::NodeIndex;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicUsize, Ordering as AtomicOrdering},
    mpsc,
};
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};

const LIB: &str = "test/asap7sc6t_SELECT_LVT_TT_nldm.lib";
const RULES: [&str; 5] = [
    "test/6t_scale_full_rules.json",
    "test/6t_inv_rules.json",
    "test/6t_dmg_rules.json",
    "test/6t_comm_rules.json",
    "test/6t_expand_rules.json",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Variant {
    V1,
    V2,
    V3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RegionPolicy {
    /// Frozen D1 behavior: sensitivity-only seeds and residual-first growth.
    Residual,
    /// Preserve two strongest physical seeds, then diversify with structural
    /// richness and local graph cohesion.  Growth uses equal-rank consensus.
    Hybrid,
    /// The same physically guarded seeds, with extra preference for a compact
    /// block (many internal and few external connections) during growth.
    Block,
}

impl RegionPolicy {
    fn from_env() -> Result<Self> {
        match std::env::var("EGG_REGION_POLICY")
            .unwrap_or_else(|_| "residual".to_owned())
            .as_str()
        {
            "residual" => Ok(Self::Residual),
            "hybrid" => Ok(Self::Hybrid),
            "block" => Ok(Self::Block),
            value => bail!("EGG_REGION_POLICY must be residual, hybrid, or block; got {value}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
struct AnchorFeatures {
    legal_alternatives: usize,
    topology_richness: usize,
    active_degree: usize,
    local_internal_edges: usize,
    local_external_edges: usize,
    local_cohesion: f64,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct FrontierFeatures {
    anchor: usize,
    opportunity: f64,
    topology_richness: usize,
    internal_links: usize,
    external_links: usize,
    cohesion: f64,
    directional_residual: f64,
    consensus_score: f64,
}

/// Evaluate independent physical candidates concurrently while returning
/// results in the exact input order.  Candidate generation, floating-point
/// evaluation and the final deterministic sort are unchanged; only the
/// scheduling of independent work differs.
fn parallel_map_ordered<T, R, F>(items: &[T], requested_jobs: usize, work: F) -> Result<Vec<R>>
where
    T: Sync,
    R: Send,
    F: Fn(usize, &T) -> Result<R> + Sync,
{
    if items.len() <= 1 || requested_jobs <= 1 {
        return items
            .iter()
            .enumerate()
            .map(|(index, item)| work(index, item))
            .collect();
    }
    let jobs = requested_jobs.min(items.len());
    let next = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            let sender = sender.clone();
            let next = &next;
            let work = &work;
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, AtomicOrdering::Relaxed);
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    if sender.send((index, work(index, item))).is_err() {
                        break;
                    }
                }
            });
        }
    });
    drop(sender);
    let mut ordered: Vec<Option<Result<R>>> = (0..items.len()).map(|_| None).collect();
    for (index, value) in receiver {
        ordered[index] = Some(value);
    }
    ordered
        .into_iter()
        .enumerate()
        .map(|(index, value)| value.with_context(|| format!("missing parallel result {index}"))?)
        .collect()
}

fn env_jobs(name: &str, default: usize) -> Result<usize> {
    Ok(std::env::var(name)
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()
        .with_context(|| format!("invalid {name}"))?
        .unwrap_or(default)
        .max(1))
}

#[derive(Debug, Serialize)]
struct PlanEntry {
    candidate_id: String,
    region_id: String,
    region_size: usize,
    p2_rank: usize,
    p2_score: f64,
    topology_signature: String,
    choices: Vec<mac_egg::adaptive_region::RegionChoice>,
    /// Audit-only boundary displacement used to form the existing P2 score.
    components: Vec<BoundaryDualComponent>,
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    fs::write(path, serde_json::to_string_pretty(value)? + "\n")?;
    Ok(())
}

fn status(out: &Path, stage: &str, completed: usize, total: usize, current: &str) -> Result<()> {
    write_json(
        out.join("status.json").as_path(),
        &serde_json::json!({
            "stage": stage, "completed": completed, "total": total, "current": current,
            "pid": std::process::id(),
            "updated_unix_sec": SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        }),
    )
}

fn extended(
    source: &Netlist<StdCellType, ()>,
    liberty_path: &Path,
    shared_cell_nldm: Option<&HashMap<String, NLDM>>,
) -> Result<(ExtendedEGraph, extraction_gym::extract::ExtractionResult)> {
    let space = build_occurrence_preserving_original_space_direct(source);
    let json = serde_json::to_value(&space.egraph)?;
    let ext = if let Some(shared) = shared_cell_nldm {
        ExtendedEGraph::from_base_to_extention_with_preparsed_nldm(space.egraph, &json, shared)
    } else {
        ExtendedEGraph::from_base_to_extention(
            space.egraph,
            &Value::String(liberty_path.display().to_string()),
            &json,
        )
    };
    Ok((ext, space.original_extraction))
}

fn active_adjacency(
    source: &Netlist<StdCellType, ()>,
    active: &FxHashSet<usize>,
) -> FxHashMap<usize, FxHashSet<usize>> {
    let mut result = FxHashMap::default();
    for &anchor in active {
        let node = NodeIndex::new(anchor);
        for neighbor in source.inputs(node).chain(source.outputs(node)) {
            if active.contains(&neighbor.index()) {
                result
                    .entry(anchor)
                    .or_insert_with(FxHashSet::default)
                    .insert(neighbor.index());
                result
                    .entry(neighbor.index())
                    .or_insert_with(FxHashSet::default)
                    .insert(anchor);
            }
        }
    }
    result
}

fn normalize_drive_tokens(expression: &str) -> String {
    expression
        .split_inclusive(|character: char| {
            character == '(' || character == ')' || character.is_whitespace()
        })
        .map(|part| {
            let token_start = part
                .char_indices()
                .find(|(_, character)| !matches!(character, '(' | ')' | ' ' | '\t' | '\n' | '\r'))
                .map(|(index, _)| index);
            let Some(start) = token_start else {
                return part.to_owned();
            };
            let end = part[start..]
                .char_indices()
                .find(|(_, character)| matches!(character, '(' | ')' | ' ' | '\t' | '\n' | '\r'))
                .map_or(part.len(), |(index, _)| start + index);
            let token = &part[start..end];
            let Some(lib_marker) = token.find("_ASAP") else {
                return part.to_owned();
            };
            let cell_prefix = &token[..lib_marker];
            let Some(marker) = cell_prefix.rfind('x') else {
                return part.to_owned();
            };
            let drive = &cell_prefix[marker + 1..];
            let numeric_drive = !drive.is_empty()
                && (drive.chars().all(|character| character.is_ascii_digit())
                    || drive.strip_prefix('p').is_some_and(|tail| {
                        !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit())
                    }));
            if !numeric_drive {
                return part.to_owned();
            }
            format!(
                "{}{}x*{}{}",
                &part[..start],
                &cell_prefix[..marker],
                &token[lib_marker..],
                &part[end..]
            )
        })
        .collect()
}

fn anchor_features(
    space: &AdaptiveRegionSpace,
    anchor: usize,
    adjacency: &FxHashMap<usize, FxHashSet<usize>>,
) -> AnchorFeatures {
    let class = space.class_by_anchor(anchor);
    let legal_alternatives = class.map_or(0, |class| class.enodes.len().saturating_sub(1));
    let topology_richness = class.map_or(0, |class| {
        class
            .enodes
            .iter()
            .filter(|enode| !enode.incumbent)
            .map(|enode| normalize_drive_tokens(&enode.expression_text))
            .collect::<FxHashSet<_>>()
            .len()
    });
    let mut neighborhood: FxHashSet<_> = [anchor].into_iter().collect();
    neighborhood.extend(adjacency.get(&anchor).into_iter().flatten().copied());
    let internal_twice: usize = neighborhood
        .iter()
        .map(|node| {
            adjacency
                .get(node)
                .map_or(0, |neighbors| neighbors.intersection(&neighborhood).count())
        })
        .sum();
    let local_internal_edges = internal_twice / 2;
    let local_external_edges: usize = neighborhood
        .iter()
        .map(|node| {
            adjacency
                .get(node)
                .map_or(0, |neighbors| neighbors.difference(&neighborhood).count())
        })
        .sum();
    let local_cohesion = (local_internal_edges as f64 + 1.0)
        / (local_internal_edges as f64 + local_external_edges as f64 + 1.0);
    AnchorFeatures {
        legal_alternatives,
        topology_richness,
        active_degree: adjacency.get(&anchor).map_or(0, FxHashSet::len),
        local_internal_edges,
        local_external_edges,
        local_cohesion,
    }
}

fn rank_points<T, F>(items: &[T], mut value: F) -> Vec<f64>
where
    F: FnMut(&T) -> f64,
{
    let mut order: Vec<_> = (0..items.len()).collect();
    order.sort_by(|first, second| {
        value(&items[*second])
            .partial_cmp(&value(&items[*first]))
            .unwrap_or(Ordering::Equal)
            .then_with(|| first.cmp(second))
    });
    let denominator = items.len().max(1) as f64;
    let mut points = vec![0.0; items.len()];
    for (rank, index) in order.into_iter().enumerate() {
        points[index] = (items.len() - rank) as f64 / denominator;
    }
    points
}

fn choose_seeds(
    policy: RegionPolicy,
    top_seeds: usize,
    seed_trace: &[Value],
    features: &FxHashMap<usize, AnchorFeatures>,
) -> Vec<usize> {
    let physical: Vec<_> = seed_trace
        .iter()
        .filter_map(|row| row["anchor"].as_u64().map(|anchor| anchor as usize))
        .collect();
    if policy == RegionPolicy::Residual {
        return physical.into_iter().take(top_seeds).collect();
    }
    // Keep the candidate universe physically relevant.  Richness cannot pull
    // a cold, non-sensitive block into the fixed four-seed budget.
    let pool_len = physical.len().min((top_seeds * 6).max(16));
    let pool = &physical[..pool_len];
    let opportunity_points = rank_points(pool, |anchor| {
        seed_trace
            .iter()
            .find(|row| row["anchor"].as_u64() == Some(*anchor as u64))
            .and_then(|row| row["opportunity"].as_f64())
            .unwrap_or(0.0)
    });
    let richness_points = rank_points(pool, |anchor| {
        features
            .get(anchor)
            .map_or(0.0, |item| item.topology_richness as f64)
    });
    let cohesion_points = rank_points(pool, |anchor| {
        features.get(anchor).map_or(0.0, |item| item.local_cohesion)
    });
    let mut selected: Vec<_> = physical.iter().copied().take(top_seeds.min(2)).collect();
    let mut consensus: Vec<_> = pool
        .iter()
        .enumerate()
        .map(|(index, anchor)| {
            let cohesion_weight = if policy == RegionPolicy::Block {
                2.0
            } else {
                1.0
            };
            (
                *anchor,
                opportunity_points[index]
                    + richness_points[index]
                    + cohesion_weight * cohesion_points[index],
            )
        })
        .collect();
    consensus.sort_by(|first, second| {
        second
            .1
            .partial_cmp(&first.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| first.0.cmp(&second.0))
    });
    for (anchor, _) in consensus {
        if !selected.contains(&anchor) {
            selected.push(anchor);
        }
        if selected.len() == top_seeds {
            break;
        }
    }
    selected
}

fn output_price(adjoint: &extraction_gym::extract::CircuitAdjointV1, anchor: usize) -> f64 {
    let class = egraph_serialize::ClassId::from(format!("physical.{anchor}"));
    [SignalEdge::Rise, SignalEdge::Fall]
        .into_iter()
        .map(|edge| {
            adjoint
                .arrival_price
                .get(&(class.clone(), edge))
                .copied()
                .unwrap_or(0.0)
                .abs()
                + adjoint
                    .transition_price
                    .get(&(class.clone(), edge))
                    .copied()
                    .unwrap_or(0.0)
                    .abs()
        })
        .sum()
}

fn score_region(
    source: &Netlist<StdCellType, ()>,
    space: &AdaptiveRegionSpace,
    ext: &ExtendedEGraph,
    result: &extraction_gym::extract::ExtractionResult,
    trace: &extraction_gym::extract::NldmEvaluationV2,
    adjoint: &extraction_gym::extract::CircuitAdjointV1,
    default_physical: &mac_egg::physical_topology_egraph::PhysicalizedTopology,
    region: &[usize],
    limit: usize,
    unary_scores: &FxHashMap<String, f64>,
    macro_beam: bool,
    macro_pair_beam: bool,
) -> Result<(Vec<ScoredAssignment>, &'static str, usize)> {
    extract_phase_i(
        source,
        RewrittenEquivalence {
            space,
            domain: ImplementationDomain::Unresolved { anchors: region },
        },
        Some(RegionalScoring {
            ext,
            result,
            trace,
            adjoint,
            default_physical,
            limit,
            unary_scores,
            macro_beam,
            macro_pair_beam,
        }),
    )
}

fn score_singleton_region(
    source: &Netlist<StdCellType, ()>,
    space: &AdaptiveRegionSpace,
    ext: &ExtendedEGraph,
    trace: &extraction_gym::extract::NldmEvaluationV2,
    adjoint: &extraction_gym::extract::CircuitAdjointV1,
    anchor: usize,
) -> Result<(Vec<ScoredAssignment>, &'static str, usize)> {
    let class = space
        .class_by_anchor(anchor)
        .context("missing singleton active eclass")?;
    let incumbent_enode = class
        .enodes
        .iter()
        .find(|enode| enode.incumbent)
        .context("singleton eclass is missing its incumbent enode")?;
    let as_region =
        |response: mac_egg::region_local_evaluator::LocalConeResponse| LocalRegionResponse {
            input_caps: response.input_caps,
            outputs: std::iter::once((anchor, (response.rise, response.fall))).collect(),
            area: response.area,
            power: response.power,
        };
    let incumbent = as_region(
        evaluate_region_enode(&incumbent_enode.expression, anchor, trace, ext)
            .map_err(anyhow::Error::msg)?,
    );
    let mut enodes: Vec<_> = class
        .enodes
        .iter()
        .filter(|enode| !enode.incumbent)
        .collect();
    enodes.sort_by(|left, right| left.enode_id.cmp(&right.enode_id));
    let jobs = env_jobs("EGG_REGION_SCORE_JOBS", 8)?;
    let evaluated = parallel_map_ordered(&enodes, jobs, |_, enode| {
        let assignment = RegionAssignment {
            changed_eclasses: 1,
            choices: vec![mac_egg::adaptive_region::RegionChoice {
                eclass_id: class.eclass_id.clone(),
                enode_id: enode.enode_id.clone(),
            }],
        };
        // For a single selected occurrence, full-netlist physicalization adds
        // no extra closure decision.  The only possible structural legality
        // failure is a boundary reference that feeds back through the root.
        // Check that condition directly on the source DAG instead of copying
        // and topologically sorting the entire netlist for every e-node.
        let root = NodeIndex::new(anchor);
        let legal = enode.boundary_anchors.iter().all(|boundary| {
            let boundary = NodeIndex::new(*boundary);
            boundary != root
                && source.graph.node_weight(boundary).is_some()
                && !has_path_connecting(&source.graph, boundary, root, None)
        });
        if !legal {
            return Ok(None);
        }
        let response = as_region(
            evaluate_region_enode(&enode.expression, anchor, trace, ext)
                .map_err(anyhow::Error::msg)?,
        );
        let score = boundary_dual_score_region(&response, &incumbent, adjoint);
        let components = boundary_dual_components_region(&response, &incumbent, adjoint);
        Ok(Some(ScoredAssignment {
            assignment,
            score,
            components,
        }))
    })?;
    let mut rows: Vec<_> = evaluated.into_iter().flatten().collect();
    rows.sort_by(|first, second| {
        first
            .score
            .partial_cmp(&second.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                first
                    .assignment
                    .changed_eclasses
                    .cmp(&second.assignment.changed_eclasses)
            })
    });
    Ok((rows, "l1_exact_enumeration", class.enodes.len()))
}

fn component_key(component: &BoundaryDualComponent) -> (String, Option<usize>) {
    (format!("{:?}", component.kind), component.anchor)
}

fn residual_order(
    rows: &[ScoredAssignment],
    variant: Variant,
) -> Vec<(String, Option<usize>, f64)> {
    if rows.is_empty() {
        return Vec::new();
    }
    let mut residuals = Vec::new();
    match variant {
        Variant::V2 => {
            residuals.extend(rows[0].components.iter().map(|component| {
                let (kind, anchor) = component_key(component);
                (kind, anchor, component.residual)
            }));
        }
        Variant::V3 => {
            let mut states: FxHashMap<(String, Option<usize>), (f64, f64, f64)> =
                FxHashMap::default();
            for row in rows {
                for component in &row.components {
                    let key = component_key(component);
                    let entry = states.entry(key).or_insert((
                        component.candidate_response,
                        component.candidate_response,
                        component.dual_price.abs(),
                    ));
                    entry.0 = entry.0.min(component.candidate_response);
                    entry.1 = entry.1.max(component.candidate_response);
                    entry.2 = component.dual_price.abs();
                }
            }
            residuals.extend(states.into_iter().map(
                |((kind, anchor), (minimum, maximum, price))| {
                    (kind, anchor, price * (maximum - minimum))
                },
            ));
        }
        Variant::V1 => {}
    }
    residuals.sort_by(|first, second| {
        second
            .2
            .partial_cmp(&first.2)
            .unwrap_or(Ordering::Equal)
            .then_with(|| first.0.cmp(&second.0))
            .then_with(|| first.1.cmp(&second.1))
    });
    residuals
}

fn directional_neighbor(
    residuals: &[(String, Option<usize>, f64)],
    source: &Netlist<StdCellType, ()>,
    active: &FxHashSet<usize>,
    region: &FxHashSet<usize>,
    adjoint: &extraction_gym::extract::CircuitAdjointV1,
) -> Option<(usize, String, f64)> {
    for (kind, anchor, residual) in residuals {
        let Some(anchor) = anchor else { continue };
        if kind == "InputCap" {
            if active.contains(anchor) && !region.contains(anchor) {
                return Some((*anchor, kind.clone(), *residual));
            }
        } else if kind.starts_with("OutputArrival") || kind.starts_with("OutputSlew") {
            let mut consumers: Vec<_> = source
                .outputs(NodeIndex::new(*anchor))
                .map(|node| node.index())
                .filter(|candidate| active.contains(candidate) && !region.contains(candidate))
                .collect();
            consumers.sort_by(|first, second| {
                output_price(adjoint, *second)
                    .partial_cmp(&output_price(adjoint, *first))
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| first.cmp(second))
            });
            if let Some(candidate) = consumers.first() {
                return Some((*candidate, kind.clone(), *residual));
            }
        }
    }
    None
}

fn fallback_neighbor(
    region: &FxHashSet<usize>,
    adjacency: &FxHashMap<usize, FxHashSet<usize>>,
    opportunity: &FxHashMap<usize, f64>,
) -> Option<usize> {
    let mut candidates = FxHashSet::default();
    for anchor in region {
        candidates.extend(
            adjacency
                .get(anchor)
                .into_iter()
                .flatten()
                .copied()
                .filter(|value| !region.contains(value)),
        );
    }
    let mut candidates: Vec<_> = candidates.into_iter().collect();
    candidates.sort_by(|first, second| {
        opportunity
            .get(second)
            .copied()
            .unwrap_or(0.0)
            .partial_cmp(&opportunity.get(first).copied().unwrap_or(0.0))
            .unwrap_or(Ordering::Equal)
            .then_with(|| first.cmp(second))
    });
    candidates.first().copied()
}

fn frontier_candidates(
    region: &FxHashSet<usize>,
    adjacency: &FxHashMap<usize, FxHashSet<usize>>,
) -> Vec<usize> {
    let mut candidates = FxHashSet::default();
    for anchor in region {
        candidates.extend(
            adjacency
                .get(anchor)
                .into_iter()
                .flatten()
                .copied()
                .filter(|value| !region.contains(value)),
        );
    }
    let mut candidates: Vec<_> = candidates.into_iter().collect();
    candidates.sort_unstable();
    candidates
}

fn directional_support(
    residuals: &[(String, Option<usize>, f64)],
    source: &Netlist<StdCellType, ()>,
    frontier: &FxHashSet<usize>,
    adjoint: &extraction_gym::extract::CircuitAdjointV1,
) -> FxHashMap<usize, f64> {
    let mut support = FxHashMap::default();
    for (kind, anchor, residual) in residuals {
        let Some(anchor) = anchor else { continue };
        if kind == "InputCap" {
            if frontier.contains(anchor) {
                *support.entry(*anchor).or_insert(0.0) += residual.abs();
            }
        } else if kind.starts_with("OutputArrival") || kind.starts_with("OutputSlew") {
            let consumers: Vec<_> = source
                .outputs(NodeIndex::new(*anchor))
                .map(|node| node.index())
                .filter(|candidate| frontier.contains(candidate))
                .collect();
            let total_price: f64 = consumers
                .iter()
                .map(|candidate| output_price(adjoint, *candidate))
                .sum();
            for candidate in consumers {
                let share = if total_price > 0.0 {
                    output_price(adjoint, candidate) / total_price
                } else {
                    1.0
                };
                *support.entry(candidate).or_insert(0.0) += residual.abs() * share;
            }
        }
    }
    support
}

fn consensus_neighbor(
    policy: RegionPolicy,
    region: &FxHashSet<usize>,
    adjacency: &FxHashMap<usize, FxHashSet<usize>>,
    opportunity: &FxHashMap<usize, f64>,
    anchor_features: &FxHashMap<usize, AnchorFeatures>,
    residuals: &[(String, Option<usize>, f64)],
    source: &Netlist<StdCellType, ()>,
    adjoint: &extraction_gym::extract::CircuitAdjointV1,
) -> (Option<usize>, Vec<FrontierFeatures>) {
    let candidates = frontier_candidates(region, adjacency);
    let frontier: FxHashSet<_> = candidates.iter().copied().collect();
    let residual_support = directional_support(residuals, source, &frontier, adjoint);
    let mut rows: Vec<_> = candidates
        .iter()
        .map(|anchor| {
            let neighbors = adjacency.get(anchor);
            let internal_links = neighbors.map_or(0, |values| values.intersection(region).count());
            let external_links = neighbors.map_or(0, |values| {
                values
                    .iter()
                    .filter(|value| !region.contains(value) && **value != *anchor)
                    .count()
            });
            let cohesion = (internal_links as f64 + 1.0)
                / (internal_links as f64 + external_links as f64 + 1.0);
            FrontierFeatures {
                anchor: *anchor,
                opportunity: opportunity.get(anchor).copied().unwrap_or(0.0),
                topology_richness: anchor_features
                    .get(anchor)
                    .map_or(0, |item| item.topology_richness),
                internal_links,
                external_links,
                cohesion,
                directional_residual: residual_support.get(anchor).copied().unwrap_or(0.0),
                consensus_score: 0.0,
            }
        })
        .collect();
    let opportunity_points = rank_points(&rows, |row| row.opportunity);
    let richness_points = rank_points(&rows, |row| row.topology_richness as f64);
    let cohesion_points = rank_points(&rows, |row| row.cohesion);
    let residual_points = rank_points(&rows, |row| row.directional_residual);
    for (index, row) in rows.iter_mut().enumerate() {
        let (cohesion_weight, residual_weight) = match policy {
            RegionPolicy::Hybrid => (1.0, 1.0),
            RegionPolicy::Block => (2.0, 0.5),
            RegionPolicy::Residual => unreachable!(),
        };
        row.consensus_score = opportunity_points[index]
            + richness_points[index]
            + cohesion_weight * cohesion_points[index]
            + residual_weight * residual_points[index];
    }
    rows.sort_by(|first, second| {
        second
            .consensus_score
            .partial_cmp(&first.consensus_score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                second
                    .directional_residual
                    .partial_cmp(&first.directional_residual)
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| first.anchor.cmp(&second.anchor))
    });
    (rows.first().map(|row| row.anchor), rows)
}

#[allow(clippy::too_many_arguments)]
fn plan_seed_regions(
    variant: Variant,
    variant_label: &str,
    region_policy: RegionPolicy,
    seed_index: usize,
    seed: usize,
    sizes: &[usize],
    p2_keep: usize,
    source: &Netlist<StdCellType, ()>,
    space: &AdaptiveRegionSpace,
    ext: &ExtendedEGraph,
    result: &extraction_gym::extract::ExtractionResult,
    trace: &extraction_gym::extract::NldmEvaluationV2,
    adjoint: &extraction_gym::extract::CircuitAdjointV1,
    default_physical: &mac_egg::physical_topology_egraph::PhysicalizedTopology,
    active: &FxHashSet<usize>,
    adjacency: &FxHashMap<usize, FxHashSet<usize>>,
    opportunity: &FxHashMap<usize, f64>,
    anchor_features: &FxHashMap<usize, AnchorFeatures>,
    unary_scores: &FxHashMap<String, f64>,
    macro_beam: bool,
    macro_pair_beam: bool,
) -> Result<(Vec<PlanEntry>, Vec<Value>)> {
    let mut plan = Vec::new();
    let mut growth_trace = Vec::new();
    let mut region: FxHashSet<usize> = [seed].into_iter().collect();
    for &target in sizes {
        while region.len() < target {
            let mut current: Vec<_> = region.iter().copied().collect();
            current.sort_unstable();
            let (rows, solver, theoretical) = score_region(
                source,
                space,
                ext,
                result,
                trace,
                adjoint,
                default_physical,
                &current,
                p2_keep,
                unary_scores,
                macro_beam,
                macro_pair_beam,
            )?;
            let residuals = residual_order(&rows, variant);
            let (next, reason, residual, frontier_audit) =
                if region_policy == RegionPolicy::Residual {
                    let direction = if variant == Variant::V1 {
                        None
                    } else {
                        directional_neighbor(&residuals, source, active, &region, adjoint)
                    };
                    if let Some((next, reason, residual)) = direction {
                        (Some(next), reason, residual, Vec::new())
                    } else {
                        (
                            fallback_neighbor(&region, adjacency, opportunity),
                            "connected_fallback".to_owned(),
                            0.0,
                            Vec::new(),
                        )
                    }
                } else {
                    let (next, audit) = consensus_neighbor(
                        region_policy,
                        &region,
                        adjacency,
                        opportunity,
                        anchor_features,
                        &residuals,
                        source,
                        adjoint,
                    );
                    let residual = next
                        .and_then(|anchor| {
                            audit
                                .iter()
                                .find(|row| row.anchor == anchor)
                                .map(|row| row.directional_residual)
                        })
                        .unwrap_or(0.0);
                    (
                        next,
                        format!("{region_policy:?}_rank_consensus"),
                        residual,
                        audit,
                    )
                };
            let Some(next) = next else { break };
            growth_trace.push(serde_json::json!({
                "variant":variant_label, "region_policy":region_policy, "seed":seed,
                "target_size":target, "region_before":current, "selected_anchor":next,
                "reason":reason, "residual":residual, "solver":solver,
                "theoretical_combinations":theoretical,
                "macro_component_sizes":macro_components(space, &current).iter().map(Vec::len).collect::<Vec<_>>(),
                "frontier_features":frontier_audit,
                "top_residuals":residuals.iter().take(8).map(|(kind,anchor,value)|serde_json::json!({"kind":kind,"anchor":anchor,"residual":value})).collect::<Vec<_>>(),
            }));
            region.insert(next);
        }
        if region.len() != target {
            continue;
        }
        let mut current: Vec<_> = region.iter().copied().collect();
        current.sort_unstable();
        let (rows, solver, theoretical) = score_region(
            source,
            space,
            ext,
            result,
            trace,
            adjoint,
            default_physical,
            &current,
            p2_keep,
            unary_scores,
            macro_beam,
            macro_pair_beam,
        )?;
        let region_id = format!("{variant_label}_R{target}_S{seed_index}_{seed}");
        for (rank, row) in rows.into_iter().enumerate() {
            let entries: Vec<_> = row
                .assignment
                .choices
                .iter()
                .map(|choice| (choice.eclass_id.clone(), choice.enode_id.clone()))
                .collect();
            plan.push(PlanEntry {
                candidate_id: format!("{region_id}_C{rank:03}"),
                region_id: region_id.clone(),
                region_size: target,
                p2_rank: rank,
                p2_score: row.score,
                topology_signature: topology_signature(&entries),
                choices: row.assignment.choices,
                components: row.components,
            });
        }
        growth_trace.push(serde_json::json!({
            "variant":variant_label,"region_policy":region_policy,"seed":seed,
            "target_size":target,"region":current,"event":"region_scored","solver":solver,
            "theoretical_combinations":theoretical,
            "macro_component_sizes":macro_components(space, &current).iter().map(Vec::len).collect::<Vec<_>>()
        }));
    }
    Ok((plan, growth_trace))
}

fn run_cli_inner(
    values: Vec<String>,
    shared_cell_nldm: Option<&HashMap<String, NLDM>>,
    shared_cell_nldm_receiver: Option<mpsc::Receiver<HashMap<String, NLDM>>>,
    shared_pins: Option<&Library>,
    objective_weights: Option<CircuitObjectiveWeights>,
) -> Result<HashMap<String, NLDM>> {
    let planner_started = Instant::now();
    if values.len() < 3 {
        bail!(
            "usage: run_dual_region_planner INPUT OUT_DIR v1|v2|v3 [TOP_SEEDS=4] [P2_KEEP=8] [MAX_REGION_SIZE=8]"
        );
    }
    let input = PathBuf::from(&values[0]);
    let out = PathBuf::from(&values[1]);
    let variant = match values[2].as_str() {
        "v1" => Variant::V1,
        "v2" => Variant::V2,
        "v3" => Variant::V3,
        _ => bail!("variant must be v1/v2/v3"),
    };
    let top_seeds = values
        .get(3)
        .and_then(|value| value.parse().ok())
        .unwrap_or(4usize);
    let p2_keep = values
        .get(4)
        .and_then(|value| value.parse().ok())
        .unwrap_or(8usize);
    let max_region_size = values
        .get(5)
        .and_then(|value| value.parse().ok())
        .unwrap_or(8usize);
    let region_policy = RegionPolicy::from_env()?;
    let timing_boundary = TimingBoundary::from_env()?;
    let macro_beam = std::env::var("EGG_MACRO_BEAM")
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(true);
    let macro_pair_beam = std::env::var("EGG_MACRO_PAIR_BEAM")
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(false);
    if ![1usize, 2, 4, 8, 16].contains(&max_region_size) {
        bail!("MAX_REGION_SIZE must be 1, 2, 4, 8, or 16");
    }
    fs::create_dir_all(&out)?;
    fs::write(out.join("pid.txt"), format!("{}\n", std::process::id()))?;
    status(&out, "initializing", 0, 1, "load incumbent")?;
    let liberty_path = std::env::var("EGG_LIB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(LIB));
    let load_started = Instant::now();
    let pins = if let Some(pins) = shared_pins {
        pins.clone()
    } else {
        let liberty = read_liberty(&liberty_path).map_err(anyhow::Error::msg)?;
        get_direction_of_pins(&liberty).map_err(anyhow::Error::msg)?
    };
    let (source, module_name) =
        read_verilog_with_lib_to_netlist(&input, pins.clone()).map_err(anyhow::Error::msg)?;
    let load_sec = load_started.elapsed().as_secs_f64();
    // Stable machine-readable graph for external/replaceable structural
    // generators.  Verilog net names such as `n_17` are not occurrence ids;
    // generators must use these parser-assigned anchors and ordered inputs to
    // construct a physically valid regional cut.
    let source_occurrences: Vec<_> = source
        .graph
        .node_indices()
        .map(|anchor| {
            let mut consumers: Vec<_> = source.outputs(anchor).map(|item| item.index()).collect();
            consumers.sort_unstable();
            serde_json::json!({
                "anchor": anchor.index(),
                "op": source.graph[anchor].to_string(),
                "inputs": source.inputs(anchor).map(|item| item.index()).collect::<Vec<_>>(),
                "consumers": consumers,
                "is_leaf": source.leaves.contains(&anchor),
                "is_root": source.roots.contains(&anchor),
                "is_constant": source.graph[anchor].is_constant(),
            })
        })
        .collect();
    write_json(
        &out.join("source_occurrences.json"),
        &serde_json::json!({"module":module_name.clone(),"occurrences":source_occurrences}),
    )?;
    // A single-process controller may parse the immutable NLDM database in
    // parallel with the planner's Liberty-pin/netlist front end.  Join that
    // work only at the first point where physical tables are required.
    let received_nldm = shared_cell_nldm_receiver
        .map(|receiver| {
            receiver
                .recv()
                .map_err(|error| anyhow::anyhow!("shared NLDM preload failed: {error}"))
        })
        .transpose()?;
    let shared_cell_nldm = shared_cell_nldm.or(received_nldm.as_ref());
    let extended_started = Instant::now();
    let (mut ext, result) = extended(&source, &liberty_path, shared_cell_nldm)?;
    if timing_boundary.internal_timing_model.is_v3() {
        if let Some(driver) =
            shared_cell_nldm.and_then(|cache| cache.get(timing_boundary.primary_input_driver_cell))
        {
            ext.cell_nldm.insert(
                timing_boundary.primary_input_driver_cell.to_owned(),
                driver.clone(),
            );
        } else {
            ext.ensure_cell_nldm_from_lib(
                liberty_path.to_string_lossy().as_ref(),
                timing_boundary.primary_input_driver_cell,
            )
            .map_err(anyhow::Error::msg)?;
        }
    }
    let extended_sec = extended_started.elapsed().as_secs_f64();
    let timing_started = Instant::now();
    let trace = result.evaluate_nldm_v2_with_trace(&ext, None, timing_boundary.nldm_config());
    let timing_sec = timing_started.elapsed().as_secs_f64();
    let adjoint_started = Instant::now();
    let adjoint_config = CircuitAdjointConfig {
        timing_control: TimingAdjointControl::G2LocalSoftBackprop,
        transition_backward: TransitionBackward::E1SoftAllocation,
        tau_ratio: 0.02,
    };
    // Keep the frozen D2AP path on the historical wrapper.  Only an explicit
    // programmable objective enters the weighted adjoint entry point.
    let adjoint = if let Some(weights) = objective_weights {
        circuit_adjoint_weighted(&result, &ext, &trace, adjoint_config, weights)
    } else {
        circuit_adjoint(&result, &ext, &trace, adjoint_config)
    }
    .map_err(anyhow::Error::msg)?;
    let adjoint_sec = adjoint_started.elapsed().as_secs_f64();
    let configured_rules = std::env::var_os("EGG_RULE_PATHS")
        .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_else(|| RULES.iter().map(PathBuf::from).collect());
    let rule_paths: Vec<_> = configured_rules.iter().map(PathBuf::as_path).collect();
    let structural_started = Instant::now();
    let phase1_drive_expansion_enabled = std::env::var("EGG_PHASE1_DRIVE_EXPANSION")
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(true);
    let (space, structural_rule_audit) = if phase1_drive_expansion_enabled {
        build_rule_agnostic_space(&source, &rule_paths, 2, 3).map_err(anyhow::Error::msg)?
    } else {
        let scale_rule_path = std::env::var("EGG_SCALE_RULES")
            .context("EGG_SCALE_RULES is required when Phase-I drive expansion is disabled")?;
        build_rule_agnostic_space_without_drive_expansion(
            &source,
            &rule_paths,
            Path::new(&scale_rule_path),
            2,
            3,
        )
        .map_err(anyhow::Error::msg)?
    };
    // Populate every rule-generated cell once.  Without this, each singleton
    // worker cloned the whole ExtendedEGraph and reparsed the same missing
    // cells independently.
    let required_ops: BTreeSet<_> = space
        .active_eclasses()
        .flat_map(|class| class.enodes.iter())
        .flat_map(|enode| required_cell_ops(&enode.expression))
        .collect();
    for op in required_ops {
        if ext.cell_nldm.contains_key(&op) {
            continue;
        }
        if let Some(nldm) = shared_cell_nldm.and_then(|cache| cache.get(&op)) {
            ext.cell_nldm.insert(op, nldm.clone());
        } else {
            // Standalone planner compatibility: callers without a process
            // cache retain the historical lazy Liberty fallback.
            ext.ensure_cell_nldm_from_lib(&liberty_path.display().to_string(), &op)
                .map_err(anyhow::Error::msg)?;
        }
    }
    let structural_sec = structural_started.elapsed().as_secs_f64();
    let active: FxHashSet<_> = space
        .active_eclasses()
        .map(|class| class.root_anchor)
        .collect();
    // Audit-only state receipts.  These are deliberately emitted after every
    // cold planner invocation so an outer fixed-point loop can prove it did
    // not reuse circuit-dependent timing/adjoint state across accepted rounds.
    fs::write(out.join("timing_trace_debug.txt"), format!("{trace:#?}\n"))?;
    fs::write(out.join("adjoint_debug.txt"), format!("{adjoint:#?}\n"))?;
    let mut anchor_map: Vec<_> = space.active_eclasses().map(|class| serde_json::json!({
        "root_anchor": class.root_anchor,
        "eclass_id": class.eclass_id,
        "enode_ids": class.enodes.iter().map(|enode| enode.enode_id.clone()).collect::<Vec<_>>(),
    })).collect();
    anchor_map.sort_by_key(|row| row["root_anchor"].as_u64());
    write_json(&out.join("anchor_map.json"), &anchor_map)?;
    let adjacency = active_adjacency(&source, &active);
    let anchor_features: FxHashMap<_, _> = active
        .iter()
        .map(|anchor| (*anchor, anchor_features(&space, *anchor, &adjacency)))
        .collect();
    let default_physical =
        physicalize_topology(&source, &TopologySelection::default()).map_err(anyhow::Error::msg)?;

    let mut opportunity = FxHashMap::default();
    let mut seed_trace = Vec::new();
    let mut anchors: Vec<_> = active.iter().copied().collect();
    anchors.sort_unstable();
    let dual_seed_started = Instant::now();
    status(
        &out,
        "dual_seed",
        0,
        anchors.len(),
        "parallel singleton scoring",
    )?;
    let dual_seed_jobs = env_jobs("EGG_DUAL_SEED_JOBS", 32)?;
    let singleton_rows = parallel_map_ordered(&anchors, dual_seed_jobs, |_, anchor| {
        score_singleton_region(&source, &space, &ext, &trace, &adjoint, *anchor)
    })?;
    for (anchor, (rows, solver, theoretical)) in anchors.iter().zip(singleton_rows) {
        let best = rows.first().map(|row| row.score).unwrap_or(0.0);
        let value = (-best).max(0.0);
        opportunity.insert(*anchor, value);
        seed_trace.push(serde_json::json!({"anchor":anchor,"best_p2_score":best,"opportunity":value,"legal_candidates":rows.len(),"solver":solver,"theoretical_combinations":theoretical,
            "region_features":anchor_features.get(anchor),
            "enode_scores":rows.iter().flat_map(|row|row.assignment.choices.iter().map(move |choice|serde_json::json!({"enode_id":choice.enode_id,"score":row.score}))).collect::<Vec<_>>() }));
    }
    let dual_seed_sec = dual_seed_started.elapsed().as_secs_f64();
    seed_trace.sort_by(|first, second| {
        second["opportunity"]
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&first["opportunity"].as_f64().unwrap_or(0.0))
            .unwrap_or(Ordering::Equal)
            .then_with(|| first["anchor"].as_u64().cmp(&second["anchor"].as_u64()))
    });
    write_json(&out.join("dual_seed_trace.json"), &seed_trace)?;
    let mut unary_scores = FxHashMap::default();
    for row in &seed_trace {
        if let Some(items) = row["enode_scores"].as_array() {
            for item in items {
                if let (Some(id), Some(score)) = (item["enode_id"].as_str(), item["score"].as_f64())
                {
                    unary_scores.insert(id.to_owned(), score);
                }
            }
        }
    }
    let seeds = choose_seeds(region_policy, top_seeds, &seed_trace, &anchor_features);

    let sizes: Vec<_> = [1usize, 2, 4, 8, 16]
        .into_iter()
        .filter(|size| *size <= max_region_size)
        .collect();
    status(
        &out,
        "region_planning",
        0,
        seeds.len(),
        "independent seed trajectories",
    )?;
    let requested_region_jobs = std::env::var("EGG_REGION_JOBS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    let region_jobs = requested_region_jobs.min(seeds.len().max(1));
    let mut per_seed: Vec<Option<Result<(Vec<PlanEntry>, Vec<Value>)>>> =
        (0..seeds.len()).map(|_| None).collect();
    let region_planning_started = Instant::now();
    if region_jobs == 1 {
        for (seed_index, seed) in seeds.iter().copied().enumerate() {
            per_seed[seed_index] = Some(plan_seed_regions(
                variant,
                &values[2],
                region_policy,
                seed_index,
                seed,
                &sizes,
                p2_keep,
                &source,
                &space,
                &ext,
                &result,
                &trace,
                &adjoint,
                &default_physical,
                &active,
                &adjacency,
                &opportunity,
                &anchor_features,
                &unary_scores,
                macro_beam,
                macro_pair_beam,
            ));
        }
    } else {
        let next = AtomicUsize::new(0);
        let (sender, receiver) = mpsc::channel();
        let source_ref = &source;
        let space_ref = &space;
        let ext_ref = &ext;
        let extraction_result_ref = &result;
        let trace_ref = &trace;
        let adjoint_ref = &adjoint;
        let default_physical_ref = &default_physical;
        let active_ref = &active;
        let adjacency_ref = &adjacency;
        let opportunity_ref = &opportunity;
        let anchor_features_ref = &anchor_features;
        let unary_scores_ref = &unary_scores;
        std::thread::scope(|scope| {
            for _ in 0..region_jobs {
                let sender = sender.clone();
                let next = &next;
                let seeds = &seeds;
                let sizes = &sizes;
                let variant_label = &values[2];
                scope.spawn(move || {
                    loop {
                        let seed_index = next.fetch_add(1, AtomicOrdering::Relaxed);
                        let Some(seed) = seeds.get(seed_index).copied() else {
                            break;
                        };
                        let result = plan_seed_regions(
                            variant,
                            variant_label,
                            region_policy,
                            seed_index,
                            seed,
                            sizes,
                            p2_keep,
                            source_ref,
                            space_ref,
                            ext_ref,
                            extraction_result_ref,
                            trace_ref,
                            adjoint_ref,
                            default_physical_ref,
                            active_ref,
                            adjacency_ref,
                            opportunity_ref,
                            anchor_features_ref,
                            unary_scores_ref,
                            macro_beam,
                            macro_pair_beam,
                        );
                        if sender.send((seed_index, result)).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        drop(sender);
        for (seed_index, result) in receiver {
            per_seed[seed_index] = Some(result);
        }
    }
    let mut plan = Vec::new();
    let mut growth_trace = Vec::new();
    for result in per_seed {
        let (mut seed_plan, mut seed_trace) = result.context("missing region worker result")??;
        plan.append(&mut seed_plan);
        growth_trace.append(&mut seed_trace);
    }
    let region_planning_sec = region_planning_started.elapsed().as_secs_f64();
    write_json(&out.join("dual_residual_growth_trace.json"), &growth_trace)?;
    write_json(&out.join("dual_region_plan_full.json"), &plan)?;
    let emit_plan: Vec<_> = plan
        .iter()
        .map(|item| serde_json::json!({"candidate_id":item.candidate_id,"choices":item.choices}))
        .collect();
    write_json(&out.join("materialization_plan.json"), &emit_plan)?;
    // The fast Frozen-D1 path consumes the same rule space immediately for P1.
    // Materialize here while `source` and `space` are still resident instead of
    // launching another process and rebuilding the complete rewrite space.
    let emit_started = Instant::now();
    if std::env::var_os("EGG_PLANNER_EMIT").is_some() {
        let materialized = out.join("materialized");
        fs::create_dir_all(materialized.join("candidates"))?;
        let emit_jobs = env_jobs("EGG_REALIZATION_JOBS", 16)?;
        let rows = parallel_map_ordered(&plan, emit_jobs, |_, item| {
            let changed = item
                .choices
                .iter()
                .filter(|choice| {
                    space
                        .enode(&choice.enode_id)
                        .is_some_and(|enode| !enode.incumbent)
                })
                .count();
            let assignment = RegionAssignment {
                choices: item.choices.clone(),
                changed_eclasses: changed,
            };
            let output_path = materialized
                .join("candidates")
                .join(format!("{}.v", item.candidate_id));
            match space.physicalize_assignment(&source, &assignment) {
                Ok(physical) => {
                    write_verilog_from_netlist_with_lib_ref(
                        &output_path,
                        &physical.netlist,
                        &module_name,
                        &pins,
                    )
                    .map_err(anyhow::Error::msg)?;
                    Ok(serde_json::json!({
                        "candidate_id": item.candidate_id,
                        "choices": item.choices,
                        "changed_eclasses": changed,
                        "output_netlist": output_path,
                        "legal": true,
                        "error": null,
                    }))
                }
                Err(error) => Ok(serde_json::json!({
                    "candidate_id": item.candidate_id,
                    "choices": item.choices,
                    "changed_eclasses": changed,
                    "output_netlist": output_path,
                    "legal": false,
                    "error": error,
                })),
            }
        })?;
        write_json(&materialized.join("materialization_trace.json"), &rows)?;
    }
    let emit_sec = emit_started.elapsed().as_secs_f64();
    write_json(
        &out.join("summary.json"),
        &serde_json::json!({
            "variant": values[2], "region_policy":region_policy, "p2_selection_policy":"global", "input": input, "top_seeds": top_seeds,
            "timing_boundary": timing_boundary,
            "objective_relative_weights": objective_weights.map(|weights| serde_json::json!({
                "delay":weights.delay,"area":weights.area,"power":weights.power,
            })).unwrap_or_else(|| serde_json::json!({"preset":"frozen_d2ap"})),
            "incumbent_v2": {
                "delay": trace.cost.components[0].into_inner(),
                "area": trace.cost.components[1].into_inner(),
                "power": trace.cost.components[2].into_inner(),
                "score": trace.cost.components[0].into_inner()
                    * trace.cost.components[0].into_inner()
                    * trace.cost.components[1].into_inner()
                    * trace.cost.components[2].into_inner(),
            },
            "p2_keep_per_region": p2_keep, "active_eclasses": active.len(),
            "seeds": seeds, "region_sizes": sizes, "regions": seeds.len() * sizes.len(), "planned_candidates": plan.len(),
            "pricing": "P2-G2-E1", "acceptance": "none; proposal only",
            "phase_i_input":"extended_rewritten_equivalence",
            "phase_i_domain":"unresolved",
            "phase_i_drive_expansion_enabled":phase1_drive_expansion_enabled,
            "phase_i_structural_rule_audit":structural_rule_audit,
            "large_region_solver": if macro_pair_beam {
                "rewrite_macro_pair_beam64"
            } else if macro_beam {
                "rewrite_macro_beam64"
            } else {
                "unary_beam64_ablation"
            },
            "rule_provenance_used": false,
            "macro_provenance": if macro_pair_beam {
                "selected-enode boundary-dependency cones + top-pair macro union"
            } else {
                "selected-enode boundary-dependency cones"
            },
            "profile_sec": {
                "load_liberty_and_netlist": load_sec,
                "build_extended_incumbent": extended_sec,
                "timing_v2": timing_sec,
                "adjoint": adjoint_sec,
                "structural_realizations": structural_sec,
                "dual_seed_scoring": dual_seed_sec,
                "region_planning": region_planning_sec,
                "emit_candidates": emit_sec,
                "total": planner_started.elapsed().as_secs_f64(),
            },
            "parallelism": {
                "dual_seed_jobs": dual_seed_jobs,
                "region_jobs": region_jobs,
                "region_score_jobs": env_jobs("EGG_REGION_SCORE_JOBS", 8)?,
            },
        }),
    )?;
    status(&out, "complete", plan.len(), plan.len(), "done")?;
    println!(
        "planned {} P2-ranked candidates across {} seeds",
        plan.len(),
        seeds.len()
    );
    Ok(ext.cell_nldm)
}

pub(crate) fn run_cli_with_nldm(
    values: Vec<String>,
    shared_cell_nldm: Option<&HashMap<String, NLDM>>,
    shared_pins: &Library,
) -> Result<HashMap<String, NLDM>> {
    run_cli_inner(values, shared_cell_nldm, None, Some(shared_pins), None)
}

pub(crate) fn run_cli_with_nldm_objective(
    values: Vec<String>,
    shared_cell_nldm: Option<&HashMap<String, NLDM>>,
    shared_pins: &Library,
    objective_weights: CircuitObjectiveWeights,
) -> Result<HashMap<String, NLDM>> {
    run_cli_inner(
        values,
        shared_cell_nldm,
        None,
        Some(shared_pins),
        Some(objective_weights),
    )
}

pub(crate) fn run_cli_with_nldm_receiver(
    values: Vec<String>,
    receiver: mpsc::Receiver<HashMap<String, NLDM>>,
    shared_pins: &Library,
) -> Result<HashMap<String, NLDM>> {
    run_cli_inner(values, None, Some(receiver), Some(shared_pins), None)
}

pub(crate) fn run_cli_with_nldm_receiver_objective(
    values: Vec<String>,
    receiver: mpsc::Receiver<HashMap<String, NLDM>>,
    shared_pins: &Library,
    objective_weights: CircuitObjectiveWeights,
) -> Result<HashMap<String, NLDM>> {
    run_cli_inner(
        values,
        None,
        Some(receiver),
        Some(shared_pins),
        Some(objective_weights),
    )
}

pub fn run_cli(values: Vec<String>) -> Result<()> {
    run_cli_inner(values, None, None, None, None).map(|_| ())
}

fn main() -> Result<()> {
    run_cli(std::env::args().skip(1).collect())
}

#[cfg(test)]
mod tests {
    use super::normalize_drive_tokens;
    use super::*;

    #[test]
    fn phase_i_unresolved_domain_uses_existing_combinations() {
        let lib = Path::new("../mac_egg/test/asap7sc6t_SELECT_LVT_TT_nldm.lib");
        let pins = read_pin_map_for_test(lib);
        let (source, _) =
            read_verilog_with_lib_to_netlist("../mac_egg/test/topology_inv_and.v", pins).unwrap();
        let (space, _) = build_rule_agnostic_space(
            &source,
            &[Path::new("../mac_egg/test/6t_adaptive_dummy_rules.json")],
            2,
            3,
        )
        .unwrap();
        let anchors: Vec<_> = space.active_eclasses().map(|c| c.root_anchor).collect();
        let (mut ext, result) = extended(&source, lib, None).unwrap();
        for op in space
            .active_eclasses()
            .flat_map(|c| &c.enodes)
            .flat_map(|e| required_cell_ops(&e.expression))
            .collect::<BTreeSet<_>>()
        {
            ext.ensure_cell_nldm_from_lib(lib.to_str().unwrap(), &op)
                .unwrap();
        }
        let trace = result.evaluate_nldm_v2_with_trace(&ext, None, Default::default());
        let adjoint = circuit_adjoint(
            &result,
            &ext,
            &trace,
            CircuitAdjointConfig {
                timing_control: TimingAdjointControl::G2LocalSoftBackprop,
                transition_backward: TransitionBackward::E1SoftAllocation,
                tau_ratio: 0.02,
            },
        )
        .unwrap();
        let physical = physicalize_topology(&source, &TopologySelection::default()).unwrap();
        let (rows, solver, _) = score_region(
            &source,
            &space,
            &ext,
            &result,
            &trace,
            &adjoint,
            &physical,
            &anchors,
            4096,
            &FxHashMap::default(),
            true,
            false,
        )
        .unwrap();
        assert_eq!(solver, "l1_exact_enumeration");
        let old = space
            .enumerate_legal_region(
                &source,
                &anchors,
                mac_egg::adaptive_region::BoundaryClosure::FixedBoundary,
                anchors.len(),
                4096,
            )
            .unwrap();
        let keys = |assignments: Vec<RegionAssignment>| {
            assignments
                .iter()
                .map(|a| serde_json::to_string(a).unwrap())
                .collect::<BTreeSet<_>>()
        };
        assert!(!rows.is_empty());
        assert_eq!(
            keys(rows.iter().map(|r| r.assignment.clone()).collect()),
            keys(old.legal_assignments)
        );
        // An assignment produced by partial expansion can be reintroduced as a
        // fixed domain, regardless of its ordinary-enode production history.
        let (fixed, fixed_solver, count) = extract_phase_i(
            &source,
            RewrittenEquivalence {
                space: &space,
                domain: ImplementationDomain::Fixed {
                    assignment: &rows[0].assignment,
                },
            },
            None,
        )
        .unwrap();
        assert_eq!(fixed_solver, "fixed_atomic_implementation");
        assert_eq!(count, 1);
        assert_eq!(
            keys(fixed.into_iter().map(|r| r.assignment).collect()),
            keys(vec![rows[0].assignment.clone()])
        );
    }

    fn read_pin_map_for_test(lib: &Path) -> Library {
        get_direction_of_pins(&read_liberty(lib).unwrap()).unwrap()
    }

    #[test]
    fn topology_signature_ignores_integer_and_fractional_drive_strength() {
        let first = "(AOI333xp17_ASAP7_6t_L (INVx1_ASAP7_6t_L a) b c)";
        let second = "(AOI333xp33_ASAP7_6t_L (INVx4_ASAP7_6t_L a) b c)";
        assert_eq!(
            normalize_drive_tokens(first),
            normalize_drive_tokens(second)
        );
        assert_eq!(
            normalize_drive_tokens(first),
            "(AOI333x*_ASAP7_6t_L (INVx*_ASAP7_6t_L a) b c)"
        );
    }

    #[test]
    fn topology_signature_preserves_logic_and_operand_order() {
        let aoi = "(AOI22x1_ASAP7_6t_L a b c d)";
        let oai = "(OAI22x1_ASAP7_6t_L a b c d)";
        let permuted = "(AOI22x1_ASAP7_6t_L b a c d)";
        assert_ne!(normalize_drive_tokens(aoi), normalize_drive_tokens(oai));
        assert_ne!(
            normalize_drive_tokens(aoi),
            normalize_drive_tokens(permuted)
        );
    }
}
