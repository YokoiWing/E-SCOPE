//! Dump occurrence-indexed Internal-NLDM-V3 forward and required-time state.
//!
//! This is a read-only development tool for electrically constrained local
//! resynthesis.  It uses the same TimingBoundary environment as the D1 batch
//! evaluator and does not alter the optimizer or a frozen trajectory.

use anyhow::{Context, Result};
use d1_series::shared_load_v2::TimingBoundary;
use extraction_gym::{DEFAULT_PRIMARY_INPUT_DRIVER_CELL, ExtendedEGraph};
use mac_egg::io::liberty::{get_direction_of_pins, read_liberty};
use mac_egg::io::stdcell::read_verilog_with_lib_to_netlist_with_symbols;
use mac_egg::language::LanguageType;
use mac_egg::netlist_to_egg_roots_with_provenance;
use mac_egg::physical_scale_egraph::build_occurrence_preserving_original_space;
use petgraph::graph::NodeIndex;
use serde::Serialize;
use std::path::PathBuf;

const LIB: &str = "test/asap7sc6t_SELECT_LVT_TT_nldm.lib";

#[derive(Serialize)]
struct AnchorState {
    anchor: usize,
    cell: String,
    load_ff: f64,
    arrival_rise_ps: f64,
    arrival_fall_ps: f64,
    slew_rise_ps: f64,
    slew_fall_ps: f64,
    required_rise_ps: f64,
    required_fall_ps: f64,
    slack_rise_ps: f64,
    slack_fall_ps: f64,
}

#[derive(Serialize)]
struct OccurrenceState {
    anchor: usize,
    op: String,
    signal: String,
    inputs: Vec<usize>,
    consumers: Vec<usize>,
    is_leaf: bool,
    is_root: bool,
    is_constant: bool,
}

#[derive(Serialize)]
struct StateDump {
    input: PathBuf,
    module: String,
    liberty: String,
    model: &'static str,
    boundary: TimingBoundary,
    delay_ps: f64,
    area: f64,
    power: f64,
    /// Parser-stable physical occurrence graph.  Keeping this beside the
    /// timing state lets bounded structural sidecars consume one cold parse
    /// without relying on stale planner anchors from an earlier parent.
    occurrences: Vec<OccurrenceState>,
    anchors: Vec<AnchorState>,
}

fn physical_anchor(text: &str) -> Option<usize> {
    text.strip_prefix("physical.")?.parse().ok()
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let input = PathBuf::from(
        args.next()
            .context("usage: dump_mapped_nldm_v3_state INPUT OUTPUT_JSON")?,
    );
    let output = PathBuf::from(
        args.next()
            .context("usage: dump_mapped_nldm_v3_state INPUT OUTPUT_JSON")?,
    );
    anyhow::ensure!(args.next().is_none(), "unexpected extra argument");
    let lib_path = std::env::var("EGG_LIB_PATH").unwrap_or_else(|_| LIB.into());
    let liberty = read_liberty(&lib_path).map_err(anyhow::Error::msg)?;
    let pins = get_direction_of_pins(&liberty).map_err(anyhow::Error::msg)?;
    let (netlist, module, symbols) =
        read_verilog_with_lib_to_netlist_with_symbols(&input, pins).map_err(anyhow::Error::msg)?;
    let occurrences = netlist
        .graph
        .node_indices()
        .map(|anchor| {
            let mut consumers: Vec<_> = netlist.outputs(anchor).map(|item| item.index()).collect();
            consumers.sort_unstable();
            OccurrenceState {
                anchor: anchor.index(),
                op: netlist.graph[anchor].to_string(),
                signal: symbols
                    .get(&anchor)
                    .cloned()
                    .unwrap_or_else(|| netlist.graph[anchor].to_string()),
                inputs: netlist.inputs(anchor).map(|item| item.index()).collect(),
                consumers,
                is_leaf: netlist.leaves.contains(&anchor),
                is_root: netlist.roots.contains(&anchor),
                is_constant: netlist.graph[anchor].is_constant(),
            }
        })
        .collect();
    let provenance =
        netlist_to_egg_roots_with_provenance::<_, ()>(&netlist).map_err(anyhow::Error::msg)?;
    let space = build_occurrence_preserving_original_space(&netlist, &provenance)
        .map_err(anyhow::Error::msg)?;
    let json = serde_json::to_value(&space.egraph)?;
    let lib_content = std::fs::read_to_string(&lib_path)?;
    let mut cache = std::collections::HashMap::new();
    let mut ext = ExtendedEGraph::from_base_to_extention_with_shared_nldm(
        space.egraph,
        &json,
        &lib_content,
        &mut cache,
    );
    let boundary = TimingBoundary::from_env()?;
    if boundary.internal_timing_model.is_v3() {
        ext.ensure_cell_nldm_from_lib(&lib_path, DEFAULT_PRIMARY_INPUT_DRIVER_CELL)
            .map_err(anyhow::Error::msg)?;
    }
    let trace =
        space
            .original_extraction
            .evaluate_nldm_v2_with_trace(&ext, None, boundary.nldm_config());
    let mut anchors = Vec::new();
    for (class, point) in &trace.timing {
        let Some(anchor) = physical_anchor(&class.to_string()) else {
            continue;
        };
        anyhow::ensure!(
            anchor < netlist.graph.node_count(),
            "timing class {class} is outside source graph"
        );
        anchors.push(AnchorState {
            anchor,
            cell: netlist.graph[NodeIndex::new(anchor)].to_string(),
            load_ff: point.load,
            arrival_rise_ps: point.rise.arrival,
            arrival_fall_ps: point.fall.arrival,
            slew_rise_ps: point.rise.transition,
            slew_fall_ps: point.fall.transition,
            required_rise_ps: point.rise.required,
            required_fall_ps: point.fall.required,
            slack_rise_ps: point.rise.slack,
            slack_fall_ps: point.fall.slack,
        });
    }
    anchors.sort_by_key(|row| row.anchor);
    let cost = trace.cost;
    let dump = StateDump {
        input,
        module,
        liberty: lib_path,
        model: boundary.internal_timing_model.model_name(),
        boundary,
        delay_ps: cost.components[0].into_inner(),
        area: cost.components[1].into_inner(),
        power: cost.components[2].into_inner(),
        occurrences,
        anchors,
    };
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output, serde_json::to_string_pretty(&dump)? + "\n")?;
    Ok(())
}
