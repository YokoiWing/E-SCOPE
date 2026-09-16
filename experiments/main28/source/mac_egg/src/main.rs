use anyhow::{Context, Result, bail};
use extraction_gym::ExtendedEGraph;
use extraction_gym::extract::ExtractionResult;
use extraction_gym::extract::Extractor as GymExtractor;
use extraction_gym::extract::iterative_greedy_dag_SA::{
    IterativeGreedyDagSaExtractor, SimulatedAnnealingConfig, SimulatedAnnealingStats,
};
#[cfg(feature = "ilp-cbc")]
use extraction_gym::extract::nldm_oracle_ilp::{NldmOracleCbcExtractor, NldmOracleConfig};
use mac_egg::SerializedEGraph;
use mac_egg::choose_first_node_in_each_class;
use mac_egg::choose_result_in_egraph;
use mac_egg::choose_result_in_serialized_egraph_into_netlist;
use mac_egg::egg_to_serialized_egraph;
use mac_egg::extractor::extractors;
use mac_egg::io::liberty::{get_direction_of_pins, read_liberty};
use mac_egg::io::stdcell::{read_verilog_with_lib_to_netlist, write_verilog_from_netlist_with_lib};
use mac_egg::language::{StdCellLanguage, StdCellType};
use mac_egg::mining::{MiningConfig, MiningSummary, mine_patterns_to_dir};
use mac_egg::netlist_to_egg_roots;
use mac_egg::rule::JsonRules;
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[cfg(test)]
pub mod test;

#[derive(Debug, Clone)]
struct CliArgs {
    input: PathBuf,
    liberty: PathBuf,
    rules: Vec<PathBuf>,
    pipeline: String,
    extractor: String,
    json_out: PathBuf,
    verilog_out: PathBuf,
    iter_limit: usize,
    node_limit: usize,
    time_limit_sec: u64,
    stage1_time_limit_sec: u64,
    delay_slack_abs: f64,
    delay_slack_rel: f64,
    fallback_delay_improve_abs: f64,
    seed: u64,
    sa_initial_temperature: f64,
    sa_cooling_rate: f64,
    sa_min_temperature: f64,
    sa_iterations_per_temp: usize,
    sa_exact_eval_budget: Option<usize>,
    skip_roundtrip_check: bool,
    baseline_runs: usize,
    baseline_report_out: Option<PathBuf>,
    mine_top_k: usize,
    mine_min_support: usize,
    mine_max_pattern_size: usize,
    mine_output_dir: Option<PathBuf>,
}

impl Default for CliArgs {
    fn default() -> Self {
        Self {
            input: PathBuf::from("test/mul32_map_genus.v"),
            liberty: PathBuf::from("test/asap7sc6t_SELECT_LVT_TT_nldm.lib"),
            rules: vec![
                PathBuf::from("test/6t_empty_rules.json"),
                PathBuf::from("test/6t_inv_rules.json"),
                PathBuf::from("test/6t_dmg_rules.json"),
                PathBuf::from("test/6t_scale_rules.json"),
            ],
            pipeline: "single".to_string(),
            extractor: "iterative-greedy-dag-SA".to_string(),
            json_out: PathBuf::from("json/test_mul32_map_genus_inv_dmg_rules.json"),
            verilog_out: PathBuf::from("verilog/test_mul32_map_genus_inv_dmg_rules_extract.v"),
            iter_limit: 200,
            node_limit: 1_000_000,
            time_limit_sec: 200,
            stage1_time_limit_sec: 60,
            delay_slack_abs: 0.0,
            delay_slack_rel: 0.0,
            fallback_delay_improve_abs: 3.0,
            seed: 0,
            sa_initial_temperature: 1000.0,
            sa_cooling_rate: 0.95,
            sa_min_temperature: 0.01,
            sa_iterations_per_temp: 100,
            sa_exact_eval_budget: None,
            skip_roundtrip_check: false,
            baseline_runs: 1,
            baseline_report_out: None,
            mine_top_k: 0,
            mine_min_support: 2,
            mine_max_pattern_size: 3,
            mine_output_dir: None,
        }
    }
}

