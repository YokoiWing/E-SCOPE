//! Shared-context mapped-netlist P1 evaluation.
//!
//! This is a trajectory-preserving execution path for batches from one
//! parent.  It keeps one parsed Liberty/NLDM database and one load-context
//! cache while each complete candidate remains an independently evaluated
//! physical circuit.  Final acceptance continues to use the canonical
//! `evaluate_mapped_nldm_v2_batch` binary.

use anyhow::{Context, Result};
use d1_series::shared_load_v2::{SharedLoadEvaluator, TimingBoundary};
use regex::Regex;
use serde::Serialize;
use std::path::{Path, PathBuf};

const LIB: &str = "test/asap7sc6t_SELECT_LVT_TT_nldm.lib";

#[derive(Serialize)]
struct Record {
    model: &'static str,
    input: PathBuf,
    module: String,
    delay: f64,
    area: f64,
    power: f64,
    score: f64,
    internal_power_legacy: f64,
    leakage_power_legacy: f64,
    switched_capacitance_ff: f64,
    vectorless_internal_power_uw: f64,
    vectorless_switching_power_uw: f64,
    vectorless_leakage_power_uw: f64,
    vectorless_power_uw: f64,
    vectorless_toggle_sum: f64,
    finite: bool,
}

fn module_name(path: &Path, module_re: &Regex) -> Result<String> {
    let text = std::fs::read_to_string(path)?;
    Ok(module_re
        .captures(&text)
        .context("mapped Verilog has no module declaration")?[1]
        .to_owned())
}

pub fn run_cli(args: Vec<String>) -> Result<()> {
    let mut args = args.into_iter();
    let output = PathBuf::from(
        args.next()
            .context("usage: evaluate_mapped_nldm_v2_shared_batch OUTPUT_JSON INPUT...")?,
    );
    let inputs: Vec<PathBuf> = args.map(PathBuf::from).collect();
    anyhow::ensure!(!inputs.is_empty(), "batch requires at least one input");
    let lib_path = std::env::var("EGG_LIB_PATH").unwrap_or_else(|_| LIB.into());
    let lib_content = std::fs::read_to_string(&lib_path)?;
    let boundary = TimingBoundary::from_env()?;
    let mut evaluator =
        SharedLoadEvaluator::new(&lib_content, &inputs, true)?.with_boundary(boundary);
    let module_re = Regex::new(r"(?m)^\s*module\s+(\w+)")?;
    let mut rows = Vec::with_capacity(inputs.len());
    for input in &inputs {
        let ppa = evaluator.evaluate(input)?;
        rows.push(Record {
            model: boundary.internal_timing_model.model_name(),
            input: input.clone(),
            module: module_name(input, &module_re)?,
            delay: ppa.delay,
            area: ppa.area,
            power: ppa.power,
            score: ppa.score,
            internal_power_legacy: ppa.internal_power_legacy,
            leakage_power_legacy: ppa.leakage_power_legacy,
            switched_capacitance_ff: ppa.switched_capacitance_ff,
            vectorless_internal_power_uw: ppa.vectorless_internal_power_uw,
            vectorless_switching_power_uw: ppa.vectorless_switching_power_uw,
            vectorless_leakage_power_uw: ppa.vectorless_leakage_power_uw,
            vectorless_power_uw: ppa.vectorless_power_uw,
            vectorless_toggle_sum: ppa.vectorless_toggle_sum,
            finite: ppa.delay.is_finite() && ppa.area.is_finite() && ppa.power.is_finite(),
        });
    }
    std::fs::write(output, serde_json::to_string_pretty(&rows)? + "\n")?;
    Ok(())
}

fn main() -> Result<()> {
    run_cli(std::env::args().skip(1).collect())
}
