//! Frozen P3/P1 reranking in Rust with shared Liberty/NLDM state.

use anyhow::{Context, Result, bail};
use d1_series::shared_load_v2::{SharedLoadEvaluator, TimingBoundary};
use d1_series::topology_signature::{capped_topology_indices, topology_signature};
use extraction_gym::ExtendedEGraph;
use mac_egg::io::liberty::{Library, get_direction_of_pins, read_liberty};
use mac_egg::io::stdcell::read_verilog_with_lib_to_netlist;
use mac_egg::netlist_to_egg_roots_with_provenance;
use mac_egg::physical_scale_egraph::build_occurrence_preserving_original_space;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

const DEFAULT_RULES: [&str; 5] = [
    "test/6t_scale_full_rules.json",
    "test/6t_inv_rules.json",
    "test/6t_dmg_rules.json",
    "test/6t_comm_rules.json",
    "test/6t_expand_rules.json",
];

fn write_json(path: &Path, value: &Value) -> Result<()> {
    fs::write(path, serde_json::to_string_pretty(value)? + "\n")?;
    Ok(())
}

fn evaluate_legacy(
    input: &Path,
    pins: &Library,
    lib_content: &str,
    shared: &mut HashMap<String, extraction_gym::NLDM>,
) -> Result<Value> {
    let (netlist, _) =
        read_verilog_with_lib_to_netlist(input, pins.clone()).map_err(anyhow::Error::msg)?;
    let provenance =
        netlist_to_egg_roots_with_provenance::<_, ()>(&netlist).map_err(anyhow::Error::msg)?;
    let space = build_occurrence_preserving_original_space(&netlist, &provenance)
        .map_err(anyhow::Error::msg)?;
    let serialized = serde_json::to_value(&space.egraph)?;
    let ext = ExtendedEGraph::from_base_to_extention_with_shared_nldm(
        space.egraph,
        &serialized,
        lib_content,
        shared,
    );
    let cost = space.original_extraction.dag_cost_nldm_v2_on_pruned(&ext);
    let delay = cost.components[0].into_inner();
    let area = cost.components[1].into_inner();
    let power = cost.components[2].into_inner();
    Ok(json!({"delay":delay,"area":area,"power":power,"score":delay*delay*area*power}))
}

fn sha(path: &Path) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(fs::read(path)?)))
}

fn merge(mut left: Map<String, Value>, right: &Map<String, Value>) -> Map<String, Value> {
    left.extend(right.iter().map(|(k, v)| (k.clone(), v.clone())));
    left
}

fn map_topology_signature(row: &Map<String, Value>) -> String {
    if let Some(signature) = row.get("topology_signature").and_then(Value::as_str) {
        return signature.to_owned();
    }
    let entries: Vec<_> = row
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|choice| {
            Some((
                choice["eclass_id"].as_str()?.to_owned(),
                choice["enode_id"].as_str()?.to_owned(),
            ))
        })
        .collect();
    topology_signature(&entries)
}

fn row_topology_signature(row: &Value) -> String {
    row.as_object()
        .map_or_else(String::new, map_topology_signature)
}