#[derive(Debug, Clone)]
struct FlowReport {
    module_name: String,
    original_ppa: extraction_gym::ExtendedCost,
    optimized_ppa: extraction_gym::ExtendedCost,
    chosen_serialized_ppa: extraction_gym::ExtendedCost,
    reparsed_optimized_ppa: extraction_gym::ExtendedCost,
    egraph_nodes: usize,
    egraph_classes: usize,
    chosen_serialized_nodes: usize,
    chosen_serialized_classes: usize,
    reparsed_nodes: usize,
    reparsed_classes: usize,
    json_out: PathBuf,
    verilog_out: PathBuf,
    sa_stats: Option<SimulatedAnnealingStats>,
    fallback_reason: Option<String>,
    mining_summary: Option<MiningSummary>,
}

#[derive(Debug, Clone, Serialize)]
struct BaselineRunRecord {
    run_index: usize,
    seed: u64,
    egraph_nodes: usize,
    egraph_classes: usize,
    original_delay: f64,
    original_area: f64,
    original_power: f64,
    optimized_delay: f64,
    optimized_area: f64,
    optimized_power: f64,
    greedy_initial_delay: f64,
    greedy_initial_area: f64,
    greedy_initial_power: f64,
    best_delay: f64,
    best_area: f64,
    best_power: f64,
    accepted_moves: usize,
    improved_moves: usize,
    total_iterations: usize,
    acceptance_rate: f64,
    improvement_rate: f64,
    greedy_iterations: usize,
    greedy_candidate_count: usize,
    greedy_improvement_count: usize,
}

#[derive(Debug, Clone, Serialize)]
struct MetricSummary {
    mean: f64,
    stddev: f64,
    best: f64,
}

#[derive(Debug, Clone, Serialize)]
struct BaselineSummary {
    module_name: String,
    runs: usize,
    extractor: String,
    seeds: Vec<u64>,
    rewrite_egraph_nodes: usize,
    rewrite_egraph_classes: usize,
    delay: MetricSummary,
    area: MetricSummary,
    power: MetricSummary,
    acceptance_rate_mean: f64,
    improvement_rate_mean: f64,
    records: Vec<BaselineRunRecord>,
}

fn usage() -> &'static str {
    "Usage: cargo run -- [--input PATH] [--lib PATH] [--rules PATH[,PATH...]] [--pipeline NAME] [--extractor NAME] [--json-out PATH] [--verilog-out PATH] [--iter-limit N] [--node-limit N] [--time-limit-sec N] [--stage1-time-limit-sec N] [--delay-slack-abs X] [--delay-slack-rel X] [--fallback-delay-improve-abs X] [--seed N] [--sa-initial-temperature X] [--sa-cooling-rate X] [--sa-min-temperature X] [--sa-iterations-per-temp N] [--skip-roundtrip-check] [--baseline-runs N] [--baseline-report-out PATH] [--mine-top-k N] [--mine-min-support N] [--mine-max-pattern-size N] [--mine-output-dir PATH]"
}

