//! Rust-native D1-reorder + Generator V2 union controller.
//!
//! Python no longer participates in candidate generation, portfolio
//! construction, progressive A2, checkpoint/resume, SHA aliasing or final
//! write/reparse validation. Planner, materialization, P1, D1 reranking and
//! progressive A2 are called as Rust modules in one process. Audit JSON and
//! Verilog receipts remain on disk so the optimized trajectory can be checked.

#![recursion_limit = "256"]

use anyhow::{Context, Result, bail};
use d1_series::equivalence_candidate::{
    EquivalenceSource, UnifiedEquivalenceCandidate, attach_unified_candidate,
    unified_equivalence_enabled, validate_extraction_portfolio,
};
use d1_series::generator_v2::{
    GeneratorCandidate, GeneratorConfig, GeneratorProofGroup, generate,
    generate_deletion_candidates, provenance_class, specific_window, topology_signature,
};
use d1_series::objective::{
    ObjectiveFunction, ObjectiveSpec, PpaPoint, PrimalDualPolicy, SearchObjective,
};
use d1_series::shared_load_v2::{SharedLoadEvaluator, TimingBoundary};
use d1_series::v8_ultra::V8UltraConfig;
use extraction_gym::{NLDM, extract::CircuitObjectiveWeights};
use mac_egg::io::liberty::{Library, read_pin_directions_fast};
use mac_egg::io::stdcell::read_verilog_with_lib_to_netlist;
use mac_egg::language::LanguageType;
use regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Instant;

#[path = "run_frozen_a2_fast.rs"]
#[allow(dead_code)]
mod a2_impl;
#[path = "materialize_structural_macro_plan.rs"]
#[allow(dead_code)]
mod materializer_impl;
#[path = "evaluate_mapped_nldm_v2_shared_batch.rs"]
#[allow(dead_code)]
mod p1_impl;
#[path = "run_dual_region_planner.rs"]
#[allow(dead_code)]
mod planner_impl;
#[path = "rerank_dual_region_p3_fast.rs"]
#[allow(dead_code)]
mod reranker_impl;

fn sha(path: &Path) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(fs::read(path)?)))
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    fs::write(path, serde_json::to_string_pretty(value)? + "\n")?;
    Ok(())
}

fn source_instance_count(path: &Path) -> Result<usize> {
    let graph: Value = serde_json::from_slice(&fs::read(path)?)?;
    Ok(graph["occurrences"]
        .as_array()
        .context("source occurrences array missing")?
        .iter()
        .filter(|row| {
            row["is_leaf"].as_bool() == Some(false)
                && row["is_constant"].as_bool() == Some(false)
                && row["is_root"].as_bool() == Some(false)
        })
        .count())
}

fn ultra_candidate_bucket(candidate: &GeneratorCandidate) -> (String, String, String) {
    (
        candidate
            .provenance
            .get(1)
            .cloned()
            .unwrap_or_else(|| candidate.window_kind.clone()),
        candidate
            .provenance
            .first()
            .cloned()
            .unwrap_or_else(|| "unknown".to_owned()),
        candidate
            .mapping
            .clone()
            .unwrap_or_else(|| "unknown".to_owned()),
    )
}

fn ultra_candidate_cmp(
    left: &GeneratorCandidate,
    right: &GeneratorCandidate,
) -> std::cmp::Ordering {
    let timing = left.mapping.as_deref() == Some("timing");
    if timing {
        left.tech_delay
            .total_cmp(&right.tech_delay)
            .then_with(|| left.tech_area.total_cmp(&right.tech_area))
    } else {
        left.tech_area
            .total_cmp(&right.tech_area)
            .then_with(|| left.tech_delay.total_cmp(&right.tech_delay))
    }
    .then_with(|| right.choices.len().cmp(&left.choices.len()))
    .then_with(|| left.logic_depth.cmp(&right.logic_depth))
    .then_with(|| left.candidate_id.cmp(&right.candidate_id))
}

/// Deterministic large-only proposal cap before Verilog materialization.
///
/// The first representative of every `(region, provenance class, mapping)`
/// bucket is retained whenever the cap permits.  This is deliberately not a
/// global prefix: the successful late-round `epfl_max` candidates are sparse
/// multi-output buckets near the end of the generator's area ordering.
fn cap_generator_candidates(
    candidates: Vec<GeneratorCandidate>,
    cap: usize,
) -> (Vec<GeneratorCandidate>, Value) {
    let original_count = candidates.len();
    if cap == 0 || original_count <= cap {
        return (
            candidates,
            json!({
                "enabled":false,
                "cap":cap,
                "original_count":original_count,
                "selected_count":original_count,
            }),
        );
    }
    let mut buckets = BTreeMap::<(String, String, String), Vec<usize>>::new();
    for (index, candidate) in candidates.iter().enumerate() {
        buckets
            .entry(ultra_candidate_bucket(candidate))
            .or_default()
            .push(index);
    }
    for indices in buckets.values_mut() {
        indices.sort_by(|left, right| ultra_candidate_cmp(&candidates[*left], &candidates[*right]));
    }

    let mut selected = BTreeSet::new();
    let representatives: Vec<_> = buckets
        .iter()
        .map(|(key, indices)| (key.clone(), indices[0]))
        .collect();
    if representatives.len() <= cap {
        selected.extend(representatives.iter().map(|(_, index)| *index));
    } else {
        // Multi-output shared-DAG candidates are rare and cannot be inferred
        // from the cheaper single-root members of their region.  Reserve them
        // first, then reconvergent buckets, before evenly sampling the rest.
        for (_, index) in representatives
            .iter()
            .filter(|((_, class, _), _)| class == "multi-output-shared-dag")
        {
            if selected.len() == cap {
                break;
            }
            selected.insert(*index);
        }
        let reconvergent_limit = cap / 4;
        for (_, index) in representatives
            .iter()
            .filter(|((_, class, _), _)| class.contains("reconvergent"))
        {
            if selected.len() == cap || selected.len() >= reconvergent_limit {
                break;
            }
            selected.insert(*index);
        }
        let remaining: Vec<_> = representatives
            .iter()
            .map(|(_, index)| *index)
            .filter(|index| !selected.contains(index))
            .collect();
        let slots = cap.saturating_sub(selected.len());
        for position in 0..slots {
            let index = remaining[position * remaining.len() / slots];
            selected.insert(index);
        }
    }
    let mut depth = 1usize;
    while selected.len() < cap {
        let before = selected.len();
        for indices in buckets.values() {
            if let Some(index) = indices.get(depth) {
                selected.insert(*index);
                if selected.len() == cap {
                    break;
                }
            }
        }
        if selected.len() == before {
            break;
        }
        depth += 1;
    }
    let selected_candidates: Vec<_> = candidates
        .into_iter()
        .enumerate()
        .filter_map(|(index, candidate)| selected.contains(&index).then_some(candidate))
        .collect();
    let mut class_counts = BTreeMap::<String, usize>::new();
    for candidate in &selected_candidates {
        *class_counts
            .entry(
                candidate
                    .provenance
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_owned()),
            )
            .or_default() += 1;
    }
    let audit = json!({
        "enabled":true,
        "method":"region-provenance-mapping representatives plus deterministic round-robin fill",
        "cap":cap,
        "original_count":original_count,
        "bucket_count":buckets.len(),
        "selected_count":selected_candidates.len(),
        "selected_class_counts":class_counts,
        "selected_candidate_ids":selected_candidates.iter().map(|row|row.candidate_id.clone()).collect::<Vec<_>>(),
    });
    (selected_candidates, audit)
}

fn transit_point(row: &Value) -> Result<PpaPoint> {
    Ok(PpaPoint {
        delay: row["point"]["delay"]
            .as_f64()
            .context("transit point delay")?,
        area: row["point"]["area"]
            .as_f64()
            .context("transit point area")?,
        power: row["point"]["power"]
            .as_f64()
            .context("transit point power")?,
    })
}

fn select_plateau_transit(
    pool: &[Value],
    objective: &SearchObjective,
    incumbent: PpaPoint,
    visited_parent_shas: &BTreeSet<String>,
    visited_topology_signatures: &BTreeSet<String>,
) -> Result<Option<Value>> {
    if !objective.feasible_ppa(incumbent.delay, incumbent.area, incumbent.power) {
        return Ok(None);
    }
    let mut eligible = Vec::new();
    for row in pool {
        if row["feasible"].as_bool() != Some(true) {
            continue;
        }
        let candidate_sha = row["best_sha256"].as_str().context("transit SHA")?;
        let topology = row["topology_signature"]
            .as_str()
            .context("transit topology signature")?;
        if topology.is_empty()
            || visited_parent_shas.contains(candidate_sha)
            || visited_topology_signatures.contains(topology)
        {
            continue;
        }
        let point = transit_point(row)?;
        if objective.exact_objective_equivalent(point, incumbent)? {
            eligible.push(row.clone());
        }
    }
    eligible.sort_by(|left, right| {
        right["minimum_relative_constraint_slack"]
            .as_f64()
            .unwrap_or(f64::NEG_INFINITY)
            .total_cmp(
                &left["minimum_relative_constraint_slack"]
                    .as_f64()
                    .unwrap_or(f64::NEG_INFINITY),
            )
            .then_with(|| {
                left["exact_objective_value"]
                    .as_f64()
                    .unwrap_or(f64::INFINITY)
                    .total_cmp(
                        &right["exact_objective_value"]
                            .as_f64()
                            .unwrap_or(f64::INFINITY),
                    )
            })
            .then_with(|| {
                right["exact_stage_budget"]
                    .as_u64()
                    .unwrap_or(0)
                    .cmp(&left["exact_stage_budget"].as_u64().unwrap_or(0))
            })
            .then_with(|| {
                left["topology_signature"]
                    .as_str()
                    .cmp(&right["topology_signature"].as_str())
            })
            .then_with(|| {
                left["candidate_id"]
                    .as_str()
                    .cmp(&right["candidate_id"].as_str())
            })
    });
    Ok(eligible.into_iter().next())
}

fn rule_cell_names(path: &Path) -> Result<BTreeSet<String>> {
    let rules: Value = serde_json::from_slice(&fs::read(path)?)?;
    let cell = Regex::new(r"^\((\S+)")?;
    let mut names = BTreeSet::new();
    for row in rules["rewrites"]
        .as_array()
        .context("rules.rewrites must be an array")?
    {
        for field in ["searcher", "applier"] {
            let expression = row[field]
                .as_str()
                .with_context(|| format!("rewrite missing {field}"))?;
            if let Some(capture) = cell.captures(expression) {
                names.insert(capture[1].to_owned());
            }
        }
    }
    Ok(names)
}

fn generator_union_row(candidate: &GeneratorCandidate, output: &str) -> Result<Value> {
    let mut value = serde_json::to_value(candidate)?;
    let object = value.as_object_mut().context("generator row not object")?;
    object.insert(
        "source_class".into(),
        json!(if provenance_class(candidate) == "cone-collapse-deletion" {
            "generator-v2-deletion"
        } else {
            "generator-v2"
        }),
    );
    object.insert(
        "topology_signature".into(),
        json!(topology_signature(candidate)),
    );
    object.insert(
        "provenance_class".into(),
        json!(provenance_class(candidate)),
    );
    object.insert("specific_window".into(), json!(specific_window(candidate)));
    object.insert("output_netlist".into(), json!(output));
    let unified = UnifiedEquivalenceCandidate::from_generator(candidate);
    attach_unified_candidate(&mut value, &unified).map_err(anyhow::Error::msg)?;
    Ok(value)
}

fn evaluate_materialized_p1(
    output: &Path,
    module: &str,
    candidates: &[materializer_impl::MaterializedCandidate],
    nldm: &HashMap<String, NLDM>,
    objective: &SearchObjective,
) -> Result<Vec<Value>> {
    let jobs = std::env::var("EGG_OBJECTIVE_P1_JOBS")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()
        .context("invalid EGG_OBJECTIVE_P1_JOBS")?
        .unwrap_or(1)
        .max(1);
    if jobs > 1 && candidates.len() > 1 {
        let chunk_size = candidates.len().div_ceil(jobs);
        let rows = std::thread::scope(|scope| -> Result<Vec<Value>> {
            let handles: Vec<_> = candidates
                .chunks(chunk_size)
                .map(|chunk| {
                    scope.spawn(move || {
                        evaluate_materialized_p1_rows(module, chunk, nldm, objective)
                    })
                })
                .collect();
            let mut rows = Vec::with_capacity(candidates.len());
            for handle in handles {
                rows.extend(
                    handle
                        .join()
                        .map_err(|_| anyhow::anyhow!("P1 worker panicked"))??,
                );
            }
            Ok(rows)
        })?;
        write_json(output, &Value::Array(rows.clone()))?;
        return Ok(rows);
    }
    let rows = evaluate_materialized_p1_rows(module, candidates, nldm, objective)?;
    write_json(output, &Value::Array(rows.clone()))?;
    Ok(rows)
}

fn evaluate_materialized_p1_rows(
    module: &str,
    candidates: &[materializer_impl::MaterializedCandidate],
    nldm: &HashMap<String, NLDM>,
    objective: &SearchObjective,
) -> Result<Vec<Value>> {
    let boundary = TimingBoundary::from_env()?;
    let mut evaluator = SharedLoadEvaluator::from_nldm_cache(nldm, true)?.with_boundary(boundary);
    let mut rows = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let ppa = evaluator.evaluate_netlist(&candidate.netlist)?;
        rows.push(json!({
            "model":boundary.internal_timing_model.model_name(),
            "input":candidate.output_netlist,
            "module":module,
            "delay":ppa.delay,"area":ppa.area,"power":ppa.power,
            "score":objective.score(ppa.delay, ppa.area, ppa.power),
            "finite":ppa.delay.is_finite() && ppa.area.is_finite() && ppa.power.is_finite(),
        }));
    }
    Ok(rows)
}

fn emit_source_occurrences(input: &Path, output: &Path, pins: &Library) -> Result<String> {
    let (source, module) =
        read_verilog_with_lib_to_netlist(input, pins.clone()).map_err(anyhow::Error::msg)?;
    let occurrences: Vec<_> = source
        .graph
        .node_indices()
        .map(|anchor| {
            let mut consumers: Vec<_> = source.outputs(anchor).map(|item| item.index()).collect();
            consumers.sort_unstable();
            json!({
                "anchor":anchor.index(),
                "op":source.graph[anchor].to_string(),
                "inputs":source.inputs(anchor).map(|item| item.index()).collect::<Vec<_>>(),
                "consumers":consumers,
                "is_leaf":source.leaves.contains(&anchor),
                "is_root":source.roots.contains(&anchor),
                "is_constant":source.graph[anchor].is_constant(),
            })
        })
        .collect();
    write_json(output, &json!({"module":module,"occurrences":occurrences}))?;
    Ok(module)
}

#[derive(Clone)]
struct LocalRewriteSelection {
    candidate_id: String,
    output: PathBuf,
    sha256: String,
    point: PpaPoint,
    objective_value: f64,
    minimum_constraint_slack: f64,
}

fn select_local_rewrite_step(
    candidates: &[Value],
    objective: &SearchObjective,
    current: PpaPoint,
    visited: &BTreeSet<String>,
) -> Result<Option<LocalRewriteSelection>> {
    let mut eligible = Vec::new();
    for candidate in candidates {
        let point = PpaPoint {
            delay: candidate["point"]["delay"]
                .as_f64()
                .context("local rewrite delay")?,
            area: candidate["point"]["area"]
                .as_f64()
                .context("local rewrite area")?,
            power: candidate["point"]["power"]
                .as_f64()
                .context("local rewrite power")?,
        };
        if !objective.feasible_ppa(point.delay, point.area, point.power)
            || !objective.strictly_better(point, current)?
        {
            continue;
        }
        let candidate_sha = candidate["best_sha256"]
            .as_str()
            .context("local rewrite SHA")?
            .to_owned();
        if visited.contains(&candidate_sha) {
            continue;
        }
        eligible.push(LocalRewriteSelection {
            candidate_id: candidate["candidate_id"]
                .as_str()
                .context("local rewrite candidate ID")?
                .to_owned(),
            output: PathBuf::from(
                candidate["best_path"]
                    .as_str()
                    .context("local rewrite best path")?,
            ),
            sha256: candidate_sha,
            point,
            objective_value: objective.exact_value(point)?,
            minimum_constraint_slack: objective
                .relative_constraint_slacks(point)?
                .into_iter()
                .fold(f64::INFINITY, f64::min),
        });
    }
    eligible.sort_by(|left, right| {
        left.objective_value
            .total_cmp(&right.objective_value)
            .then_with(|| {
                right
                    .minimum_constraint_slack
                    .total_cmp(&left.minimum_constraint_slack)
            })
            .then_with(|| left.candidate_id.cmp(&right.candidate_id))
    });
    Ok(eligible.into_iter().next())
}

#[derive(Clone)]
struct CorrelatedObservation<'a> {
    candidate: &'a GeneratorCandidate,
    point: PpaPoint,
    objective_delta: f64,
    slack_delta: Vec<f64>,
}

fn topology_moves_compatible(left: &GeneratorCandidate, right: &GeneratorCandidate) -> bool {
    if specific_window(left) == specific_window(right) {
        return false;
    }
    let left_roots: BTreeSet<_> = left
        .choices
        .iter()
        .map(|choice| choice.root_anchor)
        .collect();
    let right_roots: BTreeSet<_> = right
        .choices
        .iter()
        .map(|choice| choice.root_anchor)
        .collect();
    left_roots.is_disjoint(&right_roots)
        && left_roots
            .iter()
            .all(|root| !right.proof_boundary.contains(root))
        && right_roots
            .iter()
            .all(|root| !left.proof_boundary.contains(root))
        && left
            .proof_outputs
            .iter()
            .all(|root| !right.proof_outputs.contains(root) && !right.proof_boundary.contains(root))
        && right
            .proof_outputs
            .iter()
            .all(|root| !left.proof_outputs.contains(root) && !left.proof_boundary.contains(root))
}

#[derive(Clone)]
struct CorrelatedSet {
    members: Vec<usize>,
    predicted_objective_delta: f64,
    slack_delta: Vec<f64>,
    predicted_violation: f64,
    predicted_min_slack: f64,
}

