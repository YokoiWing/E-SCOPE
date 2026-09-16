//! White-box reverse electrical pricing for the frozen NLDM Timing V2 trace.
//!
//! `GlobalLogSumExp` is a derivative of the corresponding smooth timing
//! control. `LocalSoftBackprop` and soft transition allocation are deliberately
//! described as pseudo-adjoints: their forward value remains the hard V2
//! evaluator value.

use super::{
    criticality, CriticalityMethod, CriticalityParams, ExtractionResult, NldmEvaluationV2,
    SignalEdge, TimingArcIdV2,
};
use crate::{ExtendedEGraph, Lut2D};
use egraph_serialize::ClassId;
use ordered_float::NotNan;
use rustc_hash::FxHashMap;

type StateKey = (ClassId, SignalEdge);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimingAdjointControl {
    /// Mathematically smooth global LogSumExp timing objective.
    G1GlobalLogSumExp,
    /// Hard V2 forward plus local-soft timing pseudo-adjoint.
    G2LocalSoftBackprop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransitionBackward {
    /// Reverse transition sensitivity only through the hard winning arc.
    E0HardWinner,
    /// Allocate it over near-critical arcs using conditional timing mass.
    E1SoftAllocation,
}

#[derive(Clone, Copy, Debug)]
pub struct CircuitAdjointConfig {
    pub timing_control: TimingAdjointControl,
    pub transition_backward: TransitionBackward,
    pub tau_ratio: f64,
}

/// Relative D/A/P derivatives of the active scalar search objective.  The
/// frozen Union V7 objective is `(2, 1, 1)` and remains the default wrapper.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CircuitObjectiveWeights {
    pub delay: f64,
    pub area: f64,
    pub power: f64,
}

impl Default for CircuitObjectiveWeights {
    fn default() -> Self {
        Self {
            delay: 2.0,
            area: 1.0,
            power: 1.0,
        }
    }
}

