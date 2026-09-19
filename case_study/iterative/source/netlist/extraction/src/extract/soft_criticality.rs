//! Arc-level timing criticality models for NLDM Timing V2.
//!
//! The interface is intentionally independent of any extractor.  CCRE and
//! diagnostic runners consume only the returned `q_e` values.

use super::{NldmEvaluationV2, SignalEdge, TimingArcIdV2};
use egraph_serialize::ClassId;
use rustc_hash::FxHashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CriticalityMethod {
    /// Hard max-plus STA with deterministic equal splitting at exact ties.
    Hard,
    /// Replace every max, including the output max, by global LogSumExp.
    GlobalLogSumExp,
    /// Keep hard forward arrivals and use local soft predecessor allocation.
    LocalSoftBackprop,
}

#[derive(Clone, Copy, Debug)]
pub struct CriticalityParams {
    /// Temperature as a fraction of the exact circuit delay.
    pub tau_ratio: f64,
}

impl Default for CriticalityParams {
    fn default() -> Self { Self { tau_ratio: 0.0025 } }
}

#[derive(Clone, Debug)]
pub struct CriticalityResult {
    pub q: FxHashMap<TimingArcIdV2, f64>,
    pub soft_delay: Option<f64>,
    pub tau: Option<f64>,
}

type StateKey = (ClassId, SignalEdge);

fn state_arrival(trace: &NldmEvaluationV2, key: &StateKey) -> Result<f64, String> {
    let point = trace.timing.get(&key.0)
        .ok_or_else(|| format!("missing timing state for class {}", key.0))?;
    Ok(match key.1 { SignalEdge::Rise => point.rise.arrival, SignalEdge::Fall => point.fall.arrival })
}

fn logsumexp(values: &[f64], tau: f64) -> Result<f64, String> {
    if values.is_empty() { return Err("logsumexp requires at least one value".into()); }
    let maximum = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if !maximum.is_finite() { return Err("non-finite logsumexp input".into()); }
    let sum: f64 = values.iter().map(|value| ((*value - maximum) / tau).exp()).sum();
    Ok(maximum + tau * sum.ln())
}

fn incoming_arcs(trace: &NldmEvaluationV2) -> FxHashMap<StateKey, Vec<usize>> {
    let mut incoming = FxHashMap::default();
    for (index, arc) in trace.arcs.iter().enumerate() {
        incoming.entry((arc.id.parent.clone(), arc.id.parent_edge)).or_insert_with(Vec::new).push(index);
    }
    incoming
}

fn add_q(result: &mut FxHashMap<TimingArcIdV2, f64>, id: &TimingArcIdV2, value: f64) {
    *result.entry(id.clone()).or_insert(0.0) += value;
}

fn validate(trace: &NldmEvaluationV2) -> Result<(), String> {
    if !trace.cost.components[0].into_inner().is_finite() { return Err("invalid V2 evaluation".into()); }
    if trace.roots.is_empty() { return Err("V2 trace has no roots".into()); }
    if trace.topological_order.len() != trace.reachable_class_count {
        return Err("V2 trace topological order is incomplete".into());
    }
    Ok(())
}

/// Compute one criticality value `q_e` for every timing arc in a V2 trace.
pub fn criticality(
    trace: &NldmEvaluationV2,
    method: CriticalityMethod,
    params: CriticalityParams,
) -> Result<CriticalityResult, String> {
    validate(trace)?;
    match method {
        CriticalityMethod::Hard => hard_criticality(trace),
        CriticalityMethod::GlobalLogSumExp => soft_criticality(trace, params, true),
        CriticalityMethod::LocalSoftBackprop => soft_criticality(trace, params, false),
    }
}

fn hard_criticality(trace: &NldmEvaluationV2) -> Result<CriticalityResult, String> {
    let incoming = incoming_arcs(trace);
    let delay = trace.cost.components[0].into_inner();
    let tolerance = (delay.abs() * 1e-10).max(1e-12);
    let mut adjoint: FxHashMap<StateKey, f64> = FxHashMap::default();
    let mut winning_roots = Vec::new();
    for root in &trace.roots {
        for output_edge in [SignalEdge::Rise, SignalEdge::Fall] {
            let key = (root.clone(), output_edge);
            if (state_arrival(trace, &key)? - delay).abs() <= tolerance { winning_roots.push(key); }
        }
    }
    if winning_roots.is_empty() { return Err("no hard root state attains circuit delay".into()); }
    let root_share = 1.0 / winning_roots.len() as f64;
    for key in winning_roots { *adjoint.entry(key).or_insert(0.0) += root_share; }

    let mut q = FxHashMap::default();
    for class in trace.topological_order.iter().rev() {
        for output_edge in [SignalEdge::Rise, SignalEdge::Fall] {
            let parent = (class.clone(), output_edge);
            let mass = adjoint.get(&parent).copied().unwrap_or(0.0);
            if mass == 0.0 { continue; }
            let Some(indices) = incoming.get(&parent) else { continue; };
            let parent_arrival = state_arrival(trace, &parent)?;
            let winners: Vec<_> = indices.iter().copied()
                .filter(|index| (trace.arcs[*index].candidate_arrival - parent_arrival).abs() <= tolerance)
                .collect();
            if winners.is_empty() { return Err(format!("no winning incoming arc for class {}", class)); }
            let share = mass / winners.len() as f64;
            for index in winners {
                let arc = &trace.arcs[index];
                add_q(&mut q, &arc.id, share);
                *adjoint.entry((arc.id.predecessor.clone(), arc.id.predecessor_edge)).or_insert(0.0) += share;
            }
        }
    }
    for arc in &trace.arcs { q.entry(arc.id.clone()).or_insert(0.0); }
    Ok(CriticalityResult { q, soft_delay: None, tau: None })
}