fn correlated_member_ids(
    set: &CorrelatedSet,
    observations: &[CorrelatedObservation<'_>],
) -> Vec<String> {
    set.members
        .iter()
        .map(|index| observations[*index].candidate.candidate_id.clone())
        .collect()
}

fn correlated_set_metrics(
    members: Vec<usize>,
    observations: &[CorrelatedObservation<'_>],
    parent_slacks: &[f64],
) -> CorrelatedSet {
    let predicted_objective_delta = members
        .iter()
        .map(|index| observations[*index].objective_delta)
        .sum();
    let mut slack_delta = vec![0.0; parent_slacks.len()];
    for member in &members {
        for (total, delta) in slack_delta
            .iter_mut()
            .zip(&observations[*member].slack_delta)
        {
            *total += delta;
        }
    }
    let predicted_slacks: Vec<_> = parent_slacks
        .iter()
        .zip(&slack_delta)
        .map(|(parent, delta)| parent + delta)
        .collect();
    let predicted_violation = predicted_slacks.iter().map(|slack| (-slack).max(0.0)).sum();
    let predicted_min_slack = predicted_slacks
        .iter()
        .copied()
        .fold(f64::INFINITY, f64::min);
    CorrelatedSet {
        members,
        predicted_objective_delta,
        slack_delta,
        predicted_violation,
        predicted_min_slack,
    }
}

fn set_accepts_member(
    set: &CorrelatedSet,
    member: usize,
    observations: &[CorrelatedObservation<'_>],
) -> bool {
    set.members.iter().all(|existing| {
        topology_moves_compatible(
            observations[*existing].candidate,
            observations[member].candidate,
        )
    })
}

fn objective_pressure_cmp(
    left: &CorrelatedSet,
    right: &CorrelatedSet,
    observations: &[CorrelatedObservation<'_>],
) -> std::cmp::Ordering {
    left.predicted_objective_delta
        .total_cmp(&right.predicted_objective_delta)
        .then_with(|| {
            left.predicted_violation
                .total_cmp(&right.predicted_violation)
        })
        .then_with(|| {
            right
                .predicted_min_slack
                .total_cmp(&left.predicted_min_slack)
        })
        .then_with(|| {
            correlated_member_ids(left, observations)
                .cmp(&correlated_member_ids(right, observations))
        })
}

fn feasibility_pressure_cmp(
    left: &CorrelatedSet,
    right: &CorrelatedSet,
    observations: &[CorrelatedObservation<'_>],
) -> std::cmp::Ordering {
    left.predicted_violation
        .total_cmp(&right.predicted_violation)
        .then_with(|| {
            left.predicted_objective_delta
                .total_cmp(&right.predicted_objective_delta)
        })
        .then_with(|| {
            right
                .predicted_min_slack
                .total_cmp(&left.predicted_min_slack)
        })
        .then_with(|| {
            correlated_member_ids(left, observations)
                .cmp(&correlated_member_ids(right, observations))
        })
}

/// Compose a fixed-size portfolio of mutually compatible topology moves.
/// Search pressure is derived exclusively from ObjectiveSpec objective and
/// constraint-slack deltas.  A deterministic dual-front beam preserves both
/// objective-improving and feasibility-restoring partial compositions without
/// naming a metric, transformation class, or benchmark.  Every selected set is
/// subsequently checked by the normal macro Boolean proof and Exact pipeline.
fn correlated_multi_root_candidates(
    candidates: &[GeneratorCandidate],
    materialized: &[materializer_impl::MaterializedCandidate],
    legal_receipts: &[&Value],
    p1_rows: &[Value],
    parent: PpaPoint,
    objective: &SearchObjective,
    max_compositions: usize,
) -> Result<(Vec<GeneratorCandidate>, Vec<Value>)> {
    // Frozen V7/D2AP replay is a compatibility path, not a programmable
    // ObjectiveSpec search.  Keeping it outside this new topology portfolio is
    // what makes D2AP-mode trajectory preservation an explicit hard gate.
    if objective.programmable_context().is_none() {
        return Ok((Vec::new(), Vec::new()));
    }
    let Some(active_band) = objective.active_constraint_slack_ratio() else {
        return Ok((Vec::new(), Vec::new()));
    };
    let parent_slacks = objective.relative_constraint_slacks(parent)?;
    anyhow::ensure!(materialized.len() == legal_receipts.len());
    anyhow::ensure!(materialized.len() == p1_rows.len());
    let by_id: HashMap<_, _> = candidates
        .iter()
        .map(|candidate| (candidate.candidate_id.as_str(), candidate))
        .collect();
    let parent_objective = objective.exact_value(parent)?;
    let mut observations = Vec::new();
    for ((materialized, receipt), row) in materialized.iter().zip(legal_receipts).zip(p1_rows) {
        if receipt["changed_eclasses"].as_u64().unwrap_or(0) == 0 {
            continue;
        }
        let candidate = by_id
            .get(materialized.candidate_id.as_str())
            .context("materialized correlated source missing from generator plan")?;
        let point = PpaPoint {
            delay: row["delay"]
                .as_f64()
                .context("correlated source P1 delay")?,
            area: row["area"].as_f64().context("correlated source P1 area")?,
            power: row["power"]
                .as_f64()
                .context("correlated source P1 power")?,
        };
        let slacks = objective.relative_constraint_slacks(point)?;
        let slack_delta: Vec<_> = slacks
            .iter()
            .zip(&parent_slacks)
            .map(|(candidate, parent)| candidate - parent)
            .collect();
        let objective_delta = objective.exact_value(point)? / parent_objective - 1.0;
        let improves_a_constraint = slack_delta.iter().any(|delta| *delta > 1e-9);
        if objective_delta > active_band && !improves_a_constraint {
            continue;
        }
        observations.push(CorrelatedObservation {
            candidate,
            point,
            objective_delta,
            slack_delta,
        });
    }

    const MAX_MEMBERS: usize = 4;
    const BEAM_WIDTH: usize = 96;
    let feasibility_floor = parent_slacks.iter().copied().fold(0.0_f64, f64::min) - active_band;
    let mut beam: Vec<_> = (0..observations.len())
        .map(|index| correlated_set_metrics(vec![index], &observations, &parent_slacks))
        .collect();
    let mut complete = Vec::new();
    for member_count in 2..=MAX_MEMBERS {
        let mut expanded = Vec::new();
        for set in &beam {
            let start = set.members.last().copied().unwrap_or(0) + 1;
            for member in start..observations.len() {
                if !set_accepts_member(set, member, &observations) {
                    continue;
                }
                let mut members = set.members.clone();
                members.push(member);
                let candidate = correlated_set_metrics(members, &observations, &parent_slacks);
                // Additive P1 prediction is a screening model only.  Keep a
                // fixed band around both objective and the current feasibility
                // frontier; macro proof and Exact remain authoritative.
                if candidate.predicted_objective_delta > active_band
                    || candidate.predicted_min_slack < feasibility_floor
                {
                    continue;
                }
                expanded.push(candidate);
            }
        }
        if expanded.is_empty() {
            break;
        }
        complete.extend(expanded.iter().cloned());

        let mut objective_front = expanded.clone();
        objective_front.sort_by(|left, right| objective_pressure_cmp(left, right, &observations));
        objective_front.truncate(BEAM_WIDTH / 2);
        expanded.sort_by(|left, right| feasibility_pressure_cmp(left, right, &observations));
        expanded.truncate(BEAM_WIDTH / 2);
        objective_front.extend(expanded);
        objective_front.sort_by(|left, right| {
            correlated_member_ids(left, &observations)
                .cmp(&correlated_member_ids(right, &observations))
        });
        objective_front.dedup_by(|left, right| left.members == right.members);
        beam = objective_front;
        debug_assert!(beam.iter().all(|set| set.members.len() == member_count));
    }

    let mut by_cardinality: BTreeMap<usize, Vec<CorrelatedSet>> = BTreeMap::new();
    for set in complete {
        by_cardinality
            .entry(set.members.len())
            .or_default()
            .push(set);
    }
    for bucket in by_cardinality.values_mut() {
        bucket.sort_by(|left, right| objective_pressure_cmp(left, right, &observations));
    }
    // Cardinality is a generic topology-diversity axis.  Round-robin filling
    // prevents a large family of speculative high-order compositions from
    // crowding all simpler, more often materializable pairs out of the same
    // fixed 24-candidate portfolio.
    let mut complete = Vec::new();
    let mut rank = 0usize;
    while complete.len() < max_compositions {
        let before = complete.len();
        for member_count in 2..=MAX_MEMBERS {
            if let Some(set) = by_cardinality
                .get(&member_count)
                .and_then(|bucket| bucket.get(rank))
            {
                complete.push(set.clone());
                if complete.len() == max_compositions {
                    break;
                }
            }
        }
        if complete.len() == before {
            break;
        }
        rank += 1;
    }
    let mut compositions = Vec::new();
    let mut audit = Vec::new();
    let mut seen_topologies = BTreeSet::new();
    for set in complete {
        if compositions.len() == max_compositions {
            break;
        }
        let members: Vec<_> = set
            .members
            .iter()
            .map(|index| &observations[*index])
            .collect();
        let mut choices: Vec<_> = members
            .iter()
            .flat_map(|member| member.candidate.choices.clone())
            .collect();
        choices.sort_by_key(|choice| choice.root_anchor);
        let roots: BTreeSet<_> = choices.iter().map(|choice| choice.root_anchor).collect();
        let mut proof_boundary: Vec<_> = members
            .iter()
            .flat_map(|member| member.candidate.proof_boundary.iter().copied())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        proof_boundary.retain(|anchor| !roots.contains(anchor));
        if proof_boundary.len() > 16 {
            continue;
        }
        let mut provenance = vec!["objective-correlated-multi-root".to_owned()];
        provenance.extend(
            members
                .iter()
                .flat_map(|member| member.candidate.provenance.clone()),
        );
        let mut cell_families: Vec<_> = members
            .iter()
            .flat_map(|member| member.candidate.cell_families.clone())
            .collect();
        cell_families.sort();
        cell_families.dedup();
        let mut source_windows: Vec<_> = members
            .iter()
            .flat_map(|member| member.candidate.source_windows.clone())
            .collect();
        source_windows.sort();
        source_windows.dedup();
        let ordinal = compositions.len();
        let mut composition = GeneratorCandidate {
            candidate_id: format!("V8CORR_{ordinal:04}"),
            provenance,
            choices,
            tech_area: members
                .iter()
                .map(|member| member.candidate.tech_area)
                .sum(),
            tech_delay: members
                .iter()
                .map(|member| member.candidate.tech_delay)
                .fold(0.0_f64, f64::max),
            logic_depth: members
                .iter()
                .map(|member| member.candidate.logic_depth)
                .max()
                .unwrap_or(0),
            cell_families,
            outputs: members
                .iter()
                .flat_map(|member| member.candidate.outputs.iter().copied())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            window_kind: "correlated-multi-root".to_owned(),
            proof_boundary,
            proof_outputs: members
                .iter()
                .flat_map(|member| member.candidate.proof_outputs.iter().copied())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            proof_groups: Vec::new(),
            shared_host: None,
            divisors: members
                .iter()
                .flat_map(|member| member.candidate.divisors.iter().copied())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            replaced_anchor: None,
            logical_signature: None,
            mapping: Some("correlated".to_owned()),
            source_windows,
        };
        let signature = topology_signature(&composition);
        if !seen_topologies.insert(signature.clone()) {
            continue;
        }
        composition.logical_signature = Some(signature);
        audit.push(json!({
            "candidate_id":composition.candidate_id,
            "member_count":members.len(),
            "members":members.iter().map(|member| json!({
                "candidate_id":member.candidate.candidate_id,
                "point":member.point,
                "objective_delta":member.objective_delta,
                "constraint_slack_delta":member.slack_delta,
            })).collect::<Vec<_>>(),
            "predicted_objective_delta":set.predicted_objective_delta,
            "predicted_constraint_slack_delta":set.slack_delta,
            "predicted_total_constraint_violation":set.predicted_violation,
            "predicted_minimum_constraint_slack":set.predicted_min_slack,
            "proof_boundary_size":composition.proof_boundary.len(),
        }));
        compositions.push(composition);
    }
    Ok((compositions, audit))
}

#[derive(Clone, Copy)]
enum LocalClosureOrder {
    Objective,
    Feasibility,
    PrimalDual,
    Delay,
    Area,
    Power,
    DelayArea,
    DelayPower,
    AreaPower,
    CriticalityAware,
}

#[derive(Clone, Copy)]
enum LocalClosureScope {
    All,
    MappedWindow,
}

impl LocalClosureScope {
    fn accepts(self, candidate: &GeneratorCandidate) -> bool {
        match self {
            Self::All => true,
            Self::MappedWindow => candidate.mapping.as_deref() == Some("mapped-kfeasible-window"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::All => "all-proof-carrying-transformations",
            Self::MappedWindow => "mapped-window-recipe-transformations",
        }
    }
}

impl LocalClosureOrder {
    fn name(self) -> &'static str {
        match self {
            Self::Objective => "objective-pressure",
            Self::Feasibility => "feasibility-reserve",
            Self::PrimalDual => "primal-dual",
            Self::Delay => "delay-first",
            Self::Area => "area-first",
            Self::Power => "power-first",
            Self::DelayArea => "delay-area",
            Self::DelayPower => "delay-power",
            Self::AreaPower => "area-power",
            Self::CriticalityAware => "partition-criticality-aware",
        }
    }
}

fn closure_partition_key(candidate: &GeneratorCandidate) -> String {
    specific_window(candidate)
        .split('_')
        .find(|token| {
            token.strip_prefix('S').is_some_and(|digits| {
                !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
            })
        })
        .map(str::to_owned)
        .unwrap_or_else(|| "GLOBAL".to_owned())
}

fn closure_point_dominates(left: PpaPoint, right: PpaPoint) -> bool {
    let tolerance = |value: f64| 1e-12 * value.abs().max(1.0);
    let no_worse = left.delay <= right.delay + tolerance(right.delay)
        && left.area <= right.area + tolerance(right.area)
        && left.power <= right.power + tolerance(right.power);
    let strictly_better = left.delay < right.delay - tolerance(right.delay)
        || left.area < right.area - tolerance(right.area)
        || left.power < right.power - tolerance(right.power);
    no_worse && strictly_better
}

fn partition_pareto_indices(observations: &[CorrelatedObservation<'_>]) -> BTreeSet<usize> {
    let mut partitions: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, observation) in observations.iter().enumerate() {
        partitions
            .entry(closure_partition_key(observation.candidate))
            .or_default()
            .push(index);
    }
    partitions
        .values()
        .flat_map(|members| {
            members.iter().copied().filter(|candidate| {
                !members.iter().copied().any(|other| {
                    other != *candidate
                        && closure_point_dominates(
                            observations[other].point,
                            observations[*candidate].point,
                        )
                })
            })
        })
        .collect()
}

fn partition_delay_sensitivity(
    observations: &[CorrelatedObservation<'_>],
    parent: PpaPoint,
) -> BTreeMap<String, f64> {
    let mut ranges: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    for observation in observations {
        let entry = ranges
            .entry(closure_partition_key(observation.candidate))
            .or_insert((observation.point.delay, observation.point.delay));
        entry.0 = entry.0.min(observation.point.delay);
        entry.1 = entry.1.max(observation.point.delay);
    }
    ranges
        .into_iter()
        .map(|(partition, (minimum, maximum))| {
            // A region whose alternatives visibly move whole-net delay is
            // treated as timing-sensitive.  A 0.2% span reaches full timing
            // pressure; flat/off-critical regions retain area pressure.
            let span = ((maximum - minimum) / parent.delay.max(1e-30)).max(0.0);
            (partition, (span / 0.002).clamp(0.0, 1.0))
        })
        .collect()
}

fn partition_criticality_score(
    observation: &CorrelatedObservation<'_>,
    parent: PpaPoint,
    sensitivities: &BTreeMap<String, f64>,
) -> f64 {
    let criticality = sensitivities
        .get(&closure_partition_key(observation.candidate))
        .copied()
        .unwrap_or(0.0);
    let delay_weight = 0.25 + 2.75 * criticality;
    let area_weight = 2.0 - criticality;
    let power_weight = 0.5;
    delay_weight * (observation.point.delay / parent.delay).ln()
        + area_weight * (observation.point.area / parent.area).ln()
        + power_weight * (observation.point.power / parent.power).ln()
}

fn partition_set_metric(
    set: &CorrelatedSet,
    observations: &[CorrelatedObservation<'_>],
    parent: PpaPoint,
    sensitivities: &BTreeMap<String, f64>,
    order: LocalClosureOrder,
) -> f64 {
    set.members
        .iter()
        .map(|member| match order {
            LocalClosureOrder::Delay => (observations[*member].point.delay / parent.delay).ln(),
            LocalClosureOrder::Area => (observations[*member].point.area / parent.area).ln(),
            LocalClosureOrder::Power => (observations[*member].point.power / parent.power).ln(),
            LocalClosureOrder::DelayArea => {
                2.0 * (observations[*member].point.delay / parent.delay).ln()
                    + (observations[*member].point.area / parent.area).ln()
            }
            LocalClosureOrder::DelayPower => {
                2.0 * (observations[*member].point.delay / parent.delay).ln()
                    + (observations[*member].point.power / parent.power).ln()
            }
            LocalClosureOrder::AreaPower => {
                (observations[*member].point.area / parent.area).ln()
                    + (observations[*member].point.power / parent.power).ln()
            }
            LocalClosureOrder::CriticalityAware => {
                partition_criticality_score(&observations[*member], parent, sensitivities)
            }
            _ => observations[*member].objective_delta,
        })
        .sum()
}

/// Explore one optional realization per planner seed partition.  Unlike the
/// four historical greedy orders, this retains partial assignments at every
/// cardinality, so a locally second-best realization can survive when it is
/// the one compatible with a better downstream assignment.
fn partition_pareto_beam_sets(
    observations: &[CorrelatedObservation<'_>],
    eligible_indices: &BTreeSet<usize>,
    parent_slacks: &[f64],
    parent: PpaPoint,
    sensitivities: &BTreeMap<String, f64>,
    active_band: f64,
    feasibility_floor: f64,
    max_transactions: usize,
    max_closures: usize,
    beam_width: usize,
    per_partition_cap: usize,
    dap_diverse_pool: bool,
    minimum_dap_distance: f64,
    combination_diversity: bool,
) -> Vec<(LocalClosureOrder, CorrelatedSet)> {
    let policies = [
        LocalClosureOrder::CriticalityAware,
        LocalClosureOrder::Delay,
        LocalClosureOrder::Area,
        LocalClosureOrder::Power,
        LocalClosureOrder::DelayArea,
        LocalClosureOrder::DelayPower,
        LocalClosureOrder::AreaPower,
        LocalClosureOrder::Objective,
    ];
    let mut partitions: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for index in eligible_indices {
        partitions
            .entry(closure_partition_key(observations[*index].candidate))
            .or_default()
            .push(*index);
    }
    for members in partitions.values_mut() {
        if members.len() <= per_partition_cap && !dap_diverse_pool {
            continue;
        }
        let mut fronts = Vec::new();
        for policy in policies {
            let mut order = members.clone();
            order.sort_by(|left, right| {
                let left_set = correlated_set_metrics(vec![*left], observations, parent_slacks);
                let right_set = correlated_set_metrics(vec![*right], observations, parent_slacks);
                partition_set_metric(&left_set, observations, parent, sensitivities, policy)
                    .total_cmp(&partition_set_metric(
                        &right_set,
                        observations,
                        parent,
                        sensitivities,
                        policy,
                    ))
                    .then_with(|| objective_pressure_cmp(&left_set, &right_set, observations))
            });
            fronts.push(order);
        }
        let mut selected = Vec::new();
        // Seed the local portfolio with the best implementation under each
        // physically meaningful projection.  When the opt-in DAP-diverse
        // lane is active, fill the remainder by normalized farthest-point
        // sampling.  This admits a few dominated single-region states only
        // when they represent a genuinely different D/A/P tradeoff that may
        // interact differently after another region changes load or path.
        for front in &fronts {
            if let Some(member) = front.first() {
                if !selected.contains(member) {
                    selected.push(*member);
                }
            }
            if selected.len() == per_partition_cap {
                break;
            }
        }
        if dap_diverse_pool && selected.len() < per_partition_cap {
            let point = |member: usize| {
                [
                    (observations[member].point.delay / parent.delay).ln(),
                    (observations[member].point.area / parent.area).ln(),
                    (observations[member].point.power / parent.power).ln(),
                ]
            };
            let mut minimum = [f64::INFINITY; 3];
            let mut maximum = [f64::NEG_INFINITY; 3];
            for member in members.iter().copied() {
                for (axis, value) in point(member).into_iter().enumerate() {
                    minimum[axis] = minimum[axis].min(value);
                    maximum[axis] = maximum[axis].max(value);
                }
            }
            let distance = |left: usize, right: usize| {
                let left = point(left);
                let right = point(right);
                left.into_iter()
                    .zip(right)
                    .enumerate()
                    .map(|(axis, (a, b))| {
                        let span = (maximum[axis] - minimum[axis]).max(1e-12);
                        ((a - b) / span).powi(2)
                    })
                    .sum::<f64>()
                    .sqrt()
            };
            while selected.len() < per_partition_cap {
                let next = members
                    .iter()
                    .copied()
                    .filter(|member| !selected.contains(member))
                    .filter(|member| {
                        selected.iter().all(|prior| {
                            let raw = point(*member)
                                .into_iter()
                                .zip(point(*prior))
                                .map(|(a, b)| (a - b).powi(2))
                                .sum::<f64>()
                                .sqrt();
                            raw >= minimum_dap_distance
                        })
                    })
                    .max_by(|left, right| {
                        let left_distance = selected
                            .iter()
                            .map(|prior| distance(*left, *prior))
                            .fold(f64::INFINITY, f64::min);
                        let right_distance = selected
                            .iter()
                            .map(|prior| distance(*right, *prior))
                            .fold(f64::INFINITY, f64::min);
                        left_distance.total_cmp(&right_distance).then_with(|| {
                            observations[*right]
                                .objective_delta
                                .total_cmp(&observations[*left].objective_delta)
                        })
                    });
                let Some(next) = next else { break };
                selected.push(next);
            }
        } else {
            let mut rank = 1;
            while selected.len() < per_partition_cap {
                let before = selected.len();
                for front in &fronts {
                    if let Some(member) = front.get(rank) {
                        if !selected.contains(member) {
                            selected.push(*member);
                            if selected.len() == per_partition_cap {
                                break;
                            }
                        }
                    }
                }
                if selected.len() == before {
                    break;
                }
                rank += 1;
            }
        }
        *members = selected;
    }

    let empty = correlated_set_metrics(Vec::new(), observations, parent_slacks);
    let mut beam = vec![empty];
    for members in partitions.values() {
        let mut expanded = Vec::new();
        for set in &beam {
            expanded.push(set.clone());
            if set.members.len() == max_transactions {
                continue;
            }
            for member in members {
                if !set_accepts_member(set, *member, observations) {
                    continue;
                }
                let mut next_members = set.members.clone();
                next_members.push(*member);
                next_members.sort_unstable();
                let next = correlated_set_metrics(next_members, observations, parent_slacks);
                if next.predicted_objective_delta <= active_band
                    && next.predicted_min_slack >= feasibility_floor
                {
                    expanded.push(next);
                }
            }
        }
        expanded.sort_by(|left, right| left.members.cmp(&right.members));
        expanded.dedup_by(|left, right| left.members == right.members);

        // Preserve different numbers of participating partitions.  Without
        // this bucket, a wide high-cardinality family can erase all pairs and
        // triples before their actual full-net interaction is evaluated.
        let mut by_cardinality: BTreeMap<usize, Vec<CorrelatedSet>> = BTreeMap::new();
        for set in expanded {
            by_cardinality
                .entry(set.members.len())
                .or_default()
                .push(set);
        }
        beam.clear();
        for bucket in by_cardinality.values_mut() {
            let mut kept = Vec::new();
            let per_front = beam_width.div_ceil(policies.len()).max(1);
            for policy in policies {
                let mut front = bucket.clone();
                front.sort_by(|left, right| {
                    partition_set_metric(left, observations, parent, sensitivities, policy)
                        .total_cmp(&partition_set_metric(
                            right,
                            observations,
                            parent,
                            sensitivities,
                            policy,
                        ))
                        .then_with(|| objective_pressure_cmp(left, right, observations))
                });
                kept.extend(front.into_iter().take(per_front));
            }
            kept.sort_by(|left, right| left.members.cmp(&right.members));
            kept.dedup_by(|left, right| left.members == right.members);
            kept.sort_by(|left, right| objective_pressure_cmp(left, right, observations));
            kept.truncate(beam_width);
            beam.append(&mut kept);
        }
    }

    let mut ranked = Vec::new();
    for cardinality in 2..=max_transactions {
        for policy in policies {
            let mut front: Vec<_> = beam
                .iter()
                .filter(|set| set.members.len() == cardinality)
                .cloned()
                .collect();
            front.sort_by(|left, right| {
                partition_set_metric(left, observations, parent, sensitivities, policy)
                    .total_cmp(&partition_set_metric(
                        right,
                        observations,
                        parent,
                        sensitivities,
                        policy,
                    ))
                    .then_with(|| objective_pressure_cmp(left, right, observations))
            });
            ranked.push((policy, front));
        }
    }
    let ranked_target = if combination_diversity {
        max_closures.div_ceil(2)
    } else {
        max_closures
    };
    let mut selected = Vec::new();
    let mut seen = BTreeSet::new();
    let mut rank = 0;
    while selected.len() < ranked_target {
        let before = selected.len();
        for (policy, front) in &ranked {
            if let Some(set) = front.get(rank) {
                if seen.insert(set.members.clone()) {
                    selected.push((*policy, set.clone()));
                    if selected.len() == ranked_target {
                        break;
                    }
                }
            }
        }
        if selected.len() == before {
            break;
        }
        rank += 1;
    }
    if combination_diversity && selected.len() < max_closures {
        let set_distance = |left: &CorrelatedSet, right: &CorrelatedSet| {
            let left: BTreeSet<_> = left.members.iter().copied().collect();
            let right: BTreeSet<_> = right.members.iter().copied().collect();
            let union = left.union(&right).count().max(1);
            left.symmetric_difference(&right).count() as f64 / union as f64
        };
        let mut pool: Vec<_> = beam
            .iter()
            .filter(|set| set.members.len() >= 2 && !seen.contains(&set.members))
            .cloned()
            .collect();
        while selected.len() < max_closures && !pool.is_empty() {
            let next = pool
                .iter()
                .enumerate()
                .max_by(|(_, left), (_, right)| {
                    let left_distance = selected
                        .iter()
                        .map(|(_, prior)| set_distance(left, prior))
                        .fold(f64::INFINITY, f64::min);
                    let right_distance = selected
                        .iter()
                        .map(|(_, prior)| set_distance(right, prior))
                        .fold(f64::INFINITY, f64::min);
                    left_distance.total_cmp(&right_distance).then_with(|| {
                        right
                            .predicted_objective_delta
                            .total_cmp(&left.predicted_objective_delta)
                    })
                })
                .map(|(index, _)| index);
            let Some(next) = next else { break };
            let set = pool.swap_remove(next);
            seen.insert(set.members.clone());
            selected.push((LocalClosureOrder::Objective, set));
        }
    }
    selected
}

/// Pack a deterministic maximal set of compatible local transformations into
/// a small whole-net portfolio.  This is the PMO-style local-closure lane: it
/// spends one downstream topology slot on many individually proved rewrites,
/// instead of forcing every micro rewrite to compete for its own Top-32 slot.
///
/// All ordering and admission pressure comes from ObjectiveSpec objective,
/// constraint slack and the common primal-dual merit.  No metric name,
/// transformation provenance or benchmark name is inspected.  The frozen
/// D2AP path returns before constructing observations, proof groups or IDs;
/// only the opt-in Iterative screening band enables D2AP closure construction.
// Opt-in scheduling cleanup: omit whole proof groups whose selected roots are
// no longer reachable after simultaneous replacement. Never weaken the
// materializer's proof or reachability checks; they run again on the subset.
fn prune_unreachable_closure_groups(
    candidate: &mut GeneratorCandidate,
    graph: &Value,
) -> Result<Value> {
    use d1_series::generator_v2::GeneratorExpr;
    fn anchors(expr: &GeneratorExpr, out: &mut Vec<usize>) {
        match expr {
            GeneratorExpr::Anchor { anchor } => out.push(*anchor),
            GeneratorExpr::Cell { children, .. } => {
                for child in children {
                    anchors(child, out);
                }
            }
        }
    }
    fn find_cycle(
        node: usize,
        edges: &BTreeMap<usize, Vec<usize>>,
        colors: &mut BTreeMap<usize, u8>,
        path: &mut Vec<usize>,
    ) -> Option<Vec<usize>> {
        match colors.get(&node).copied().unwrap_or(0) {
            1 => return Some(path[path.iter().position(|x| *x == node).unwrap()..].to_vec()),
            2 => return None,
            _ => (),
        }
        colors.insert(node, 1);
        path.push(node);
        if let Some(children) = edges.get(&node) {
            for child in children {
                if let Some(cycle) = find_cycle(*child, edges, colors, path) {
                    return Some(cycle);
                }
            }
        }
        path.pop();
        colors.insert(node, 2);
        None
    }
    let rows = graph["occurrences"]
        .as_array()
        .context("missing occurrences")?;
    let mut inputs = BTreeMap::new();
    let mut roots = Vec::new();
    for row in rows {
        let anchor = row["anchor"].as_u64().context("missing anchor")? as usize;
        inputs.insert(
            anchor,
            serde_json::from_value::<Vec<usize>>(row["inputs"].clone())?,
        );
        if row["is_root"].as_bool() == Some(true) {
            roots.push(anchor);
        }
    }
    anyhow::ensure!(!roots.is_empty(), "source has no roots");
    let before = candidate.choices.len();
    let mut removed = Vec::new();
    let mut cycles_removed = Vec::new();
    loop {
        let replacements: BTreeMap<_, _> = candidate
            .choices
            .iter()
            .map(|c| (c.root_anchor, &c.expression))
            .collect();
        let mut edges = inputs.clone();
        for choice in &candidate.choices {
            let mut refs = Vec::new();
            anchors(&choice.expression, &mut refs);
            edges.insert(choice.root_anchor, refs);
        }
        let mut colors = BTreeMap::new();
        let mut path = Vec::new();
        let cycle = roots
            .iter()
            .find_map(|root| find_cycle(*root, &edges, &mut colors, &mut path));
        if let Some(cycle) = cycle {
            let group = candidate
                .proof_groups
                .iter()
                .rev()
                .find(|g| g.choices.iter().any(|c| cycle.contains(&c.root_anchor)))
                .context("dependency cycle has no removable proof group")?;
            let rejected: BTreeSet<_> = group.choices.iter().map(|c| c.root_anchor).collect();
            cycles_removed.push(json!({"cycle":cycle,"removed_roots":rejected}));
            candidate
                .proof_groups
                .retain(|g| g.choices.iter().all(|c| !rejected.contains(&c.root_anchor)));
            candidate
                .choices
                .retain(|c| !rejected.contains(&c.root_anchor));
            removed.extend(rejected);
            continue;
        }
        let mut stack = roots.clone();
        let mut reachable = BTreeSet::new();
        while let Some(anchor) = stack.pop() {
            if !reachable.insert(anchor) {
                continue;
            }
            if let Some(expr) = replacements.get(&anchor) {
                anchors(expr, &mut stack);
            } else {
                stack.extend(
                    inputs
                        .get(&anchor)
                        .context("unknown source anchor")?
                        .iter()
                        .copied(),
                );
            }
        }
        let rejected: BTreeSet<_> = candidate
            .proof_groups
            .iter()
            .filter(|g| {
                g.choices
                    .iter()
                    .any(|c| !reachable.contains(&c.root_anchor))
            })
            .flat_map(|g| g.choices.iter().map(|c| c.root_anchor))
            .collect();
        if rejected.is_empty() {
            break;
        }
        candidate
            .proof_groups
            .retain(|g| g.choices.iter().all(|c| !rejected.contains(&c.root_anchor)));
        candidate
            .choices
            .retain(|c| !rejected.contains(&c.root_anchor));
        removed.extend(rejected);
    }
    if !removed.is_empty() {
        candidate.outputs = candidate.choices.iter().map(|c| c.root_anchor).collect();
        candidate.proof_outputs = candidate
            .proof_groups
            .iter()
            .flat_map(|g| g.outputs.iter().copied())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        candidate.logical_signature = Some(topology_signature(candidate));
    }
    Ok(
        json!({"candidate_id":candidate.candidate_id,"choices_before":before,"choices_after":candidate.choices.len(),"removed_roots":removed,"cycles_removed":cycles_removed,"whole_proof_groups_only":true}),
    )
}

fn objective_local_closure_candidates(
    candidates: &[GeneratorCandidate],
    materialized: &[materializer_impl::MaterializedCandidate],
    legal_receipts: &[&Value],
    p1_rows: &[Value],
    parent: PpaPoint,
    objective: &SearchObjective,
    max_closures: usize,
    max_transactions: usize,
    scope: LocalClosureScope,
    d2ap_screening_band: Option<f64>,
) -> Result<(Vec<GeneratorCandidate>, Vec<Value>)> {
    let d2ap_closure = objective.programmable_context().is_none();
    let partition_pareto = d2ap_closure
        && std::env::var("EGG_PARTITION_PARETO_CLOSURE")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
    if d2ap_closure && d2ap_screening_band.is_none() {
        return Ok((Vec::new(), Vec::new()));
    }
    anyhow::ensure!(materialized.len() == legal_receipts.len());
    anyhow::ensure!(materialized.len() == p1_rows.len());
    let parent_slacks = if d2ap_closure {
        Vec::new()
    } else {
        objective.relative_constraint_slacks(parent)?
    };
    let active_band = d2ap_screening_band
        .or_else(|| objective.active_constraint_slack_ratio())
        .unwrap_or(0.05);
    let feasibility_floor = parent_slacks.iter().copied().fold(0.0_f64, f64::min) - active_band;
    let by_id: HashMap<_, _> = candidates
        .iter()
        .map(|candidate| (candidate.candidate_id.as_str(), candidate))
        .collect();
    let parent_objective = objective.exact_value(parent)?;
    anyhow::ensure!(parent_objective.is_finite() && parent_objective > 0.0);
    let mut observations = Vec::new();
    for ((materialized, receipt), row) in materialized.iter().zip(legal_receipts).zip(p1_rows) {
        if receipt["changed_eclasses"].as_u64().unwrap_or(0) == 0 {
            continue;
        }
        let candidate = by_id
            .get(materialized.candidate_id.as_str())
            .context("materialized closure source missing from generator plan")?;
        if !scope.accepts(candidate) {
            continue;
        }
        let point = PpaPoint {
            delay: row["delay"].as_f64().context("closure source P1 delay")?,
            area: row["area"].as_f64().context("closure source P1 area")?,
            power: row["power"].as_f64().context("closure source P1 power")?,
        };
        let slacks = if d2ap_closure {
            Vec::new()
        } else {
            objective.relative_constraint_slacks(point)?
        };
        let slack_delta: Vec<_> = slacks
            .iter()
            .zip(&parent_slacks)
            .map(|(candidate, parent)| candidate - parent)
            .collect();
        let objective_delta = objective.exact_value(point)? / parent_objective - 1.0;
        let improves_a_constraint = slack_delta.iter().any(|delta| *delta > 1e-9);
        if objective_delta > active_band && !improves_a_constraint {
            continue;
        }
        observations.push(CorrelatedObservation {
            candidate,
            point,
            objective_delta,
            slack_delta,
        });
    }
    if observations.len() < 2 {
        return Ok((Vec::new(), Vec::new()));
    }

    let standalone = |index| correlated_set_metrics(vec![index], &observations, &parent_slacks);
    let dap_diverse_pool = partition_pareto
        && std::env::var("EGG_PARTITION_DAP_DIVERSE_POOL")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
    let eligible_indices = if partition_pareto && !dap_diverse_pool {
        partition_pareto_indices(&observations)
    } else {
        (0..observations.len()).collect()
    };
    let sensitivities = partition_delay_sensitivity(&observations, parent);
    let partition_beam_width = if partition_pareto {
        std::env::var("EGG_PARTITION_CLOSURE_BEAM_WIDTH")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0)
    } else {
        0
    };
    let partition_per_group_cap = std::env::var("EGG_PARTITION_PARETO_PER_GROUP")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(usize::MAX)
        .max(1);
    let minimum_dap_distance = std::env::var("EGG_PARTITION_DAP_MIN_DISTANCE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .unwrap_or(1e-5);
    let combination_diversity = partition_pareto
        && std::env::var("EGG_PARTITION_COMBINATION_DIVERSITY")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
    let policies = if partition_pareto {
        vec![
            LocalClosureOrder::CriticalityAware,
            LocalClosureOrder::Delay,
            LocalClosureOrder::Area,
            LocalClosureOrder::Objective,
        ]
    } else if d2ap_closure {
        vec![LocalClosureOrder::Objective]
    } else {
        vec![
            LocalClosureOrder::Objective,
            LocalClosureOrder::Feasibility,
            LocalClosureOrder::PrimalDual,
        ]
    };
    let mut base_orders = Vec::new();
    for policy in policies {
        let mut order: Vec<_> = eligible_indices.iter().copied().collect();
        order.sort_by(|left, right| {
            let left_set = standalone(*left);
            let right_set = standalone(*right);
            let base = match policy {
                LocalClosureOrder::Objective => {
                    objective_pressure_cmp(&left_set, &right_set, &observations)
                }
                LocalClosureOrder::Feasibility => {
                    feasibility_pressure_cmp(&left_set, &right_set, &observations)
                }
                LocalClosureOrder::PrimalDual => objective
                    .score(
                        observations[*left].point.delay,
                        observations[*left].point.area,
                        observations[*left].point.power,
                    )
                    .total_cmp(&objective.score(
                        observations[*right].point.delay,
                        observations[*right].point.area,
                        observations[*right].point.power,
                    ))
                    .then_with(|| objective_pressure_cmp(&left_set, &right_set, &observations)),
                LocalClosureOrder::Delay => observations[*left]
                    .point
                    .delay
                    .total_cmp(&observations[*right].point.delay)
                    .then_with(|| objective_pressure_cmp(&left_set, &right_set, &observations)),
                LocalClosureOrder::Area => observations[*left]
                    .point
                    .area
                    .total_cmp(&observations[*right].point.area)
                    .then_with(|| objective_pressure_cmp(&left_set, &right_set, &observations)),
                LocalClosureOrder::Power => observations[*left]
                    .point
                    .power
                    .total_cmp(&observations[*right].point.power)
                    .then_with(|| objective_pressure_cmp(&left_set, &right_set, &observations)),
                LocalClosureOrder::DelayArea => (2.0
                    * (observations[*left].point.delay / parent.delay).ln()
                    + (observations[*left].point.area / parent.area).ln())
                .total_cmp(
                    &(2.0 * (observations[*right].point.delay / parent.delay).ln()
                        + (observations[*right].point.area / parent.area).ln()),
                )
                .then_with(|| objective_pressure_cmp(&left_set, &right_set, &observations)),
                LocalClosureOrder::DelayPower => (2.0
                    * (observations[*left].point.delay / parent.delay).ln()
                    + (observations[*left].point.power / parent.power).ln())
                .total_cmp(
                    &(2.0 * (observations[*right].point.delay / parent.delay).ln()
                        + (observations[*right].point.power / parent.power).ln()),
                )
                .then_with(|| objective_pressure_cmp(&left_set, &right_set, &observations)),
                LocalClosureOrder::AreaPower => ((observations[*left].point.area / parent.area)
                    .ln()
                    + (observations[*left].point.power / parent.power).ln())
                .total_cmp(
                    &((observations[*right].point.area / parent.area).ln()
                        + (observations[*right].point.power / parent.power).ln()),
                )
                .then_with(|| objective_pressure_cmp(&left_set, &right_set, &observations)),
                LocalClosureOrder::CriticalityAware => {
                    partition_criticality_score(&observations[*left], parent, &sensitivities)
                        .total_cmp(&partition_criticality_score(
                            &observations[*right],
                            parent,
                            &sensitivities,
                        ))
                        .then_with(|| objective_pressure_cmp(&left_set, &right_set, &observations))
                }
            };
            base.then_with(|| {
                observations[*left]
                    .candidate
                    .candidate_id
                    .cmp(&observations[*right].candidate.candidate_id)
            })
        });
        base_orders.push((policy, order));
    }
    let mut orders = Vec::new();
    if d2ap_closure {
        let mut transaction_caps = vec![2, (max_transactions + 1) / 2, max_transactions];
        transaction_caps.sort_unstable();
        transaction_caps.dedup();
        for (policy, base_order) in base_orders {
            for transaction_cap in &transaction_caps {
                if *transaction_cap <= max_transactions {
                    orders.push((policy, base_order.clone(), *transaction_cap));
                }
            }
        }
    } else {
        orders.extend(
            base_orders
                .into_iter()
                .map(|(policy, order)| (policy, order, max_transactions)),
        );
    }

    let beam_selected = (partition_beam_width > 0).then(|| {
        partition_pareto_beam_sets(
            &observations,
            &eligible_indices,
            &parent_slacks,
            parent,
            &sensitivities,
            active_band,
            feasibility_floor,
            max_transactions,
            max_closures,
            partition_beam_width,
            partition_per_group_cap,
            dap_diverse_pool,
            minimum_dap_distance,
            combination_diversity,
        )
    });
    let mut closures = Vec::new();
    let mut audit = Vec::new();
    let mut seen_members = BTreeSet::new();
    let mut seen_topologies = BTreeSet::new();
    let mut selected_sets = Vec::new();
    if let Some(beam_selected) = beam_selected {
        selected_sets = beam_selected;
    } else {
        for (policy, order, transaction_cap) in orders {
            let mut selected = CorrelatedSet {
                members: Vec::new(),
                predicted_objective_delta: 0.0,
                slack_delta: vec![0.0; parent_slacks.len()],
                predicted_violation: parent_slacks.iter().map(|slack| (-slack).max(0.0)).sum(),
                predicted_min_slack: parent_slacks.iter().copied().fold(f64::INFINITY, f64::min),
            };
            for member in order {
                if partition_pareto {
                    let member_partition = closure_partition_key(observations[member].candidate);
                    if selected.members.iter().any(|existing| {
                        closure_partition_key(observations[*existing].candidate) == member_partition
                    }) {
                        continue;
                    }
                }
                if selected.members.len() == transaction_cap
                    || !set_accepts_member(&selected, member, &observations)
                {
                    continue;
                }
                let mut members = selected.members.clone();
                members.push(member);
                members.sort_unstable();
                let proposal = correlated_set_metrics(members, &observations, &parent_slacks);
                if proposal.predicted_objective_delta > active_band
                    || proposal.predicted_min_slack < feasibility_floor
                {
                    continue;
                }
                selected = proposal;
            }
            if selected.members.len() >= 2 {
                selected_sets.push((policy, selected));
            }
        }
    }
    for (policy, selected) in selected_sets {
        if !seen_members.insert(selected.members.clone()) {
            continue;
        }
        let members: Vec<_> = selected
            .members
            .iter()
            .map(|index| &observations[*index])
            .collect();
        let mut choices: Vec<_> = members
            .iter()
            .flat_map(|member| member.candidate.choices.clone())
            .collect();
        choices.sort_by_key(|choice| choice.root_anchor);
        if choices
            .windows(2)
            .any(|pair| pair[0].root_anchor == pair[1].root_anchor)
        {
            continue;
        }
        let proof_groups: Vec<_> = members
            .iter()
            .flat_map(|member| {
                if member.candidate.proof_groups.is_empty() {
                    vec![GeneratorProofGroup {
                        choices: member.candidate.choices.clone(),
                        boundary: member.candidate.proof_boundary.clone(),
                        outputs: member.candidate.proof_outputs.clone(),
                    }]
                } else {
                    member.candidate.proof_groups.clone()
                }
            })
            .collect();
        let provenance_class = if d2ap_closure {
            "iterative-d2ap-local-closure"
        } else {
            "objective-guided-local-closure"
        };
        // Preserve the pre-existing PMO mapping label exactly. Iterative uses a
        // separate label so this opt-in lane cannot alter historical reports.
        let mapping_class = if d2ap_closure {
            "iterative-d2ap-local-closure"
        } else {
            "objective-local-closure"
        };
        let mut provenance = vec![provenance_class.to_owned(), policy.name().to_owned()];
        provenance.extend(
            members
                .iter()
                .flat_map(|member| member.candidate.provenance.clone()),
        );
        let mut cell_families: Vec<_> = members
            .iter()
            .flat_map(|member| member.candidate.cell_families.clone())
            .collect();
        cell_families.sort();
        cell_families.dedup();
        let mut source_windows: Vec<_> = members
            .iter()
            .flat_map(|member| member.candidate.source_windows.clone())
            .collect();
        source_windows.sort();
        source_windows.dedup();
        let ordinal = closures.len();
        let mut closure = GeneratorCandidate {
            candidate_id: if d2ap_closure {
                format!("ITERATIVECLOSE_{ordinal:02}")
            } else {
                format!("V8CLOSE_{ordinal:02}")
            },
            provenance,
            choices,
            tech_area: members
                .iter()
                .map(|member| member.candidate.tech_area)
                .sum(),
            tech_delay: members
                .iter()
                .map(|member| member.candidate.tech_delay)
                .fold(0.0_f64, f64::max),
            logic_depth: members
                .iter()
                .map(|member| member.candidate.logic_depth)
                .max()
                .unwrap_or(0),
            cell_families,
            outputs: members
                .iter()
                .flat_map(|member| member.candidate.outputs.iter().copied())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            window_kind: mapping_class.to_owned(),
            proof_boundary: Vec::new(),
            proof_outputs: members
                .iter()
                .flat_map(|member| member.candidate.proof_outputs.iter().copied())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            proof_groups,
            shared_host: None,
            divisors: members
                .iter()
                .flat_map(|member| member.candidate.divisors.iter().copied())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            replaced_anchor: None,
            logical_signature: None,
            mapping: Some(mapping_class.to_owned()),
            source_windows,
        };
        let signature = topology_signature(&closure);
        if !seen_topologies.insert(signature.clone()) {
            continue;
        }
        closure.logical_signature = Some(signature);
        audit.push(json!({
            "candidate_id":closure.candidate_id,
            "scope":scope.name(),
            "ordering":policy.name(),
            "transaction_count":members.len(),
            "choice_count":closure.choices.len(),
            "proof_group_count":closure.proof_groups.len(),
            "members":members.iter().map(|member| member.candidate.candidate_id.clone()).collect::<Vec<_>>(),
            "partitions":members.iter().map(|member| closure_partition_key(member.candidate)).collect::<Vec<_>>(),
            "partition_pareto_mode":partition_pareto,
            "partition_beam_width":partition_beam_width,
            "partition_per_group_cap":partition_per_group_cap,
            "partition_dap_diverse_pool":dap_diverse_pool,
            "partition_dap_min_distance":minimum_dap_distance,
            "partition_combination_diversity":combination_diversity,
            "predicted_objective_delta":selected.predicted_objective_delta,
            "predicted_constraint_slack_delta":selected.slack_delta,
            "predicted_total_constraint_violation":selected.predicted_violation,
            "predicted_minimum_constraint_slack":selected.predicted_min_slack.is_finite().then_some(selected.predicted_min_slack),
        }));
        closures.push(closure);
        if closures.len() == max_closures {
            break;
        }
    }
    Ok((closures, audit))
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 8 {
        bail!(
            "usage: run_generator_union_native BENCHMARK INPUT OUT_DIR v1|v2|v3 MAX_ROUNDS LIBERTY SCALE_RULES REWRITE_RULES"
        );
    }
    let benchmark = &args[0];
    let input = PathBuf::from(&args[1]);
    let out = PathBuf::from(&args[2]);
    let variant = &args[3];
    let requested_max_rounds: usize = args[4].parse()?;
    let ablation_mode =
        std::env::var("EGG_V8_PHASE_ABLATION").unwrap_or_else(|_| "full".to_owned());
    anyhow::ensure!(
        matches!(
            ablation_mode.as_str(),
            "full" | "phase1-only" | "phase2-only" | "early-commitment" | "no-pi-drive-expansion"
        ),
        "EGG_V8_PHASE_ABLATION must be full, phase1-only, phase2-only, early-commitment, or no-pi-drive-expansion"
    );
    let phase_i_drive_expansion_enabled = ablation_mode != "no-pi-drive-expansion";
    // A supplied mapped-choice sidecar may be locked for a controlled replay:
    // accept its round-zero topology, perform the normal cold rebuild, but do
    // not let a later V8 round rewrite the protected window.  The environment
    // variable is opt-in and therefore leaves every ordinary trajectory
    // unchanged.
    let lock_sidecar = std::env::var("EGG_ITERATIVE_LOCK_SIDECAR")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
        && std::env::var_os("EGG_ITERATIVE_EXTRA_NETLIST").is_some();
    let max_rounds: usize = if lock_sidecar {
        1
    } else {
        requested_max_rounds
    };
    let liberty = PathBuf::from(&args[5]);
    let scale_rules = PathBuf::from(&args[6]);
    let rewrite_rules = PathBuf::from(&args[7]);
    let ultra_config_source = V8UltraConfig::from_env()?;
    let ultra_config_path = ultra_config_source.as_ref().map(|(path, _)| path.clone());
    let ultra_config = ultra_config_source
        .as_ref()
        .map(|(_, config)| config.clone());
    let objective_engine = std::env::var("EGG_OBJECTIVE_ENGINE").ok();
    let objective_local_rewrite_enabled = std::env::var("EGG_OBJECTIVE_LOCAL_REWRITE_CLOSURE")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let objective_mapped_window_enabled = std::env::var("EGG_OBJECTIVE_MAPPED_WINDOW_REWRITE")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let objective_compatible_closure_enabled = std::env::var("EGG_OBJECTIVE_COMPATIBLE_CLOSURE")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let objective_bounded_functional_enabled =
        std::env::var("EGG_OBJECTIVE_BOUNDED_FUNCTIONAL_WINDOW")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
    let d2ap_functional_window_candidate_cap = ultra_config
        .as_ref()
        .map_or(0, |config| config.d2ap_functional_window_candidate_cap);
    let d2ap_functional_window_root_cap = ultra_config
        .as_ref()
        .map_or(0, |config| config.d2ap_functional_window_root_cap);
    let d2ap_functional_window_divisor_cap = ultra_config
        .as_ref()
        .map_or(0, |config| config.d2ap_functional_window_divisor_cap);
    let d2ap_functional_window_max_area_debt_cells = ultra_config.as_ref().map_or(0, |config| {
        config.d2ap_functional_window_max_area_debt_cells
    });
    let mut objective = SearchObjective::from_env()?;
    let timing_boundary = TimingBoundary::from_env()?;
    let objective_spec = std::env::var_os("EGG_OBJECTIVE_SPEC")
        .map(ObjectiveSpec::from_json_path)
        .transpose()?;
    if timing_boundary.internal_timing_model.is_genlib() {
        let path = std::env::var("EGG_OBJECTIVE_SPEC")
            .context("GENLIB mode requires an explicit power-free objective")?;
        let value: Value = serde_json::from_str(&fs::read_to_string(path)?)?;
        anyhow::ensure!(
            value["objective"]["kind"] == "product"
                && value["objective"]["exponents"]["power"].as_f64() == Some(0.0)
                && value["constraints"]
                    .as_array()
                    .is_some_and(|rows| rows.iter().all(|row| row["metric"] != "power")),
            "GENLIB mode supports product objectives without power terms/constraints only"
        );
        anyhow::ensure!(
            fs::read_to_string(&liberty)?.contains("GENLIB STATIC TRANSPORT ONLY"),
            "GENLIB mode requires a labelled constant-table adapter"
        );
    }
    if objective_spec.is_some() {
        anyhow::ensure!(
            matches!(objective, SearchObjective::D2ap),
            "EGG_OBJECTIVE_SPEC cannot be combined with legacy EGG_EXPERIMENTAL_OBJECTIVE"
        );
    }
    let programmable_spec = objective_spec
        .clone()
        .filter(|spec| !matches!(spec.objective, ObjectiveFunction::FrozenD2ap));
    let primal_dual_policy = PrimalDualPolicy::default();
    anyhow::ensure!(max_rounds > 0);
    anyhow::ensure!(!out.exists(), "output already exists: {}", out.display());
    fs::create_dir_all(&out)?;
    let started = Instant::now();
    // Resource-only ceiling for shared-host runs.  With no override, retain
    // every frozen V8 worker count below.  Capping changes only how many
    // independent ordered tasks execute concurrently; candidate order,
    // budgets, seeds and merges remain unchanged.
    let resource_job_cap = std::env::var("EGG_FROZEN_RESOURCE_JOBS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0);
    let resource_jobs = |frozen: usize| {
        resource_job_cap
            .map(|cap| frozen.min(cap))
            .unwrap_or(frozen)
            .max(1)
            .to_string()
    };
    let env = vec![
        ("EGG_LIB_PATH".to_owned(), liberty.display().to_string()),
        (
            "EGG_SCALE_RULES".to_owned(),
            scale_rules.display().to_string(),
        ),
        (
            "EGG_RULE_PATHS".to_owned(),
            rewrite_rules.display().to_string(),
        ),
        ("EGG_REGION_POLICY".to_owned(), "hybrid".to_owned()),
        ("EGG_PLANNER_EMIT".to_owned(), "1".to_owned()),
        // Structural realization is a deterministic per-anchor map followed
        // by an ordered merge.  c1355 has hundreds of independent roots, so
        // 16 workers left most of the host idle without buying determinism.
        ("EGG_REALIZATION_JOBS".to_owned(), resource_jobs(32)),
        ("EGG_REGION_JOBS".to_owned(), resource_jobs(4)),
        ("EGG_DUAL_SEED_JOBS".to_owned(), resource_jobs(32)),
        ("EGG_REGION_SCORE_JOBS".to_owned(), resource_jobs(8)),
        ("EGG_A2_FD_JOBS".to_owned(), resource_jobs(16)),
        ("EGG_A2_QUIET".to_owned(), "1".to_owned()),
        (
            "EGG_PHASE1_DRIVE_EXPANSION".to_owned(),
            if phase_i_drive_expansion_enabled {
                "1"
            } else {
                "0"
            }
            .to_owned(),
        ),
    ];
    // Configuration is frozen before any worker thread is created and never
    // mutated afterwards.  Rust 2024 marks process-environment mutation
    // unsafe specifically because concurrent mutation is racy; this
    // initialization point precedes all scoped parallel work.
    for (key, value) in &env {
        unsafe { std::env::set_var(key, value) };
    }
    // Build the immutable Liberty/NLDM database concurrently with the first
    // planner's pin/netlist front end.  The planner joins it only when the
    // ExtendedEGraph needs physical tables.
    let (nldm_sender, nldm_receiver) = mpsc::channel();
    let database_input = input.clone();
    let database_liberty = liberty.clone();
    let database_rules = scale_rules.clone();
    let mut structural_cells = rule_cell_names(&rewrite_rules)?;
    if timing_boundary.internal_timing_model.is_v3() {
        structural_cells.insert(timing_boundary.primary_input_driver_cell.to_owned());
    }
    let mut database_loader = Some(std::thread::spawn(move || -> Result<_> {
        let started = Instant::now();
        let mut database = a2_impl::CellDatabase::load(
            std::slice::from_ref(&database_input),
            &database_liberty,
            &database_rules,
        )?;
        // The planner needs these rule-produced cells in every fresh round.
        // Parse them once in the existing background database build rather
        // than lazily rebuilding the same NLDM records in each planner.
        database.ensure_cell_names(structural_cells, &database_liberty)?;
        let elapsed = started.elapsed().as_secs_f64();
        nldm_sender
            .send(database.nldm_cache())
            .map_err(|_| anyhow::anyhow!("planner dropped shared NLDM preload"))?;
        Ok((database, elapsed))
    }));
    // Pin directions are immutable across every fresh parent.  Scan only the
    // cell/pin/direction groups while the NLDM loader is running, then share
    // the compact map with planner and materializer; constructing a second
    // 52-MiB Liberty AST here used to dominate the controller startup.
    let shared_pins = read_pin_directions_fast(&liberty).map_err(anyhow::Error::msg)?;
    let mut first_nldm_receiver = Some(nldm_receiver);
    let mut shared_database: Option<a2_impl::CellDatabase> = None;
    let mut shared_nldm: Option<HashMap<String, NLDM>> = None;
    let mut database_load_sec = 0.0;
    let mut current_ppa: Option<PpaPoint> = None;
    let mut primal_dual_updates = Vec::new();
    if let Some(spec) = programmable_spec.clone() {
        // Custom objectives need the actual G0 as their fixed normalization
        // reference before the first planner call.  Joining the already
        // running database loader here affects only the experimental path;
        // frozen D2AP retains the original overlapped startup trajectory.
        let (database, elapsed) = database_loader
            .take()
            .expect("custom objective database loader")
            .join()
            .map_err(|_| anyhow::anyhow!("shared database loader panicked"))??;
        database_load_sec = elapsed;
        let nldm = first_nldm_receiver
            .take()
            .expect("custom objective NLDM receiver")
            .recv()
            .map_err(|error| anyhow::anyhow!("shared NLDM preload failed: {error}"))?;
        let (g0_netlist, _) = read_verilog_with_lib_to_netlist(&input, shared_pins.clone())
            .map_err(anyhow::Error::msg)?;
        let mut evaluator =
            SharedLoadEvaluator::from_nldm_cache(&nldm, true)?.with_boundary(timing_boundary);
        let g0 = evaluator.evaluate_netlist(&g0_netlist)?;
        let g0_point = PpaPoint {
            delay: g0.delay,
            area: g0.area,
            power: g0.power,
        };
        let mut context = spec.bind(g0_point)?;
        context.initialize_primal_dual(primal_dual_policy)?;
        let initial_update = context.update_primal_dual(g0_point, primal_dual_policy)?;
        primal_dual_updates.push(json!({
            "stage":"before_round_1","point":g0_point,"update":initial_update,
        }));
        objective = SearchObjective::programmable(context, primal_dual_policy)?;
        current_ppa = Some(g0_point);
        shared_database = Some(database);
        shared_nldm = Some(nldm);
    }
    let mut parent = input.clone();
    let mut parent_origin = "g0".to_owned();
    let mut incumbent_path = input.clone();
    let mut incumbent_sha = sha(&input)?;
    let mut incumbent_ppa: Option<PpaPoint> = None;
    let mut visited_parent_shas = BTreeSet::from([incumbent_sha.clone()]);
    let mut visited_topology_signatures = BTreeSet::new();
    let mut best_score = None;
    let mut g0_score = None;
    let mut rounds = Vec::new();
    let mut accepted_rounds = 0usize;
    let mut total_exact = 0usize;
    let mut total_physical_exact = 0usize;
    let mut terminal_attempted_round = None;
    let mut terminal_reason = None;
    let mut transit_count = 0usize;
    let mut transit_rounds = Vec::new();
    let mut transit_followup_search_exact = 0usize;
    let mut transit_followup_physical_exact = 0usize;
    let mut total_local_rewrite_closure_exact = 0usize;
    let mut ultra_saturation_streak = 0usize;
    let mut ultra_stop_receipt: Option<Value> = None;

    for round_index in 0..max_rounds {
        let round_started = Instant::now();
        let round = out.join(format!("round_{round_index:02}"));
        fs::create_dir_all(&round)?;
        let round_parent_path = parent.clone();
        let round_parent_origin = parent_origin.clone();
        let parent_sha = sha(&parent)?;
        // A2 may select a legal drive variant that was absent from the G0
        // database.  Before a fresh accepted parent is replanned, merge all of
        // its cells into the process cache used by planner and D1 reranking.
        if let Some(database) = shared_database.as_mut() {
            database.ensure_netlists(std::slice::from_ref(&parent), &liberty)?;
            shared_nldm
                .as_mut()
                .context("shared NLDM cache missing before planner")?
                .extend(database.nldm_cache());
        }
        write_json(
            &round.join("status.json"),
            &json!({"stage":"planner","parent_sha256":parent_sha}),
        )?;
        let planner_dir = round.join("planner");
        let planner_started = Instant::now();
        let planner_top_seeds = ultra_config
            .as_ref()
            .map_or(4, V8UltraConfig::planner_top_seeds);
        let planner_p2_keep = ultra_config
            .as_ref()
            .map_or(8, V8UltraConfig::planner_p2_keep_per_region);
        let planner_max_region_size = ultra_config
            .as_ref()
            .map_or(16, V8UltraConfig::planner_max_region_size);
        let planner_args = vec![
            parent.display().to_string(),
            planner_dir.display().to_string(),
            variant.clone(),
            planner_top_seeds.to_string(),
            planner_p2_keep.to_string(),
            planner_max_region_size.to_string(),
        ];
        let planner_objective_weights = current_ppa
            .filter(|_| objective.programmable_context().is_some())
            .map(|point| objective.relative_weights(point))
            .transpose()?
            .map(|weights| CircuitObjectiveWeights {
                delay: weights.delay,
                area: weights.area,
                power: weights.power,
            });
        if let Some(receiver) = first_nldm_receiver.take() {
            let planner_nldm = if let Some(weights) = planner_objective_weights {
                planner_impl::run_cli_with_nldm_receiver_objective(
                    planner_args,
                    receiver,
                    &shared_pins,
                    weights,
                )?
            } else {
                planner_impl::run_cli_with_nldm_receiver(planner_args, receiver, &shared_pins)?
            };
            let (database, elapsed) = database_loader
                .take()
                .expect("first-round database loader")
                .join()
                .map_err(|_| anyhow::anyhow!("shared database loader panicked"))??;
            database_load_sec = elapsed;
            shared_nldm = Some(planner_nldm);
            shared_database = Some(database);
        } else {
            let planner_nldm = if let Some(weights) = planner_objective_weights {
                planner_impl::run_cli_with_nldm_objective(
                    planner_args,
                    shared_nldm.as_ref(),
                    &shared_pins,
                    weights,
                )?
            } else {
                planner_impl::run_cli_with_nldm(planner_args, shared_nldm.as_ref(), &shared_pins)?
            };
            // The planner's ExtendedEGraph only retains cells referenced by
            // the current parent and this round's structural space.  Replacing
            // the process cache here therefore discarded cells learned in
            // earlier rounds and forced the same Liberty/NLDM reconstruction
            // on every fresh parent.  Keep the process-wide superset instead;
            // the key is the exact cell name, so extending is deterministic
            // and cannot change any evaluator result.
            shared_nldm
                .as_mut()
                .context("shared NLDM cache missing after planner")?
                .extend(planner_nldm);
        }
        let planner_sec = planner_started.elapsed().as_secs_f64();
        let planner_summary: Value =
            serde_json::from_slice(&fs::read(planner_dir.join("summary.json"))?)?;
        let round_instance_count =
            source_instance_count(&planner_dir.join("source_occurrences.json"))?;
        let ultra_large_round = ultra_config
            .as_ref()
            .is_some_and(|config| config.enabled_for_instances(round_instance_count));
        let mut planner_parent_ppa = PpaPoint {
            delay: planner_summary["incumbent_v2"]["delay"]
                .as_f64()
                .context("incumbent delay")?,
            area: planner_summary["incumbent_v2"]["area"]
                .as_f64()
                .context("incumbent area")?,
            power: planner_summary["incumbent_v2"]["power"]
                .as_f64()
                .context("incumbent power")?,
        };
        if std::env::var_os("EGG_PORT_BOUNDARY_JSON").is_some() {
            let mut evaluator = SharedLoadEvaluator::from_nldm_cache(
                shared_nldm.as_ref().context("shared NLDM cache missing")?,
                true,
            )?
            .with_boundary(timing_boundary);
            let point = evaluator.evaluate(&parent)?;
            planner_parent_ppa = PpaPoint {
                delay: point.delay,
                area: point.area,
                power: point.power,
            };
        }
        current_ppa = Some(planner_parent_ppa);
        if best_score.is_none() {
            best_score = Some(objective.exact_value(planner_parent_ppa)?);
            g0_score = best_score;
            incumbent_ppa = Some(planner_parent_ppa);
            write_json(
                &out.join("g0_exact.json"),
                &json!([{
                    "model":timing_boundary.internal_timing_model.model_name(),
                    "input":parent,"module":benchmark,
                    "delay":planner_parent_ppa.delay,
                    "area":planner_parent_ppa.area,
                    "power":planner_parent_ppa.power,
                    "score":best_score,"finite":true,
                }]),
            )?;
        }
        let parent_score = best_score.unwrap();
        let parent_search_score = objective.score(
            planner_parent_ppa.delay,
            planner_parent_ppa.area,
            planner_parent_ppa.power,
        );
        let d1_rerank = round.join("d1_rerank");
        let rerank_started = Instant::now();
        let rerank_args = vec![
            parent.display().to_string(),
            planner_dir.display().to_string(),
            d1_rerank.display().to_string(),
            "8".to_owned(),
            "4".to_owned(),
        ];
        let rerank_nldm = shared_nldm
            .as_ref()
            .context("shared NLDM cache missing before D1 rerank")?
            .clone();
        let rerank_timing_boundary = ultra_config
            .as_ref()
            .filter(|config| config.d1_p1_use_timing_boundary)
            .map(|_| timing_boundary);
        let reranker = std::thread::spawn(move || {
            if let Some(boundary) = rerank_timing_boundary {
                reranker_impl::run_cli_with_nldm_boundary(rerank_args, &rerank_nldm, boundary)
            } else {
                reranker_impl::run_cli_with_nldm(rerank_args, &rerank_nldm)
            }
        });

        let generator_started = Instant::now();
        let macro_plan = round.join("macro_plan.json");
        let generator_realization_weights = if objective_engine.as_deref() == Some("v8-pareto") {
            Some(objective.relative_weights(planner_parent_ppa)?)
        } else {
            None
        };
        let generator_max_candidates_per_region =
            std::env::var("EGG_GENERATOR_MAX_CANDIDATES_PER_REGION")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(16);
        let mut macro_rows = if generator_max_candidates_per_region == 0 {
            write_json(&macro_plan, &json!([]))?;
            Vec::new()
        } else {
            generate(
                &planner_dir.join("source_occurrences.json"),
                &planner_dir.join("dual_region_plan_full.json"),
                &liberty,
                &macro_plan,
                GeneratorConfig {
                    realization_weights: generator_realization_weights,
                    max_candidates_per_region: generator_max_candidates_per_region,
                    expand_drive_mappings: phase_i_drive_expansion_enabled,
                    ..GeneratorConfig::default()
                },
            )?
        };
        let d2ap_functional_window_active =
            objective_engine.is_none() && d2ap_functional_window_candidate_cap > 0;
        let d2ap_functional_preferred_roots: Vec<usize> = if d2ap_functional_window_active {
            let rows: Vec<Value> =
                serde_json::from_slice(&fs::read(planner_dir.join("dual_region_plan_full.json"))?)?;
            rows.iter()
                .filter_map(|row| row["region_id"].as_str())
                .filter_map(|region| region.rsplit('_').next())
                .filter_map(|anchor| anchor.parse().ok())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        } else {
            Vec::new()
        };
        let mut deletion_rows = if (objective_engine.as_deref() == Some("v8-pareto")
            && objective.programmable_context().is_some())
            || d2ap_functional_window_active
        {
            let deletion_cap = if d2ap_functional_window_active {
                d2ap_functional_window_candidate_cap
            } else {
                std::env::var("EGG_OBJECTIVE_DELETION_CANDIDATE_CAP")
                    .ok()
                    .map(|value| value.parse::<usize>())
                    .transpose()
                    .context("invalid EGG_OBJECTIVE_DELETION_CANDIDATE_CAP")?
                    .unwrap_or(64)
            };
            generate_deletion_candidates(
                &planner_dir.join("source_occurrences.json"),
                &liberty,
                &round.join("deletion_plan.json"),
                deletion_cap,
                d2ap_functional_window_active
                    || objective_local_rewrite_enabled
                    || objective_mapped_window_enabled,
                d2ap_functional_window_active || objective_bounded_functional_enabled,
                if d2ap_functional_window_active {
                    d2ap_functional_window_root_cap
                } else {
                    64
                },
                if d2ap_functional_window_active {
                    d2ap_functional_window_divisor_cap
                } else {
                    128
                },
                d2ap_functional_window_active,
                if d2ap_functional_window_active {
                    d2ap_functional_window_max_area_debt_cells
                } else {
                    1
                },
                &d2ap_functional_preferred_roots,
                phase_i_drive_expansion_enabled,
            )?
        } else {
            Vec::new()
        };
        if objective.programmable_context().is_some()
            && std::env::var("EGG_OBJECTIVE_FAST_FUNCTIONAL_UNION").as_deref() == Ok("1")
        {
            let extra = generate_deletion_candidates(
                &planner_dir.join("source_occurrences.json"),
                &liberty,
                &round.join("fast_functional_plan.json"),
                256,
                false,
                true,
                16,
                32,
                true,
                1,
                &[],
                phase_i_drive_expansion_enabled,
            )?;
            deletion_rows.extend(extra);
        }
        let deletion_candidate_count = deletion_rows.len();
        if !deletion_rows.is_empty() {
            macro_rows.extend(deletion_rows);
            write_json(&macro_plan, &serde_json::to_value(&macro_rows)?)?;
        }
        let generated_candidates_before_ultra_cap = macro_rows.len();
        let mut ultra_candidate_cap = json!({
            "enabled":false,
            "instance_count":round_instance_count,
            "original_count":macro_rows.len(),
            "selected_count":macro_rows.len(),
        });
        if ultra_large_round {
            let cap = ultra_config.as_ref().map_or(0, |config| {
                config.candidate_cap_for_instances(round_instance_count)
            });
            if cap > 0 && macro_rows.len() > cap {
                fs::copy(&macro_plan, round.join("macro_plan_before_ultra_cap.json"))?;
                let (selected, audit) = cap_generator_candidates(macro_rows, cap);
                macro_rows = selected;
                ultra_candidate_cap = audit;
                write_json(&macro_plan, &serde_json::to_value(&macro_rows)?)?;
                write_json(
                    &round.join("v8_ultra_candidate_cap_receipt.json"),
                    &ultra_candidate_cap,
                )?;
            }
        }
        let generator_sec = generator_started.elapsed().as_secs_f64();
        let materialized = round.join("materialized");
        let materializer_started = Instant::now();
        let rewritten_equivalence: Vec<_> = macro_rows
            .iter()
            .map(UnifiedEquivalenceCandidate::from_generator)
            .collect();
        let materialized_batch = materializer_impl::materialize_equivalence_in_memory(
            &parent,
            &rewritten_equivalence,
            &materialized,
            &shared_pins,
        )?;
        let materializer_sec = materializer_started.elapsed().as_secs_f64();
        let trace: Vec<Value> =
            serde_json::from_slice(&fs::read(materialized.join("materialization_trace.json"))?)?;
        let legal: Vec<_> = trace
            .iter()
            .filter(|row| row["legal"].as_bool() == Some(true))
            .collect();
        anyhow::ensure!(
            legal.len() <= macro_rows.len(),
            "legal generator count exceeds proposal count"
        );
        let p1_path = round.join("p1_exact.json");
        anyhow::ensure!(
            materialized_batch.candidates.len() == legal.len(),
            "in-memory and receipt candidate counts differ"
        );
        for (memory, receipt) in materialized_batch.candidates.iter().zip(&legal) {
            anyhow::ensure!(
                memory.candidate_id == receipt["candidate_id"].as_str().unwrap(),
                "in-memory and receipt candidate order differs"
            );
        }
        // The initial process database contains the G0 and all scale-family
        // cells.  Add only structural-generator cell types that are genuinely
        // new, directly from the in-memory netlists (no Verilog scan).
        let generator_cell_names = materialized_batch
            .candidates
            .iter()
            .flat_map(|candidate| candidate.netlist.graph.node_weights())
            .map(|cell| cell.to_string())
            .filter(|name| name.ends_with("_ASAP7_6t_L") || name.ends_with("_ASAP7_75t_R"));
        let database = shared_database
            .as_mut()
            .context("shared physical database missing after planner")?;
        let mut database_cells_added =
            database.ensure_cell_names(generator_cell_names, &liberty)?;
        shared_nldm
            .as_mut()
            .context("shared NLDM cache missing after planner")?
            .extend(database.nldm_cache());
        let p1_started = Instant::now();
        let p1_rows = evaluate_materialized_p1(
            &p1_path,
            &materialized_batch.module,
            &materialized_batch.candidates,
            shared_nldm.as_ref().context("shared NLDM cache missing")?,
            &objective,
        )?;
        let p1_sec = p1_started.elapsed().as_secs_f64();

        // PMO-inspired immediate local rewriting, expressed as a generic
        // constrained fixed-point lane.  Each transaction is selected by the
        // active ObjectiveSpec, is independently Boolean-proved, is reparsed
        // from written Verilog, and must be a strict feasible Exact
        // improvement.  Only the final scratch topology joins the unchanged
        // union Top-32; frozen D2AP never enters this lane.
        let local_rewrite_step_cap = std::env::var("EGG_OBJECTIVE_LOCAL_REWRITE_STEP_CAP")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .context("invalid EGG_OBJECTIVE_LOCAL_REWRITE_STEP_CAP")?
            .unwrap_or(8);
        let local_rewrite_started = Instant::now();
        let local_rewrite_root = round.join("local_rewrite_closure");
        let mut local_rewrite_union_rows = Vec::new();
        let mut local_rewrite_p1_rows = Vec::new();
        let mut local_rewrite_audit = Vec::new();
        let mut local_rewrite_exact = 0usize;
        if objective_local_rewrite_enabled && objective.programmable_context().is_some() {
            fs::create_dir_all(&local_rewrite_root)?;
            let mut scratch_parent = parent.clone();
            let mut scratch_point = planner_parent_ppa;
            let mut scratch_shas = BTreeSet::from([sha(&scratch_parent)?]);
            for step_index in 0..local_rewrite_step_cap {
                let step = local_rewrite_root.join(format!("step_{step_index:02}"));
                fs::create_dir_all(&step)?;
                let graph_path = step.join("source_occurrences.json");
                let module = emit_source_occurrences(&scratch_parent, &graph_path, &shared_pins)?;
                let step_plan = step.join("deletion_plan.json");
                let step_rows = generate_deletion_candidates(
                    &graph_path,
                    &liberty,
                    &step_plan,
                    64,
                    true,
                    false,
                    64,
                    128,
                    false,
                    1,
                    &[],
                    phase_i_drive_expansion_enabled,
                )?;
                if step_rows.is_empty() {
                    local_rewrite_audit.push(json!({
                        "step":step_index+1,"action":"stop","reason":"no_candidates",
                    }));
                    break;
                }
                let step_materialized = step.join("materialized");
                let step_equivalence: Vec<_> = step_rows
                    .iter()
                    .map(UnifiedEquivalenceCandidate::from_generator)
                    .collect();
                let step_batch = materializer_impl::materialize_equivalence_in_memory(
                    &scratch_parent,
                    &step_equivalence,
                    &step_materialized,
                    &shared_pins,
                )?;
                let step_cell_names = step_batch
                    .candidates
                    .iter()
                    .flat_map(|candidate| candidate.netlist.graph.node_weights())
                    .map(|cell| cell.to_string())
                    .filter(|name| name.ends_with("_ASAP7_6t_L") || name.ends_with("_ASAP7_75t_R"));
                database_cells_added += database.ensure_cell_names(step_cell_names, &liberty)?;
                shared_nldm
                    .as_mut()
                    .context("shared NLDM cache missing in local rewrite closure")?
                    .extend(database.nldm_cache());
                let step_p1_path = step.join("p1_exact.json");
                let _step_p1 = evaluate_materialized_p1(
                    &step_p1_path,
                    &module,
                    &step_batch.candidates,
                    shared_nldm.as_ref().context("shared NLDM cache missing")?,
                    &objective,
                )?;
                local_rewrite_exact += step_batch.candidates.len();
                let step_output_by_id: HashMap<_, _> = step_batch
                    .candidates
                    .iter()
                    .map(|candidate| {
                        (
                            candidate.candidate_id.clone(),
                            candidate.output_netlist.display().to_string(),
                        )
                    })
                    .collect();
                let step_union_rows: Vec<_> = step_rows
                    .iter()
                    .filter_map(|candidate| {
                        step_output_by_id
                            .get(&candidate.candidate_id)
                            .map(|output| generator_union_row(candidate, output))
                    })
                    .collect::<Result<_>>()?;
                if step_union_rows.is_empty() {
                    local_rewrite_audit.push(json!({
                        "step":step_index+1,"action":"stop","reason":"no_legal_candidates",
                    }));
                    break;
                }
                let step_union_plan = step.join("union_plan.json");
                write_json(&step_union_plan, &Value::Array(step_union_rows))?;
                let conditional = a2_impl::fixed_budget_native_with_database_objective(
                    &step_union_plan,
                    &step_p1_path,
                    &step.join("conditional_a2"),
                    &liberty,
                    &scale_rules,
                    25,
                    16,
                    database,
                    objective.clone(),
                )?;
                let conditional_exact = conditional["stage"]["paid_exact"]
                    .as_u64()
                    .context("local rewrite conditional Exact count")?
                    as usize;
                local_rewrite_exact += conditional_exact;
                let Some(selected) = select_local_rewrite_step(
                    conditional["candidates"]
                        .as_array()
                        .context("local rewrite conditional candidates")?,
                    &objective,
                    scratch_point,
                    &scratch_shas,
                )?
                else {
                    local_rewrite_audit.push(json!({
                        "step":step_index+1,"action":"stop",
                        "reason":"no_strict_feasible_improvement",
                        "proposed":step_rows.len(),"legal":step_batch.candidates.len(),
                        "conditional_a2_exact":conditional_exact,
                    }));
                    break;
                };
                local_rewrite_audit.push(json!({
                    "step":step_index+1,"action":"commit",
                    "candidate_id":selected.candidate_id,
                    "parent":scratch_point,"candidate":selected.point,
                    "objective_value":selected.objective_value,
                    "minimum_constraint_slack":selected.minimum_constraint_slack,
                    "sha256":selected.sha256,
                    "proposed":step_rows.len(),"legal":step_batch.candidates.len(),
                    "conditional_a2_exact":conditional_exact,
                }));
                scratch_parent = selected.output;
                scratch_point = selected.point;
                scratch_shas.insert(selected.sha256);
            }
            if local_rewrite_audit
                .iter()
                .any(|row| row["action"].as_str() == Some("commit"))
            {
                // P1 rows are keyed by the written candidate file stem.
                let final_path = local_rewrite_root.join("V8LOCALFIX_00.v");
                fs::copy(&scratch_parent, &final_path)?;
                let topology = sha(&final_path)?;
                let mut union_row = json!({
                    "candidate_id":"V8LOCALFIX_00",
                    "source_class":"objective-local-rewrite-closure",
                    "mapping":"objective-local-rewrite-closure",
                    "topology_signature":topology,
                    "provenance_class":"objective-local-rewrite-closure",
                    "specific_window":"local-rewrite-closure",
                    "region_id":"local-rewrite-closure",
                    "region_size":local_rewrite_audit.iter().filter(|row| row["action"].as_str()==Some("commit")).count(),
                    "output_netlist":final_path,
                });
                let unified = UnifiedEquivalenceCandidate::whole_netlist(
                    "V8LOCALFIX_00",
                    EquivalenceSource::MappedRewriteClosure,
                    "objective-local-rewrite-closure",
                );
                attach_unified_candidate(&mut union_row, &unified).map_err(anyhow::Error::msg)?;
                local_rewrite_union_rows.push(union_row);
                local_rewrite_p1_rows.push(json!({
                    "model":timing_boundary.internal_timing_model.model_name(),
                    "input":final_path,"module":benchmark,
                    "delay":scratch_point.delay,"area":scratch_point.area,
                    "power":scratch_point.power,
                    "score":objective.score(scratch_point.delay,scratch_point.area,scratch_point.power),
                    "finite":true,
                }));
            }
        }
        write_json(
            &round.join("local_rewrite_closure_audit.json"),
            &json!({
                "policy":"ObjectiveSpec strict-feasible local rewrite fixed point",
                "enabled":objective_local_rewrite_enabled,
                "fixed_step_cap":local_rewrite_step_cap,
                "cold_rebuild_after_each_commit":true,
                "additional_p1_exact":local_rewrite_exact,
                "transactions":local_rewrite_audit,
            }),
        )?;
        total_local_rewrite_closure_exact += local_rewrite_exact;
        let local_rewrite_sec = local_rewrite_started.elapsed().as_secs_f64();

        let correlated_started = Instant::now();
        let (correlated_rows, correlated_selection_audit) =
            if objective_engine.as_deref() == Some("v8-pareto") {
                correlated_multi_root_candidates(
                    &macro_rows,
                    &materialized_batch.candidates,
                    &legal,
                    &p1_rows,
                    planner_parent_ppa,
                    &objective,
                    24,
                )?
            } else {
                (Vec::new(), Vec::new())
            };
        let d2ap_closure_config = ultra_config
            .as_ref()
            .filter(|config| config.d2ap_closure_enabled());
        let d2ap_closure_active = objective_engine.is_none() && d2ap_closure_config.is_some();
        let compatible_closure_active = (objective_engine.as_deref() == Some("v8-pareto")
            && (objective_local_rewrite_enabled || objective_compatible_closure_enabled))
            || d2ap_closure_active;
        let closure_transaction_cap = if d2ap_closure_active {
            d2ap_closure_config
                .map(|config| config.d2ap_closure_max_transactions)
                .unwrap_or(0)
        } else if objective_local_rewrite_enabled {
            std::env::var("EGG_OBJECTIVE_CLOSURE_TRANSACTION_CAP")
                .ok()
                .map(|value| value.parse::<usize>())
                .transpose()
                .context("invalid EGG_OBJECTIVE_CLOSURE_TRANSACTION_CAP")?
                .unwrap_or(32)
        } else {
            8
        };
        let closure_candidate_cap = if d2ap_closure_active {
            d2ap_closure_config
                .map(|config| config.d2ap_closure_candidate_cap)
                .unwrap_or(0)
        } else {
            3
        };
        let (mut closure_rows, closure_selection_audit) = if compatible_closure_active {
            let closure_scope = if objective_local_rewrite_enabled || d2ap_closure_active {
                LocalClosureScope::All
            } else {
                LocalClosureScope::MappedWindow
            };
            objective_local_closure_candidates(
                &macro_rows,
                &materialized_batch.candidates,
                &legal,
                &p1_rows,
                planner_parent_ppa,
                &objective,
                closure_candidate_cap,
                closure_transaction_cap,
                closure_scope,
                d2ap_closure_config.map(|config| config.d2ap_closure_max_predicted_regression),
            )?
        } else {
            (Vec::new(), Vec::new())
        };
        write_json(
            &round.join("correlated_selection_audit.json"),
            &json!({
                "policy":"objective-spec dual-front correlated multi-root composition",
                "active_slack_ratio":objective.active_constraint_slack_ratio(),
                "maximum_members":4,
                "beam_width":96,
                "fixed_candidate_cap":24,
                "selected":correlated_selection_audit,
            }),
        )?;
        write_json(
            &round.join("local_closure_selection_audit.json"),
            &json!({
                "policy":"objective-spec maximal-compatible local closure",
                "enabled":compatible_closure_active,
                "d2ap_iterative_mode":d2ap_closure_active,
                "active_slack_ratio":objective.active_constraint_slack_ratio(),
                "fixed_closure_candidate_cap":closure_candidate_cap,
                "fixed_transaction_cap_per_candidate":closure_transaction_cap,
                "scope":if objective_local_rewrite_enabled || d2ap_closure_active {LocalClosureScope::All.name()} else {LocalClosureScope::MappedWindow.name()},
                "compositional_boolean_proof":true,
                "selected":closure_selection_audit,
            }),
        )?;
        if std::env::var("EGG_OBJECTIVE_CLOSURE_PRUNE_UNREACHABLE").as_deref() == Ok("1") {
            let graph: Value =
                serde_json::from_slice(&fs::read(planner_dir.join("source_occurrences.json"))?)?;
            let mut cleanup = Vec::new();
            for closure in &mut closure_rows {
                cleanup.push(prune_unreachable_closure_groups(closure, &graph)?);
            }
            closure_rows.retain(|closure| !closure.choices.is_empty());
            write_json(
                &round.join("closure_reachability_cleanup.json"),
                &json!(cleanup),
            )?;
        }
        let mut objective_extension_rows = correlated_rows.clone();
        objective_extension_rows.extend(closure_rows.clone());
        let mut correlated_union_rows = Vec::new();
        let mut correlated_p1_rows = Vec::new();
        let mut legal_correlated_count = 0usize;
        let mut legal_closure_count = 0usize;
        if !objective_extension_rows.is_empty() {
            let correlated_plan = round.join("correlated_plan.json");
            write_json(
                &correlated_plan,
                &serde_json::to_value(&objective_extension_rows)?,
            )?;
            let correlated_materialized_dir = round.join("correlated_materialized");
            let correlated_equivalence: Vec<_> = objective_extension_rows
                .iter()
                .map(UnifiedEquivalenceCandidate::from_generator)
                .collect();
            let correlated_batch = materializer_impl::materialize_equivalence_in_memory(
                &parent,
                &correlated_equivalence,
                &correlated_materialized_dir,
                &shared_pins,
            )?;
            let correlated_trace: Vec<Value> = serde_json::from_slice(&fs::read(
                correlated_materialized_dir.join("materialization_trace.json"),
            )?)?;
            let correlated_legal: Vec<_> = correlated_trace
                .iter()
                .filter(|row| row["legal"].as_bool() == Some(true))
                .collect();
            anyhow::ensure!(correlated_batch.candidates.len() == correlated_legal.len());
            for (memory, receipt) in correlated_batch.candidates.iter().zip(&correlated_legal) {
                anyhow::ensure!(
                    memory.candidate_id == receipt["candidate_id"].as_str().unwrap(),
                    "correlated in-memory and receipt candidate order differs"
                );
            }
            let correlated_cell_names = correlated_batch
                .candidates
                .iter()
                .flat_map(|candidate| candidate.netlist.graph.node_weights())
                .map(|cell| cell.to_string())
                .filter(|name| name.ends_with("_ASAP7_6t_L") || name.ends_with("_ASAP7_75t_R"));
            database_cells_added += database.ensure_cell_names(correlated_cell_names, &liberty)?;
            shared_nldm
                .as_mut()
                .context("shared NLDM cache missing before correlated P1")?
                .extend(database.nldm_cache());
            correlated_p1_rows = evaluate_materialized_p1(
                &round.join("correlated_p1_exact.json"),
                &correlated_batch.module,
                &correlated_batch.candidates,
                shared_nldm.as_ref().context("shared NLDM cache missing")?,
                &objective,
            )?;
            let correlated_output_by_id: HashMap<_, _> = correlated_legal
                .iter()
                .map(|row| {
                    (
                        row["candidate_id"].as_str().unwrap().to_owned(),
                        row["output_netlist"].as_str().unwrap().to_owned(),
                    )
                })
                .collect();
            for candidate in &objective_extension_rows {
                if let Some(output) = correlated_output_by_id.get(&candidate.candidate_id) {
                    let mut row = generator_union_row(candidate, output)?;
                    if matches!(
                        candidate.mapping.as_deref(),
                        Some("objective-local-closure" | "iterative-d2ap-local-closure")
                    ) {
                        row["source_class"] = json!("generator-v2-local-closure");
                        row["provenance_class"] = json!(candidate.mapping);
                    } else {
                        row["source_class"] = json!("generator-v2-correlated");
                        row["provenance_class"] = json!("objective-correlated-multi-root");
                    }
                    correlated_union_rows.push(row);
                }
            }
            legal_correlated_count = correlated_batch
                .candidates
                .iter()
                .filter(|candidate| candidate.candidate_id.starts_with("V8CORR_"))
                .count();
            legal_closure_count = correlated_batch
                .candidates
                .iter()
                .filter(|candidate| {
                    candidate.candidate_id.starts_with("V8CLOSE_")
                        || candidate.candidate_id.starts_with("ITERATIVECLOSE_")
                })
                .count();
        }
        let correlated_sec = correlated_started.elapsed().as_secs_f64();

        reranker
            .join()
            .map_err(|_| anyhow::anyhow!("D1 reranker thread panicked"))??;
        let d1_rerank_sec = rerank_started.elapsed().as_secs_f64();
        let mut ranked_d1: Vec<Value> =
            serde_json::from_slice(&fs::read(d1_rerank.join("p3_ranked_candidates.json"))?)?;
        if objective.programmable_context().is_some() {
            ranked_d1.sort_by(|left, right| {
                let score = |row: &Value| {
                    objective.score(
                        row["p1_delay"].as_f64().unwrap_or(f64::INFINITY),
                        row["p1_area"].as_f64().unwrap_or(f64::INFINITY),
                        row["p1_power"].as_f64().unwrap_or(f64::INFINITY),
                    )
                };
                score(left).total_cmp(&score(right)).then_with(|| {
                    left["candidate_id"]
                        .as_str()
                        .cmp(&right["candidate_id"].as_str())
                })
            });
        }
        let output_by_id: HashMap<_, _> = legal
            .iter()
            .map(|row| {
                (
                    row["candidate_id"].as_str().unwrap().to_owned(),
                    row["output_netlist"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        let mut union_rows = Vec::new();
        for candidate in &macro_rows {
            if let Some(output) = output_by_id.get(&candidate.candidate_id) {
                union_rows.push(generator_union_row(candidate, output)?);
            }
        }
        let mut union_p1 = p1_rows;
        union_rows.extend(correlated_union_rows);
        union_p1.extend(correlated_p1_rows);
        union_rows.extend(local_rewrite_union_rows);
        union_p1.extend(local_rewrite_p1_rows);
        // Optional fixed topology sidecar for a single controlled replay.
        // The sidecar is injected only on round zero and only when the caller
        // explicitly supplies a complete mapped netlist.  It is therefore
        // absent from every ordinary Iterative trajectory and cannot alter
        // other anchors.  Progressive A2 and the normal strict acceptance
        // guard still decide whether this proposal survives.
        if round_index == 0 {
            if let Ok(extra_value) = std::env::var("EGG_ITERATIVE_EXTRA_NETLIST") {
                let extra_path = PathBuf::from(extra_value);
                anyhow::ensure!(
                    extra_path.is_file(),
                    "fixed Iterative sidecar missing: {}",
                    extra_path.display()
                );
                let added =
                    database.ensure_netlists(std::slice::from_ref(&extra_path), &liberty)?;
                database_cells_added += added;
                shared_nldm
                    .as_mut()
                    .context("shared NLDM cache missing for fixed Iterative sidecar")?
                    .extend(database.nldm_cache());
                let extra_netlist =
                    read_verilog_with_lib_to_netlist(&extra_path, shared_pins.clone())
                        .map_err(anyhow::Error::msg)?
                        .0;
                let extra_eval = SharedLoadEvaluator::from_nldm_cache(
                    shared_nldm.as_ref().context(
                        "shared NLDM cache missing for fixed Iterative sidecar evaluation",
                    )?,
                    true,
                )?
                .with_boundary(timing_boundary)
                .evaluate_netlist(&extra_netlist)?;
                // Progressive A2 keys P1 rows by the written input stem, so
                // keep the fixed sidecar id identical to that stem.
                let extra_id = extra_path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .context("fixed Iterative sidecar filename is not UTF-8")?;
                let extra_sha = sha(&extra_path)?;
                let mut union_row = json!({
                    "candidate_id":extra_id,
                    "source_class":"generator-v2",
                    "mapping":"timing",
                    "topology_signature":extra_sha,
                    "provenance_class":"cross-anchor-fixed-fusion",
                    "specific_window":"sin-t0-fixed-loo",
                    "region_id":"sin-t0-fixed-loo",
                    "region_size":5,
                    "output_netlist":extra_path,
                    "choices":[],
                    "proof_groups":[],
                });
                let unified = UnifiedEquivalenceCandidate::whole_netlist(
                    extra_id,
                    EquivalenceSource::ExternalSidecar,
                    "cross-anchor-fixed-fusion",
                );
                attach_unified_candidate(&mut union_row, &unified).map_err(anyhow::Error::msg)?;
                union_rows.push(union_row);
                union_p1.push(json!({
                    "model":timing_boundary.internal_timing_model.model_name(),
                    "input":extra_path,
                    "module":benchmark,
                    "delay":extra_eval.delay,
                    "area":extra_eval.area,
                    "power":extra_eval.power,
                    "score":objective.score(extra_eval.delay, extra_eval.area, extra_eval.power),
                    "finite":true,
                }));
                write_json(
                    &round.join("fixed_iterative_sidecar_receipt.json"),
                    &json!({
                        "candidate_id":extra_id,
                        "input":extra_path,
                        "sha256":extra_sha,
                        "internal":extra_eval,
                        "round":round_index,
                        "candidate_adaptation":false,
                    }),
                )?;
            }
        }
        let mut d1_count = 0usize;
        for row in ranked_d1
            .iter()
            .filter(|row| row["legal"].as_bool() == Some(true))
        {
            let mut union_row = json!({
                "candidate_id":row["candidate_id"],"source_class":"d1-rewrite",
                "mapping":"d1","topology_signature":row["topology_signature"],
                "provenance_class":"d1-rewrite","specific_window":row["region_id"],
                "region_id":row["region_id"],"region_size":row["region_size"],
                "p2_score":row["p2_score"],"changed_eclasses":row["changed_eclasses"],
                "output_netlist":row["output_netlist"],
            });
            let unified =
                UnifiedEquivalenceCandidate::from_d1_row(row).map_err(anyhow::Error::msg)?;
            attach_unified_candidate(&mut union_row, &unified).map_err(anyhow::Error::msg)?;
            union_rows.push(union_row);
            let p1_delay = row["p1_delay"].as_f64().context("D1 P1 delay")?;
            let p1_area = row["p1_area"].as_f64().context("D1 P1 area")?;
            let p1_power = row["p1_power"].as_f64().context("D1 P1 power")?;
            union_p1.push(json!({
                "model":timing_boundary.internal_timing_model.model_name(),
                "input":row["output_netlist"],"module":benchmark,
                "delay":p1_delay,"area":p1_area,
                "power":p1_power,"score":objective.score(p1_delay, p1_area, p1_power),"finite":true,
            }));
            d1_count += 1;
        }
        // Attribution-only fixed-topology lane.  Candidate construction above
        // is deliberately left untouched so the normal path remains byte-for-
        // byte stable; its results are discarded here.  The sole Phase-II
        // input is a copy of the current parent, so progressive A2 can change
        // drive strengths but cannot change connectivity or Boolean structure.
        if ablation_mode == "phase2-only" {
            let phase2_parent = round.join("phase2_parent.v");
            fs::copy(&parent, &phase2_parent)?;
            let phase2_id = "phase2_parent";
            let mut row = json!({
                "candidate_id":phase2_id,
                "source_class":"ablation-fixed-topology",
                "mapping":"fixed-parent",
                "topology_signature":parent_sha,
                "provenance_class":"phase2-only-parent",
                "specific_window":"whole-parent",
                "region_id":"whole-parent",
                "region_size":round_instance_count,
                "output_netlist":phase2_parent,
                "choices":[],
                "proof_groups":[],
            });
            let unified = UnifiedEquivalenceCandidate::whole_netlist(
                phase2_id,
                EquivalenceSource::ExternalSidecar,
                "phase2-only-parent",
            );
            attach_unified_candidate(&mut row, &unified).map_err(anyhow::Error::msg)?;
            union_rows = vec![row];
            union_p1 = vec![json!({
                "model":timing_boundary.internal_timing_model.model_name(),
                "input":phase2_parent,"module":benchmark,
                "delay":planner_parent_ppa.delay,"area":planner_parent_ppa.area,
                "power":planner_parent_ppa.power,"score":parent_search_score,"finite":true,
            })];
            d1_count = 0;
        }
        if union_rows.is_empty() {
            terminal_attempted_round = Some(round_index + 1);
            terminal_reason = Some("no_legal_union_candidates");
            write_json(
                &round.join("status.json"),
                &json!({
                    "stage":"terminal",
                    "reason":"no_legal_union_candidates",
                    "accepted":false,
                    "generated_candidates":macro_rows.len(),
                    "rejected_generator_candidates":macro_rows.len()-legal.len(),
                    "d1_union_candidates":d1_count
                }),
            )?;
            break;
        }
        validate_extraction_portfolio(&union_rows).map_err(anyhow::Error::msg)?;
        if unified_equivalence_enabled() {
            write_json(
                &round.join("equivalence_representation_receipt.json"),
                &json!({
                    "schema":"egg-unified-equivalence-portfolio-receipt-v1",
                    "candidate_count":union_rows.len(),
                    "ordinary_enode_candidates":union_rows.iter().filter(|row|
                        row["equivalence_candidate"]["source"].as_str()
                            == Some("primitive_egraph_rewrite")
                    ).count(),
                    "virtual_generated_candidates":union_rows.iter().filter(|row|
                        row["equivalence_candidate"]["source"].as_str()
                            == Some("compiled_contextual_rewrite")
                    ).count(),
                    "mapped_transaction_candidates":union_rows.iter().filter(|row| matches!(
                        row["equivalence_candidate"]["source"].as_str(),
                        Some("mapped_rewrite_closure" | "external_sidecar")
                    )).count(),
                    "candidate_order":union_rows.iter().map(|row|
                        row["candidate_id"].clone()
                    ).collect::<Vec<_>>(),
                    "ranking_inputs_unchanged":true,
                    "materialization_inputs_unchanged":true,
                }),
            )?;
        }
        let union_plan = round.join("union_plan.json");
        let union_p1_path = round.join("union_p1_exact.json");
        fs::write(
            &union_plan,
            serde_json::to_string_pretty(&union_rows)? + "\n",
        )?;
        fs::write(
            &union_p1_path,
            serde_json::to_string_pretty(&union_p1)? + "\n",
        )?;
        // Scale rules cover the normal drive families.  Generator/D1
        // topologies may nevertheless introduce a previously unseen legal
        // FULL-186 cell, so extend the process database lazily from the
        // complete union before A2 and before this accepted netlist can become
        // the next fresh parent.
        let database_extend_started = Instant::now();
        let d1_inputs: Vec<_> = ranked_d1
            .iter()
            .filter(|row| row["legal"].as_bool() == Some(true))
            .filter_map(|row| row["output_netlist"].as_str().map(PathBuf::from))
            .collect();
        let added_from_d1 = database.ensure_netlists(&d1_inputs, &liberty)?;
        database_cells_added += added_from_d1;
        shared_nldm
            .as_mut()
            .context("shared NLDM cache missing after D1 extension")?
            .extend(database.nldm_cache());
        let database_extend_sec = database_extend_started.elapsed().as_secs_f64();
        let progressive_dir = round.join("progressive");
        let progressive_started = Instant::now();
        let progressive_args = [
            "progressive".to_owned(),
            union_plan.display().to_string(),
            union_p1_path.display().to_string(),
            progressive_dir.display().to_string(),
            liberty.display().to_string(),
            scale_rules.display().to_string(),
            parent_search_score.to_string(),
            resource_jobs(32),
        ];
        if objective.programmable_context().is_some() {
            a2_impl::progressive_native_with_database_objective(
                &progressive_args,
                database,
                database_load_sec,
                objective.clone(),
            )?;
        } else {
            a2_impl::progressive_native_with_database(
                &progressive_args,
                database,
                database_load_sec,
            )?;
        }
        let progressive_sec = progressive_started.elapsed().as_secs_f64();
        let progressive: Value =
            serde_json::from_slice(&fs::read(progressive_dir.join("summary.json"))?)?;
        let leader = progressive["leader"].as_str().context("leader")?.to_owned();
        let proposed = progressive["final_best_path"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                progressive_dir
                    .join("stage_500")
                    .join(&leader)
                    .join("best.v")
            });
        let candidate_ppa = PpaPoint {
            delay: progressive["final_reparse_exact"]["delay"]
                .as_f64()
                .context("candidate delay")?,
            area: progressive["final_reparse_exact"]["area"]
                .as_f64()
                .context("candidate area")?,
            power: progressive["final_reparse_exact"]["power"]
                .as_f64()
                .context("candidate power")?,
        };
        let candidate_search_score = progressive["final_reparse_exact"]["score"]
            .as_f64()
            .context("candidate search score")?;
        let proposed_ppa = a2_impl::evaluate_written_netlist_with_database_objective(
            &proposed,
            database,
            objective.clone(),
        )?;
        for (name, reparsed, reported) in [
            ("delay", proposed_ppa.delay, candidate_ppa.delay),
            ("area", proposed_ppa.area, candidate_ppa.area),
            ("power", proposed_ppa.power, candidate_ppa.power),
        ] {
            anyhow::ensure!(
                (reparsed - reported).abs() <= 2e-7_f64.max(64.0 * f64::EPSILON * reported.abs()),
                "outer controller proposed-netlist {name} does not match progressive cold reparse"
            );
        }
        let candidate_exact_objective = if matches!(objective, SearchObjective::D2ap) {
            // Preserve the exact frozen score emitted by progressive A2; a
            // second multiplication can differ by one final floating bit.
            candidate_search_score
        } else {
            objective.exact_value(candidate_ppa)?
        };
        let candidate = json!({
            "model":timing_boundary.internal_timing_model.model_name(),
            "input":proposed,"module":benchmark,
            "delay":progressive["final_reparse_exact"]["delay"],
            "area":progressive["final_reparse_exact"]["area"],
            "power":progressive["final_reparse_exact"]["power"],
            "score":progressive["final_reparse_exact"]["score"],
            "exact_objective_value":candidate_exact_objective,"finite":true,
        });
        let candidate_score = candidate_search_score;
        let candidate_sha = sha(&proposed)?;
        let incumbent_point = incumbent_ppa.context("incumbent PPA missing")?;
        let strict_improvement = if candidate_sha == incumbent_sha {
            false
        } else if matches!(objective, SearchObjective::D2ap) {
            // Keep the frozen V7 acceptance expression byte-for-byte.
            candidate_score < parent_score - 1e-9
        } else {
            objective.strictly_better(candidate_ppa, incumbent_point)?
        };
        let accepted = round.join("accepted.v");
        let mut round_primal_dual_update = None;
        let mut accepted_sha = None;
        let mut transit_receipt = None;
        let mut parent_action = "terminal";
        let transit_pool = progressive["transit_pool"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if strict_improvement {
            fs::copy(&proposed, &accepted)?;
            parent = accepted.clone();
            parent_origin = "improvement".to_owned();
            let written_sha = sha(&accepted)?;
            incumbent_path = accepted.clone();
            incumbent_sha = written_sha.clone();
            incumbent_ppa = Some(candidate_ppa);
            best_score = Some(candidate_exact_objective);
            current_ppa = Some(candidate_ppa);
            visited_parent_shas.insert(written_sha.clone());
            if let Some(topology) = transit_pool
                .iter()
                .find(|row| row["candidate_id"].as_str() == Some(&leader))
                .and_then(|row| row["topology_signature"].as_str())
            {
                visited_topology_signatures.insert(topology.to_owned());
            }
            accepted_rounds += 1;
            accepted_sha = Some(written_sha);
            parent_action = "improvement";
            let (updated_objective, update) = objective.with_primal_dual_update(candidate_ppa)?;
            if let Some(update) = update {
                let audit = json!({
                    "stage":format!("after_round_{}", round_index+1),
                    "point":candidate_ppa,
                    "update":update,
                });
                primal_dual_updates.push(audit.clone());
                round_primal_dual_update = Some(audit);
            }
            objective = updated_objective;
        } else if objective_engine.as_deref() == Some("v8-pareto")
            && objective.programmable_context().is_some()
            && round_index + 1 < max_rounds
        {
            if let Some(selected) = select_plateau_transit(
                &transit_pool,
                &objective,
                incumbent_point,
                &visited_parent_shas,
                &visited_topology_signatures,
            )? {
                let selected_path = PathBuf::from(
                    selected["best_path"]
                        .as_str()
                        .context("selected transit path")?,
                );
                let selected_point = transit_point(&selected)?;
                let reparsed_point = a2_impl::evaluate_written_netlist_with_database_objective(
                    &selected_path,
                    database,
                    objective.clone(),
                )?;
                for (metric, recorded, reparsed) in [
                    ("delay", selected_point.delay, reparsed_point.delay),
                    ("area", selected_point.area, reparsed_point.area),
                    ("power", selected_point.power, reparsed_point.power),
                ] {
                    anyhow::ensure!(
                        (recorded - reparsed).abs()
                            <= 2e-7_f64.max(64.0 * f64::EPSILON * recorded.abs()),
                        "transit write/reparse Exact {metric} mismatch: {recorded} vs {reparsed}"
                    );
                }
                anyhow::ensure!(
                    objective.feasible_ppa(
                        reparsed_point.delay,
                        reparsed_point.area,
                        reparsed_point.power
                    ),
                    "selected plateau transit is not feasible after cold parse"
                );
                anyhow::ensure!(
                    objective.exact_objective_equivalent(reparsed_point, incumbent_point)?,
                    "selected plateau transit is not objective-equivalent to incumbent"
                );
                let transit_path = round.join("transit.v");
                fs::copy(&selected_path, &transit_path)?;
                let selected_sha = sha(&transit_path)?;
                anyhow::ensure!(
                    selected_sha == selected["best_sha256"].as_str().unwrap_or_default(),
                    "transit copy SHA mismatch"
                );
                let topology = selected["topology_signature"]
                    .as_str()
                    .context("selected transit topology")?
                    .to_owned();
                visited_parent_shas.insert(selected_sha.clone());
                visited_topology_signatures.insert(topology.clone());
                parent = transit_path.clone();
                parent_origin = "transit".to_owned();
                current_ppa = Some(reparsed_point);
                parent_action = "transit";
                transit_count += 1;
                let audit = json!({
                    "round":round_index+1,
                    "candidate_id":selected["candidate_id"],
                    "source_class":selected["source_class"],
                    "topology_signature":topology,
                    "sha256":selected_sha,
                    "path":transit_path,
                    "point":reparsed_point,
                    "minimum_relative_constraint_slack":selected["minimum_relative_constraint_slack"],
                    "exact_stage_budget":selected["exact_stage_budget"],
                    "endpoint_exact_evaluations":selected["exact_evaluations"],
                    "selection_additional_exact":0,
                    "qor_improvement":false,
                });
                transit_rounds.push(audit.clone());
                transit_receipt = Some(audit);
                let (updated_objective, update) =
                    objective.with_primal_dual_update(reparsed_point)?;
                if let Some(update) = update {
                    let update_audit = json!({
                        "stage":format!("after_round_{}_transit", round_index+1),
                        "point":reparsed_point,
                        "update":update,
                    });
                    primal_dual_updates.push(update_audit.clone());
                    round_primal_dual_update = Some(update_audit);
                }
                objective = updated_objective;
            }
        }
        let logical_exact = progressive["total_exact"].as_u64().unwrap() as usize;
        let physical_exact = progressive["physical_total_exact"].as_u64().unwrap() as usize;
        total_exact += logical_exact;
        total_physical_exact += physical_exact;
        if round_parent_origin == "transit" {
            transit_followup_search_exact += logical_exact;
            transit_followup_physical_exact += physical_exact;
        }
        let ultra_relative_gain = strict_improvement.then(|| {
            ((parent_score - candidate_exact_objective) / parent_score.abs().max(1e-30)).max(0.0)
        });
        let mut ultra_stop_now = false;
        if ultra_large_round {
            if let Some(config) = ultra_config
                .as_ref()
                .filter(|config| config.early_stop_enabled())
            {
                if ultra_relative_gain
                    .is_some_and(|gain| gain < config.early_stop_min_relative_gain)
                {
                    ultra_saturation_streak += 1;
                } else {
                    ultra_saturation_streak = 0;
                }
                ultra_stop_now = round_index + 1 >= config.early_stop_min_rounds
                    && ultra_saturation_streak >= config.early_stop_patience;
                if ultra_stop_now {
                    ultra_stop_receipt = Some(json!({
                        "reason":"gain_saturation",
                        "round":round_index+1,
                        "instance_count":round_instance_count,
                        "relative_gain":ultra_relative_gain,
                        "minimum_relative_gain":config.early_stop_min_relative_gain,
                        "patience":config.early_stop_patience,
                        "saturation_streak":ultra_saturation_streak,
                    }));
                }
            }
        }
        let elapsed = round_started.elapsed().as_secs_f64();
        let receipt = json!({
            "round":round_index+1,"parent_path":round_parent_path,
            "parent_origin":round_parent_origin,
            "parent_sha256":parent_sha,"parent_score":parent_score,
            "objective":objective.name(),"delay_cap_ps":objective.delay_cap_ps(),
            "objective_relative_weights":planner_objective_weights.map(|weights| json!({
                "delay":weights.delay,"area":weights.area,"power":weights.power,
            })),
            "generator_realization_weights":generator_realization_weights,
            "primal_dual_update":round_primal_dual_update,
            "region_plan_sha256":sha(&planner_dir.join("dual_region_plan_full.json"))?,
            "macro_plan_semantic_sha256":sha(&macro_plan)?,
            "instance_count":round_instance_count,
            "generated_candidates_before_ultra_cap":generated_candidates_before_ultra_cap,
            "v8_ultra_candidate_cap":ultra_candidate_cap,
            "v8_ultra_relative_gain":ultra_relative_gain,
            "v8_ultra_saturation_streak":ultra_saturation_streak,
            "v8_ultra_stop_after_round":ultra_stop_now,
            "generated_candidates":macro_rows.len(),"d1_union_candidates":d1_count,
            "deletion_candidates":deletion_candidate_count,
            "d2ap_functional_window_active":d2ap_functional_window_active,
            "correlated_candidates":correlated_rows.len(),
            "legal_correlated_candidates":legal_correlated_count,
            "local_closure_candidates":closure_rows.len(),
            "legal_local_closure_candidates":legal_closure_count,
            "local_rewrite_closure_candidates":union_rows.iter().filter(|row| row["source_class"].as_str()==Some("objective-local-rewrite-closure")).count(),
            "local_rewrite_closure_exact":local_rewrite_exact,
            "union_candidates":union_rows.len(),"legal_candidates":legal.len(),
            "rejected_generator_candidates":macro_rows.len()-legal.len(),
            "progressive_exact":logical_exact,"progressive_physical_exact":physical_exact,
            "leader":leader,"candidate":candidate,"candidate_sha256":candidate_sha,
            "strict_improvement":strict_improvement,
            "parent_action":parent_action,
            "accepted_path":if strict_improvement {Some(accepted.display().to_string())} else {None},
            "accepted_sha256":accepted_sha,
            "transit":transit_receipt,
            "stage_time_sec":{
                "shared_physical_database_load_once":database_load_sec,
                "shared_physical_database_extend":database_extend_sec,
                "shared_physical_database_cells_added":database_cells_added,
                "planner":planner_sec,"planner_profile":planner_summary["profile_sec"],
                "generator_native":generator_sec,"materialization_and_equivalence":materializer_sec,
                "p1_exact_all_candidates":p1_sec,"d1_materialization_and_p1":d1_rerank_sec,
                "correlated_select_materialize_equivalence_p1":correlated_sec,
                "local_rewrite_closure":local_rewrite_sec,
                "d1_p1_overlapped_with_generator_pipeline":true,
                "progressive_native_including_final_reparse":progressive_sec,
            },
            "elapsed_sec":elapsed,
        });
        write_json(&round.join("round_receipt.json"), &receipt)?;
        rounds.push(receipt);
        let summary = json!({
            "method":match ablation_mode.as_str() {
                "phase1-only" => "V8 Phase-I-only unified extraction + fresh-parent loop",
                "phase2-only" => "V8 Phase-II-only fixed-topology sizing + fresh-parent loop",
                _ if unified_equivalence_enabled() => "D1 ordinary enodes + Generator virtual enodes through unified topology extraction + native progressive refinement + fresh-parent loop",
                _ => "D1 rewrite + native Rust Generator V2 union + native progressive refinement + fresh-parent loop",
            },
            "unified_equivalence_representation":unified_equivalence_enabled(),
            "phase_ablation_mode":ablation_mode,
            "phase_i_drive_expansion_enabled":phase_i_drive_expansion_enabled,
            "objective_engine":objective_engine,
            "objective_local_rewrite_closure_enabled":objective_local_rewrite_enabled,
            "objective_mapped_window_rewrite_enabled":objective_mapped_window_enabled,
            "objective_compatible_closure_enabled":objective_compatible_closure_enabled,
            "objective_bounded_functional_window_enabled":objective_bounded_functional_enabled,
            "v8_ultra_config_path":ultra_config_path,
            "v8_ultra_config":ultra_config,
            "v8_ultra_stop":ultra_stop_receipt,
            "objective":objective.name(),"delay_cap_ps":objective.delay_cap_ps(),
            "objective_spec":objective.programmable_context().map(|context| context.spec()),
            "primal_dual_policy":if objective.programmable_context().is_some() {Some(primal_dual_policy)} else {None},
            "primal_dual_updates":primal_dual_updates.clone(),
            "benchmark":benchmark,"variant":variant,"region_policy":"hybrid",
            "timing_boundary":timing_boundary,
            "max_rounds":max_rounds,"rounds_executed":rounds.len(),
            "accepted_rounds":accepted_rounds,"g0_score":g0_score,
            "final_score":best_score,"final_over_g0":best_score.unwrap()/g0_score.unwrap(),
            "incumbent_path":incumbent_path,
            "incumbent_sha256":incumbent_sha,
            "incumbent_ppa":incumbent_ppa,
            "transit_count":transit_count,"transit_rounds":transit_rounds.clone(),
            "transit_followup_search_exact":transit_followup_search_exact,
            "transit_followup_physical_exact":transit_followup_physical_exact,
            "total_search_exact":total_exact,"total_physical_search_exact":total_physical_exact,
            "total_local_rewrite_closure_exact":total_local_rewrite_closure_exact,
            "total_all_search_exact":total_exact+total_local_rewrite_closure_exact,
            "terminal_attempted_round":terminal_attempted_round,
            "terminal_reason":terminal_reason,
            "rounds":rounds,"elapsed_sec":started.elapsed().as_secs_f64(),
            "current_process_elapsed_sec":started.elapsed().as_secs_f64(),
            "python_runtime_dependencies":0,
        });
        write_json(&out.join("summary.json"), &summary)?;
        write_json(
            &round.join("status.json"),
            &json!({
                "stage":"complete","accepted":strict_improvement,
                "parent_action":parent_action,"transit_selected":parent_action=="transit",
                "candidate_score":candidate_score,
            }),
        )?;
        if ultra_stop_now {
            terminal_attempted_round = Some(round_index + 1);
            terminal_reason = Some("v8_ultra_gain_saturation");
            break;
        }
        if parent_action == "terminal" {
            terminal_attempted_round = Some(round_index + 1);
            terminal_reason = Some("no_strict_improvement");
            break;
        }
    }
    let final_incumbent = out.join("final_incumbent.v");
    fs::copy(&incumbent_path, &final_incumbent)?;
    let final_incumbent_sha = sha(&final_incumbent)?;
    anyhow::ensure!(
        final_incumbent_sha == incumbent_sha,
        "final incumbent copy SHA mismatch"
    );
    let summary = json!({
        "method":match ablation_mode.as_str() {
            "phase1-only" => "V8 Phase-I-only unified extraction + fresh-parent loop",
            "phase2-only" => "V8 Phase-II-only fixed-topology sizing + fresh-parent loop",
            _ if unified_equivalence_enabled() => "D1 ordinary enodes + Generator virtual enodes through unified topology extraction + native progressive refinement + fresh-parent loop",
            _ => "D1 rewrite + native Rust Generator V2 union + native progressive refinement + fresh-parent loop",
        },
        "unified_equivalence_representation":unified_equivalence_enabled(),
        "phase_ablation_mode":ablation_mode,
        "phase_i_drive_expansion_enabled":phase_i_drive_expansion_enabled,
        "objective_engine":objective_engine,
        "objective_local_rewrite_closure_enabled":objective_local_rewrite_enabled,
        "objective_mapped_window_rewrite_enabled":objective_mapped_window_enabled,
        "objective_compatible_closure_enabled":objective_compatible_closure_enabled,
        "objective_bounded_functional_window_enabled":objective_bounded_functional_enabled,
        "v8_ultra_config_path":ultra_config_path,
        "v8_ultra_config":ultra_config,
        "v8_ultra_stop":ultra_stop_receipt,
        "objective":objective.name(),"delay_cap_ps":objective.delay_cap_ps(),
        "objective_spec":objective.programmable_context().map(|context| context.spec()),
        "primal_dual_policy":if objective.programmable_context().is_some() {Some(primal_dual_policy)} else {None},
        "primal_dual_updates":primal_dual_updates,
        "benchmark":benchmark,"variant":variant,"region_policy":"hybrid",
        "timing_boundary":timing_boundary,
        "max_rounds":max_rounds,"rounds_executed":rounds.len(),
        "accepted_rounds":accepted_rounds,"g0_score":g0_score,
        "final_score":best_score,"final_over_g0":best_score.unwrap()/g0_score.unwrap(),
        "incumbent_path":final_incumbent,
        "incumbent_sha256":incumbent_sha,
        "incumbent_ppa":incumbent_ppa,
        "transit_count":transit_count,"transit_rounds":transit_rounds,
        "transit_followup_search_exact":transit_followup_search_exact,
        "transit_followup_physical_exact":transit_followup_physical_exact,
        "total_search_exact":total_exact,"total_physical_search_exact":total_physical_exact,
        "total_local_rewrite_closure_exact":total_local_rewrite_closure_exact,
        "total_all_search_exact":total_exact+total_local_rewrite_closure_exact,
        "terminal_attempted_round":terminal_attempted_round,
        "terminal_reason":terminal_reason,
        "rounds":rounds,"elapsed_sec":started.elapsed().as_secs_f64(),
        "current_process_elapsed_sec":started.elapsed().as_secs_f64(),
        "python_runtime_dependencies":0,
    });
    write_json(&out.join("summary.json"), &summary)?;
    print!("{}\n", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

#[cfg(test)]
mod controller_tests {
    use super::*;
    use d1_series::generator_v2::{GeneratorChoice, GeneratorExpr};
    use d1_series::objective::{ConstraintRelation, Metric, MetricConstraint};

    fn area_under_delay() -> SearchObjective {
        let spec = ObjectiveSpec {
            schema_version: 1,
            name: "test-area-under-delay".to_owned(),
            objective: ObjectiveFunction::MinimizeMetric {
                metric: Metric::Area,
            },
            constraints: vec![MetricConstraint {
                metric: Metric::Delay,
                relation: ConstraintRelation::AtMost,
                bound: 100.0,
            }],
        };
        SearchObjective::programmable(
            spec.bind(PpaPoint {
                delay: 100.0,
                area: 10.0,
                power: 4.0,
            })
            .unwrap(),
            PrimalDualPolicy::default(),
        )
        .unwrap()
    }

    fn transit_row(id: &str, sha: &str, topology: &str, delay: f64) -> Value {
        json!({
            "candidate_id":id,"source_class":"test",
            "topology_signature":topology,"best_sha256":sha,
            "point":{"delay":delay,"area":10.0,"power":5.0},
            "feasible":true,"minimum_relative_constraint_slack":(100.0-delay)/100.0,
            "exact_objective_value":10.0,"exact_stage_budget":25,
        })
    }

    fn topology_move(id: &str, window: &str, root: usize, boundary: usize) -> GeneratorCandidate {
        GeneratorCandidate {
            candidate_id: id.to_owned(),
            provenance: vec![
                "multi-output-shared-host-resynthesis".to_owned(),
                window.to_owned(),
            ],
            choices: vec![GeneratorChoice {
                root_anchor: root,
                expression: GeneratorExpr::Anchor { anchor: boundary },
            }],
            tech_area: 1.0,
            tech_delay: 1.0,
            logic_depth: 1,
            cell_families: vec!["INV".to_owned()],
            outputs: vec![root],
            window_kind: "test".to_owned(),
            proof_boundary: vec![boundary],
            proof_outputs: vec![root],
            proof_groups: Vec::new(),
            shared_host: None,
            divisors: Vec::new(),
            replaced_anchor: None,
            logical_signature: None,
            mapping: Some("test".to_owned()),
            source_windows: vec!["test".to_owned()],
        }
    }

    #[test]
    fn closure_cleanup_removes_consumed_groups_and_preserves_reachable_choices() {
        let graph = json!({"occurrences":[
            {"anchor":0,"inputs":[],"is_root":false},
            {"anchor":1,"inputs":[0],"is_root":false},
            {"anchor":2,"inputs":[1],"is_root":false},
            {"anchor":3,"inputs":[2],"is_root":true}
        ]});
        let mut c = topology_move("cleanup", "S0", 2, 0);
        c.choices.push(GeneratorChoice {
            root_anchor: 1,
            expression: GeneratorExpr::Anchor { anchor: 0 },
        });
        c.proof_groups = c
            .choices
            .iter()
            .map(|choice| GeneratorProofGroup {
                choices: vec![choice.clone()],
                boundary: vec![0],
                outputs: vec![choice.root_anchor],
            })
            .collect();
        let audit = prune_unreachable_closure_groups(&mut c, &graph).unwrap();
        assert_eq!(audit["removed_roots"], json!([1]));
        assert_eq!(c.choices.len(), 1);
        assert_eq!(c.choices[0].root_anchor, 2);
        assert_eq!(c.proof_groups.len(), 1);
        assert_eq!(c.proof_outputs, vec![2]);
        let retained = c.choices.clone();
        let again = prune_unreachable_closure_groups(&mut c, &graph).unwrap();
        assert_eq!(again["removed_roots"], json!([]));
        assert_eq!(c.choices, retained);
        // Each cross substitution is acyclic alone; together they form 1<->2.
        let graph = json!({"occurrences":[
            {"anchor":0,"inputs":[],"is_root":false},
            {"anchor":1,"inputs":[0],"is_root":false},
            {"anchor":2,"inputs":[0],"is_root":false},
            {"anchor":3,"inputs":[1,2],"is_root":true}
        ]});
        c.choices = vec![
            GeneratorChoice {
                root_anchor: 1,
                expression: GeneratorExpr::Anchor { anchor: 2 },
            },
            GeneratorChoice {
                root_anchor: 2,
                expression: GeneratorExpr::Anchor { anchor: 1 },
            },
        ];
        c.proof_groups = c
            .choices
            .iter()
            .map(|choice| GeneratorProofGroup {
                choices: vec![choice.clone()],
                boundary: vec![0],
                outputs: vec![choice.root_anchor],
            })
            .collect();
        let audit = prune_unreachable_closure_groups(&mut c, &graph).unwrap();
        assert_eq!(audit["cycles_removed"].as_array().unwrap().len(), 1);
        assert_eq!(c.choices.len(), 1);
        assert_eq!(c.choices[0].root_anchor, 1);
    }

    #[test]
    fn plateau_transit_prefers_slack_and_filters_visited_topology() {
        let objective = area_under_delay();
        let incumbent = PpaPoint {
            delay: 99.0,
            area: 10.0,
            power: 4.0,
        };
        let pool = vec![
            transit_row("low-slack", "sha-a", "topo-a", 98.0),
            transit_row("high-slack", "sha-b", "topo-b", 90.0),
        ];
        let selected = select_plateau_transit(
            &pool,
            &objective,
            incumbent,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(selected["candidate_id"], "high-slack");

        let selected = select_plateau_transit(
            &pool,
            &objective,
            incumbent,
            &BTreeSet::new(),
            &BTreeSet::from(["topo-b".to_owned()]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(selected["candidate_id"], "low-slack");
    }

    #[test]
    fn correlated_composition_requires_distinct_windows_and_nonconflicting_cuts() {
        let left = topology_move("left", "S0_1_H2_STAR", 2, 1);
        let independent = topology_move("right", "S1_3_H4_STAR", 4, 3);
        assert!(topology_moves_compatible(&left, &independent));

        let same_window = topology_move("same", "S0_1_H2_STAR", 6, 5);
        assert!(!topology_moves_compatible(&left, &same_window));

        let depends_on_left = topology_move("dependent", "S2_7_H8_STAR", 8, 2);
        assert!(!topology_moves_compatible(&left, &depends_on_left));
    }

    #[test]
    fn ultra_candidate_cap_preserves_each_structural_bucket_when_capacity_allows() {
        let mut candidates = Vec::new();
        for region in 0..4 {
            for variant in 0..3 {
                let mut candidate = topology_move(
                    &format!("r{region}-v{variant}"),
                    &format!("S{region}_H{region}_STAR"),
                    2 * region + 1,
                    2 * region,
                );
                candidate.tech_area = 1.0 + variant as f64;
                if region == 3 {
                    candidate.provenance[0] = "multi-output-shared-dag".to_owned();
                    candidate.choices.push(GeneratorChoice {
                        root_anchor: 100 + region,
                        expression: GeneratorExpr::Anchor {
                            anchor: 200 + region,
                        },
                    });
                }
                candidate.mapping = Some("area".to_owned());
                candidates.push(candidate);
            }
        }
        let (selected, audit) = cap_generator_candidates(candidates, 4);
        assert_eq!(selected.len(), 4);
        assert_eq!(audit["bucket_count"], 4);
        assert!(selected.iter().any(|row| {
            row.provenance.first().map(String::as_str) == Some("multi-output-shared-dag")
        }));
        assert!(selected.iter().all(|row| row.candidate_id.ends_with("-v0")));
    }

    #[test]
    fn ultra_candidate_cap_reserves_rare_multi_output_bucket_when_overfull() {
        let mut candidates: Vec<_> = (0..10)
            .map(|region| {
                topology_move(
                    &format!("r{region}"),
                    &format!("S{region}_H{region}_STAR"),
                    2 * region + 1,
                    2 * region,
                )
            })
            .collect();
        candidates[9].provenance[0] = "multi-output-shared-dag".to_owned();
        let (selected, audit) = cap_generator_candidates(candidates, 3);
        assert_eq!(selected.len(), 3);
        assert_eq!(audit["bucket_count"], 10);
        assert!(selected.iter().any(|row| row.candidate_id == "r9"));
    }

    #[test]
    fn correlated_set_accumulates_generic_pressure_and_checks_every_root() {
        let first = topology_move("first", "S0_1_H2_STAR", 2, 1);
        let second = topology_move("second", "S1_3_H4_STAR", 4, 3);
        let third = topology_move("third", "S2_5_H6_STAR", 6, 5);
        let conflicts_with_first = topology_move("conflict", "S3_7_H8_STAR", 8, 2);
        let observations = vec![
            CorrelatedObservation {
                candidate: &first,
                point: PpaPoint {
                    delay: 99.0,
                    area: 9.9,
                    power: 4.0,
                },
                objective_delta: -0.01,
                slack_delta: vec![0.01],
            },
            CorrelatedObservation {
                candidate: &second,
                point: PpaPoint {
                    delay: 101.0,
                    area: 9.8,
                    power: 4.0,
                },
                objective_delta: -0.02,
                slack_delta: vec![-0.01],
            },
            CorrelatedObservation {
                candidate: &third,
                point: PpaPoint {
                    delay: 100.5,
                    area: 9.95,
                    power: 4.0,
                },
                objective_delta: -0.005,
                slack_delta: vec![-0.005],
            },
            CorrelatedObservation {
                candidate: &conflicts_with_first,
                point: PpaPoint {
                    delay: 98.0,
                    area: 9.9,
                    power: 4.0,
                },
                objective_delta: -0.01,
                slack_delta: vec![0.02],
            },
        ];
        let pair = correlated_set_metrics(vec![0, 1], &observations, &[0.0]);
        assert!(set_accepts_member(&pair, 2, &observations));
        assert!(!set_accepts_member(&pair, 3, &observations));
        let triple = correlated_set_metrics(vec![0, 1, 2], &observations, &[0.0]);
        assert!((triple.predicted_objective_delta + 0.035).abs() < 1e-12);
        assert!((triple.predicted_min_slack + 0.005).abs() < 1e-12);
        assert!((triple.predicted_violation - 0.005).abs() < 1e-12);
    }

    #[test]
    fn local_closure_packs_more_than_four_independent_transactions() {
        let objective = area_under_delay();
        let candidates: Vec<_> = (0..6)
            .map(|index| {
                topology_move(
                    &format!("move-{index}"),
                    &format!("S{index}_{}_H{}_STAR", index * 2, index * 2 + 1),
                    index * 2 + 1,
                    index * 2,
                )
            })
            .collect();
        let materialized: Vec<_> = candidates
            .iter()
            .map(|candidate| materializer_impl::MaterializedCandidate {
                candidate_id: candidate.candidate_id.clone(),
                output_netlist: PathBuf::from(format!("{}.v", candidate.candidate_id)),
                netlist: Default::default(),
            })
            .collect();
        let receipts: Vec<_> = candidates
            .iter()
            .map(|candidate| {
                json!({
                    "candidate_id":candidate.candidate_id,
                    "changed_eclasses":1,
                    "legal":true,
                })
            })
            .collect();
        let legal: Vec<_> = receipts.iter().collect();
        let p1: Vec<_> = (0..candidates.len())
            .map(|index| {
                json!({
                    "delay":99.0,
                    "area":9.9-index as f64*0.01,
                    "power":4.0,
                })
            })
            .collect();
        let (closures, audit) = objective_local_closure_candidates(
            &candidates,
            &materialized,
            &legal,
            &p1,
            PpaPoint {
                delay: 100.0,
                area: 10.0,
                power: 4.0,
            },
            &objective,
            3,
            32,
            LocalClosureScope::All,
            None,
        )
        .unwrap();
        assert!(!closures.is_empty());
        assert!(
            closures
                .iter()
                .any(|closure| closure.proof_groups.len() > 4)
        );
        assert!(
            audit
                .iter()
                .any(|row| row["transaction_count"].as_u64().unwrap() > 4)
        );
    }

    #[test]
    fn local_closure_is_empty_before_any_d2ap_state_is_constructed() {
        let (closures, audit) = objective_local_closure_candidates(
            &[],
            &[],
            &[],
            &[],
            PpaPoint {
                delay: 1.0,
                area: 1.0,
                power: 1.0,
            },
            &SearchObjective::D2ap,
            3,
            32,
            LocalClosureScope::All,
            None,
        )
        .unwrap();
        assert!(closures.is_empty());
        assert!(audit.is_empty());
    }

    #[test]
    fn iterative_d2ap_closure_builds_progressive_cardinality_candidates() {
        let candidates: Vec<_> = (0..6)
            .map(|index| {
                topology_move(
                    &format!("move-{index}"),
                    &format!("S{index}_{}_H{}_STAR", index * 2, index * 2 + 1),
                    index * 2 + 1,
                    index * 2,
                )
            })
            .collect();
        let materialized: Vec<_> = candidates
            .iter()
            .map(|candidate| materializer_impl::MaterializedCandidate {
                candidate_id: candidate.candidate_id.clone(),
                output_netlist: PathBuf::from(format!("{}.v", candidate.candidate_id)),
                netlist: Default::default(),
            })
            .collect();
        let receipts: Vec<_> = candidates
            .iter()
            .map(|candidate| json!({"candidate_id":candidate.candidate_id,"changed_eclasses":1,"legal":true}))
            .collect();
        let legal: Vec<_> = receipts.iter().collect();
        let p1: Vec<_> = (0..candidates.len())
            .map(|index| json!({"delay":99.0,"area":9.9-index as f64*0.01,"power":4.0}))
            .collect();
        let (closures, audit) = objective_local_closure_candidates(
            &candidates,
            &materialized,
            &legal,
            &p1,
            PpaPoint {
                delay: 100.0,
                area: 10.0,
                power: 4.0,
            },
            &SearchObjective::D2ap,
            3,
            6,
            LocalClosureScope::All,
            Some(0.002),
        )
        .unwrap();
        assert_eq!(closures.len(), 3);
        assert_eq!(audit[0]["transaction_count"], 2);
        assert_eq!(audit[1]["transaction_count"], 3);
        assert_eq!(audit[2]["transaction_count"], 6);
        assert!(
            closures
                .iter()
                .all(|row| row.candidate_id.starts_with("ITERATIVECLOSE_"))
        );
    }
}
