//! Rise/fall- and pin-arc-aware NLDM graph timing for mapped scale-only DAGs.
//! V1 remains unchanged for regression.

use super::*;
use crate::Lut2D;
use crate::TimingSense;
use egraph_serialize::{ClassId, NodeId};
use ordered_float::NotNan;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SignalEdge {
    Rise,
    Fall,
}

#[derive(Clone, Debug)]
pub struct CriticalArcV2 {
    pub predecessor: ClassId,
    pub predecessor_edge: SignalEdge,
    pub related_pin: String,
    pub local_delay: f64,
}

/// Stable identity for a physical timing arc in the occurrence-preserving
/// topology-preserving scale-only V0 graph.
///
/// `arc_ordinal` distinguishes multiple Liberty timing blocks with the same
/// related pin and rise/fall mapping.  It deliberately does not contain a
/// NodeId so that the same physical arc can be compared across scale choices.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TimingArcIdV2 {
    pub parent: ClassId,
    pub parent_edge: SignalEdge,
    pub predecessor: ClassId,
    pub predecessor_edge: SignalEdge,
    pub related_pin: String,
    pub arc_ordinal: usize,
}

#[derive(Clone, Debug)]
pub struct TimingArcPointV2 {
    pub id: TimingArcIdV2,
    pub local_delay: f64,
    pub local_transition: f64,
    pub candidate_arrival: f64,
    pub delay_d_slew: f64,
    pub delay_d_load: f64,
    pub transition_d_slew: f64,
    pub transition_d_load: f64,
}

#[derive(Clone, Debug)]
pub struct EdgeTimingPointV2 {
    pub arrival: f64,
    pub required: f64,
    pub slack: f64,
    pub transition: f64,
    pub critical_arc: Option<CriticalArcV2>,
}

#[derive(Clone, Debug)]
pub struct TimingPointV2 {
    pub selected_node: NodeId,
    pub load: f64,
    pub local_area: f64,
    pub local_power: f64,
    pub rise: EdgeTimingPointV2,
    pub fall: EdgeTimingPointV2,
}