fn parse_args() -> Result<CliArgs> {
    let mut args = CliArgs::default();
    let mut iter = std::env::args().skip(1);

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--input" => {
                args.input = PathBuf::from(iter.next().context("missing value for --input")?)
            }
            "--lib" => {
                args.liberty = PathBuf::from(iter.next().context("missing value for --lib")?)
            }
            "--rules" => {
                let value = iter.next().context("missing value for --rules")?;
                args.rules = value
                    .split(',')
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| PathBuf::from(s.trim()))
                    .collect();
                if args.rules.is_empty() {
                    bail!("--rules requires at least one path");
                }
            }
            "--pipeline" => args.pipeline = iter.next().context("missing value for --pipeline")?,
            "--extractor" => {
                args.extractor = iter.next().context("missing value for --extractor")?
            }
            "--json-out" => {
                args.json_out = PathBuf::from(iter.next().context("missing value for --json-out")?)
            }
            "--verilog-out" => {
                args.verilog_out =
                    PathBuf::from(iter.next().context("missing value for --verilog-out")?)
            }
            "--iter-limit" => {
                args.iter_limit = iter
                    .next()
                    .context("missing value for --iter-limit")?
                    .parse()
                    .context("invalid --iter-limit")?
            }
            "--node-limit" => {
                args.node_limit = iter
                    .next()
                    .context("missing value for --node-limit")?
                    .parse()
                    .context("invalid --node-limit")?
            }
            "--time-limit-sec" => {
                args.time_limit_sec = iter
                    .next()
                    .context("missing value for --time-limit-sec")?
                    .parse()
                    .context("invalid --time-limit-sec")?
            }
            "--stage1-time-limit-sec" => {
                args.stage1_time_limit_sec = iter
                    .next()
                    .context("missing value for --stage1-time-limit-sec")?
                    .parse()
                    .context("invalid --stage1-time-limit-sec")?
            }
            "--delay-slack-abs" => {
                args.delay_slack_abs = iter
                    .next()
                    .context("missing value for --delay-slack-abs")?
                    .parse()
                    .context("invalid --delay-slack-abs")?
            }
            "--delay-slack-rel" => {
                args.delay_slack_rel = iter
                    .next()
                    .context("missing value for --delay-slack-rel")?
                    .parse()
                    .context("invalid --delay-slack-rel")?
            }
            "--fallback-delay-improve-abs" => {
                args.fallback_delay_improve_abs = iter
                    .next()
                    .context("missing value for --fallback-delay-improve-abs")?
                    .parse()
                    .context("invalid --fallback-delay-improve-abs")?
            }
            "--seed" => {
                args.seed = iter
                    .next()
                    .context("missing value for --seed")?
                    .parse()
                    .context("invalid --seed")?
            }
            "--sa-initial-temperature" => {
                args.sa_initial_temperature = iter
                    .next()
                    .context("missing value for --sa-initial-temperature")?
                    .parse()
                    .context("invalid --sa-initial-temperature")?
            }
            "--sa-cooling-rate" => {
                args.sa_cooling_rate = iter
                    .next()
                    .context("missing value for --sa-cooling-rate")?
                    .parse()
                    .context("invalid --sa-cooling-rate")?
            }
            "--sa-min-temperature" => {
                args.sa_min_temperature = iter
                    .next()
                    .context("missing value for --sa-min-temperature")?
                    .parse()
                    .context("invalid --sa-min-temperature")?
            }
            "--sa-iterations-per-temp" => {
                args.sa_iterations_per_temp = iter
                    .next()
                    .context("missing value for --sa-iterations-per-temp")?
                    .parse()
                    .context("invalid --sa-iterations-per-temp")?
            }
            "--sa-exact-eval-budget" => {
                args.sa_exact_eval_budget = Some(
                    iter.next()
                        .context("missing value for --sa-exact-eval-budget")?
                        .parse()
                        .context("invalid --sa-exact-eval-budget")?,
                )
            }
            "--skip-roundtrip-check" => args.skip_roundtrip_check = true,
            "--baseline-runs" => {
                args.baseline_runs = iter
                    .next()
                    .context("missing value for --baseline-runs")?
                    .parse()
                    .context("invalid --baseline-runs")?
            }
            "--baseline-report-out" => {
                args.baseline_report_out = Some(PathBuf::from(
                    iter.next()
                        .context("missing value for --baseline-report-out")?,
                ))
            }
            "--mine-top-k" => {
                args.mine_top_k = iter
                    .next()
                    .context("missing value for --mine-top-k")?
                    .parse()
                    .context("invalid --mine-top-k")?
            }
            "--mine-min-support" => {
                args.mine_min_support = iter
                    .next()
                    .context("missing value for --mine-min-support")?
                    .parse()
                    .context("invalid --mine-min-support")?
            }
            "--mine-max-pattern-size" => {
                args.mine_max_pattern_size = iter
                    .next()
                    .context("missing value for --mine-max-pattern-size")?
                    .parse()
                    .context("invalid --mine-max-pattern-size")?
            }
            "--mine-output-dir" => {
                args.mine_output_dir = Some(PathBuf::from(
                    iter.next().context("missing value for --mine-output-dir")?,
                ))
            }
            "--help" | "-h" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other}\n{}", usage()),
        }
    }

    Ok(args)
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent dir for {}", path.display()))?;
    }
    Ok(())
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    parent.join(format!("{file_name}{suffix}"))
}

fn serialized_to_value(egraph: &SerializedEGraph) -> Result<Value> {
    serde_json::to_value(egraph).context("failed to serialize egraph into JSON value")
}

fn single_choice_ppa(
    serialized: &SerializedEGraph,
    lib_path: &Path,
) -> Result<extraction_gym::ExtendedCost> {
    let json_value = serialized_to_value(serialized)?;
    let lib_value = Value::String(lib_path.display().to_string());
    let ext = ExtendedEGraph::from_base_to_extention(serialized.clone(), &lib_value, &json_value);
    let result = choose_first_node_in_each_class(serialized).map_err(anyhow::Error::msg)?;
    Ok(result.dag_cost_nldm_on_pruned(&ext))
}