impl Default for CircuitAdjointConfig {
    fn default() -> Self {
        Self {
            timing_control: TimingAdjointControl::G2LocalSoftBackprop,
            transition_backward: TransitionBackward::E0HardWinner,
            tau_ratio: 0.02,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CircuitAdjointV1 {
    pub arc_delay_price: FxHashMap<TimingArcIdV2, f64>,
    pub arrival_price: FxHashMap<StateKey, f64>,
    pub transition_price: FxHashMap<StateKey, f64>,
    pub load_price: FxHashMap<ClassId, f64>,
    /// Price of each selected cell input capacitance, keyed by driver and sink.
    pub input_cap_price: FxHashMap<(ClassId, ClassId, usize), f64>,
    pub area_price: f64,
    pub power_price: f64,
    pub timing_normalizer: f64,
}

fn transition(trace: &NldmEvaluationV2, class: &ClassId, edge: SignalEdge) -> Result<f64, String> {
    let point = trace
        .timing
        .get(class)
        .ok_or_else(|| format!("missing timing point for class {class}"))?;
    Ok(match edge {
        SignalEdge::Rise => point.rise.transition,
        SignalEdge::Fall => point.fall.transition,
    })
}

fn add<K: std::hash::Hash + Eq>(map: &mut FxHashMap<K, f64>, key: K, value: f64) {
    *map.entry(key).or_insert(0.0) += value;
}

fn selected_op<'a>(
    result: &ExtractionResult,
    ext: &'a ExtendedEGraph,
    class: &ClassId,
) -> Result<(&'a egraph_serialize::Node, String), String> {
    let node_id = result
        .choices
        .get(class)
        .ok_or_else(|| format!("missing selected node for class {class}"))?;
    let node = &ext.inner[node_id];
    let op = ext
        .node_ops
        .get(node_id)
        .cloned()
        .unwrap_or_else(|| node.op.clone());
    Ok((node, op))
}

/// Reverse the exact LUT interpolation used by Timing V2 while preserving its
/// hard forward values. The only approximations are selected explicitly by
/// `timing_control` and `transition_backward`.
pub fn circuit_adjoint(
    result: &ExtractionResult,
    ext: &ExtendedEGraph,
    trace: &NldmEvaluationV2,
    config: CircuitAdjointConfig,
) -> Result<CircuitAdjointV1, String> {
    circuit_adjoint_weighted(
        result,
        ext,
        trace,
        config,
        CircuitObjectiveWeights::default(),
    )
}

pub fn circuit_adjoint_weighted(
    result: &ExtractionResult,
    ext: &ExtendedEGraph,
    trace: &NldmEvaluationV2,
    config: CircuitAdjointConfig,
    objective_weights: CircuitObjectiveWeights,
) -> Result<CircuitAdjointV1, String> {
    let delay = trace.cost.components[0].into_inner();
    let area = trace.cost.components[1].into_inner();
    let power = trace.cost.components[2].into_inner();
    if !delay.is_finite()
        || delay <= 0.0
        || !area.is_finite()
        || area <= 0.0
        || !power.is_finite()
        || power <= 0.0
    {
        return Err("circuit adjoint requires finite positive D/A/P".into());
    }
    let method = match config.timing_control {
        TimingAdjointControl::G1GlobalLogSumExp => CriticalityMethod::GlobalLogSumExp,
        TimingAdjointControl::G2LocalSoftBackprop => CriticalityMethod::LocalSoftBackprop,
    };
    let crit = criticality(
        trace,
        method,
        CriticalityParams {
            tau_ratio: config.tau_ratio,
        },
    )?;
    let timing_normalizer = crit.soft_delay.unwrap_or(delay);
    if !timing_normalizer.is_finite() || timing_normalizer <= 0.0 {
        return Err("invalid timing normalizer".into());
    }
    if [
        objective_weights.delay,
        objective_weights.area,
        objective_weights.power,
    ]
    .into_iter()
    .any(|weight| !weight.is_finite() || weight < 0.0)
    {
        return Err("circuit objective weights must be finite and nonnegative".into());
    }
    let timing_scale = objective_weights.delay / timing_normalizer;
    let power_price = objective_weights.power / power;
    let mut arc_delay_price = FxHashMap::default();
    let mut arrival_price = FxHashMap::default();
    for arc in &trace.arcs {
        let price = timing_scale * crit.q.get(&arc.id).copied().unwrap_or(0.0);
        arc_delay_price.insert(arc.id.clone(), price);
        add(
            &mut arrival_price,
            (arc.id.parent.clone(), arc.id.parent_edge),
            price,
        );
    }

    let mut transition_price: FxHashMap<StateKey, f64> = FxHashMap::default();
    let mut load_price: FxHashMap<ClassId, f64> = FxHashMap::default();

    // Reverse the current V2 internal-power aggregation exactly: average over
    // input pins, each pin receiving max(rise, fall) input transition.
    for class in &trace.topological_order {
        let (node, op) = selected_op(result, ext, class)?;
        let Some(cell) = ext.cell_nldm.get(&op) else {
            continue;
        };
        if node.children.is_empty() || cell.internal_power.is_empty() {
            continue;
        }
        let mut pin_rows = Vec::new();
        for (child_index, child_node) in node.children.iter().enumerate() {
            let Some(pin) = cell.pin_order.get(child_index + 1) else {
                continue;
            };
            let Some(rows) = cell.internal_power.get(pin) else {
                continue;
            };
            let child = ext.inner.nid_to_cid(child_node).clone();
            pin_rows.push((child_index, child, Lut2D::from_indexed_rows(rows)?));
        }
        if pin_rows.is_empty() {
            continue;
        }
        let pin_weight = power_price / pin_rows.len() as f64;
        let load = NotNan::new(trace.timing[class].load).map_err(|_| "NaN cell load")?;
        for (_, child, lut) in pin_rows {
            let rise = transition(trace, &child, SignalEdge::Rise)?;
            let fall = transition(trace, &child, SignalEdge::Fall)?;
            let selected_slew = rise.max(fall);
            let partial = lut.lookup_with_partials(
                NotNan::new(selected_slew).map_err(|_| "NaN input slew")?,
                load,
            )?;
            let slew_price = pin_weight * partial.d_slew;
            let tolerance = selected_slew.abs().max(1.0) * 1e-12;
            if (rise - fall).abs() <= tolerance {
                add(
                    &mut transition_price,
                    (child.clone(), SignalEdge::Rise),
                    0.5 * slew_price,
                );
                add(
                    &mut transition_price,
                    (child, SignalEdge::Fall),
                    0.5 * slew_price,
                );
            } else if rise > fall {
                add(&mut transition_price, (child, SignalEdge::Rise), slew_price);
            } else {
                add(&mut transition_price, (child, SignalEdge::Fall), slew_price);
            }
            add(&mut load_price, class.clone(), pin_weight * partial.d_load);
        }
    }

    let mut incoming: FxHashMap<StateKey, Vec<usize>> = FxHashMap::default();
    for (index, arc) in trace.arcs.iter().enumerate() {
        incoming
            .entry((arc.id.parent.clone(), arc.id.parent_edge))
            .or_default()
            .push(index);
    }
    let delay_tolerance = delay.abs().max(1.0) * 1e-10;
    for class in trace.topological_order.iter().rev() {
        for output_edge in [SignalEdge::Rise, SignalEdge::Fall] {
            let parent_key = (class.clone(), output_edge);
            let output_transition_price = transition_price.get(&parent_key).copied().unwrap_or(0.0);
            let Some(indices) = incoming.get(&parent_key) else {
                continue;
            };
            let mut transition_weights = vec![0.0; indices.len()];
            match config.transition_backward {
                TransitionBackward::E0HardWinner => {
                    let parent_point = &trace.timing[class];
                    let parent_arrival = match output_edge {
                        SignalEdge::Rise => parent_point.rise.arrival,
                        SignalEdge::Fall => parent_point.fall.arrival,
                    };
                    let winners: Vec<_> = indices
                        .iter()
                        .enumerate()
                        .filter(|(_, index)| {
                            (trace.arcs[**index].candidate_arrival - parent_arrival).abs()
                                <= delay_tolerance
                        })
                        .map(|(position, _)| position)
                        .collect();
                    if !winners.is_empty() {
                        let share = 1.0 / winners.len() as f64;
                        for position in winners {
                            transition_weights[position] = share;
                        }
                    }
                }
                TransitionBackward::E1SoftAllocation => {
                    let total: f64 = indices
                        .iter()
                        .map(|index| crit.q.get(&trace.arcs[*index].id).copied().unwrap_or(0.0))
                        .sum();
                    if total > 0.0 {
                        for (position, index) in indices.iter().enumerate() {
                            transition_weights[position] =
                                crit.q.get(&trace.arcs[*index].id).copied().unwrap_or(0.0) / total;
                        }
                    }
                }
            }
            for ((index, transition_weight),) in
                indices.iter().zip(transition_weights).map(|pair| (pair,))
            {
                let arc = &trace.arcs[*index];
                let delay_price = arc_delay_price.get(&arc.id).copied().unwrap_or(0.0);
                let arc_transition_price = output_transition_price * transition_weight;
                add(
                    &mut transition_price,
                    (arc.id.predecessor.clone(), arc.id.predecessor_edge),
                    delay_price * arc.delay_d_slew + arc_transition_price * arc.transition_d_slew,
                );
                add(
                    &mut load_price,
                    class.clone(),
                    delay_price * arc.delay_d_load + arc_transition_price * arc.transition_d_load,
                );
            }
        }
    }

    // A fanout pin capacitance is a term of its upstream driver's output load.
    let mut input_cap_price = FxHashMap::default();
    for sink in &trace.topological_order {
        let (node, op) = selected_op(result, ext, sink)?;
        if !ext.cell_nldm.contains_key(&op) {
            continue;
        }
        for (pin_index, child_node) in node.children.iter().enumerate() {
            let driver = ext.inner.nid_to_cid(child_node).clone();
            input_cap_price.insert(
                (driver.clone(), sink.clone(), pin_index),
                load_price.get(&driver).copied().unwrap_or(0.0),
            );
        }
    }

    for class in &trace.topological_order {
        load_price.entry(class.clone()).or_insert(0.0);
        for signal_edge in [SignalEdge::Rise, SignalEdge::Fall] {
            transition_price
                .entry((class.clone(), signal_edge))
                .or_insert(0.0);
            arrival_price
                .entry((class.clone(), signal_edge))
                .or_insert(0.0);
        }
    }
    if arc_delay_price
        .values()
        .chain(arrival_price.values())
        .chain(transition_price.values())
        .chain(load_price.values())
        .chain(input_cap_price.values())
        .any(|value| !value.is_finite())
    {
        return Err("non-finite circuit adjoint price".into());
    }
    Ok(CircuitAdjointV1 {
        arc_delay_price,
        arrival_price,
        transition_price,
        load_price,
        input_cap_price,
        area_price: objective_weights.area / area,
        power_price,
        timing_normalizer,
    })
}