#[derive(Clone, Copy, Debug)]
pub struct NldmV2Config {
    pub primary_input_driver_cell: &'static str,
    /// Explicit GENLIB transport mode; false for every existing NLDM run.
    pub genlib_static: bool,
    pub primary_input_arrival_ps: f64,
    pub primary_input_slew_ps: f64,
    pub primary_output_load_ff: f64,
    pub uniform_vectorless_power: bool,
    pub power_period_ps: f64,
    pub power_voltage_v: f64,
    pub toggle_per_cycle: f64,
    pub internal_transition_factor: f64,
    /// V3 mode: derive each PI state from the configured mapped driver and
    /// that PI's actual sink capacitance.
    pub driver_aware_primary_inputs: bool,
    pub driver_input_slew_ps: f64,
    /// V3 mode: propagate worst output slew independently from the arc that
    /// wins arrival, matching conservative graph-based STA behavior.
    pub independent_worst_transition: bool,
}
impl Default for NldmV2Config {
    fn default() -> Self {
        Self {
            primary_input_driver_cell: crate::DEFAULT_PRIMARY_INPUT_DRIVER_CELL,
            genlib_static: false,
            primary_input_arrival_ps: 0.0,
            primary_input_slew_ps: 0.0,
            primary_output_load_ff: 0.0,
            uniform_vectorless_power: false,
            power_period_ps: 1000.0,
            power_voltage_v: 0.7,
            toggle_per_cycle: 0.1,
            internal_transition_factor: 1.78,
            driver_aware_primary_inputs: false,
            driver_input_slew_ps: 0.0,
            independent_worst_transition: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct NldmEvaluationV2 {
    pub cost: ExtendedCost,
    pub timing: FxHashMap<ClassId, TimingPointV2>,
    pub arcs: Vec<TimingArcPointV2>,
    pub roots: Vec<ClassId>,
    pub topological_order: Vec<ClassId>,
    pub reachable_class_count: usize,
    pub root_count: usize,
}

/// Test/validation-only continuous perturbations.  The normal V2 entry point
/// always evaluates with an empty overlay, so production value semantics are
/// unchanged.
#[derive(Clone, Debug, Default)]
pub struct NldmV2PerturbationOverlay {
    /// Diagnostic-only conservative graph-based slew propagation. Arrival
    /// provenance remains unchanged, while each output edge keeps the worst
    /// transition across all legal input arcs instead of the arrival winner's
    /// transition.
    pub independent_worst_transition: bool,
    pub arc_local_delay: FxHashMap<TimingArcIdV2, f64>,
    pub cell_output_load: FxHashMap<ClassId, f64>,
    pub arrival_state: FxHashMap<(ClassId, SignalEdge), f64>,
    pub transition_state: FxHashMap<(ClassId, SignalEdge), f64>,
    pub local_power: FxHashMap<ClassId, f64>,
}

#[derive(Clone, Debug)]
struct State {
    arrival: [NotNan<f64>; 2],
    transition: [NotNan<f64>; 2],
    critical: [Option<CriticalArcV2>; 2],
}
#[derive(Clone, Debug)]
struct Candidate {
    child: ClassId,
    input_edge: SignalEdge,
    output_edge: SignalEdge,
    delay: NotNan<f64>,
    transition: NotNan<f64>,
    delay_d_slew: f64,
    delay_d_load: f64,
    transition_d_slew: f64,
    transition_d_load: f64,
    related_pin: String,
    arc_ordinal: usize,
}
fn ei(edge: SignalEdge) -> usize {
    match edge {
        SignalEdge::Rise => 0,
        SignalEdge::Fall => 1,
    }
}
fn edge(i: usize) -> SignalEdge {
    if i == 0 {
        SignalEdge::Rise
    } else {
        SignalEdge::Fall
    }
}
fn invalid(order: Vec<ClassId>, reachable: usize) -> NldmEvaluationV2 {
    NldmEvaluationV2 {
        cost: ExtendedCost::infinity(),
        timing: FxHashMap::default(),
        arcs: vec![],
        roots: vec![],
        topological_order: order,
        reachable_class_count: reachable,
        root_count: 0,
    }
}

impl ExtractionResult {
    pub fn dag_cost_nldm_v2_on_pruned(&self, ext: &ExtendedEGraph) -> ExtendedCost {
        self.evaluate_nldm_v2_with_trace(ext, None, NldmV2Config::default())
            .cost
    }

    pub fn evaluate_nldm_v2_with_trace(
        &self,
        ext: &ExtendedEGraph,
        delay_target: Option<f64>,
        config: NldmV2Config,
    ) -> NldmEvaluationV2 {
        self.evaluate_nldm_v2_with_trace_overlay(ext, delay_target, config, None)
    }

    pub fn evaluate_nldm_v2_with_trace_overlay(
        &self,
        ext: &ExtendedEGraph,
        delay_target: Option<f64>,
        config: NldmV2Config,
        overlay: Option<&NldmV2PerturbationOverlay>,
    ) -> NldmEvaluationV2 {
        let zero = NotNan::new(0.0).unwrap();
        let inf = NotNan::new(f64::INFINITY).unwrap();
        let mut reachable = FxHashSet::default();
        let mut q: VecDeque<ClassId> = ext.inner.root_eclasses.clone().into();
        while let Some(cid) = q.pop_front() {
            let Some(nid) = self.choices.get(&cid) else {
                return invalid(vec![], reachable.len());
            };
            if !reachable.insert(cid.clone()) {
                continue;
            }
            for ch in &ext.inner[nid].children {
                q.push_back(ext.inner.nid_to_cid(ch).clone())
            }
        }
        if reachable.is_empty() {
            return invalid(vec![], 0);
        }
        let mut fanout: FxHashMap<ClassId, Vec<(ClassId, usize)>> = FxHashMap::default();
        let mut indegree = FxHashMap::default();
        for c in &reachable {
            fanout.insert(c.clone(), vec![]);
            indegree.insert(c.clone(), 0usize);
        }
        for p in &reachable {
            let node = &ext.inner[&self.choices[p]];
            for (idx, ch) in node.children.iter().enumerate() {
                let c = ext.inner.nid_to_cid(ch);
                if reachable.contains(c) {
                    fanout.entry(c.clone()).or_default().push((p.clone(), idx));
                    *indegree.get_mut(p).unwrap() += 1;
                }
            }
        }
        for edges in fanout.values_mut() {
            edges.sort_by_key(|(c, i)| (c.to_string(), *i));
        }
        let mut ready: Vec<_> = indegree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(c, _)| c.clone())
            .collect();
        ready.sort_by_key(|c| std::cmp::Reverse(c.to_string()));
        let mut states: FxHashMap<ClassId, State> = FxHashMap::default();
        let mut locals: FxHashMap<ClassId, ExtendedCost> = FxHashMap::default();
        let mut loads: FxHashMap<ClassId, NotNan<f64>> = FxHashMap::default();
        let mut candidates: FxHashMap<ClassId, Vec<Candidate>> = FxHashMap::default();
        let mut order = Vec::with_capacity(reachable.len());
        while let Some(cid) = ready.pop() {
            order.push(cid.clone());
            let nid = &self.choices[&cid];
            let node = &ext.inner[nid];
            let op = ext
                .node_ops
                .get(nid)
                .cloned()
                .unwrap_or_else(|| node.op.clone());
            let mut load = zero;
            for (p, idx) in &fanout[&cid] {
                let pnid = &self.choices[p];
                let pop = ext
                    .node_ops
                    .get(pnid)
                    .cloned()
                    .unwrap_or_else(|| ext.inner[pnid].op.clone());
                if let Some(pn) = ext.cell_nldm.get(&pop) {
                    if let Some(pin) = pn.pin_order.get(idx + 1) {
                        load += pn.pin_info.get(pin).map(|x| x.0).unwrap_or(zero)
                    }
                } else {
                    load += NotNan::new(config.primary_output_load_ff).unwrap();
                }
            }
            if let Some(delta) = overlay.and_then(|value| value.cell_output_load.get(&cid)) {
                load += NotNan::new(*delta).unwrap();
            }
            loads.insert(cid.clone(), load);
            let mut local = ExtendedCost::zero();
            let mut st = State {
                arrival: [zero; 2],
                transition: [zero; 2],
                critical: [None, None],
            };
            let mut cs = Vec::new();
            if node.children.is_empty() {
                // Historical serialized graphs use either `0`/`1` or
                // egg's `false`/`true` spelling for Boolean constants.
                // Every other childless node is a primary input.
                if !matches!(op.as_str(), "0" | "1" | "false" | "true") {
                    if config.driver_aware_primary_inputs {
                        let Some(driver) =
                            ext.cell_nldm.get(config.primary_input_driver_cell)
                        else {
                            return invalid(order, reachable.len());
                        };
                        let Ok(driver_timing) = driver.primary_input_driver_timing(
                            config.driver_input_slew_ps,
                            load.into_inner(),
                        ) else {
                            return invalid(order, reachable.len());
                        };
                        for output_edge in 0..2 {
                            st.arrival[output_edge] = NotNan::new(
                                config.primary_input_arrival_ps
                                    + driver_timing.arrival_adjust_ps[output_edge],
                            )
                            .unwrap();
                            st.transition[output_edge] =
                                NotNan::new(driver_timing.output_slew_ps[output_edge]).unwrap();
                        }
                    } else {
                        let arrival = NotNan::new(config.primary_input_arrival_ps).unwrap();
                        let slew = NotNan::new(config.primary_input_slew_ps).unwrap();
                        st.arrival = [arrival; 2];
                        st.transition = [slew; 2];
                    }
                    if config.uniform_vectorless_power {
                        local.components[2] = NotNan::new(
                            config.toggle_per_cycle
                                * load.into_inner()
                                * config.power_voltage_v.powi(2)
                                * 1000.0
                                / config.power_period_ps,
                        )
                        .unwrap();
                    }
                }
            } else if let Some(nldm) = ext.cell_nldm.get(&op) {
                local.components[1] = nldm.area;
                let mut pin_child: HashMap<String, ClassId> = HashMap::new();
                let mut power_slews = HashMap::new();
                for (i, ch) in node.children.iter().enumerate() {
                    if let Some(pin) = nldm.pin_order.get(i + 1) {
                        let cc = ext.inner.nid_to_cid(ch).clone();
                        pin_child.insert(pin.clone(), cc.clone());
                        let s = &states[&cc];
                        power_slews.insert(pin.clone(), s.transition[0].max(s.transition[1]));
                    }
                }
                if config.uniform_vectorless_power {
                    let frequency = 1000.0 / config.power_period_ps;
                    let internal_energy = nldm
                        .pin_order
                        .iter()
                        .skip(1)
                        .filter_map(|pin| {
                            let slew = power_slews.get(pin)?;
                            let rows = nldm.internal_power.get(pin)?;
                            Lut2D::from_indexed_rows(rows)
                                .ok()?
                                .lookup_f64(slew.into_inner(), load.into_inner())
                                .ok()
                        })
                        .sum::<f64>();
                    let internal = internal_energy
                        * config.toggle_per_cycle
                        * config.internal_transition_factor
                        * frequency;
                    let switching = config.toggle_per_cycle
                        * load.into_inner()
                        * config.power_voltage_v.powi(2)
                        * frequency;
                    let leakage = nldm.leakage_power.into_inner() / 1_000_000.0;
                    local.components[2] = NotNan::new(internal + switching + leakage).unwrap();
                } else {
                    local.components[2] = nldm
                        .lookup_table("internal_power", power_slews, load)
                        .unwrap_or(zero)
                        + nldm.leakage_power / NotNan::new(1000.0).unwrap();
                }
                if nldm.timing_arcs.is_empty() {
                    return invalid(order, reachable.len());
                }
                for (arc_ordinal, arc) in nldm.timing_arcs.iter().enumerate() {
                    let Some(cc) = pin_child.get(&arc.related_pin) else {
                        continue;
                    };
                    let mappings: &[(SignalEdge, SignalEdge)] = match arc.timing_sense {
                        TimingSense::PositiveUnate => &[
                            (SignalEdge::Rise, SignalEdge::Rise),
                            (SignalEdge::Fall, SignalEdge::Fall),
                        ],
                        TimingSense::NegativeUnate => &[
                            (SignalEdge::Fall, SignalEdge::Rise),
                            (SignalEdge::Rise, SignalEdge::Fall),
                        ],
                        TimingSense::NonUnate => &[
                            (SignalEdge::Rise, SignalEdge::Rise),
                            (SignalEdge::Fall, SignalEdge::Rise),
                            (SignalEdge::Rise, SignalEdge::Fall),
                            (SignalEdge::Fall, SignalEdge::Fall),
                        ],
                    };
                    for &(ine, oute) in mappings {
                        let input = &states[cc];
                        let slew = input.transition[ei(ine)];
                        let (delay_lut, tr_lut) = match oute {
                            SignalEdge::Rise => (&arc.cell_rise, &arc.rise_transition),
                            SignalEdge::Fall => (&arc.cell_fall, &arc.fall_transition),
                        };
                        let Ok(dp) = delay_lut.lookup_with_partials(slew, load) else {
                            continue;
                        };
                        let Ok(tp) = tr_lut.lookup_with_partials(slew, load) else {
                            continue;
                        };
                        let arc_id = TimingArcIdV2 {
                            parent: cid.clone(),
                            parent_edge: oute,
                            predecessor: cc.clone(),
                            predecessor_edge: ine,
                            related_pin: arc.related_pin.clone(),
                            arc_ordinal,
                        };
                        let d = dp.value
                            + NotNan::new(
                                overlay
                                    .and_then(|value| value.arc_local_delay.get(&arc_id))
                                    .copied()
                                    .unwrap_or(0.0),
                            )
                            .unwrap();
                        let t = tp.value;
                        let cand = if config.genlib_static {
                            NotNan::new(
                                ((input.arrival[ei(ine)].into_inner()
                                    + (d.into_inner() * 100.0).ceil() / 100.0)
                                    * 100.0)
                                    .ceil()
                                    / 100.0,
                            )
                            .unwrap()
                        } else {
                            input.arrival[ei(ine)] + d
                        };
                        let oi = ei(oute);
                        let independent_transition = config.independent_worst_transition
                            || overlay
                                .map(|value| value.independent_worst_transition)
                                .unwrap_or(false);
                        if st.critical[oi].is_none() || cand > st.arrival[oi] {
                            st.arrival[oi] = cand;
                            if !independent_transition {
                                st.transition[oi] = t;
                            }
                            st.critical[oi] = Some(CriticalArcV2 {
                                predecessor: cc.clone(),
                                predecessor_edge: ine,
                                related_pin: arc.related_pin.clone(),
                                local_delay: d.into_inner(),
                            });
                        }
                        if independent_transition && t > st.transition[oi] {
                            st.transition[oi] = t;
                        }
                        cs.push(Candidate {
                            child: cc.clone(),
                            input_edge: ine,
                            output_edge: oute,
                            delay: d,
                            transition: t,
                            delay_d_slew: dp.d_slew,
                            delay_d_load: dp.d_load,
                            transition_d_slew: tp.d_slew,
                            transition_d_load: tp.d_load,
                            related_pin: arc.related_pin.clone(),
                            arc_ordinal,
                        });
                    }
                }
                local.components[0] = st
                    .critical
                    .iter()
                    .flatten()
                    .map(|x| NotNan::new(x.local_delay).unwrap())
                    .max()
                    .unwrap_or(zero);
                local.components[3] = st.transition[0].max(st.transition[1]);
            } else {
                for (arc_ordinal, ch) in node.children.iter().enumerate() {
                    let cc = ext.inner.nid_to_cid(ch).clone();
                    for i in 0..2 {
                        let cand = states[&cc].arrival[i];
                        let independent_transition = config.independent_worst_transition
                            || overlay
                                .map(|value| value.independent_worst_transition)
                                .unwrap_or(false);
                        if st.critical[i].is_none() || cand > st.arrival[i] {
                            st.arrival[i] = cand;
                            if !independent_transition {
                                st.transition[i] = states[&cc].transition[i];
                            }
                            st.critical[i] = Some(CriticalArcV2 {
                                predecessor: cc.clone(),
                                predecessor_edge: edge(i),
                                related_pin: String::new(),
                                local_delay: 0.0,
                            });
                        }
                        if independent_transition && states[&cc].transition[i] > st.transition[i] {
                            st.transition[i] = states[&cc].transition[i];
                        }
                        cs.push(Candidate {
                            child: cc.clone(),
                            input_edge: edge(i),
                            output_edge: edge(i),
                            delay: zero,
                            transition: states[&cc].transition[i],
                            delay_d_slew: 0.0,
                            delay_d_load: 0.0,
                            transition_d_slew: 1.0,
                            transition_d_load: 0.0,
                            related_pin: String::new(),
                            arc_ordinal,
                        });
                    }
                }
            }
            if let Some(delta) = overlay.and_then(|value| value.local_power.get(&cid)) {
                local.components[2] += NotNan::new(*delta).unwrap();
            }
            for output_edge in [SignalEdge::Rise, SignalEdge::Fall] {
                if let Some(delta) =
                    overlay.and_then(|value| value.arrival_state.get(&(cid.clone(), output_edge)))
                {
                    st.arrival[ei(output_edge)] += NotNan::new(*delta).unwrap();
                }
                if let Some(delta) = overlay
                    .and_then(|value| value.transition_state.get(&(cid.clone(), output_edge)))
                {
                    st.transition[ei(output_edge)] += NotNan::new(*delta).unwrap();
                }
            }
            states.insert(cid.clone(), st);
            locals.insert(cid.clone(), local);
            candidates.insert(cid.clone(), cs);
            for (p, _) in &fanout[&cid] {
                let d = indegree.get_mut(p).unwrap();
                *d -= 1;
                if *d == 0 {
                    ready.push(p.clone());
                    ready.sort_by_key(|c| std::cmp::Reverse(c.to_string()));
                }
            }
        }
        if states.len() != reachable.len() {
            return invalid(order, reachable.len());
        }
        let roots: Vec<_> = ext
            .inner
            .root_eclasses
            .iter()
            .filter(|c| reachable.contains(*c))
            .cloned()
            .collect();
        let mut cost = ExtendedCost::zero();
        for c in &reachable {
            cost.components[1] += locals[c].components[1];
            cost.components[2] += locals[c].components[2];
        }
        for r in &roots {
            cost.components[0] = cost.components[0]
                .max(states[r].arrival[0])
                .max(states[r].arrival[1]);
        }
        if config.genlib_static {
            cost.components[1] =
                NotNan::new((cost.components[1].into_inner() * 100.0).round() / 100.0).unwrap();
            // Power is unavailable, represented only by a neutral coordinate.
            cost.components[2] = NotNan::new(1.0).unwrap();
        }
        let target = NotNan::new(delay_target.unwrap_or(cost.components[0].into_inner())).unwrap();
        let mut req: FxHashMap<ClassId, [NotNan<f64>; 2]> =
            reachable.iter().map(|c| (c.clone(), [inf; 2])).collect();
        for r in &roots {
            req.insert(r.clone(), [target; 2]);
        }
        for c in order.iter().rev() {
            let parent_req = req[c];
            for cand in &candidates[c] {
                let value = parent_req[ei(cand.output_edge)] - cand.delay;
                let slot = &mut req.get_mut(&cand.child).unwrap()[ei(cand.input_edge)];
                if value < *slot {
                    *slot = value;
                }
            }
        }
        let timing = reachable
            .iter()
            .map(|c| {
                let s = &states[c];
                let r = req[c];
                (
                    c.clone(),
                    TimingPointV2 {
                        selected_node: self.choices[c].clone(),
                        load: loads[c].into_inner(),
                        local_area: locals[c].components[1].into_inner(),
                        local_power: locals[c].components[2].into_inner(),
                        rise: EdgeTimingPointV2 {
                            arrival: s.arrival[0].into_inner(),
                            required: r[0].into_inner(),
                            slack: (r[0] - s.arrival[0]).into_inner(),
                            transition: s.transition[0].into_inner(),
                            critical_arc: s.critical[0].clone(),
                        },
                        fall: EdgeTimingPointV2 {
                            arrival: s.arrival[1].into_inner(),
                            required: r[1].into_inner(),
                            slack: (r[1] - s.arrival[1]).into_inner(),
                            transition: s.transition[1].into_inner(),
                            critical_arc: s.critical[1].clone(),
                        },
                    },
                )
            })
            .collect();
        let mut arcs = Vec::new();
        for parent in &order {
            for cand in &candidates[parent] {
                arcs.push(TimingArcPointV2 {
                    id: TimingArcIdV2 {
                        parent: parent.clone(),
                        parent_edge: cand.output_edge,
                        predecessor: cand.child.clone(),
                        predecessor_edge: cand.input_edge,
                        related_pin: cand.related_pin.clone(),
                        arc_ordinal: cand.arc_ordinal,
                    },
                    local_delay: cand.delay.into_inner(),
                    local_transition: cand.transition.into_inner(),
                    candidate_arrival: (states[&cand.child].arrival[ei(cand.input_edge)]
                        + cand.delay)
                        .into_inner(),
                    delay_d_slew: cand.delay_d_slew,
                    delay_d_load: cand.delay_d_load,
                    transition_d_slew: cand.transition_d_slew,
                    transition_d_load: cand.transition_d_load,
                });
            }
        }
        NldmEvaluationV2 {
            cost,
            timing,
            arcs,
            roots: roots.clone(),
            topological_order: order,
            reachable_class_count: reachable.len(),
            root_count: roots.len(),
        }
    }
}