fn load_rules(paths: &[PathBuf]) -> Result<Vec<egg::Rewrite<StdCellLanguage, ()>>> {
    let mut rules = Vec::new();
    for path in paths {
        rules.extend(
            JsonRules::from_path(path)
                .map_err(anyhow::Error::msg)?
                .into_egg_rules::<StdCellLanguage>()
                .map_err(anyhow::Error::msg)?,
        );
    }
    Ok(rules)
}

fn pct_improvement(before: f64, after: f64) -> f64 {
    if before.abs() < f64::EPSILON {
        0.0
    } else {
        (before - after) / before * 100.0
    }
}

fn cost_triplet(cost: &extraction_gym::ExtendedCost) -> [f64; 3] {
    [
        cost.components[0].into_inner(),
        cost.components[1].into_inner(),
        cost.components[2].into_inner(),
    ]
}

fn ppa_score(cost: &extraction_gym::ExtendedCost) -> f64 {
    let [delay, area, power] = cost_triplet(cost);
    delay * delay * area * power
}

fn normalized_ppa_score(
    cost: &extraction_gym::ExtendedCost,
    baseline: &extraction_gym::ExtendedCost,
) -> f64 {
    cost.components
        .iter()
        .zip(baseline.components.iter())
        .map(|(value, base)| {
            let denom = base.into_inner().abs().max(1e-9);
            value.into_inner() / denom
        })
        .sum::<f64>()
        / 3.0
}

