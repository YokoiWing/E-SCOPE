//! Fast mapped-netlist NLDM-V2 evaluator with producer-independent load reuse.
//!
//! For a fixed downstream realization, every producer e-node of an e-class
//! sees the same output load.  The cache key below therefore contains only the
//! ordered downstream `(cell, input-pin)` context and deliberately excludes
//! the producer cell.  Timing, slew and power are still recomputed exactly for
//! the selected producer realization.

use anyhow::{Context, Result};
use egraph_serialize::{ClassId, Cost, Node, NodeId};
use extraction_gym::extract::NldmV2Config;
use extraction_gym::{DEFAULT_PRIMARY_INPUT_DRIVER_CELL, ExtendedEGraph, Lut2D, NLDM, TimingSense};
use mac_egg::SerializedEGraph;
use mac_egg::language::{LanguageType, StdCellType};
use mac_egg::netlist::Netlist;
use petgraph::algo::toposort;
use petgraph::graph::NodeIndex;
use regex::Regex;
use rustc_hash::FxHashSet;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

/// Return the Boolean value of a scalar Verilog constant accepted by the
/// mapped-netlist readers.  Continuous assignments are otherwise treated as
/// signal aliases, so constant-driven outputs must be recognized explicitly.
pub fn verilog_scalar_constant(signal: &str) -> Option<bool> {
    match signal.trim().to_ascii_lowercase().as_str() {
        "0" | "1'b0" | "1'h0" | "1'd0" | "false" => Some(false),
        "1" | "1'b1" | "1'h1" | "1'd1" | "true" => Some(true),
        _ => None,
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Ppa {
    pub delay: f64,
    pub area: f64,
    pub power: f64,
    pub score: f64,
    /// Legacy Internal-NLDM-V2 cell-internal energy sum.  This remains the
    /// `power` value unless an explicitly selected activity model replaces it.
    pub internal_power_legacy: f64,
    pub leakage_power_legacy: f64,
    /// Sum of all reachable driven-net capacitances, including primary-input
    /// nets and configured primary-output loads, in fF.
    pub switched_capacitance_ff: f64,
    pub vectorless_internal_power_uw: f64,
    pub vectorless_switching_power_uw: f64,
    pub vectorless_leakage_power_uw: f64,
    pub vectorless_power_uw: f64,
    pub vectorless_toggle_sum: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InternalTimingModel {
    /// Author GENLIB adapter: fixed pin delay, no electrical/power claim.
    GenlibStatic,
    LegacyScalarCoupledV2,
    DriverAwareIndependentSlewV3,
    DriverAwareCorrelatedSlewV3,
}

impl InternalTimingModel {
    pub fn model_name(self) -> &'static str {
        match self {
            Self::GenlibStatic => "GENLIB fixed-pin-delay (power unavailable)",
            Self::LegacyScalarCoupledV2 => "Internal-NLDM-V2 scalar-PI coupled-slew",
            Self::DriverAwareIndependentSlewV3 => {
                "Internal-NLDM-V3 driver-aware independent-worst-slew"
            }
            Self::DriverAwareCorrelatedSlewV3 => {
                "Internal-NLDM-V3.1 driver-aware arrival-correlated-slew"
            }
        }
    }

    pub fn is_v3(self) -> bool {
        matches!(
            self,
            Self::DriverAwareIndependentSlewV3 | Self::DriverAwareCorrelatedSlewV3
        )
    }

    pub fn is_genlib(self) -> bool {
        self == Self::GenlibStatic
    }

    pub fn propagate(self, arrival: f64, delay: f64) -> f64 {
        if self.is_genlib() {
            ((arrival + (delay * 100.0).ceil() / 100.0) * 100.0).ceil() / 100.0
        } else {
            arrival + delay
        }
    }

    pub fn area(self, area: f64) -> f64 {
        if self.is_genlib() {
            // GENLIB areas have exactly two decimal places; avoid an extra
            // cent caused solely by reordering an equivalent mapped netlist.
            (area * 100.0).round() / 100.0
        } else {
            area
        }
    }

    pub fn uses_independent_worst_transition(self) -> bool {
        matches!(self, Self::DriverAwareIndependentSlewV3)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct TimingBoundary {
    pub primary_input_driver_cell: &'static str,
    pub primary_input_arrival_ps: f64,
    pub primary_input_slew_ps: f64,
    pub primary_output_load_ff: f64,
    pub vectorless_power: bool,
    pub power_period_ps: f64,
    pub power_voltage_v: f64,
    pub primary_input_probability: f64,
    pub primary_input_toggle_per_cycle: f64,
    pub power_activity_samples: usize,
    pub uniform_comb_activity: bool,
    pub internal_transition_factor: f64,
    pub internal_timing_model: InternalTimingModel,
    pub driver_input_slew_ps: f64,
}

/// Complete fixed electrical boundary for an extracted mapped subnetwork.
///
/// This is deliberately separate from `TimingBoundary`: ordinary whole-net
/// V8 continues to use the historical scalar PI/PO model bit-for-bit, while
/// the partition-local experiment opts into a complete ordered port vector by
/// setting `EGG_PORT_BOUNDARY_JSON`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PortInputBoundary {
    pub name: String,
    pub arrival_rise_ps: f64,
    pub arrival_fall_ps: f64,
    pub slew_rise_ps: f64,
    pub slew_fall_ps: f64,
    /// Input capacitance of the untouched partition.  It is optional because
    /// only objective-aware partition experiments use it as an upstream-load
    /// contract.
    #[serde(default)]
    pub baseline_capacitance_ff: Option<f64>,
    /// Optional input-specific capacitance regression allowance.  This is an
    /// opt-in partition-search contract; when absent, the objective-wide
    /// allowance (if any) is used exactly as before.
    #[serde(default)]
    pub max_cap_regression_ff: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PortOutputBoundary {
    pub name: String,
    pub load_ff: f64,
    /// Optional downstream timing contract used only by opt-in partition-local
    /// objective experiments.  Ordinary whole-net V8 ignores these fields.
    #[serde(default)]
    pub required_rise_ps: Option<f64>,
    #[serde(default)]
    pub required_fall_ps: Option<f64>,
    /// Arrival of the untouched local implementation.  These fields are
    /// carried only for opt-in boundary-certified sizing; ordinary whole-net
    /// and partition objectives continue to ignore them.
    #[serde(default)]
    pub baseline_arrival_rise_ps: Option<f64>,
    #[serde(default)]
    pub baseline_arrival_fall_ps: Option<f64>,
    #[serde(default)]
    pub baseline_slew_rise_ps: Option<f64>,
    #[serde(default)]
    pub baseline_slew_fall_ps: Option<f64>,
    /// Optional output-specific electrical guards.  They override the
    /// objective-wide limits and are consumed only by an opt-in partition
    /// objective.
    #[serde(default)]
    pub max_arrival_regression_ps: Option<f64>,
    #[serde(default)]
    pub max_slew_regression_ps: Option<f64>,
    /// Relative exposure to reachable critical endpoints and shared
    /// reconvergence.  One is neutral.  Historical objective files do not
    /// set this field and therefore retain their previous behavior.
    #[serde(default)]
    pub path_family_weight: Option<f64>,
    /// Audit fields emitted by the whole-aware boundary builder.
    #[serde(default)]
    pub reachable_endpoint_count: Option<usize>,
    #[serde(default)]
    pub critical_endpoint_count: Option<usize>,
    #[serde(default)]
    pub shared_reconvergence_count: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PortBoundaryOverrides {
    pub schema: String,
    pub inputs: Vec<PortInputBoundary>,
    pub outputs: Vec<PortOutputBoundary>,
}

impl PortBoundaryOverrides {
    pub fn from_env() -> Result<Option<Self>> {
        let Some(path) = std::env::var_os("EGG_PORT_BOUNDARY_JSON") else {
            return Ok(None);
        };
        let path = PathBuf::from(path);
        let value: Self = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("cannot parse {}", path.display()))?;
        anyhow::ensure!(
            value.schema == "egg-port-boundary-v1",
            "unsupported port boundary schema {}",
            value.schema
        );
        anyhow::ensure!(
            !value.inputs.is_empty() && !value.outputs.is_empty(),
            "port boundary must contain inputs and outputs"
        );
        for input in &value.inputs {
            for (field, number) in [
                ("arrival_rise_ps", input.arrival_rise_ps),
                ("arrival_fall_ps", input.arrival_fall_ps),
                ("slew_rise_ps", input.slew_rise_ps),
                ("slew_fall_ps", input.slew_fall_ps),
            ] {
                anyhow::ensure!(
                    number.is_finite() && number >= 0.0,
                    "{}.{field} must be finite and non-negative",
                    input.name
                );
            }
            if let Some(number) = input.baseline_capacitance_ff {
                anyhow::ensure!(
                    number.is_finite() && number >= 0.0,
                    "{}.baseline_capacitance_ff must be finite and non-negative",
                    input.name
                );
            }
            if let Some(number) = input.max_cap_regression_ff {
                anyhow::ensure!(
                    number.is_finite() && number >= 0.0,
                    "{}.max_cap_regression_ff must be finite and non-negative",
                    input.name
                );
            }
        }
        for output in &value.outputs {
            anyhow::ensure!(
                output.load_ff.is_finite() && output.load_ff >= 0.0,
                "{}.load_ff must be finite and non-negative",
                output.name
            );
            for (field, number) in [
                ("required_rise_ps", output.required_rise_ps),
                ("required_fall_ps", output.required_fall_ps),
                ("baseline_arrival_rise_ps", output.baseline_arrival_rise_ps),
                ("baseline_arrival_fall_ps", output.baseline_arrival_fall_ps),
                ("baseline_slew_rise_ps", output.baseline_slew_rise_ps),
                ("baseline_slew_fall_ps", output.baseline_slew_fall_ps),
                (
                    "max_arrival_regression_ps",
                    output.max_arrival_regression_ps,
                ),
                ("max_slew_regression_ps", output.max_slew_regression_ps),
            ] {
                if let Some(number) = number {
                    anyhow::ensure!(
                        number.is_finite() && number >= 0.0,
                        "{}.{field} must be finite and non-negative",
                        output.name
                    );
                }
            }
            if let Some(number) = output.path_family_weight {
                anyhow::ensure!(
                    number.is_finite() && number > 0.0,
                    "{}.path_family_weight must be finite and positive",
                    output.name
                );
            }
        }
        Ok(Some(value))
    }

    pub fn ordered_inputs(&self, names: &[String]) -> Result<Vec<PortInputBoundary>> {
        let by_name: HashMap<_, _> = self
            .inputs
            .iter()
            .map(|row| (row.name.as_str(), row))
            .collect();
        anyhow::ensure!(
            by_name.len() == self.inputs.len(),
            "duplicate input in port boundary"
        );
        anyhow::ensure!(
            names.len() == self.inputs.len(),
            "port boundary has {} inputs but netlist has {}",
            self.inputs.len(),
            names.len()
        );
        names
            .iter()
            .map(|name| {
                by_name
                    .get(name.as_str())
                    .with_context(|| format!("input {name} absent from port boundary"))
                    .map(|row| (*row).clone())
            })
            .collect()
    }

    pub fn ordered_outputs(&self, names: &[String]) -> Result<Vec<PortOutputBoundary>> {
        let by_name: HashMap<_, _> = self
            .outputs
            .iter()
            .map(|row| (row.name.as_str(), row))
            .collect();
        anyhow::ensure!(
            by_name.len() == self.outputs.len(),
            "duplicate output in port boundary"
        );
        anyhow::ensure!(
            names.len() == self.outputs.len(),
            "port boundary has {} outputs but netlist has {}",
            self.outputs.len(),
            names.len()
        );
        names
            .iter()
            .map(|name| {
                by_name
                    .get(name.as_str())
                    .with_context(|| format!("output {name} absent from port boundary"))
                    .map(|row| (*row).clone())
            })
            .collect()
    }
}

impl Default for TimingBoundary {
    fn default() -> Self {
        Self {
            primary_input_driver_cell: DEFAULT_PRIMARY_INPUT_DRIVER_CELL,
            primary_input_arrival_ps: 0.0,
            primary_input_slew_ps: 0.0,
            primary_output_load_ff: 0.0,
            vectorless_power: false,
            power_period_ps: 1000.0,
            power_voltage_v: 0.7,
            primary_input_probability: 0.5,
            primary_input_toggle_per_cycle: 0.1,
            power_activity_samples: 4096,
            uniform_comb_activity: false,
            internal_transition_factor: 1.78,
            internal_timing_model: InternalTimingModel::LegacyScalarCoupledV2,
            driver_input_slew_ps: 0.0,
        }
    }
}

impl TimingBoundary {
    pub fn nldm_config(self) -> NldmV2Config {
        NldmV2Config {
            primary_input_driver_cell: self.primary_input_driver_cell,
            genlib_static: self.internal_timing_model.is_genlib(),
            primary_input_arrival_ps: self.primary_input_arrival_ps,
            primary_input_slew_ps: self.primary_input_slew_ps,
            primary_output_load_ff: self.primary_output_load_ff,
            uniform_vectorless_power: self.vectorless_power && self.uniform_comb_activity,
            power_period_ps: self.power_period_ps,
            power_voltage_v: self.power_voltage_v,
            toggle_per_cycle: self.primary_input_toggle_per_cycle,
            internal_transition_factor: self.internal_transition_factor,
            driver_aware_primary_inputs: self.internal_timing_model.is_v3(),
            driver_input_slew_ps: self.driver_input_slew_ps,
            independent_worst_transition: self
                .internal_timing_model
                .uses_independent_worst_transition(),
        }
    }

    pub fn from_env() -> Result<Self> {
        let parse_or = |name: &str, default: f64| -> Result<f64> {
            let value = std::env::var(name)
                .ok()
                .map(|value| value.parse::<f64>())
                .transpose()
                .with_context(|| format!("{name} must be a finite non-negative number"))?
                .unwrap_or(default);
            anyhow::ensure!(
                value.is_finite() && value >= 0.0,
                "{name} must be a finite non-negative number"
            );
            Ok(value)
        };
        let vectorless_power = match std::env::var("EGG_POWER_MODE") {
            Ok(value) if value == "vectorless" => true,
            Ok(value) if value == "legacy" => false,
            Err(std::env::VarError::NotPresent) => false,
            Ok(value) => anyhow::bail!("EGG_POWER_MODE must be legacy or vectorless, got {value}"),
            Err(error) => return Err(error.into()),
        };
        let uniform_comb_activity = match std::env::var("EGG_POWER_ACTIVITY_MODEL") {
            Ok(value) if value == "uniform" => true,
            Ok(value) if value == "simulation" => false,
            Err(std::env::VarError::NotPresent) => false,
            Ok(value) => {
                anyhow::bail!("EGG_POWER_ACTIVITY_MODEL must be simulation or uniform, got {value}")
            }
            Err(error) => return Err(error.into()),
        };
        let internal_timing_model = match std::env::var("EGG_INTERNAL_TIMING_MODEL") {
            Ok(value) if value == "genlib-static" => InternalTimingModel::GenlibStatic,
            Ok(value) if matches!(value.as_str(), "legacy-v2" | "legacy") => {
                InternalTimingModel::LegacyScalarCoupledV2
            }
            Ok(value) if matches!(value.as_str(), "driver-aware-v3" | "v3") => {
                InternalTimingModel::DriverAwareIndependentSlewV3
            }
            Ok(value)
                if matches!(
                    value.as_str(),
                    "driver-aware-correlated-v3" | "correlated-v3" | "v3.1"
                ) =>
            {
                InternalTimingModel::DriverAwareCorrelatedSlewV3
            }
            Err(std::env::VarError::NotPresent) => {
                InternalTimingModel::DriverAwareIndependentSlewV3
            }
            Ok(value) => anyhow::bail!(
                "EGG_INTERNAL_TIMING_MODEL must be genlib-static, legacy-v2, driver-aware-v3, or driver-aware-correlated-v3, got {value}"
            ),
            Err(error) => return Err(error.into()),
        };
        let primary_input_driver_cell = match std::env::var("EGG_PRIMARY_INPUT_DRIVER_CELL") {
            Err(std::env::VarError::NotPresent) => DEFAULT_PRIMARY_INPUT_DRIVER_CELL,
            Ok(value) if value == DEFAULT_PRIMARY_INPUT_DRIVER_CELL => {
                DEFAULT_PRIMARY_INPUT_DRIVER_CELL
            }
            Ok(value) if value == "BUFx2_ASAP7_75t_R" => "BUFx2_ASAP7_75t_R",
            other => anyhow::bail!("unsupported EGG_PRIMARY_INPUT_DRIVER_CELL: {other:?}"),
        };
        let boundary = Self {
            primary_input_driver_cell,
            primary_input_arrival_ps: parse_or("EGG_PI_ARRIVAL_PS", 0.0)?,
            primary_input_slew_ps: parse_or("EGG_PI_SLEW_PS", 0.0)?,
            primary_output_load_ff: parse_or("EGG_PO_LOAD_FF", 0.0)?,
            vectorless_power,
            power_period_ps: parse_or("EGG_POWER_PERIOD_PS", 1000.0)?,
            power_voltage_v: parse_or("EGG_POWER_VOLTAGE_V", 0.7)?,
            primary_input_probability: parse_or("EGG_PI_STATIC_PROBABILITY", 0.5)?,
            primary_input_toggle_per_cycle: parse_or("EGG_PI_TOGGLE_PER_CYCLE", 0.1)?,
            power_activity_samples: std::env::var("EGG_POWER_ACTIVITY_SAMPLES")
                .ok()
                .map(|value| value.parse::<usize>())
                .transpose()
                .context("EGG_POWER_ACTIVITY_SAMPLES must be a positive integer")?
                .unwrap_or(4096),
            uniform_comb_activity,
            internal_transition_factor: parse_or("EGG_INTERNAL_TRANSITION_FACTOR", 1.78)?,
            internal_timing_model,
            driver_input_slew_ps: parse_or("EGG_DRIVER_INPUT_SLEW_PS", 0.0)?,
        };
        anyhow::ensure!(
            boundary.power_period_ps > 0.0,
            "EGG_POWER_PERIOD_PS must be positive"
        );
        if boundary.internal_timing_model.is_genlib() {
            anyhow::ensure!(
                boundary.primary_input_arrival_ps == 0.0
                    && boundary.primary_output_load_ff == 0.0
                    && !boundary.vectorless_power,
                "GENLIB static mode requires zero PI arrival/PO load and legacy (unavailable) power"
            );
        }
        anyhow::ensure!(
            boundary.primary_input_probability <= 1.0,
            "EGG_PI_STATIC_PROBABILITY must be at most 1"
        );
        anyhow::ensure!(
            boundary.primary_input_toggle_per_cycle <= 1.0,
            "EGG_PI_TOGGLE_PER_CYCLE must be at most 1"
        );
        anyhow::ensure!(
            boundary.power_activity_samples > 0,
            "EGG_POWER_ACTIVITY_SAMPLES must be positive"
        );
        Ok(boundary)
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SharedLoadStats {
    pub requests: usize,
    pub hits: usize,
    pub misses: usize,
    pub unique_contexts: usize,
}

#[derive(Clone)]
struct Inst {
    name: String,
    cell: usize,
    inputs: Vec<String>,
    input_sources: Vec<Option<usize>>,
    input_is_primary: Vec<bool>,
    input_primary_index: Vec<Option<usize>>,
}

struct Circuit {
    instances: Vec<Inst>,
    roots: Vec<usize>,
    reachable: Vec<usize>,
    reachable_set: HashSet<usize>,
    topo: Vec<usize>,
    consumers: Vec<Vec<(usize, usize)>>,
    output_load_count: Vec<usize>,
    primary_output_load_count: Vec<usize>,
    canonical_sum_order: Vec<usize>,
    primary_input_count: usize,
    fixed_primary_input: Option<Vec<PortInputBoundary>>,
    fixed_output_load_ff: Option<Vec<f64>>,
    fixed_primary_output_load_ff: Option<Vec<f64>>,
}

impl Circuit {
    #[inline]
    fn external_output_load(&self, source: usize, scalar_load_ff: f64) -> f64 {
        self.fixed_output_load_ff.as_ref().map_or_else(
            || self.output_load_count[source] as f64 * scalar_load_ff,
            |loads| loads[source],
        )
    }

    #[inline]
    fn primary_external_output_load(&self, primary: usize, scalar_load_ff: f64) -> f64 {
        self.fixed_primary_output_load_ff.as_ref().map_or_else(
            || self.primary_output_load_count[primary] as f64 * scalar_load_ff,
            |loads| loads[primary],
        )
    }
}

fn next_activity_random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn bernoulli_activity_word(state: &mut u64, probability: f64) -> u64 {
    if probability <= 0.0 {
        return 0;
    }
    if probability >= 1.0 {
        return u64::MAX;
    }
    if probability == 0.5 {
        return next_activity_random(state);
    }
    let threshold = (probability * u64::MAX as f64) as u64;
    (0..64).fold(0u64, |word, bit| {
        word | (u64::from(next_activity_random(state) <= threshold) << bit)
    })
}

fn evaluate_truth_words(truth: &[bool], inputs: &[&[u64]], words: usize) -> Result<Vec<u64>> {
    anyhow::ensure!(
        truth.len() == 1usize << inputs.len(),
        "Boolean truth table/input mismatch"
    );
    let mut output = vec![0u64; words];
    for (assignment, _) in truth.iter().enumerate().filter(|(_, value)| **value) {
        for word in 0..words {
            let mut term = u64::MAX;
            for (input, values) in inputs.iter().enumerate() {
                term &= if assignment & (1 << input) == 0 {
                    !values[word]
                } else {
                    values[word]
                };
            }
            output[word] |= term;
        }
    }
    Ok(output)
}

fn simulate_vectorless_activity(
    circuit: &Circuit,
    cells: &[NLDM],
    boundary: &TimingBoundary,
) -> Result<(Vec<f64>, Vec<f64>)> {
    let samples = boundary.power_activity_samples;
    let words = samples.div_ceil(64);
    let remainder = samples % 64;
    let final_mask = if remainder == 0 {
        u64::MAX
    } else {
        (1u64 << remainder) - 1
    };
    let mut random = 0x8f6c_4d21_95a7_b3e1u64;
    let mut primary_old = vec![vec![0u64; words]; circuit.primary_input_count];
    let mut primary_new = vec![vec![0u64; words]; circuit.primary_input_count];
    for input in 0..circuit.primary_input_count {
        for word in 0..words {
            let old = bernoulli_activity_word(&mut random, boundary.primary_input_probability);
            let flip =
                bernoulli_activity_word(&mut random, boundary.primary_input_toggle_per_cycle);
            primary_old[input][word] = old;
            primary_new[input][word] = old ^ flip;
        }
        primary_old[input][words - 1] &= final_mask;
        primary_new[input][words - 1] &= final_mask;
    }
    let zero = vec![0u64; words];
    let mut one = vec![u64::MAX; words];
    one[words - 1] &= final_mask;
    let mut old = vec![Vec::<u64>::new(); circuit.instances.len()];
    let mut new = vec![Vec::<u64>::new(); circuit.instances.len()];
    let mut probability = vec![0.0; circuit.instances.len()];
    let mut toggle = vec![0.0; circuit.instances.len()];
    for &index in &circuit.topo {
        let instance = &circuit.instances[index];
        let cell = &cells[instance.cell];
        anyhow::ensure!(
            !cell.logic_truth_table.is_empty(),
            "cell {} has no parsed Liberty Boolean function",
            instance.name
        );
        let old_inputs: Vec<&[u64]> = instance
            .input_sources
            .iter()
            .enumerate()
            .map(|(pin, source)| {
                source.map_or_else(
                    || {
                        instance.input_primary_index[pin].map_or_else(
                            || {
                                if instance.inputs[pin] == "1" {
                                    one.as_slice()
                                } else {
                                    zero.as_slice()
                                }
                            },
                            |input| primary_old[input].as_slice(),
                        )
                    },
                    |source| old[source].as_slice(),
                )
            })
            .collect();
        let new_inputs: Vec<&[u64]> = instance
            .input_sources
            .iter()
            .enumerate()
            .map(|(pin, source)| {
                source.map_or_else(
                    || {
                        instance.input_primary_index[pin].map_or_else(
                            || {
                                if instance.inputs[pin] == "1" {
                                    one.as_slice()
                                } else {
                                    zero.as_slice()
                                }
                            },
                            |input| primary_new[input].as_slice(),
                        )
                    },
                    |source| new[source].as_slice(),
                )
            })
            .collect();
        let output_old = evaluate_truth_words(&cell.logic_truth_table, &old_inputs, words)?;
        let output_new = evaluate_truth_words(&cell.logic_truth_table, &new_inputs, words)?;
        probability[index] = output_old
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum::<usize>() as f64
            / samples as f64;
        toggle[index] = output_old
            .iter()
            .zip(&output_new)
            .map(|(before, after)| (before ^ after).count_ones() as usize)
            .sum::<usize>() as f64
            / samples as f64;
        old[index] = output_old;
        new[index] = output_new;
    }
    Ok((probability, toggle))
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct LoadContext(Vec<(usize, usize)>);

pub struct SharedLoadEvaluator {
    cells: Vec<NLDM>,
    cell_by_name: HashMap<String, usize>,
    load_cache: HashMap<LoadContext, f64>,
    reuse_load_contexts: bool,
    stats: SharedLoadStats,
    boundary: TimingBoundary,
    port_boundary: Option<PortBoundaryOverrides>,
    comment_re: Regex,
    input_re: Regex,
    output_re: Regex,
    qualifier_re: Regex,
    assign_re: Regex,
    inst_re: Regex,
    conn_re: Regex,
}

fn mapped_cell_names(path: &Path, inst_re: &Regex) -> Result<BTreeSet<String>> {
    let text = fs::read_to_string(path)?;
    Ok(inst_re
        .captures_iter(&text)
        .map(|capture| capture[1].to_owned())
        .collect())
}

fn load_cells(lib_content: &str, names: &BTreeSet<String>) -> Result<(Vec<String>, Vec<NLDM>)> {
    let mut graph = SerializedEGraph::default();
    for (index, name) in names.iter().enumerate() {
        graph.add_node(
            NodeId::from(format!("n{index}")),
            Node {
                op: name.clone(),
                children: Vec::new(),
                eclass: ClassId::from(format!("c{index}")),
                cost: Cost::new(1.0).unwrap(),
            },
        );
    }
    let serialized = serde_json::to_value(&graph)?;
    let ext = ExtendedEGraph::from_base_to_extention_with_shared_nldm(
        graph,
        &serialized,
        lib_content,
        &mut HashMap::new(),
    );
    let ordered: Vec<_> = names.iter().cloned().collect();
    let cells = ordered
        .iter()
        .map(|name| {
            ext.cell_nldm
                .get(name)
                .with_context(|| format!("cannot parse NLDM cell {name}"))
                .cloned()
        })
        .collect::<Result<_>>()?;
    Ok((ordered, cells))
}

impl SharedLoadEvaluator {
    fn with_cells(
        cell_names: Vec<String>,
        cells: Vec<NLDM>,
        reuse_load_contexts: bool,
    ) -> Result<Self> {
        let cell_by_name = cell_names
            .into_iter()
            .enumerate()
            .map(|(index, name)| (name, index))
            .collect();
        Ok(Self {
            cells,
            cell_by_name,
            load_cache: HashMap::new(),
            reuse_load_contexts,
            stats: SharedLoadStats::default(),
            boundary: TimingBoundary::default(),
            port_boundary: PortBoundaryOverrides::from_env()?,
            comment_re: Regex::new(r"//[^\n]*")?,
            input_re: Regex::new(r"(?s)\binput\b\s+([^;]+);")?,
            output_re: Regex::new(r"(?s)\boutput\b\s+([^;]+);")?,
            qualifier_re: Regex::new(r"\b(?:wire|reg|logic|signed)\b")?,
            assign_re: Regex::new(r"\bassign\s+(\w+)\s*=\s*([^;\s]+)\s*;")?,
            inst_re: Regex::new(r"(?ms)^\s*(\w+_ASAP7_(?:6t_L|75t_R))\s+(\w+)\s*\((.*?)\)\s*;")?,
            conn_re: Regex::new(r"\.(\w+)\s*\(\s*([^()]+?)\s*\)")?,
        })
    }

    pub fn new(lib_content: &str, netlists: &[PathBuf], reuse_load_contexts: bool) -> Result<Self> {
        let inst_re = Regex::new(r"(?ms)^\s*(\w+_ASAP7_(?:6t_L|75t_R))\s+(\w+)\s*\((.*?)\)\s*;")?;
        let mut names = BTreeSet::new();
        for path in netlists {
            names.extend(mapped_cell_names(path, &inst_re)?);
        }
        if TimingBoundary::from_env()?.internal_timing_model.is_v3() {
            names.insert(
                TimingBoundary::from_env()?
                    .primary_input_driver_cell
                    .to_owned(),
            );
        }
        anyhow::ensure!(!names.is_empty(), "candidate set contains no mapped cells");
        let (cell_names, cells) = load_cells(lib_content, &names)?;
        Self::with_cells(cell_names, cells, reuse_load_contexts)
    }

    /// Construct directly from the process-wide immutable NLDM database.
    /// NLDM's large lookup tables are Arc-backed, so this only clones small
    /// descriptors and shared handles rather than reparsing Liberty.
    pub fn from_nldm_cache(
        cache: &HashMap<String, NLDM>,
        reuse_load_contexts: bool,
    ) -> Result<Self> {
        let mut names: Vec<_> = cache.keys().cloned().collect();
        names.sort();
        let cells = names.iter().map(|name| cache[name].clone()).collect();
        Self::with_cells(names, cells, reuse_load_contexts)
    }

    pub fn stats(&self) -> SharedLoadStats {
        let mut value = self.stats.clone();
        value.unique_contexts = self.load_cache.len();
        value
    }

    pub fn with_boundary(mut self, boundary: TimingBoundary) -> Self {
        self.boundary = boundary;
        self
    }

    fn parse(&self, path: &Path) -> Result<Circuit> {
        let source = fs::read_to_string(path)?;
        let text = self.comment_re.replace_all(&source, "");
        let mut inputs = Vec::new();
        for capture in self.input_re.captures_iter(&text) {
            let chunk = self.qualifier_re.replace_all(&capture[1], "");
            inputs.extend(
                chunk
                    .replace('\n', " ")
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned),
            );
        }
        let mut outputs = Vec::new();
        for capture in self.output_re.captures_iter(&text) {
            let chunk = self.qualifier_re.replace_all(&capture[1], "");
            outputs.extend(
                chunk
                    .replace('\n', " ")
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned),
            );
        }
        let assigns: HashMap<String, String> = self
            .assign_re
            .captures_iter(&text)
            .map(|capture| (capture[1].to_owned(), capture[2].to_owned()))
            .collect();
        let mut raw = Vec::new();
        for capture in self.inst_re.captures_iter(&text) {
            let connections: HashMap<String, String> = self
                .conn_re
                .captures_iter(&capture[3])
                .map(|value| (value[1].to_owned(), value[2].trim().to_owned()))
                .collect();
            raw.push((capture[2].to_owned(), capture[1].to_owned(), connections));
        }
        anyhow::ensure!(!raw.is_empty(), "no mapped instances in {}", path.display());

        let mut instances = Vec::with_capacity(raw.len());
        let mut driver = HashMap::new();
        for (index, (name, cell_name, connections)) in raw.iter().enumerate() {
            let cell_id = *self
                .cell_by_name
                .get(cell_name)
                .with_context(|| format!("cell {cell_name} absent from shared database"))?;
            let cell = &self.cells[cell_id];
            let output = connections
                .get(&cell.pin_order[0])
                .with_context(|| format!("{name} missing output pin {}", cell.pin_order[0]))?
                .clone();
            let inputs = cell
                .pin_order
                .iter()
                .skip(1)
                .map(|pin| {
                    connections
                        .get(pin)
                        .with_context(|| format!("{name} missing input pin {pin}"))
                        .cloned()
                })
                .collect::<Result<Vec<_>>>()?;
            driver.insert(output, index);
            instances.push(Inst {
                name: name.clone(),
                cell: cell_id,
                inputs,
                input_sources: Vec::new(),
                input_is_primary: Vec::new(),
                input_primary_index: Vec::new(),
            });
        }
        for instance in &mut instances {
            instance.input_sources = instance
                .inputs
                .iter()
                .map(|signal| driver.get(signal).copied())
                .collect();
        }
        fn resolve(assigns: &HashMap<String, String>, mut signal: String) -> String {
            let mut seen = HashSet::new();
            while let Some(next) = assigns.get(&signal) {
                if !seen.insert(signal.clone()) {
                    break;
                }
                signal = next.clone();
            }
            signal
        }
        let input_names: HashSet<_> = inputs.iter().cloned().collect();
        let input_indices: HashMap<_, _> = inputs
            .iter()
            .enumerate()
            .map(|(index, name)| (name.clone(), index))
            .collect();
        for instance in &mut instances {
            instance.input_is_primary = instance
                .inputs
                .iter()
                .map(|signal| input_names.contains(&resolve(&assigns, signal.clone())))
                .collect();
            instance.input_primary_index = instance
                .inputs
                .iter()
                .map(|signal| {
                    input_indices
                        .get(&resolve(&assigns, signal.clone()))
                        .copied()
                })
                .collect();
        }
        let roots: Vec<_> = outputs
            .iter()
            .filter_map(|output| driver.get(&resolve(&assigns, output.clone())).copied())
            .collect();
        let mut primary_output_load_count = vec![0usize; inputs.len()];
        for output in &outputs {
            if let Some(primary) = input_indices.get(&resolve(&assigns, output.clone())) {
                primary_output_load_count[*primary] += 1;
            }
        }
        let mut output_load_count = vec![0usize; instances.len()];
        for &root in &roots {
            output_load_count[root] += 1;
        }
        let (fixed_primary_input, fixed_output_load_ff, fixed_primary_output_load_ff) =
            if let Some(boundary) = &self.port_boundary {
                let fixed_inputs = boundary.ordered_inputs(&inputs)?;
                let fixed_outputs = boundary.ordered_outputs(&outputs)?;
                let mut gate_loads = vec![0.0; instances.len()];
                let mut primary_loads = vec![0.0; inputs.len()];
                for (output, fixed) in outputs.iter().zip(&fixed_outputs) {
                    let resolved = resolve(&assigns, output.clone());
                    if let Some(source) = driver.get(&resolved) {
                        gate_loads[*source] += fixed.load_ff;
                    } else if let Some(primary) = input_indices.get(&resolved) {
                        primary_loads[*primary] += fixed.load_ff;
                    } else if verilog_scalar_constant(&resolved).is_some() {
                        // A static source has zero transition activity and no
                        // mapped driver whose load/timing must be evaluated.
                    } else {
                        anyhow::bail!("output {output} has no mapped or primary driver");
                    }
                }
                (Some(fixed_inputs), Some(gate_loads), Some(primary_loads))
            } else {
                (None, None, None)
            };
        let mut reachable_set = HashSet::new();
        let mut stack = roots.clone();
        while let Some(index) = stack.pop() {
            if reachable_set.insert(index) {
                stack.extend(instances[index].input_sources.iter().flatten().copied());
            }
        }
        let mut indegree = vec![0usize; instances.len()];
        let mut fanout = vec![Vec::new(); instances.len()];
        let mut consumers = vec![Vec::new(); instances.len()];
        for sink in 0..instances.len() {
            if !reachable_set.contains(&sink) {
                continue;
            }
            for (pin, source) in instances[sink].input_sources.iter().enumerate() {
                if let Some(source) = source.filter(|source| reachable_set.contains(source)) {
                    indegree[sink] += 1;
                    fanout[source].push(sink);
                    consumers[source].push((sink, pin));
                }
            }
        }
        let mut ready: Vec<_> = (0..instances.len())
            .filter(|index| reachable_set.contains(index) && indegree[*index] == 0)
            .collect();
        ready.sort_by_key(|index| std::cmp::Reverse(instances[*index].name.clone()));
        let mut topo = Vec::with_capacity(reachable_set.len());
        while let Some(index) = ready.pop() {
            topo.push(index);
            fanout[index].sort_by_key(|sink| instances[*sink].name.clone());
            for &sink in &fanout[index] {
                indegree[sink] -= 1;
                if indegree[sink] == 0 {
                    ready.push(sink);
                    ready.sort_by_key(|next| std::cmp::Reverse(instances[*next].name.clone()));
                }
            }
        }
        anyhow::ensure!(topo.len() == reachable_set.len(), "cycle in mapped netlist");
        let reachable: Vec<usize> = (0..instances.len())
            .filter(|index| reachable_set.contains(index))
            .collect();

        // Reproduce the canonical occurrence-space FxHashSet accumulation
        // order exactly.  The full evaluator names classes by the parser's
        // physical NodeIndex: primary inputs, textual-order instances, then
        // output symbols. Constant-driven outputs have no mapped occurrence
        // beneath their output symbol and therefore add no physical class.
        let input_nid: HashMap<_, _> = inputs
            .iter()
            .enumerate()
            .map(|(index, name)| (name.clone(), index))
            .collect();
        let gate_base = inputs.len();
        let output_base = gate_base + instances.len();
        let class_id = |index: usize| egraph_serialize::ClassId::from(format!("physical.{index}"));
        let mut children = vec![Vec::<usize>::new(); output_base + outputs.len()];
        for (index, instance) in instances.iter().enumerate() {
            for (pin, signal) in instance.inputs.iter().enumerate() {
                if let Some(source) = instance.input_sources[pin] {
                    children[gate_base + index].push(gate_base + source);
                } else {
                    let resolved = resolve(&assigns, signal.clone());
                    if let Some(source) = input_nid.get(&resolved) {
                        children[gate_base + index].push(*source);
                    }
                }
            }
        }
        for (ordinal, output) in outputs.iter().enumerate() {
            let resolved = resolve(&assigns, output.clone());
            if let Some(source) = driver.get(&resolved) {
                children[output_base + ordinal].push(gate_base + *source);
            } else if let Some(source) = input_nid.get(&resolved) {
                children[output_base + ordinal].push(*source);
            }
        }
        let mut canonical_reachable = FxHashSet::default();
        let mut queue: VecDeque<_> = (0..outputs.len())
            .map(|ordinal| output_base + ordinal)
            .collect();
        while let Some(index) = queue.pop_front() {
            if !canonical_reachable.insert(class_id(index)) {
                continue;
            }
            queue.extend(children[index].iter().copied());
        }
        let canonical_sum_order: Vec<usize> = canonical_reachable
            .iter()
            .filter_map(|cid| {
                cid.to_string()
                    .strip_prefix("physical.")?
                    .parse::<usize>()
                    .ok()
            })
            .filter(|index| *index >= gate_base && *index < output_base)
            .map(|index| index - gate_base)
            .collect();
        anyhow::ensure!(
            canonical_sum_order.len() == reachable.len(),
            "canonical physical-class map differs from reachable instance set"
        );
        Ok(Circuit {
            instances,
            roots,
            reachable,
            reachable_set,
            topo,
            consumers,
            output_load_count,
            primary_output_load_count,
            canonical_sum_order,
            primary_input_count: inputs.len(),
            fixed_primary_input,
            fixed_output_load_ff,
            fixed_primary_output_load_ff,
        })
    }

    /// Build the exact circuit order emitted by
    /// `write_verilog_from_netlist_with_lib`: topological textual instance
    /// order, writer instance names, and the canonical physical-class sum
    /// order.  This lets a `(parent, local assignment)` candidate go directly
    /// from physicalization to P1 without a write/read/regex round trip.
    fn from_netlist(&self, netlist: &Netlist<StdCellType, ()>) -> Result<Circuit> {
        let leaves: HashSet<_> = netlist.leaves.iter().copied().collect();
        let roots_set: HashSet<_> = netlist.roots.iter().copied().collect();
        let topo_nodes = toposort(&netlist.graph, None)
            .map_err(|cycle| anyhow::anyhow!("cycle in mapped netlist: {cycle:?}"))?;
        let gate_nodes: Vec<_> = topo_nodes
            .iter()
            .rev()
            .copied()
            .filter(|node| {
                !leaves.contains(node)
                    && !roots_set.contains(node)
                    && !netlist.graph[*node].is_constant()
            })
            .collect();
        anyhow::ensure!(!gate_nodes.is_empty(), "candidate contains no mapped cells");
        let gate_index: HashMap<NodeIndex, usize> = gate_nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (*node, index))
            .collect();
        let input_index: HashMap<NodeIndex, usize> = netlist
            .leaves
            .iter()
            .enumerate()
            .map(|(index, node)| (*node, index))
            .collect();

        let mut ordered_children = Vec::with_capacity(gate_nodes.len());
        let mut instances = Vec::with_capacity(gate_nodes.len());
        for node in &gate_nodes {
            let name = netlist.graph[*node].to_string();
            let cell = *self
                .cell_by_name
                .get(&name)
                .with_context(|| format!("cell {name} absent from shared database"))?;
            let children: Vec<_> = netlist.inputs(*node).collect();
            let input_sources = children
                .iter()
                .map(|child| gate_index.get(child).copied())
                .collect();
            let inputs = children
                .iter()
                .map(|child| netlist.graph[*child].to_string())
                .collect();
            let input_is_primary = children
                .iter()
                .map(|child| leaves.contains(child) && !netlist.graph[*child].is_constant())
                .collect();
            let input_primary_index = children
                .iter()
                .map(|child| input_index.get(child).copied())
                .collect();
            ordered_children.push(children);
            instances.push(Inst {
                name: format!("_nid_{}", node.index()),
                cell,
                inputs,
                input_sources,
                input_is_primary,
                input_primary_index,
            });
        }
        let roots: Vec<_> = netlist
            .roots
            .iter()
            .filter_map(|root| netlist.inputs(*root).next())
            .filter_map(|source| gate_index.get(&source).copied())
            .collect();
        let mut primary_output_load_count = vec![0usize; netlist.leaves.len()];
        for root in &netlist.roots {
            if let Some(source) = netlist.inputs(*root).next() {
                if let Some(primary) = input_index.get(&source) {
                    primary_output_load_count[*primary] += 1;
                }
            }
        }
        let mut output_load_count = vec![0usize; instances.len()];
        for &root in &roots {
            output_load_count[root] += 1;
        }
        let (fixed_primary_input, fixed_output_load_ff, fixed_primary_output_load_ff) =
            if let Some(boundary) = &self.port_boundary {
                let input_names = netlist
                    .leaves
                    .iter()
                    .map(|node| netlist.graph[*node].to_string())
                    .collect::<Vec<_>>();
                let output_names = netlist
                    .roots
                    .iter()
                    .map(|node| netlist.graph[*node].to_string())
                    .collect::<Vec<_>>();
                let fixed_inputs = boundary.ordered_inputs(&input_names)?;
                let fixed_outputs = boundary.ordered_outputs(&output_names)?;
                let mut gate_loads = vec![0.0; instances.len()];
                let mut primary_loads = vec![0.0; netlist.leaves.len()];
                for (root, fixed) in netlist.roots.iter().zip(&fixed_outputs) {
                    if let Some(source) = netlist.inputs(*root).next() {
                        if let Some(source) = gate_index.get(&source) {
                            gate_loads[*source] += fixed.load_ff;
                        } else if let Some(primary) = input_index.get(&source) {
                            primary_loads[*primary] += fixed.load_ff;
                        }
                    }
                }
                (Some(fixed_inputs), Some(gate_loads), Some(primary_loads))
            } else {
                (None, None, None)
            };
        let mut reachable_set = HashSet::new();
        let mut stack = roots.clone();
        while let Some(index) = stack.pop() {
            if reachable_set.insert(index) {
                stack.extend(instances[index].input_sources.iter().flatten().copied());
            }
        }
        let mut indegree = vec![0usize; instances.len()];
        let mut fanout = vec![Vec::new(); instances.len()];
        let mut consumers = vec![Vec::new(); instances.len()];
        for sink in 0..instances.len() {
            if !reachable_set.contains(&sink) {
                continue;
            }
            for (pin, source) in instances[sink].input_sources.iter().enumerate() {
                if let Some(source) = source.filter(|source| reachable_set.contains(source)) {
                    indegree[sink] += 1;
                    fanout[source].push(sink);
                    consumers[source].push((sink, pin));
                }
            }
        }
        let mut ready: Vec<_> = (0..instances.len())
            .filter(|index| reachable_set.contains(index) && indegree[*index] == 0)
            .collect();
        ready.sort_by_key(|index| std::cmp::Reverse(instances[*index].name.clone()));
        let mut topo = Vec::with_capacity(reachable_set.len());
        while let Some(index) = ready.pop() {
            topo.push(index);
            fanout[index].sort_by_key(|sink| instances[*sink].name.clone());
            for &sink in &fanout[index] {
                indegree[sink] -= 1;
                if indegree[sink] == 0 {
                    ready.push(sink);
                    ready.sort_by_key(|next| std::cmp::Reverse(instances[*next].name.clone()));
                }
            }
        }
        anyhow::ensure!(topo.len() == reachable_set.len(), "cycle in mapped netlist");
        let reachable: Vec<_> = (0..instances.len())
            .filter(|index| reachable_set.contains(index))
            .collect();

        // Reproduce the parser's physical NodeIndex layout: input symbols,
        // textual-order instances, then output wrappers.
        let gate_base = netlist.leaves.len();
        let output_base = gate_base + instances.len();
        let mut children = vec![Vec::<usize>::new(); output_base + netlist.roots.len()];
        for (index, gate_children) in ordered_children.iter().enumerate() {
            for child in gate_children {
                if let Some(source) = gate_index.get(child) {
                    children[gate_base + index].push(gate_base + *source);
                } else if let Some(source) = input_index.get(child) {
                    children[gate_base + index].push(*source);
                }
            }
        }
        for (ordinal, root) in netlist.roots.iter().enumerate() {
            if let Some(source) = netlist.inputs(*root).next() {
                if let Some(source) = gate_index.get(&source) {
                    children[output_base + ordinal].push(gate_base + *source);
                } else if let Some(source) = input_index.get(&source) {
                    children[output_base + ordinal].push(*source);
                }
            }
        }
        let class_id = |index: usize| ClassId::from(format!("physical.{index}"));
        let mut canonical_reachable = FxHashSet::default();
        let mut queue: VecDeque<_> = (0..netlist.roots.len())
            .map(|ordinal| output_base + ordinal)
            .collect();
        while let Some(index) = queue.pop_front() {
            if !canonical_reachable.insert(class_id(index)) {
                continue;
            }
            queue.extend(children[index].iter().copied());
        }
        let canonical_sum_order: Vec<_> = canonical_reachable
            .iter()
            .filter_map(|cid| {
                cid.to_string()
                    .strip_prefix("physical.")?
                    .parse::<usize>()
                    .ok()
            })
            .filter(|index| *index >= gate_base && *index < output_base)
            .map(|index| index - gate_base)
            .collect();
        anyhow::ensure!(
            canonical_sum_order.len() == reachable.len(),
            "canonical physical-class map differs from reachable instance set"
        );
        Ok(Circuit {
            instances,
            roots,
            reachable,
            reachable_set,
            topo,
            consumers,
            output_load_count,
            primary_output_load_count,
            canonical_sum_order,
            primary_input_count: netlist.leaves.len(),
            fixed_primary_input,
            fixed_output_load_ff,
            fixed_primary_output_load_ff,
        })
    }

    fn output_load(&mut self, circuit: &Circuit, source: usize) -> f64 {
        // The producer cell is intentionally absent: all producer e-nodes at
        // this occurrence/e-class share the selected downstream pin loads.
        let context = LoadContext(
            circuit.consumers[source]
                .iter()
                .map(|(sink, pin)| (circuit.instances[*sink].cell, *pin))
                .collect(),
        );
        self.stats.requests += 1;
        if self.reuse_load_contexts {
            if let Some(load) = self.load_cache.get(&context) {
                self.stats.hits += 1;
                return *load
                    + circuit.external_output_load(source, self.boundary.primary_output_load_ff);
            }
        }
        let internal_load = context
            .0
            .iter()
            .map(|(cell, pin)| {
                let value = &self.cells[*cell];
                value.pin_info[&value.pin_order[*pin + 1]].0.into_inner()
            })
            .sum();
        if self.reuse_load_contexts {
            self.load_cache.insert(context, internal_load);
        }
        self.stats.misses += 1;
        internal_load + circuit.external_output_load(source, self.boundary.primary_output_load_ff)
    }

    pub fn evaluate(&mut self, path: &Path) -> Result<Ppa> {
        let circuit = self.parse(path)?;
        self.evaluate_circuit(&circuit)
    }

    pub fn evaluate_netlist(&mut self, netlist: &Netlist<StdCellType, ()>) -> Result<Ppa> {
        let circuit = self.from_netlist(netlist)?;
        self.evaluate_circuit(&circuit)
    }

    fn evaluate_circuit(&mut self, circuit: &Circuit) -> Result<Ppa> {
        let count = circuit.instances.len();
        let mut arrival = vec![[0.0_f64; 2]; count];
        let mut transition = vec![[0.0_f64; 2]; count];
        let mut loads = vec![0.0; count];
        for &source in &circuit.reachable {
            loads[source] = self.output_load(&circuit, source);
        }
        let mut primary_input_loads: Vec<f64> = circuit
            .primary_output_load_count
            .iter()
            .enumerate()
            .map(|(primary, _)| {
                circuit.primary_external_output_load(primary, self.boundary.primary_output_load_ff)
            })
            .collect();
        for &index in &circuit.reachable {
            let instance = &circuit.instances[index];
            let cell = &self.cells[instance.cell];
            for (pin, primary) in instance.input_primary_index.iter().enumerate() {
                if let Some(primary) = primary {
                    primary_input_loads[*primary] +=
                        cell.pin_info[&cell.pin_order[pin + 1]].0.into_inner();
                }
            }
        }
        let mut primary_input_arrival = circuit.fixed_primary_input.as_ref().map_or_else(
            || vec![[self.boundary.primary_input_arrival_ps; 2]; circuit.primary_input_count],
            |rows| {
                rows.iter()
                    .map(|row| [row.arrival_rise_ps, row.arrival_fall_ps])
                    .collect()
            },
        );
        let mut primary_input_transition = circuit.fixed_primary_input.as_ref().map_or_else(
            || vec![[self.boundary.primary_input_slew_ps; 2]; circuit.primary_input_count],
            |rows| {
                rows.iter()
                    .map(|row| [row.slew_rise_ps, row.slew_fall_ps])
                    .collect()
            },
        );
        if self.boundary.internal_timing_model.is_v3() && circuit.fixed_primary_input.is_none() {
            let driver = self
                .cell_by_name
                .get(self.boundary.primary_input_driver_cell)
                .map(|index| &self.cells[*index])
                .with_context(|| {
                    format!(
                        "V3 timing requires input driver {}",
                        self.boundary.primary_input_driver_cell
                    )
                })?;
            for (primary, load) in primary_input_loads.iter().copied().enumerate() {
                let timing = driver
                    .primary_input_driver_timing(self.boundary.driver_input_slew_ps, load)
                    .map_err(anyhow::Error::msg)?;
                for edge in 0..2 {
                    primary_input_arrival[primary][edge] =
                        self.boundary.primary_input_arrival_ps + timing.arrival_adjust_ps[edge];
                    primary_input_transition[primary][edge] = timing.output_slew_ps[edge];
                }
            }
        }
        let mut area = vec![0.0; count];
        let mut internal_power = vec![0.0; count];
        let mut leakage_power = vec![0.0; count];
        let mut vectorless_internal_power = vec![0.0; count];
        let (_, toggle) = if self.boundary.vectorless_power && !self.boundary.uniform_comb_activity
        {
            simulate_vectorless_activity(circuit, &self.cells, &self.boundary)?
        } else if self.boundary.vectorless_power {
            (
                vec![self.boundary.primary_input_probability; count],
                vec![self.boundary.primary_input_toggle_per_cycle; count],
            )
        } else {
            (vec![0.0; count], vec![0.0; count])
        };
        for &index in &circuit.topo {
            let instance = &circuit.instances[index];
            let cell = &self.cells[instance.cell];
            area[index] = cell.area.into_inner();
            let slews: Vec<_> = instance
                .input_sources
                .iter()
                .enumerate()
                .map(|(pin, source)| {
                    source.map_or_else(
                        || {
                            if instance.input_is_primary[pin] {
                                instance.input_primary_index[pin]
                                    .map(|primary| {
                                        primary_input_transition[primary][0]
                                            .max(primary_input_transition[primary][1])
                                    })
                                    .unwrap_or(self.boundary.primary_input_slew_ps)
                            } else {
                                0.0
                            }
                        },
                        |value| transition[value][0].max(transition[value][1]),
                    )
                })
                .collect();
            let input_toggles: Vec<_> = instance
                .input_sources
                .iter()
                .enumerate()
                .map(|(pin, source)| {
                    if self.boundary.uniform_comb_activity {
                        return self.boundary.primary_input_toggle_per_cycle;
                    }
                    source.map_or_else(
                        || {
                            if instance.input_is_primary[pin] {
                                self.boundary.primary_input_toggle_per_cycle
                            } else {
                                0.0
                            }
                        },
                        |value| toggle[value],
                    )
                })
                .collect();
            leakage_power[index] = cell.leakage_power.into_inner() / 1000.0;
            internal_power[index] = if slews.is_empty() {
                0.0
            } else {
                let mut value = 0.0;
                for (pin, slew) in cell.pin_order.iter().skip(1).zip(&slews) {
                    value += Lut2D::from_indexed_rows(&cell.internal_power[pin])
                        .map_err(anyhow::Error::msg)?
                        .lookup_f64(*slew, loads[index])
                        .map_err(anyhow::Error::msg)?;
                }
                value / slews.len() as f64
            };
            let mut activity_weighted_energy = 0.0;
            for ((pin, slew), activity) in cell
                .pin_order
                .iter()
                .skip(1)
                .zip(&slews)
                .zip(&input_toggles)
            {
                activity_weighted_energy += Lut2D::from_indexed_rows(&cell.internal_power[pin])
                    .map_err(anyhow::Error::msg)?
                    .lookup_f64(*slew, loads[index])
                    .map_err(anyhow::Error::msg)?
                    * activity;
            }
            vectorless_internal_power[index] =
                activity_weighted_energy * self.boundary.internal_transition_factor * 1000.0
                    / self.boundary.power_period_ps;
            let mut winner: [Option<(f64, f64)>; 2] = [None, None];
            let mut worst_transition: [Option<f64>; 2] = [None, None];
            for arc in cell.timing_arcs.iter() {
                let pin = cell
                    .pin_order
                    .iter()
                    .skip(1)
                    .position(|name| name == &arc.related_pin)
                    .context("timing arc pin absent")?;
                let source = instance.input_sources[pin]
                    .filter(|source| circuit.reachable_set.contains(source));
                let mappings: &[(usize, usize)] = match arc.timing_sense {
                    TimingSense::PositiveUnate => &[(0, 0), (1, 1)],
                    TimingSense::NegativeUnate => &[(1, 0), (0, 1)],
                    TimingSense::NonUnate => &[(0, 0), (1, 0), (0, 1), (1, 1)],
                };
                for &(input_edge, output_edge) in mappings {
                    let input_arrival = source.map_or_else(
                        || {
                            if instance.input_is_primary[pin] {
                                instance.input_primary_index[pin]
                                    .map(|primary| primary_input_arrival[primary][input_edge])
                                    .unwrap_or(self.boundary.primary_input_arrival_ps)
                            } else {
                                0.0
                            }
                        },
                        |value| arrival[value][input_edge],
                    );
                    let input_slew = source.map_or_else(
                        || {
                            if instance.input_is_primary[pin] {
                                instance.input_primary_index[pin]
                                    .map(|primary| primary_input_transition[primary][input_edge])
                                    .unwrap_or(self.boundary.primary_input_slew_ps)
                            } else {
                                0.0
                            }
                        },
                        |value| transition[value][input_edge],
                    );
                    let delay_lut = if output_edge == 0 {
                        &arc.cell_rise
                    } else {
                        &arc.cell_fall
                    };
                    let transition_lut = if output_edge == 0 {
                        &arc.rise_transition
                    } else {
                        &arc.fall_transition
                    };
                    let delay = delay_lut
                        .lookup_f64(input_slew, loads[index])
                        .map_err(anyhow::Error::msg)?;
                    let output_slew = transition_lut
                        .lookup_f64(input_slew, loads[index])
                        .map_err(anyhow::Error::msg)?;
                    let candidate = self
                        .boundary
                        .internal_timing_model
                        .propagate(input_arrival, delay);
                    if winner[output_edge].is_none() || candidate > winner[output_edge].unwrap().0 {
                        winner[output_edge] = Some((candidate, output_slew));
                    }
                    if self.boundary.internal_timing_model.is_v3() {
                        worst_transition[output_edge] = Some(
                            worst_transition[output_edge]
                                .map_or(output_slew, |current| current.max(output_slew)),
                        );
                    }
                }
            }
            for edge in 0..2 {
                if let Some((value, slew)) = winner[edge] {
                    arrival[index][edge] = value;
                    transition[index][edge] = worst_transition[edge].unwrap_or(slew);
                }
            }
        }
        let delay = circuit
            .roots
            .iter()
            .flat_map(|root| arrival[*root])
            .fold(0.0, f64::max);
        let area: f64 = circuit
            .canonical_sum_order
            .iter()
            .map(|index| area[*index])
            .sum();
        let area = self.boundary.internal_timing_model.area(area);
        let internal_power_legacy: f64 = circuit
            .canonical_sum_order
            .iter()
            .map(|index| internal_power[*index])
            .sum();
        let leakage_power_legacy: f64 = circuit
            .canonical_sum_order
            .iter()
            .map(|index| leakage_power[*index])
            .sum();
        let vectorless_internal_power_uw: f64 = circuit
            .canonical_sum_order
            .iter()
            .map(|index| vectorless_internal_power[*index])
            .sum();
        let vectorless_leakage_power_uw = leakage_power_legacy / 1000.0;
        let primary_input_pin_capacitance_ff: f64 = circuit
            .reachable
            .iter()
            .map(|index| {
                let instance = &circuit.instances[*index];
                let cell = &self.cells[instance.cell];
                instance
                    .input_is_primary
                    .iter()
                    .enumerate()
                    .filter(|(_, primary)| **primary)
                    .map(|(pin, _)| cell.pin_info[&cell.pin_order[pin + 1]].0.into_inner())
                    .sum::<f64>()
            })
            .sum();
        let primary_output_load_on_inputs_ff: f64 = circuit
            .primary_output_load_count
            .iter()
            .enumerate()
            .map(|(primary, _)| {
                circuit.primary_external_output_load(primary, self.boundary.primary_output_load_ff)
            })
            .sum();
        let primary_input_capacitance_ff =
            primary_input_pin_capacitance_ff + primary_output_load_on_inputs_ff;
        let switched_capacitance_ff = circuit
            .canonical_sum_order
            .iter()
            .map(|index| loads[*index])
            .sum::<f64>()
            + primary_input_capacitance_ff;
        let cell_switching_power_uw: f64 = circuit
            .canonical_sum_order
            .iter()
            .map(|index| toggle[*index] * loads[*index])
            .sum::<f64>()
            * self.boundary.power_voltage_v.powi(2)
            * 1000.0
            / self.boundary.power_period_ps;
        let primary_input_switching_power_uw = primary_input_capacitance_ff
            * self.boundary.primary_input_toggle_per_cycle
            * self.boundary.power_voltage_v.powi(2)
            * 1000.0
            / self.boundary.power_period_ps;
        let vectorless_toggle_sum = circuit
            .canonical_sum_order
            .iter()
            .map(|index| toggle[*index])
            .sum::<f64>()
            + circuit.primary_input_count as f64 * self.boundary.primary_input_toggle_per_cycle;
        let vectorless_switching_power_uw =
            cell_switching_power_uw + primary_input_switching_power_uw;
        let vectorless_power_uw = vectorless_internal_power_uw
            + vectorless_leakage_power_uw
            + vectorless_switching_power_uw;
        // Positive neutral coordinate required by the objective API. This is
        // not measured power; GENLIB runs must use a power-free objective.
        let power = if self.boundary.internal_timing_model.is_genlib() {
            1.0
        } else if self.boundary.vectorless_power {
            vectorless_power_uw
        } else {
            internal_power_legacy + leakage_power_legacy
        };
        Ok(Ppa {
            delay,
            area,
            power,
            score: delay * delay * area * power,
            internal_power_legacy,
            leakage_power_legacy,
            switched_capacitance_ff,
            vectorless_internal_power_uw,
            vectorless_switching_power_uw,
            vectorless_leakage_power_uw,
            vectorless_power_uw,
            vectorless_toggle_sum,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        InternalTimingModel, PortBoundaryOverrides, TimingBoundary, verilog_scalar_constant,
    };

    #[test]
    fn nldm_config_preserves_the_explicit_driver_library() {
        let mut boundary = TimingBoundary::default();
        assert_eq!(
            boundary.nldm_config().primary_input_driver_cell,
            "BUFx2_ASAP7_6t_L"
        );
        boundary.primary_input_driver_cell = "BUFx2_ASAP7_75t_R";
        assert_eq!(
            boundary.nldm_config().primary_input_driver_cell,
            "BUFx2_ASAP7_75t_R"
        );
    }

    #[test]
    fn genlib_rounding_is_opt_in_and_uses_author_pin_then_arrival_ceiling() {
        let model = InternalTimingModel::GenlibStatic;
        assert_eq!(model.propagate(1.001, 2.001), 3.02);
        assert_eq!(model.area(23.97000000000003), 23.97);
        let ordinary = TimingBoundary::default().internal_timing_model;
        assert_eq!(ordinary.propagate(1.001, 2.001), 1.001 + 2.001);
        assert_eq!(ordinary.area(23.97000000000003), 23.97000000000003);
        let mut boundary = TimingBoundary::default();
        assert!(!boundary.nldm_config().genlib_static);
        boundary.internal_timing_model = model;
        assert!(boundary.nldm_config().genlib_static);
    }

    #[test]
    fn scalar_verilog_constants_are_distinct_from_signal_aliases() {
        for zero in ["0", "1'b0", "1'h0", "1'd0", "FALSE"] {
            assert_eq!(verilog_scalar_constant(zero), Some(false));
        }
        for one in ["1", "1'b1", "1'h1", "1'd1", "TRUE"] {
            assert_eq!(verilog_scalar_constant(one), Some(true));
        }
        assert_eq!(verilog_scalar_constant("n0"), None);
        assert_eq!(verilog_scalar_constant("1'bx"), None);
    }

    #[test]
    fn port_input_cap_allowance_is_backward_compatible_and_optional() {
        let without: PortBoundaryOverrides = serde_json::from_str(
            r#"{
                "schema":"egg-port-boundary-v1",
                "inputs":[{
                    "name":"a",
                    "arrival_rise_ps":0.0,
                    "arrival_fall_ps":0.0,
                    "slew_rise_ps":20.0,
                    "slew_fall_ps":20.0,
                    "baseline_capacitance_ff":1.0
                }],
                "outputs":[{"name":"y","load_ff":5.76}]
            }"#,
        )
        .unwrap();
        assert_eq!(without.inputs[0].max_cap_regression_ff, None);

        let with: PortBoundaryOverrides = serde_json::from_str(
            r#"{
                "schema":"egg-port-boundary-v1",
                "inputs":[{
                    "name":"a",
                    "arrival_rise_ps":0.0,
                    "arrival_fall_ps":0.0,
                    "slew_rise_ps":20.0,
                    "slew_fall_ps":20.0,
                    "baseline_capacitance_ff":1.0,
                    "max_cap_regression_ff":0.25
                }],
                "outputs":[{"name":"y","load_ff":5.76}]
            }"#,
        )
        .unwrap();
        assert_eq!(with.inputs[0].max_cap_regression_ff, Some(0.25));
    }

    #[test]
    fn driver_aware_timing_models_differ_only_in_slew_correlation_policy() {
        let mut independent = TimingBoundary::default();
        independent.internal_timing_model = InternalTimingModel::DriverAwareIndependentSlewV3;
        let mut correlated = independent;
        correlated.internal_timing_model = InternalTimingModel::DriverAwareCorrelatedSlewV3;

        let independent_config = independent.nldm_config();
        let correlated_config = correlated.nldm_config();

        assert!(independent_config.driver_aware_primary_inputs);
        assert!(correlated_config.driver_aware_primary_inputs);
        assert!(independent_config.independent_worst_transition);
        assert!(!correlated_config.independent_worst_transition);
        assert_eq!(
            independent_config.primary_input_arrival_ps,
            correlated_config.primary_input_arrival_ps
        );
        assert_eq!(
            independent_config.primary_output_load_ff,
            correlated_config.primary_output_load_ff
        );
        assert_eq!(
            independent_config.driver_input_slew_ps,
            correlated_config.driver_input_slew_ps
        );
    }
}