fn choose_exact_candidates(rows: &[Value], top_exact: usize, policy: &str) -> Vec<Value> {
    if policy == "topology_cap2" {
        let top_exact = top_exact.min(rows.len());
        if top_exact == 0 {
            return Vec::new();
        }
        // Keep the original P1 winner and, if available, its second sizing
        // start. Then prefer unseen topology skeletons and finally fill any
        // remaining slots, with a maximum of two starts per topology.
        let signatures: Vec<_> = rows.iter().map(row_topology_signature).collect();
        let indices = capped_topology_indices(&signatures, top_exact, 2);
        return indices
            .into_iter()
            .map(|index| rows[index].clone())
            .collect();
    }
    if policy == "topology_stratified" {
        let top_exact = top_exact.min(rows.len());
        if top_exact == 0 {
            return Vec::new();
        }
        let mut by_signature: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (index, row) in rows.iter().enumerate() {
            by_signature
                .entry(row_topology_signature(row))
                .or_default()
                .push(index);
        }
        let mut groups: Vec<_> = by_signature
            .into_iter()
            .map(|(signature, indices)| {
                let best = &rows[indices[0]];
                (
                    signature,
                    indices,
                    best["p1_delta_phi"].as_f64().unwrap_or(f64::INFINITY),
                    best["p2_score"].as_f64().unwrap_or(f64::INFINITY),
                )
            })
            .collect();
        groups.sort_by(|(sig_a, idx_a, p1_a, p2_a), (sig_b, idx_b, p1_b, p2_b)| {
            p1_a.total_cmp(p1_b)
                .then_with(|| p2_a.total_cmp(p2_b))
                .then_with(|| idx_a[0].cmp(&idx_b[0]))
                .then_with(|| sig_a.cmp(sig_b))
        });
        let mut selected = Vec::with_capacity(top_exact);
        let mut selected_indices = HashSet::new();
        let mut counts = BTreeMap::<String, usize>::new();
        const PER_TOPOLOGY_CAP: usize = 2;

        for depth in 0..PER_TOPOLOGY_CAP {
            for (signature, indices, _, _) in &groups {
                if selected.len() == top_exact {
                    break;
                }
                if counts.get(signature).copied().unwrap_or(0) >= PER_TOPOLOGY_CAP {
                    continue;
                }
                if let Some(index) = indices.get(depth) {
                    if selected_indices.insert(*index) {
                        selected.push(rows[*index].clone());
                        *counts.entry(signature.clone()).or_default() += 1;
                    }
                }
            }
        }

        if selected.len() < top_exact {
            for (signature, indices, _, _) in &groups {
                if selected.len() == top_exact {
                    break;
                }
                for index in indices {
                    if selected.len() == top_exact {
                        break;
                    }
                    if selected_indices.contains(index) {
                        continue;
                    }
                    if counts.get(signature).copied().unwrap_or(0) >= PER_TOPOLOGY_CAP {
                        continue;
                    }
                    selected_indices.insert(*index);
                    selected.push(rows[*index].clone());
                    *counts.entry(signature.clone()).or_default() += 1;
                }
            }
        }
        return selected;
    }
    if policy != "diverse" || top_exact <= 1 {
        return rows.iter().take(top_exact).cloned().collect();
    }
    // P1 rows are already in global score order.  The diversity policy keeps
    // that order as the tie-break, but first prefers a candidate from an
    // unseen region identity and an unseen region size.  This prevents a
    // one-shot portfolio from spending all four expensive A2 calls on copies
    // of the same small region, without introducing benchmark-specific
    // weights or a second search loop.
    let mut selected = Vec::with_capacity(top_exact);
    let mut region_ids = std::collections::HashSet::<String>::new();
    let mut region_sizes = std::collections::HashSet::<usize>::new();
    while selected.len() < top_exact && selected.len() < rows.len() {
        let next_index = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                !selected
                    .iter()
                    .any(|item: &Value| item["candidate_id"] == row["candidate_id"])
            })
            .max_by(|(first_index, first), (second_index, second)| {
                let first_new_both = usize::from(
                    !region_ids.contains(first["region_id"].as_str().unwrap_or_default())
                        && !region_sizes.contains(
                            &(first["region_size"].as_u64().unwrap_or_default() as usize),
                        ),
                );
                let second_new_both = usize::from(
                    !region_ids.contains(second["region_id"].as_str().unwrap_or_default())
                        && !region_sizes.contains(
                            &(second["region_size"].as_u64().unwrap_or_default() as usize),
                        ),
                );
                first_new_both
                    .cmp(&second_new_both)
                    .then_with(|| {
                        let first_new_region = usize::from(
                            !region_ids.contains(first["region_id"].as_str().unwrap_or_default()),
                        );
                        let second_new_region = usize::from(
                            !region_ids.contains(second["region_id"].as_str().unwrap_or_default()),
                        );
                        first_new_region.cmp(&second_new_region)
                    })
                    .then_with(|| {
                        let first_new_size = usize::from(!region_sizes.contains(
                            &(first["region_size"].as_u64().unwrap_or_default() as usize),
                        ));
                        let second_new_size = usize::from(!region_sizes.contains(
                            &(second["region_size"].as_u64().unwrap_or_default() as usize),
                        ));
                        first_new_size.cmp(&second_new_size)
                    })
                    // rows are sorted best-first, so a lower index wins the
                    // final deterministic tie-break.
                    .then_with(|| second_index.cmp(first_index))
            })
            .map(|(index, _)| index);
        let Some(next_index) = next_index else { break };
        let next = rows[next_index].clone();
        if let Some(region_id) = next["region_id"].as_str() {
            region_ids.insert(region_id.to_owned());
        }
        if let Some(region_size) = next["region_size"].as_u64() {
            region_sizes.insert(region_size as usize);
        }
        selected.push(next);
    }
    selected
}