fn should_keep_original_for_small_delay_gain(
    original: &extraction_gym::ExtendedCost,
    candidate: &extraction_gym::ExtendedCost,
    delay_improve_abs_threshold: f64,
) -> Option<String> {
    let original_delay = original.components[0].into_inner();
    let candidate_delay = candidate.components[0].into_inner();
    let delay_improve_abs = original_delay - candidate_delay;
    if delay_improve_abs >= delay_improve_abs_threshold {
        return None;
    }

    let original_score = normalized_ppa_score(original, original);
    let candidate_score = normalized_ppa_score(candidate, original);
    if candidate_score <= original_score + 1e-9 {
        return None;
    }

    Some(format!(
        "fallback to original netlist: delay gain {:.6} < threshold {:.6} and normalized PPA score worsened ({:.6} -> {:.6})",
        delay_improve_abs, delay_improve_abs_threshold, original_score, candidate_score,
    ))
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

fn stddev(values: &[f64]) -> f64 {
    if values.len() <= 1 {
        0.0
    } else {
        let avg = mean(values);
        let variance = values
            .iter()
            .map(|value| {
                let diff = value - avg;
                diff * diff
            })
            .sum::<f64>()
            / values.len() as f64;
        variance.sqrt()
    }
}

fn summarize_metric(values: &[f64]) -> MetricSummary {
    MetricSummary {
        mean: mean(values),
        stddev: stddev(values),
        best: values.iter().copied().fold(f64::INFINITY, f64::min),
    }
}

fn run_flow(args: &CliArgs) -> Result<FlowReport> {
    ensure_parent_dir(&args.json_out)?;
    ensure_parent_dir(&args.verilog_out)?;

    let liberty = read_liberty(&args.liberty).map_err(anyhow::Error::msg)?;
    let lib = get_direction_of_pins(&liberty).map_err(anyhow::Error::msg)?;
    let (input_netlist, module_name) =
        read_verilog_with_lib_to_netlist(&args.input, lib.clone()).map_err(anyhow::Error::msg)?;

    let input_egraph_roots: mac_egg::egraph_roots::EGraphRoots<_, ()> =
        netlist_to_egg_roots(&input_netlist).map_err(anyhow::Error::msg)?;
    let original_serialized = SerializedEGraph::from(&input_egraph_roots);
    let original_ppa = single_choice_ppa(&original_serialized, &args.liberty)?;

    let rules = load_rules(&args.rules)?;
    let runner = egg::Runner::default()
        .with_egraph(input_egraph_roots.egraph)
        .with_node_limit(args.node_limit)
        .with_iter_limit(args.iter_limit)
        .with_time_limit(std::time::Duration::from_secs(args.time_limit_sec))
        .run(&rules);

    let rewritten = egg_to_serialized_egraph(&runner.egraph, &input_egraph_roots.roots);
    rewritten
        .to_json_file(&args.json_out)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("failed to write {}", args.json_out.display()))?;

    let rewritten_json = serialized_to_value(&rewritten)?;
    let lib_value = Value::String(args.liberty.display().to_string());
    let ext =
        ExtendedEGraph::from_base_to_extention(rewritten.clone(), &lib_value, &rewritten_json);
    let mining_summary = if args.mine_top_k > 0 {
        let output_dir = args
            .mine_output_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("mined_patterns"));
        ensure_parent_dir(&output_dir.join("summary.json"))?;
        Some(
            mine_patterns_to_dir(
                &rewritten,
                &lib,
                MiningConfig {
                    min_support: args.mine_min_support,
                    max_pattern_size: args.mine_max_pattern_size,
                    top_k: args.mine_top_k,
                },
                &output_dir,
            )
            .map_err(anyhow::Error::msg)?,
        )
    } else {
        None
    };

    let (result, sa_stats): (ExtractionResult, Option<SimulatedAnnealingStats>) = if args.extractor
        == "iterative-greedy-dag-SA"
    {
        let extractor = IterativeGreedyDagSaExtractor {
            config: SimulatedAnnealingConfig {
                seed: args.seed,
                initial_temperature: args.sa_initial_temperature,
                cooling_rate: args.sa_cooling_rate,
                min_temperature: args.sa_min_temperature,
                iterations_per_temp: args.sa_iterations_per_temp,
                exact_eval_budget: args.sa_exact_eval_budget,
            },
        };
        let (result, stats) = extractor.extract_with_stats(&ext, &ext.inner.root_eclasses);
        (result, Some(stats))
    } else if args.extractor == "nldm-oracle-ilp" {
        #[cfg(feature = "ilp-cbc")]
        {
            let extractor = NldmOracleCbcExtractor {
                config: NldmOracleConfig {
                    timeout_seconds: args.time_limit_sec.min(u32::MAX as u64) as u32,
                    stage1_timeout_seconds: args
                        .stage1_time_limit_sec
                        .min(args.time_limit_sec.saturating_sub(1).max(1))
                        .min(u32::MAX as u64) as u32,
                    delay_slack_abs: args.delay_slack_abs,
                    delay_slack_rel: args.delay_slack_rel,
                    ..NldmOracleConfig::default()
                },
            };
            (extractor.extract(&ext, &ext.inner.root_eclasses), None)
        }
        #[cfg(not(feature = "ilp-cbc"))]
        {
            anyhow::bail!("nldm-oracle-ilp requires --features ilp-cbc");
        }
    } else {
        let mut registered = extractors();
        registered.retain(|_, detail| detail.get_use_for_bench());
        let extractor = registered
            .get(args.extractor.as_str())
            .with_context(|| format!("unknown extractor: {}", args.extractor))?;
        (
            extractor
                .get_extractor()
                .extract(&ext, &ext.inner.root_eclasses),
            None,
        )
    };
    result.check(&ext);
    let optimized_ppa = result.dag_cost_nldm_on_pruned(&ext);
    let chosen_serialized =
        choose_result_in_egraph(&ext.inner, &result).map_err(anyhow::Error::msg)?;
    let chosen_serialized_ppa = single_choice_ppa(&chosen_serialized, &args.liberty)?;

    let optimized_netlist =
        choose_result_in_serialized_egraph_into_netlist::<StdCellType>(&ext.inner, &result)
            .map_err(anyhow::Error::msg)?;
    write_verilog_from_netlist_with_lib(
        &args.verilog_out,
        optimized_netlist,
        &module_name,
        lib.clone(),
    )
    .map_err(anyhow::Error::msg)?;

    let (reparsed_nodes, reparsed_classes, mut reparsed_optimized_ppa) =
        if args.skip_roundtrip_check {
            (
                chosen_serialized.nodes.len(),
                chosen_serialized.classes().len(),
                optimized_ppa.clone(),
            )
        } else {
            let (reparsed_optimized_netlist, _) =
                read_verilog_with_lib_to_netlist(&args.verilog_out, lib.clone())
                    .map_err(anyhow::Error::msg)?;
            let reparsed_roots: mac_egg::egraph_roots::EGraphRoots<_, ()> =
                netlist_to_egg_roots(&reparsed_optimized_netlist).map_err(anyhow::Error::msg)?;
            let reparsed_serialized = SerializedEGraph::from(&reparsed_roots);
            (
                reparsed_serialized.nodes.len(),
                reparsed_serialized.classes().len(),
                single_choice_ppa(&reparsed_serialized, &args.liberty)?,
            )
        };
    let mut fallback_reason = None;

    if args.extractor == "nldm-oracle-ilp" {
        if let Some(reason) = should_keep_original_for_small_delay_gain(
            &original_ppa,
            &reparsed_optimized_ppa,
            args.fallback_delay_improve_abs,
        ) {
            write_verilog_from_netlist_with_lib(
                &args.verilog_out,
                input_netlist.clone(),
                &module_name,
                lib.clone(),
            )
            .map_err(anyhow::Error::msg)?;
            reparsed_optimized_ppa = original_ppa.clone();
            fallback_reason = Some(reason);
        }
    }

    Ok(FlowReport {
        module_name,
        original_ppa,
        optimized_ppa,
        chosen_serialized_ppa,
        reparsed_optimized_ppa,
        egraph_nodes: runner.egraph.total_size(),
        egraph_classes: runner.egraph.number_of_classes(),
        chosen_serialized_nodes: chosen_serialized.nodes.len(),
        chosen_serialized_classes: chosen_serialized.classes().len(),
        reparsed_nodes,
        reparsed_classes,
        json_out: args.json_out.clone(),
        verilog_out: args.verilog_out.clone(),
        sa_stats,
        fallback_reason,
        mining_summary,
    })
}