fn soft_criticality(
    trace: &NldmEvaluationV2,
    params: CriticalityParams,
    global_forward: bool,
) -> Result<CriticalityResult, String> {
    let delay = trace.cost.components[0].into_inner();
    let tau = params.tau_ratio * delay;
    if !tau.is_finite() || tau <= 0.0 { return Err("soft criticality requires tau_ratio * delay > 0".into()); }
    let incoming = incoming_arcs(trace);
    let mut forward: FxHashMap<StateKey, f64> = FxHashMap::default();
    for class in &trace.topological_order {
        for output_edge in [SignalEdge::Rise, SignalEdge::Fall] {
            let key = (class.clone(), output_edge);
            let value = if global_forward {
                match incoming.get(&key) {
                    None => 0.0,
                    Some(indices) => {
                        let values: Vec<_> = indices.iter().map(|index| {
                            let arc = &trace.arcs[*index];
                            forward[&(arc.id.predecessor.clone(), arc.id.predecessor_edge)] + arc.local_delay
                        }).collect();
                        logsumexp(&values, tau)?
                    }
                }
            } else { state_arrival(trace, &key)? };
            forward.insert(key, value);
        }
    }
    let root_keys: Vec<_> = trace.roots.iter().flat_map(|root| {
        [(root.clone(), SignalEdge::Rise), (root.clone(), SignalEdge::Fall)]
    }).collect();
    let root_values: Vec<_> = root_keys.iter().map(|key| forward[key]).collect();
    let allocation_reference = logsumexp(&root_values, tau)?;
    let mut root_weights: Vec<_> = root_values.iter().map(|value| ((*value - allocation_reference) / tau).exp()).collect();
    let root_sum: f64 = root_weights.iter().sum();
    for weight in &mut root_weights { *weight /= root_sum; }

    let mut adjoint: FxHashMap<StateKey, f64> = FxHashMap::default();
    for (key, weight) in root_keys.into_iter().zip(root_weights) { adjoint.insert(key, weight); }
    let mut q = FxHashMap::default();
    for class in trace.topological_order.iter().rev() {
        for output_edge in [SignalEdge::Rise, SignalEdge::Fall] {
            let parent = (class.clone(), output_edge);
            let mass = adjoint.get(&parent).copied().unwrap_or(0.0);
            if mass == 0.0 { continue; }
            let Some(indices) = incoming.get(&parent) else { continue; };
            let parent_reference = if global_forward { forward[&parent] } else { state_arrival(trace, &parent)? };
            let mut weights: Vec<_> = indices.iter().map(|index| {
                let arc = &trace.arcs[*index];
                let input = if global_forward {
                    forward[&(arc.id.predecessor.clone(), arc.id.predecessor_edge)]
                } else {
                    state_arrival(trace, &(arc.id.predecessor.clone(), arc.id.predecessor_edge)).unwrap()
                };
                ((input + arc.local_delay - parent_reference) / tau).exp()
            }).collect();
            let sum: f64 = weights.iter().sum();
            if !sum.is_finite() || sum <= 0.0 { return Err(format!("invalid predecessor allocation for class {}", class)); }
            for weight in &mut weights { *weight /= sum; }
            for (index, weight) in indices.iter().zip(weights) {
                let arc = &trace.arcs[*index];
                let arc_mass = mass * weight;
                add_q(&mut q, &arc.id, arc_mass);
                *adjoint.entry((arc.id.predecessor.clone(), arc.id.predecessor_edge)).or_insert(0.0) += arc_mass;
            }
        }
    }
    for arc in &trace.arcs { q.entry(arc.id.clone()).or_insert(0.0); }
    Ok(CriticalityResult { q, soft_delay: if global_forward { Some(allocation_reference) } else { None }, tau: Some(tau) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{EdgeTimingPointV2, TimingArcPointV2, TimingPointV2};
    use crate::ExtendedCost;
    use egraph_serialize::NodeId;
    use ordered_float::NotNan;

    fn edge_point(arrival: f64) -> EdgeTimingPointV2 {
        EdgeTimingPointV2 { arrival, required: 10.0, slack: 10.0-arrival, transition: 1.0, critical_arc: None }
    }
    fn point(name: &str, arrival: f64) -> TimingPointV2 {
        TimingPointV2 { selected_node: NodeId::from(name), load: 0.0, local_area:0.0, local_power:0.0, rise: edge_point(arrival), fall: edge_point(0.0) }
    }
    fn arc(parent: &str, predecessor: &str, delay: f64, ordinal: usize) -> TimingArcPointV2 {
        TimingArcPointV2 { id: TimingArcIdV2 { parent: ClassId::from(parent), parent_edge: SignalEdge::Rise,
            predecessor: ClassId::from(predecessor), predecessor_edge: SignalEdge::Rise,
            related_pin: format!("A{ordinal}"), arc_ordinal: ordinal }, local_delay: delay, local_transition:1.0, candidate_arrival: delay, delay_d_slew:0.0,delay_d_load:0.0,transition_d_slew:0.0,transition_d_load:0.0 }
    }
    fn diamond(second_delay: f64) -> NldmEvaluationV2 {
        let input = ClassId::from("i"); let a=ClassId::from("a"); let b=ClassId::from("b"); let root=ClassId::from("r");
        let mut timing=FxHashMap::default(); timing.insert(input.clone(),point("ni",0.0)); timing.insert(a.clone(),point("na",5.0)); timing.insert(b.clone(),point("nb",second_delay)); timing.insert(root.clone(),point("nr",10.0));
        let mut cost=ExtendedCost::zero(); cost.components[0]=NotNan::new(10.0).unwrap();
        NldmEvaluationV2 { cost, timing, arcs:vec![arc("a","i",5.0,0),arc("b","i",second_delay,0),
            TimingArcPointV2{id:TimingArcIdV2{parent:root.clone(),parent_edge:SignalEdge::Rise,predecessor:a.clone(),predecessor_edge:SignalEdge::Rise,related_pin:"A".into(),arc_ordinal:0},local_delay:5.0,local_transition:1.0,candidate_arrival:10.0,delay_d_slew:0.0,delay_d_load:0.0,transition_d_slew:0.0,transition_d_load:0.0},
            TimingArcPointV2{id:TimingArcIdV2{parent:root.clone(),parent_edge:SignalEdge::Rise,predecessor:b.clone(),predecessor_edge:SignalEdge::Rise,related_pin:"B".into(),arc_ordinal:1},local_delay:5.0,local_transition:1.0,candidate_arrival:second_delay+5.0,delay_d_slew:0.0,delay_d_load:0.0,transition_d_slew:0.0,transition_d_load:0.0}],
            roots:vec![root],topological_order:vec![input,a,b,ClassId::from("r")],reachable_class_count:4,root_count:1 }
    }

    #[test]
    fn hard_splits_equal_diamond_deterministically() {
        let trace=diamond(5.0); let first=criticality(&trace,CriticalityMethod::Hard,CriticalityParams::default()).unwrap(); let second=criticality(&trace,CriticalityMethod::Hard,CriticalityParams::default()).unwrap();
        assert_eq!(first.q,second.q);
        let root_arcs:Vec<_>=trace.arcs.iter().filter(|a|a.id.parent==ClassId::from("r")).collect();
        assert!((first.q[&root_arcs[0].id]-0.5).abs()<1e-12); assert!((first.q[&root_arcs[1].id]-0.5).abs()<1e-12);
    }

    #[test]
    fn local_soft_assigns_nonzero_mass_to_near_critical_branch() {
        let trace=diamond(4.8); let result=criticality(&trace,CriticalityMethod::LocalSoftBackprop,CriticalityParams{tau_ratio:0.02}).unwrap();
        let root_arcs:Vec<_>=trace.arcs.iter().filter(|a|a.id.parent==ClassId::from("r")).collect();
        assert!(result.q[&root_arcs[0].id]>result.q[&root_arcs[1].id]); assert!(result.q[&root_arcs[1].id]>0.0);
    }

    #[test]
    fn global_lse_is_finite_and_preserves_root_mass() {
        let trace=diamond(4.8); let result=criticality(&trace,CriticalityMethod::GlobalLogSumExp,CriticalityParams{tau_ratio:0.01}).unwrap();
        assert!(result.soft_delay.unwrap().is_finite());
        let mass:f64=trace.arcs.iter().filter(|a|a.id.parent==ClassId::from("r")).map(|a|result.q[&a.id]).sum();
        assert!((mass-1.0).abs()<1e-12); assert!(result.q.values().all(|q|q.is_finite()&&*q>=0.0));
    }
}