fn run_cli_inner(
    args: Vec<String>,
    preloaded_nldm: Option<&HashMap<String, extraction_gym::NLDM>>,
    timing_boundary: Option<TimingBoundary>,
) -> Result<()> {
    if args.len() < 3 {
        bail!(
            "usage: rerank_dual_region_p3_fast INCUMBENT PLANNER_DIR OUT_DIR [P1_PER_REGION=8] [TOP_EXACT=1]"
        )
    }
    let incumbent = PathBuf::from(&args[0]);
    let planner = PathBuf::from(&args[1]);
    let out = PathBuf::from(&args[2]);
    let p1_per_region = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(8usize);
    let top_exact = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1usize);
    let selection_policy = std::env::var("EGG_P1_SELECTION").unwrap_or_else(|_| "global".into());
    anyhow::ensure!(
        selection_policy == "global"
            || selection_policy == "diverse"
            || selection_policy == "topology_cap2"
            || selection_policy == "topology_stratified",
        "EGG_P1_SELECTION must be global, diverse, topology_cap2, or topology_stratified"
    );
    fs::create_dir_all(&out)?;
    let started = Instant::now();
    let plan: Vec<Value> =
        serde_json::from_slice(&fs::read(planner.join("dual_region_plan_full.json"))?)?;
    let mut by_region: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for row in &plan {
        by_region
            .entry(row["region_id"].as_str().context("region_id")?.to_owned())
            .or_default()
            .push(row.clone());
    }
    let mut selected = Vec::new();
    for rows in by_region.values_mut() {
        rows.sort_by(|a, b| {
            a["p2_score"]
                .as_f64()
                .unwrap_or(f64::INFINITY)
                .total_cmp(&b["p2_score"].as_f64().unwrap_or(f64::INFINITY))
                .then_with(|| a["candidate_id"].as_str().cmp(&b["candidate_id"].as_str()))
        });
        selected.extend(rows.iter().take(p1_per_region).cloned());
    }
    let emit: Vec<_> = selected
        .iter()
        .map(|row| {
            json!({
                "candidate_id":row["candidate_id"], "choices":row["choices"]
            })
        })
        .collect();
    let materialization_plan = out.join("materialization_plan.json");
    write_json(&materialization_plan, &Value::Array(emit))?;
    let exe_dir = std::env::current_exe()?
        .parent()
        .context("binary has no directory")?
        .to_path_buf();
    let planner_materialized = planner.join("materialized");
    let materialized_dir = if planner_materialized
        .join("materialization_trace.json")
        .exists()
    {
        planner_materialized
    } else {
        out.join("materialized")
    };
    let rule_paths: Vec<PathBuf> = std::env::var_os("EGG_RULE_PATHS")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_else(|| DEFAULT_RULES.iter().map(PathBuf::from).collect());
    if !materialized_dir.join("materialization_trace.json").exists() {
        let mut command = Command::new(exe_dir.join("run_adaptive_region_v1"));
        command.args([
            "emit-plan",
            incumbent.to_str().context("path")?,
            materialization_plan.to_str().context("path")?,
            materialized_dir.to_str().context("path")?,
            "2",
            "3",
        ]);
        command.args(&rule_paths);
        let result = command.status()?;
        anyhow::ensure!(result.success(), "materialization failed: {result}");
    }
    let materialized: Vec<Value> = serde_json::from_slice(&fs::read(
        materialized_dir.join("materialization_trace.json"),
    )?)?;
    let meta: HashMap<_, _> = selected
        .into_iter()
        .map(|row| (row["candidate_id"].as_str().unwrap().to_owned(), row))
        .collect();
    let lib_path = PathBuf::from(
        std::env::var("EGG_LIB_PATH")
            .unwrap_or_else(|_| "test/asap7sc6t_SELECT_LVT_TT_nldm.lib".into()),
    );
    let legal: Vec<_> = materialized
        .into_iter()
        .filter(|item| item["legal"] == Value::Bool(true))
        .collect();
    let evaluator_mode = std::env::var("EGG_P1_EVALUATOR").unwrap_or_else(|_| "shared-load".into());
    let reuse_load_contexts = std::env::var_os("EGG_P1_DISABLE_LOAD_CACHE").is_none();
    let reuse_candidate_sha = std::env::var_os("EGG_P1_DISABLE_CANDIDATE_CACHE").is_none();
    anyhow::ensure!(
        evaluator_mode == "shared-load" || evaluator_mode == "legacy",
        "EGG_P1_EVALUATOR must be shared-load or legacy"
    );
    let legacy_context = if evaluator_mode == "legacy" {
        let liberty = read_liberty(&lib_path).map_err(anyhow::Error::msg)?;
        let pins = get_direction_of_pins(&liberty).map_err(anyhow::Error::msg)?;
        Some((pins, fs::read_to_string(&lib_path)?))
    } else {
        None
    };
    let mut shared_cells = HashMap::new();
    let mut fast = if evaluator_mode == "shared-load" {
        if let Some(cache) = preloaded_nldm {
            let evaluator = SharedLoadEvaluator::from_nldm_cache(cache, reuse_load_contexts)?;
            Some(if let Some(boundary) = timing_boundary {
                evaluator.with_boundary(boundary)
            } else {
                evaluator
            })
        } else {
            let mut paths = Vec::with_capacity(legal.len() + 1);
            paths.push(incumbent.clone());
            paths.extend(legal.iter().map(|item| {
                PathBuf::from(item["output_netlist"].as_str().expect("candidate netlist"))
            }));
            let lib_content = fs::read_to_string(&lib_path)?;
            let evaluator = SharedLoadEvaluator::new(&lib_content, &paths, reuse_load_contexts)?;
            Some(if let Some(boundary) = timing_boundary {
                evaluator.with_boundary(boundary)
            } else {
                evaluator
            })
        }
    } else {
        None
    };
    let base = if let Some(evaluator) = &mut fast {
        serde_json::to_value(evaluator.evaluate(&incumbent)?)?
    } else {
        let (pins, lib_content) = legacy_context
            .as_ref()
            .context("legacy evaluator context")?;
        evaluate_legacy(&incumbent, pins, lib_content, &mut shared_cells)?
    };
    let base_score = base["score"].as_f64().unwrap();
    let mut rows = Vec::new();
    let mut candidate_cache = HashMap::<String, Value>::new();
    let mut candidate_cache_hits = 0usize;
    for item in legal {
        let id = item["candidate_id"].as_str().context("candidate id")?;
        let netlist = Path::new(item["output_netlist"].as_str().context("netlist")?);
        let candidate_sha = sha(netlist)?;
        let cache_hit = evaluator_mode == "shared-load"
            && reuse_candidate_sha
            && candidate_cache.contains_key(&candidate_sha);
        let value = if cache_hit {
            candidate_cache_hits += 1;
            candidate_cache[&candidate_sha].clone()
        } else {
            let value = if let Some(evaluator) = &mut fast {
                serde_json::to_value(evaluator.evaluate(netlist)?)?
            } else {
                let (pins, lib_content) = legacy_context
                    .as_ref()
                    .context("legacy evaluator context")?;
                evaluate_legacy(netlist, pins, lib_content, &mut shared_cells)?
            };
            if evaluator_mode == "shared-load" && reuse_candidate_sha {
                candidate_cache.insert(candidate_sha.clone(), value.clone());
            }
            value
        };
        let mut row = merge(
            meta[id].as_object().unwrap().clone(),
            item.as_object().unwrap(),
        );
        let signature = map_topology_signature(&row);
        row.insert("topology_signature".into(), json!(signature));
        row.insert("p1_candidate_sha256".into(), json!(candidate_sha));
        row.insert("p1_candidate_cache_hit".into(), json!(cache_hit));
        row.insert(
            "p1_delta_phi".into(),
            json!((value["score"].as_f64().unwrap() / base_score).ln()),
        );
        for key in ["score", "delay", "area", "power"] {
            row.insert(format!("p1_{key}"), value[key].clone());
        }
        rows.push(Value::Object(row));
    }
    rows.sort_by(|a, b| {
        a["p1_delta_phi"]
            .as_f64()
            .unwrap()
            .total_cmp(&b["p1_delta_phi"].as_f64().unwrap())
            .then_with(|| {
                a["p2_score"]
                    .as_f64()
                    .unwrap()
                    .total_cmp(&b["p2_score"].as_f64().unwrap())
            })
            .then_with(|| a["candidate_id"].as_str().cmp(&b["candidate_id"].as_str()))
    });
    write_json(
        &out.join("p3_ranked_candidates.json"),
        &Value::Array(rows.clone()),
    )?;
    write_json(
        &out.join("exact_candidate_plan.json"),
        &Value::Array(choose_exact_candidates(&rows, top_exact, &selection_policy)),
    )?;
    let load_stats = fast.as_ref().map(SharedLoadEvaluator::stats);
    write_json(
        &out.join("summary.json"),
        &json!({"p2_planned":plan.len(),"p1_evaluations":rows.len(),
        "p1_per_region":p1_per_region,"top_exact":top_exact.min(rows.len()),"selection_policy":selection_policy,
        "p2_selection_policy":"global",
        "best_p1_delta_phi":rows.first().and_then(|r|r["p1_delta_phi"].as_f64()),
        "elapsed_sec":started.elapsed().as_secs_f64(),
        "implementation":"Rust producer-independent shared-load P3/P1",
        "evaluator_mode":evaluator_mode,"candidate_sha_cache_hits":candidate_cache_hits,
        "reuse_load_contexts":reuse_load_contexts,"reuse_candidate_sha":reuse_candidate_sha,
        "candidate_sha_unique":if evaluator_mode=="shared-load"&&reuse_candidate_sha{candidate_cache.len()}else{rows.len()},
        "shared_load":load_stats}),
    )?;
    Ok(())
}