fn print_report(report: &FlowReport) {
    let before = report.original_ppa.components;
    let after = report.reparsed_optimized_ppa.components;

    println!("Module: {}", report.module_name);
    println!(
        "Rewritten e-graph: {} e-nodes, {} e-classes",
        report.egraph_nodes, report.egraph_classes
    );
    println!(
        "Original PPA:  delay={:.6}, area={:.6}, power={:.6}",
        before[0], before[1], before[2]
    );
    println!(
        "Optimized PPA: delay={:.6}, area={:.6}, power={:.6}",
        after[0], after[1], after[2]
    );
    println!(
        "Extracted PPA: delay={:.6}, area={:.6}, power={:.6}",
        report.optimized_ppa.components[0],
        report.optimized_ppa.components[1],
        report.optimized_ppa.components[2]
    );
    println!(
        "Chosen Serialized PPA: delay={:.6}, area={:.6}, power={:.6}",
        report.chosen_serialized_ppa.components[0],
        report.chosen_serialized_ppa.components[1],
        report.chosen_serialized_ppa.components[2]
    );
    println!(
        "Diag sizes: chosen-serialized={} nodes/{} classes, reparsed={} nodes/{} classes",
        report.chosen_serialized_nodes,
        report.chosen_serialized_classes,
        report.reparsed_nodes,
        report.reparsed_classes,
    );
    println!(
        "Improvement:  delay={:.2}%, area={:.2}%, power={:.2}%",
        pct_improvement(before[0].into_inner(), after[0].into_inner()),
        pct_improvement(before[1].into_inner(), after[1].into_inner()),
        pct_improvement(before[2].into_inner(), after[2].into_inner()),
    );
    let original_score = ppa_score(&report.original_ppa);
    println!(
        "Score (D^2*A*P): original={:.6}, extracted={:.6} ({:.6}x), reparsed={:.6} ({:.6}x)",
        original_score,
        ppa_score(&report.optimized_ppa),
        ppa_score(&report.optimized_ppa) / original_score,
        ppa_score(&report.reparsed_optimized_ppa),
        ppa_score(&report.reparsed_optimized_ppa) / original_score,
    );
    if let Some(sa_stats) = &report.sa_stats {
        println!(
            "SA stats: seed={}, acceptance={:.2}%, improvement={:.2}%, greedy iters={}, final best delay/area/power={:.6}/{:.6}/{:.6}",
            sa_stats.seed,
            sa_stats.acceptance_rate * 100.0,
            sa_stats.improvement_rate * 100.0,
            sa_stats.greedy_iterations,
            sa_stats.best_cost.components[0],
            sa_stats.best_cost.components[1],
            sa_stats.best_cost.components[2],
        );
    }
    if let Some(reason) = &report.fallback_reason {
        println!("Fallback: {}", reason);
    }
    if let Some(summary) = &report.mining_summary {
        println!(
            "Mining: exported {} patterns (min_support={}, max_size={}, top_k={})",
            summary.exported_patterns.len(),
            summary.min_support,
            summary.max_pattern_size,
            summary.top_k,
        );
        if let Some(first) = summary.exported_patterns.first() {
            println!(
                "Top pattern: support={}, gates={}, root={}, file={}",
                first.support,
                first.gate_count,
                first.root_op,
                first.verilog_path.display(),
            );
        }
    }
    println!("JSON out: {}", report.json_out.display());
    println!("Verilog out: {}", report.verilog_out.display());
}

fn build_baseline_record(run_index: usize, report: &FlowReport) -> BaselineRunRecord {
    let original = cost_triplet(&report.original_ppa);
    let optimized = cost_triplet(&report.reparsed_optimized_ppa);
    let (
        greedy,
        best,
        accepted_moves,
        improved_moves,
        total_iterations,
        acceptance_rate,
        improvement_rate,
        greedy_iterations,
        greedy_candidate_count,
        greedy_improvement_count,
        seed,
    ) = if let Some(stats) = &report.sa_stats {
        let greedy = cost_triplet(&stats.greedy_initial_cost);
        let best = cost_triplet(&stats.best_cost);
        (
            greedy,
            best,
            stats.accepted_moves,
            stats.improved_moves,
            stats.total_iterations,
            stats.acceptance_rate,
            stats.improvement_rate,
            stats.greedy_iterations,
            stats.greedy_candidate_count,
            stats.greedy_improvement_count,
            stats.seed,
        )
    } else {
        (
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
            0,
            0,
            0,
            0.0,
            0.0,
            0,
            0,
            0,
            0,
        )
    };

    BaselineRunRecord {
        run_index,
        seed,
        egraph_nodes: report.egraph_nodes,
        egraph_classes: report.egraph_classes,
        original_delay: original[0],
        original_area: original[1],
        original_power: original[2],
        optimized_delay: optimized[0],
        optimized_area: optimized[1],
        optimized_power: optimized[2],
        greedy_initial_delay: greedy[0],
        greedy_initial_area: greedy[1],
        greedy_initial_power: greedy[2],
        best_delay: best[0],
        best_area: best[1],
        best_power: best[2],
        accepted_moves,
        improved_moves,
        total_iterations,
        acceptance_rate,
        improvement_rate,
        greedy_iterations,
        greedy_candidate_count,
        greedy_improvement_count,
    }
}

fn run_baseline(mut args: CliArgs) -> Result<BaselineSummary> {
    if args.extractor != "iterative-greedy-dag-SA" {
        bail!("baseline mode currently requires --extractor iterative-greedy-dag-SA");
    }

    let base_seed = args.seed;
    let mut records = Vec::with_capacity(args.baseline_runs);
    let mut module_name = String::new();
    let mut rewrite_egraph_nodes = 0;
    let mut rewrite_egraph_classes = 0;

    for run_index in 0..args.baseline_runs {
        args.seed = base_seed + run_index as u64;
        if args.baseline_runs > 1 {
            args.json_out = PathBuf::from(format!("json/phase0_run_{run_index:02}.json"));
            args.verilog_out = PathBuf::from(format!("verilog/phase0_run_{run_index:02}.v"));
        }
        let report = run_flow(&args)?;
        module_name = report.module_name.clone();
        rewrite_egraph_nodes = report.egraph_nodes;
        rewrite_egraph_classes = report.egraph_classes;
        records.push(build_baseline_record(run_index, &report));
    }

    let delay_values: Vec<f64> = records.iter().map(|r| r.optimized_delay).collect();
    let area_values: Vec<f64> = records.iter().map(|r| r.optimized_area).collect();
    let power_values: Vec<f64> = records.iter().map(|r| r.optimized_power).collect();
    let acceptance_values: Vec<f64> = records.iter().map(|r| r.acceptance_rate).collect();
    let improvement_values: Vec<f64> = records.iter().map(|r| r.improvement_rate).collect();

    Ok(BaselineSummary {
        module_name,
        runs: args.baseline_runs,
        extractor: args.extractor,
        seeds: records.iter().map(|r| r.seed).collect(),
        rewrite_egraph_nodes,
        rewrite_egraph_classes,
        delay: summarize_metric(&delay_values),
        area: summarize_metric(&area_values),
        power: summarize_metric(&power_values),
        acceptance_rate_mean: mean(&acceptance_values),
        improvement_rate_mean: mean(&improvement_values),
        records,
    })
}