pub(crate) fn run_cli_with_nldm(
    args: Vec<String>,
    preloaded_nldm: &HashMap<String, extraction_gym::NLDM>,
) -> Result<()> {
    run_cli_inner(args, Some(preloaded_nldm), None)
}

pub(crate) fn run_cli_with_nldm_boundary(
    args: Vec<String>,
    preloaded_nldm: &HashMap<String, extraction_gym::NLDM>,
    timing_boundary: TimingBoundary,
) -> Result<()> {
    run_cli_inner(args, Some(preloaded_nldm), Some(timing_boundary))
}

pub fn run_cli(args: Vec<String>) -> Result<()> {
    run_cli_inner(args, None, None)
}

fn main() -> Result<()> {
    run_cli(std::env::args().skip(1).collect())
}

#[cfg(test)]
mod tests {
    use super::choose_exact_candidates;
    use serde_json::json;

    #[test]
    fn topology_stratified_round_robins_topologies_before_second_starts() {
        let rows = vec![
            json!({"candidate_id":"a0","topology_signature":"A","p1_delta_phi":0.1,"p2_score":1.0}),
            json!({"candidate_id":"a1","topology_signature":"A","p1_delta_phi":0.2,"p2_score":2.0}),
            json!({"candidate_id":"b0","topology_signature":"B","p1_delta_phi":0.3,"p2_score":3.0}),
            json!({"candidate_id":"b1","topology_signature":"B","p1_delta_phi":0.4,"p2_score":4.0}),
        ];
        let selected = choose_exact_candidates(&rows, 4, "topology_stratified");
        let ids: Vec<_> = selected
            .iter()
            .map(|row| row["candidate_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["a0", "b0", "a1", "b1"]);
    }
}