fn print_baseline_summary(summary: &BaselineSummary) {
    println!("Phase 0 baseline report for {}", summary.module_name);
    println!(
        "Runs: {}, extractor: {}, rewrite e-graph: {} e-nodes / {} e-classes",
        summary.runs,
        summary.extractor,
        summary.rewrite_egraph_nodes,
        summary.rewrite_egraph_classes
    );
    println!(
        "Delay mean/std/best: {:.6} / {:.6} / {:.6}",
        summary.delay.mean, summary.delay.stddev, summary.delay.best
    );
    println!(
        "Area mean/std/best: {:.6} / {:.6} / {:.6}",
        summary.area.mean, summary.area.stddev, summary.area.best
    );
    println!(
        "Power mean/std/best: {:.6} / {:.6} / {:.6}",
        summary.power.mean, summary.power.stddev, summary.power.best
    );
    println!(
        "SA acceptance mean: {:.2}%, improvement mean: {:.2}%",
        summary.acceptance_rate_mean * 100.0,
        summary.improvement_rate_mean * 100.0
    );
}

fn print_pipeline_stage(label: &str, report: &FlowReport) {
    println!("== {} ==", label);
    print_report(report);
}

fn run_comm_inv_scale_two_stage_pipeline(args: &CliArgs) -> Result<()> {
    let stage1_json = append_suffix(&args.json_out, ".stage1_comm");
    let stage1_verilog = append_suffix(&args.verilog_out, ".stage1_comm");

    let mut stage1_args = args.clone();
    stage1_args.pipeline = "single".to_string();
    stage1_args.rules = vec![PathBuf::from("test/6t_comm_rules.json")];
    stage1_args.json_out = stage1_json;
    stage1_args.verilog_out = stage1_verilog;

    let stage1_report = run_flow(&stage1_args)?;
    print_pipeline_stage("Stage 1: comm", &stage1_report);

    let mut stage2_args = args.clone();
    stage2_args.pipeline = "single".to_string();
    stage2_args.input = stage1_args.verilog_out.clone();
    stage2_args.rules = vec![
        PathBuf::from("test/6t_inv_rules.json"),
        PathBuf::from("test/6t_scale_rules.json"),
    ];

    let stage2_report = run_flow(&stage2_args)?;
    print_pipeline_stage("Stage 2: inv+scale", &stage2_report);

    let overall_before = stage1_report.original_ppa.components;
    let overall_after = stage2_report.reparsed_optimized_ppa.components;
    println!("== Overall ==");
    println!(
        "Input -> final improvement: delay={:.2}%, area={:.2}%, power={:.2}%",
        pct_improvement(
            overall_before[0].into_inner(),
            overall_after[0].into_inner()
        ),
        pct_improvement(
            overall_before[1].into_inner(),
            overall_after[1].into_inner()
        ),
        pct_improvement(
            overall_before[2].into_inner(),
            overall_after[2].into_inner()
        ),
    );
    println!(
        "Intermediate stage-1 verilog: {}",
        stage1_args.verilog_out.display()
    );
    println!(
        "Intermediate stage-1 json: {}",
        stage1_args.json_out.display()
    );
    println!("Final verilog out: {}", stage2_report.verilog_out.display());
    println!("Final json out: {}", stage2_report.json_out.display());
    Ok(())
}

fn main() -> Result<()> {
    let args = parse_args()?;
    if args.pipeline != "single" && args.baseline_runs > 1 {
        bail!("pipeline mode does not support --baseline-runs > 1");
    }
    if args.pipeline == "comm-inv-scale-two-stage" {
        return run_comm_inv_scale_two_stage_pipeline(&args);
    }
    if args.pipeline != "single" {
        bail!("unknown pipeline: {}", args.pipeline);
    }
    if args.baseline_runs > 1 {
        let summary = run_baseline(args.clone())?;
        print_baseline_summary(&summary);
        if let Some(path) = args
            .baseline_report_out
            .clone()
            .or_else(|| Some(PathBuf::from("json/phase0_baseline_report.json")))
        {
            ensure_parent_dir(&path)?;
            std::fs::write(&path, serde_json::to_string_pretty(&summary)?)
                .with_context(|| format!("failed to write {}", path.display()))?;
            println!("Baseline JSON out: {}", path.display());
        }
    } else {
        let report = run_flow(&args)?;
        print_report(&report);
    }
    Ok(())
}
