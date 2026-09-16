//! Local NLDM response of one closed `RegionEnode` cone at incumbent boundary.
//! The evaluator consumes only the selected expression and physical boundary;
//! rewrite provenance is intentionally unavailable.

use crate::language::LanguageType;
use crate::physical_topology_egraph::TopologyExpr;
use crate::physical_topology_egraph::{PhysicalOccurrenceId, PhysicalizedTopology};
use extraction_gym::extract::CircuitAdjointV1;
use extraction_gym::extract::{NldmEvaluationV2, SignalEdge};
use extraction_gym::{ExtendedEGraph, NLDM, TimingSense};
use petgraph::graph::NodeIndex;
use rustc_hash::FxHashMap;
use rustc_hash::FxHashSet;
use serde::Serialize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

static LOCAL_LIBERTY_QUERY_COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn local_liberty_query_count() -> usize {
    LOCAL_LIBERTY_QUERY_COUNT.load(Ordering::Relaxed)
}

fn ensure_cached_cell(ext: &mut ExtendedEGraph, op: &str) -> Result<(), String> {
    LOCAL_LIBERTY_QUERY_COUNT.fetch_add(1, Ordering::Relaxed);
    if ext.cell_nldm.contains_key(op) {
        return Ok(());
    }
    let lib_path = std::env::var("EGG_LIB_PATH")
        .unwrap_or_else(|_| "test/asap7sc6t_SELECT_LVT_TT_nldm.lib".to_owned());
    // A process can evaluate more than one library in tests and A/B drivers.
    // Cell names alone are therefore not a safe cache identity.
    static CACHE: OnceLock<
        Mutex<std::collections::HashMap<(String, String), extraction_gym::NLDM>>,
    > = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let cache_key = (lib_path.clone(), op.to_owned());
    if let Some(cell) = cache
        .lock()
        .map_err(|_| "NLDM cache poisoned")?
        .get(&cache_key)
        .cloned()
    {
        ext.cell_nldm.insert(op.to_owned(), cell);
        return Ok(());
    }
    ext.ensure_cell_nldm_from_lib(&lib_path, op)?;
    let cell = ext
        .cell_nldm
        .get(op)
        .cloned()
        .ok_or_else(|| format!("failed to load {op}"))?;
    cache
        .lock()
        .map_err(|_| "NLDM cache poisoned")?
        .insert(cache_key, cell);
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub struct LocalEdgeState {
    pub arrival: f64,
    pub transition: f64,
}
#[derive(Clone, Debug)]
pub struct LocalConeResponse {
    pub input_caps: FxHashMap<usize, f64>,
    pub rise: LocalEdgeState,
    pub fall: LocalEdgeState,
    pub area: f64,
    pub power: f64,
}

/// Joint response of a connected set of occurrence-anchored eclasses.
///
/// Each selected `RegionEnode` remains a root plus a closed internal cone.
/// This response is evaluated only after the selected cones have gone through
/// the existing legal physicalizer.  Consequently internal cuts between two
/// selected anchors disappear, generated cells are counted exactly once, and
/// a downstream candidate pin capacitance changes the load seen by an upstream
/// region cell.  Rewrite provenance is neither accepted nor inspected.
#[derive(Clone, Debug)]
pub struct LocalRegionResponse {
    pub input_caps: FxHashMap<usize, f64>,
    pub outputs: FxHashMap<usize, (LocalEdgeState, LocalEdgeState)>,
    pub area: f64,
    pub power: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryResidualKind {
    InputCap,
    OutputArrivalRise,
    OutputArrivalFall,
    OutputSlewRise,
    OutputSlewFall,
    Area,
    Power,
}

#[derive(Clone, Debug, Serialize)]
pub struct BoundaryDualComponent {
    pub kind: BoundaryResidualKind,
    pub anchor: Option<usize>,
    pub incumbent_response: f64,
    pub candidate_response: f64,
    pub dual_price: f64,
    pub signed_contribution: f64,
    pub residual: f64,
}
#[derive(Clone, Debug)]
struct EvalNode {
    rise: LocalEdgeState,
    fall: LocalEdgeState,
    area: f64,
    power: f64,
    input_caps: FxHashMap<usize, f64>,
}

fn constant_state() -> EvalNode {
    EvalNode {
        rise: LocalEdgeState {
            arrival: 0.0,
            transition: 0.0,
        },
        fall: LocalEdgeState {
            arrival: 0.0,
            transition: 0.0,
        },
        area: 0.0,
        power: 0.0,
        input_caps: FxHashMap::default(),
    }
}

fn boundary_state(trace: &NldmEvaluationV2, anchor: usize) -> Result<EvalNode, String> {
    let class = egraph_serialize::ClassId::from(format!("physical.{anchor}"));
    let point = trace
        .timing
        .get(&class)
        .ok_or_else(|| format!("missing boundary anchor {anchor}"))?;
    Ok(EvalNode {
        rise: LocalEdgeState {
            arrival: point.rise.arrival,
            transition: point.rise.transition,
        },
        fall: LocalEdgeState {
            arrival: point.fall.arrival,
            transition: point.fall.transition,
        },
        area: 0.0,
        power: 0.0,
        input_caps: FxHashMap::default(),
    })
}
fn edge(x: &EvalNode, e: SignalEdge) -> LocalEdgeState {
    match e {
        SignalEdge::Rise => x.rise,
        SignalEdge::Fall => x.fall,
    }
}
fn add_caps(dst: &mut FxHashMap<usize, f64>, src: FxHashMap<usize, f64>) {
    for (k, v) in src {
        *dst.entry(k).or_insert(0.0) += v
    }
}

fn evaluate(
    expression: &TopologyExpr,
    load: f64,
    trace: &NldmEvaluationV2,
    cells: &std::collections::HashMap<String, NLDM>,
) -> Result<EvalNode, String> {
    match expression {
        TopologyExpr::Anchor(anchor) => boundary_state(trace, anchor.index()),
        TopologyExpr::Cell { op, children }
            if children.is_empty() && matches!(op.as_str(), "true" | "false") =>
        {
            Ok(constant_state())
        }
        TopologyExpr::Cell { op, children } => {
            let cell = cells
                .get(op)
                .ok_or_else(|| format!("local evaluator has no Liberty cell {op}"))?;
            let mut inputs = Vec::new();
            let mut area = cell.area.into_inner();
            let mut power = cell.leakage_power.into_inner() / 1000.0;
            let mut caps = FxHashMap::default();
            for (i, child) in children.iter().enumerate() {
                let pin = cell
                    .pin_order
                    .get(i + 1)
                    .ok_or_else(|| format!("cell {op} missing input pin {i}"))?;
                let cap = cell
                    .pin_info
                    .get(pin)
                    .map(|x| x.0.into_inner())
                    .unwrap_or(0.0);
                let value = evaluate(child, cap, trace, cells)?;
                if let TopologyExpr::Anchor(anchor) = child {
                    *caps.entry(anchor.index()).or_insert(0.0) += cap
                } else {
                    add_caps(&mut caps, value.input_caps.clone())
                }
                area += value.area;
                power += value.power;
                inputs.push((pin.clone(), value));
            }
            if !inputs.is_empty() && !cell.internal_power.is_empty() {
                let mut sum = 0.0;
                let mut count = 0;
                for (pin, input) in &inputs {
                    if let Some(rows) = cell.internal_power.get(pin) {
                        sum += extraction_gym::Lut2D::from_indexed_rows(rows)?
                            .lookup_f64(input.rise.transition.max(input.fall.transition), load)?;
                        count += 1
                    }
                }
                if count > 0 {
                    power += sum / count as f64
                }
            }
            let mut output = [LocalEdgeState {
                arrival: 0.0,
                transition: 0.0,
            }; 2];
            let mut seen = [false; 2];
            for arc in cell.timing_arcs.iter() {
                let Some((_, input)) = inputs.iter().find(|(pin, _)| pin == &arc.related_pin)
                else {
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
                    let source = edge(input, ine);
                    let (delay_lut, transition_lut) = match oute {
                        SignalEdge::Rise => (&arc.cell_rise, &arc.rise_transition),
                        SignalEdge::Fall => (&arc.cell_fall, &arc.fall_transition),
                    };
                    let arrival = source.arrival + delay_lut.lookup_f64(source.transition, load)?;
                    let transition = transition_lut.lookup_f64(source.transition, load)?;
                    let oi = if oute == SignalEdge::Rise { 0 } else { 1 };
                    if !seen[oi] || arrival > output[oi].arrival {
                        output[oi] = LocalEdgeState {
                            arrival,
                            transition,
                        };
                        seen[oi] = true
                    }
                }
            }
            Ok(EvalNode {
                rise: output[0],
                fall: output[1],
                area,
                power,
                input_caps: caps,
            })
        }
    }
}

pub fn evaluate_region_enode(
    expression: &TopologyExpr,
    root_anchor: usize,
    trace: &NldmEvaluationV2,
    ext: &ExtendedEGraph,
) -> Result<LocalConeResponse, String> {
    fn ops(expr: &TopologyExpr, out: &mut Vec<String>) {
        match expr {
            TopologyExpr::Anchor(_) => {}
            TopologyExpr::Cell { op, children } => {
                if !(children.is_empty() && matches!(op.as_str(), "true" | "false")) {
                    out.push(op.clone());
                }
                for child in children {
                    ops(child, out)
                }
            }
        }
    }
    let mut required = Vec::new();
    ops(expression, &mut required);
    required.sort();
    required.dedup();
    let mut extended = None;
    if required.iter().any(|op| !ext.cell_nldm.contains_key(op)) {
        let mut owned = ext.clone();
        for op in &required {
            ensure_cached_cell(&mut owned, op)?;
        }
        extended = Some(owned);
    }
    let complete = extended.as_ref().unwrap_or(ext);
    let root = egraph_serialize::ClassId::from(format!("physical.{root_anchor}"));
    let load = trace.timing.get(&root).ok_or("missing region root")?.load;
    let value = evaluate(expression, load, trace, &complete.cell_nldm)?;
    Ok(LocalConeResponse {
        input_caps: value.input_caps,
        rise: value.rise,
        fall: value.fall,
        area: value.area,
        power: value.power,
    })
}

pub fn evaluate_region_enode_with_lib(
    expression: &TopologyExpr,
    root_anchor: usize,
    trace: &NldmEvaluationV2,
    ext: &ExtendedEGraph,
    lib_path: &str,
) -> Result<LocalConeResponse, String> {
    fn ops(expr: &TopologyExpr, out: &mut Vec<String>) {
        match expr {
            TopologyExpr::Anchor(_) => {}
            TopologyExpr::Cell { op, children } => {
                if !(children.is_empty() && matches!(op.as_str(), "true" | "false")) {
                    out.push(op.clone());
                }
                for child in children {
                    ops(child, out)
                }
            }
        }
    }
    let mut complete = ext.clone();
    let mut required = Vec::new();
    ops(expression, &mut required);
    required.sort();
    required.dedup();
    for op in required {
        complete.ensure_cell_nldm_from_lib(lib_path, &op)?;
    }
    evaluate_region_enode(expression, root_anchor, trace, &complete)
}

pub fn required_cell_ops(expression: &TopologyExpr) -> Vec<String> {
    fn walk(expr: &TopologyExpr, out: &mut Vec<String>) {
        match expr {
            TopologyExpr::Anchor(_) => {}
            TopologyExpr::Cell { op, children } => {
                if !(children.is_empty() && matches!(op.as_str(), "true" | "false")) {
                    out.push(op.clone());
                }
                for child in children {
                    walk(child, out)
                }
            }
        }
    }
    let mut result = Vec::new();
    walk(expression, &mut result);
    result.sort();
    result.dedup();
    result
}

/// Boundary-dual score of one complete root cone relative to the incumbent
/// one-cell cone.  Only physical response and circuit prices are consumed.
pub fn boundary_dual_score(
    candidate: &LocalConeResponse,
    incumbent: &LocalConeResponse,
    root_anchor: usize,
    adjoint: &CircuitAdjointV1,
) -> f64 {
    let mut score = adjoint.area_price * (candidate.area - incumbent.area)
        + adjoint.power_price * (candidate.power - incumbent.power);
    let mut anchors: rustc_hash::FxHashSet<usize> = candidate
        .input_caps
        .keys()
        .chain(incumbent.input_caps.keys())
        .copied()
        .collect();
    for anchor in anchors.drain() {
        let class = egraph_serialize::ClassId::from(format!("physical.{anchor}"));
        score += adjoint.load_price.get(&class).copied().unwrap_or(0.0)
            * (candidate.input_caps.get(&anchor).copied().unwrap_or(0.0)
                - incumbent.input_caps.get(&anchor).copied().unwrap_or(0.0));
    }
    let root = egraph_serialize::ClassId::from(format!("physical.{root_anchor}"));
    for (edge, ca, ia, cs, is) in [
        (
            SignalEdge::Rise,
            candidate.rise.arrival,
            incumbent.rise.arrival,
            candidate.rise.transition,
            incumbent.rise.transition,
        ),
        (
            SignalEdge::Fall,
            candidate.fall.arrival,
            incumbent.fall.arrival,
            candidate.fall.transition,
            incumbent.fall.transition,
        ),
    ] {
        score += adjoint
            .arrival_price
            .get(&(root.clone(), edge))
            .copied()
            .unwrap_or(0.0)
            * (ca - ia);
        score += adjoint
            .transition_price
            .get(&(root.clone(), edge))
            .copied()
            .unwrap_or(0.0)
            * (cs - is);
    }
    score
}

fn occurrence_owner(id: &PhysicalOccurrenceId) -> usize {
    match id {
        PhysicalOccurrenceId::Original(anchor) => *anchor,
        PhysicalOccurrenceId::Generated { anchor, .. } => *anchor,
    }
}

fn original_anchor(id: &PhysicalOccurrenceId) -> Option<usize> {
    match id {
        PhysicalOccurrenceId::Original(anchor) => Some(*anchor),
        PhysicalOccurrenceId::Generated { .. } => None,
    }
}

fn materialized_cell<'a>(
    physical: &'a PhysicalizedTopology,
    ext: &'a ExtendedEGraph,
    node: NodeIndex,
) -> Option<&'a NLDM> {
    let op = physical.netlist.graph[node].to_string();
    ext.cell_nldm.get(&op)
}

fn external_state(
    physical: &PhysicalizedTopology,
    trace: &NldmEvaluationV2,
    node: NodeIndex,
) -> Result<EvalNode, String> {
    let id = physical.physical_ids.get(&node).ok_or_else(|| {
        format!(
            "materialized node {} has no physical identity",
            node.index()
        )
    })?;
    let anchor = original_anchor(id)
        .ok_or_else(|| format!("generated node {:?} lies outside the requested region", id))?;
    boundary_state(trace, anchor)
}

fn joint_node_state(
    physical: &PhysicalizedTopology,
    trace: &NldmEvaluationV2,
    ext: &ExtendedEGraph,
    owned: &FxHashSet<NodeIndex>,
    loads: &FxHashMap<NodeIndex, f64>,
    node: NodeIndex,
    memo: &mut FxHashMap<NodeIndex, EvalNode>,
    active: &mut FxHashSet<NodeIndex>,
) -> Result<EvalNode, String> {
    if let Some(value) = memo.get(&node) {
        return Ok(value.clone());
    }
    if physical.netlist.graph[node].is_constant() {
        return Ok(constant_state());
    }
    if !owned.contains(&node) {
        return external_state(physical, trace, node);
    }
    if !active.insert(node) {
        return Err(format!(
            "cycle while evaluating local region at node {}",
            node.index()
        ));
    }
    let cell = materialized_cell(physical, ext, node).ok_or_else(|| {
        format!(
            "local region evaluator has no Liberty cell {}",
            physical.netlist.graph[node].to_string()
        )
    })?;
    let load = loads.get(&node).copied().unwrap_or(0.0);
    let children: Vec<_> = physical.netlist.inputs(node).collect();
    let mut inputs = Vec::with_capacity(children.len());
    for (index, child) in children.iter().enumerate() {
        let pin = cell.pin_order.get(index + 1).ok_or_else(|| {
            format!(
                "cell {} missing input pin {index}",
                physical.netlist.graph[node].to_string()
            )
        })?;
        let value = joint_node_state(physical, trace, ext, owned, loads, *child, memo, active)?;
        inputs.push((pin.clone(), value));
    }

    let mut power = cell.leakage_power.into_inner() / 1000.0;
    if !inputs.is_empty() && !cell.internal_power.is_empty() {
        let mut sum = 0.0;
        let mut count = 0usize;
        for (pin, input) in &inputs {
            if let Some(rows) = cell.internal_power.get(pin) {
                sum += extraction_gym::Lut2D::from_indexed_rows(rows)?
                    .lookup_f64(input.rise.transition.max(input.fall.transition), load)?;
                count += 1;
            }
        }
        if count > 0 {
            power += sum / count as f64;
        }
    }

    let mut output = [LocalEdgeState {
        arrival: 0.0,
        transition: 0.0,
    }; 2];
    let mut seen = [false; 2];
    for arc in cell.timing_arcs.iter() {
        let Some((_, input)) = inputs.iter().find(|(pin, _)| pin == &arc.related_pin) else {
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
        for &(input_edge, output_edge) in mappings {
            let source = edge(input, input_edge);
            let (delay_lut, transition_lut) = match output_edge {
                SignalEdge::Rise => (&arc.cell_rise, &arc.rise_transition),
                SignalEdge::Fall => (&arc.cell_fall, &arc.fall_transition),
            };
            let arrival = source.arrival + delay_lut.lookup_f64(source.transition, load)?;
            let transition = transition_lut.lookup_f64(source.transition, load)?;
            let output_index = usize::from(output_edge == SignalEdge::Fall);
            if !seen[output_index] || arrival > output[output_index].arrival {
                output[output_index] = LocalEdgeState {
                    arrival,
                    transition,
                };
                seen[output_index] = true;
            }
        }
    }
    active.remove(&node);
    let value = EvalNode {
        rise: output[0],
        fall: output[1],
        area: cell.area.into_inner(),
        power,
        input_caps: FxHashMap::default(),
    };
    memo.insert(node, value.clone());
    Ok(value)
}

/// Evaluate a legally physicalized multi-eclass region at the frozen incumbent
/// boundary.  `output_anchors` must be the incumbent region output boundary;
/// it is deliberately supplied explicitly so an anchor-eliminating expression
/// is still priced at the same logical cut.
pub fn evaluate_physicalized_region(
    physical: &PhysicalizedTopology,
    region_anchors: &FxHashSet<usize>,
    output_anchors: &FxHashSet<usize>,
    trace: &NldmEvaluationV2,
    ext: &ExtendedEGraph,
) -> Result<LocalRegionResponse, String> {
    if region_anchors.is_empty() {
        return Err("region must not be empty".into());
    }
    let mut required: Vec<_> = physical
        .netlist
        .graph
        .node_indices()
        .filter(|node| {
            physical
                .physical_ids
                .get(node)
                .is_some_and(|id| region_anchors.contains(&occurrence_owner(id)))
        })
        .filter(|node| !physical.netlist.graph[*node].is_constant())
        .map(|node| physical.netlist.graph[node].to_string())
        .collect();
    required.sort();
    required.dedup();
    // The production controller preloads the complete immutable cell table.
    // Avoid cloning the full ExtendedEGraph for every local candidate when all
    // required cells are already present.  Retain the exact historical lazy
    // fill path for standalone callers and partial test libraries.
    let mut extended = None;
    if required.iter().any(|op| !ext.cell_nldm.contains_key(op)) {
        let mut owned = ext.clone();
        for op in &required {
            ensure_cached_cell(&mut owned, op)?;
        }
        extended = Some(owned);
    }
    let complete = extended.as_ref().unwrap_or(ext);

    let owned: FxHashSet<_> = physical
        .netlist
        .graph
        .node_indices()
        .filter(|node| {
            physical
                .physical_ids
                .get(node)
                .is_some_and(|id| region_anchors.contains(&occurrence_owner(id)))
                && materialized_cell(physical, complete, *node).is_some()
        })
        .collect();

    let mut loads = FxHashMap::default();
    let mut input_caps = FxHashMap::default();
    for sink in physical.netlist.graph.node_indices() {
        let Some(cell) = materialized_cell(physical, complete, sink) else {
            continue;
        };
        for (pin_index, driver) in physical.netlist.inputs(sink).enumerate() {
            let cap = cell
                .pin_order
                .get(pin_index + 1)
                .and_then(|pin| cell.pin_info.get(pin))
                .map(|info| info.0.into_inner())
                .unwrap_or(0.0);
            if owned.contains(&driver) {
                *loads.entry(driver).or_insert(0.0) += cap;
            }
            if owned.contains(&sink)
                && !owned.contains(&driver)
                && !physical.netlist.graph[driver].is_constant()
            {
                let id = physical.physical_ids.get(&driver).ok_or_else(|| {
                    format!(
                        "boundary driver {} has no physical identity",
                        driver.index()
                    )
                })?;
                let anchor = original_anchor(id)
                    .ok_or_else(|| format!("boundary driver {:?} is not an original anchor", id))?;
                *input_caps.entry(anchor).or_insert(0.0) += cap;
            }
        }
    }

    let mut memo = FxHashMap::default();
    let mut active = FxHashSet::default();
    let mut area = 0.0;
    let mut power = 0.0;
    for node in &owned {
        let value = joint_node_state(
            physical,
            trace,
            complete,
            &owned,
            &loads,
            *node,
            &mut memo,
            &mut active,
        )?;
        area += value.area;
        power += value.power;
    }

    let mut outputs = FxHashMap::default();
    for anchor in output_anchors {
        let original = NodeIndex::new(*anchor);
        let node = physical
            .original_to_materialized
            .get(&original)
            .ok_or_else(|| {
                format!("region output anchor {anchor} is unreachable after physicalization")
            })?;
        let value = joint_node_state(
            physical,
            trace,
            complete,
            &owned,
            &loads,
            *node,
            &mut memo,
            &mut active,
        )?;
        outputs.insert(*anchor, (value.rise, value.fall));
    }
    Ok(LocalRegionResponse {
        input_caps,
        outputs,
        area,
        power,
    })
}

pub fn boundary_dual_score_region(
    candidate: &LocalRegionResponse,
    incumbent: &LocalRegionResponse,
    adjoint: &CircuitAdjointV1,
) -> f64 {
    let mut score = adjoint.area_price * (candidate.area - incumbent.area)
        + adjoint.power_price * (candidate.power - incumbent.power);
    let mut input_anchors: FxHashSet<_> = candidate
        .input_caps
        .keys()
        .chain(incumbent.input_caps.keys())
        .copied()
        .collect();
    for anchor in input_anchors.drain() {
        let class = egraph_serialize::ClassId::from(format!("physical.{anchor}"));
        score += adjoint.load_price.get(&class).copied().unwrap_or(0.0)
            * (candidate.input_caps.get(&anchor).copied().unwrap_or(0.0)
                - incumbent.input_caps.get(&anchor).copied().unwrap_or(0.0));
    }
    let mut output_anchors: FxHashSet<_> = candidate
        .outputs
        .keys()
        .chain(incumbent.outputs.keys())
        .copied()
        .collect();
    for anchor in output_anchors.drain() {
        let class = egraph_serialize::ClassId::from(format!("physical.{anchor}"));
        let candidate_state = candidate.outputs.get(&anchor);
        let incumbent_state = incumbent.outputs.get(&anchor);
        for (edge, index) in [(SignalEdge::Rise, 0usize), (SignalEdge::Fall, 1usize)] {
            let state = |value: Option<&(LocalEdgeState, LocalEdgeState)>| match (value, index) {
                (Some((rise, _)), 0) => Some(*rise),
                (Some((_, fall)), 1) => Some(*fall),
                _ => None,
            };
            let candidate_value = state(candidate_state).unwrap_or(LocalEdgeState {
                arrival: 0.0,
                transition: 0.0,
            });
            let incumbent_value = state(incumbent_state).unwrap_or(LocalEdgeState {
                arrival: 0.0,
                transition: 0.0,
            });
            score += adjoint
                .arrival_price
                .get(&(class.clone(), edge))
                .copied()
                .unwrap_or(0.0)
                * (candidate_value.arrival - incumbent_value.arrival);
            score += adjoint
                .transition_price
                .get(&(class.clone(), edge))
                .copied()
                .unwrap_or(0.0)
                * (candidate_value.transition - incumbent_value.transition);
        }
    }
    score
}

/// Decompose the exact same boundary-dual score into auditable components.
/// The sum of `signed_contribution` is equal to
/// `boundary_dual_score_region(candidate, incumbent, adjoint)` up to floating
/// point summation order.  Rule provenance is not available to this API.
pub fn boundary_dual_components_region(
    candidate: &LocalRegionResponse,
    incumbent: &LocalRegionResponse,
    adjoint: &CircuitAdjointV1,
) -> Vec<BoundaryDualComponent> {
    let mut output = Vec::new();
    let mut push =
        |kind, anchor, incumbent_response: f64, candidate_response: f64, dual_price: f64| {
            let signed_contribution = dual_price * (candidate_response - incumbent_response);
            output.push(BoundaryDualComponent {
                kind,
                anchor,
                incumbent_response,
                candidate_response,
                dual_price,
                signed_contribution,
                residual: signed_contribution.abs(),
            });
        };
    push(
        BoundaryResidualKind::Area,
        None,
        incumbent.area,
        candidate.area,
        adjoint.area_price,
    );
    push(
        BoundaryResidualKind::Power,
        None,
        incumbent.power,
        candidate.power,
        adjoint.power_price,
    );
    let mut inputs: Vec<_> = candidate
        .input_caps
        .keys()
        .chain(incumbent.input_caps.keys())
        .copied()
        .collect();
    inputs.sort_unstable();
    inputs.dedup();
    for anchor in inputs {
        let class = egraph_serialize::ClassId::from(format!("physical.{anchor}"));
        push(
            BoundaryResidualKind::InputCap,
            Some(anchor),
            incumbent.input_caps.get(&anchor).copied().unwrap_or(0.0),
            candidate.input_caps.get(&anchor).copied().unwrap_or(0.0),
            adjoint.load_price.get(&class).copied().unwrap_or(0.0),
        );
    }
    let mut outputs: Vec<_> = candidate
        .outputs
        .keys()
        .chain(incumbent.outputs.keys())
        .copied()
        .collect();
    outputs.sort_unstable();
    outputs.dedup();
    for anchor in outputs {
        let class = egraph_serialize::ClassId::from(format!("physical.{anchor}"));
        let incumbent_state = incumbent.outputs.get(&anchor).copied().unwrap_or((
            LocalEdgeState {
                arrival: 0.0,
                transition: 0.0,
            },
            LocalEdgeState {
                arrival: 0.0,
                transition: 0.0,
            },
        ));
        let candidate_state = candidate.outputs.get(&anchor).copied().unwrap_or((
            LocalEdgeState {
                arrival: 0.0,
                transition: 0.0,
            },
            LocalEdgeState {
                arrival: 0.0,
                transition: 0.0,
            },
        ));
        for (kind, edge, incumbent_value, candidate_value) in [
            (
                BoundaryResidualKind::OutputArrivalRise,
                SignalEdge::Rise,
                incumbent_state.0.arrival,
                candidate_state.0.arrival,
            ),
            (
                BoundaryResidualKind::OutputArrivalFall,
                SignalEdge::Fall,
                incumbent_state.1.arrival,
                candidate_state.1.arrival,
            ),
        ] {
            push(
                kind,
                Some(anchor),
                incumbent_value,
                candidate_value,
                adjoint
                    .arrival_price
                    .get(&(class.clone(), edge))
                    .copied()
                    .unwrap_or(0.0),
            );
        }
        for (kind, edge, incumbent_value, candidate_value) in [
            (
                BoundaryResidualKind::OutputSlewRise,
                SignalEdge::Rise,
                incumbent_state.0.transition,
                candidate_state.0.transition,
            ),
            (
                BoundaryResidualKind::OutputSlewFall,
                SignalEdge::Fall,
                incumbent_state.1.transition,
                candidate_state.1.transition,
            ),
        ] {
            push(
                kind,
                Some(anchor),
                incumbent_value,
                candidate_value,
                adjoint
                    .transition_price
                    .get(&(class.clone(), edge))
                    .copied()
                    .unwrap_or(0.0),
            );
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_constants_need_no_liberty_cell_or_boundary_anchor() {
        let trace = NldmEvaluationV2 {
            cost: extraction_gym::ExtendedCost::zero(),
            timing: FxHashMap::default(),
            arcs: Vec::new(),
            roots: Vec::new(),
            topological_order: Vec::new(),
            reachable_class_count: 0,
            root_count: 0,
        };
        for op in ["false", "true"] {
            let constant = TopologyExpr::cell(op, Vec::new());
            assert!(required_cell_ops(&constant).is_empty());
            let value = evaluate(&constant, 1.0, &trace, &Default::default()).unwrap();
            assert_eq!(
                (
                    value.area,
                    value.power,
                    value.rise.arrival,
                    value.fall.arrival
                ),
                (0.0, 0.0, 0.0, 0.0)
            );
            assert!(value.input_caps.is_empty());
            let nested = TopologyExpr::cell("INVx1", vec![constant]);
            assert_eq!(required_cell_ops(&nested), vec!["INVx1"]);
        }
    }

    #[test]
    fn joint_boundary_component_sum_matches_dual_score() {
        let mut incumbent = LocalRegionResponse {
            input_caps: FxHashMap::default(),
            outputs: FxHashMap::default(),
            area: 10.0,
            power: 2.0,
        };
        incumbent.input_caps.insert(3, 0.25);
        incumbent.outputs.insert(
            7,
            (
                LocalEdgeState {
                    arrival: 1.0,
                    transition: 0.10,
                },
                LocalEdgeState {
                    arrival: 1.2,
                    transition: 0.12,
                },
            ),
        );
        let mut candidate = LocalRegionResponse {
            input_caps: FxHashMap::default(),
            outputs: FxHashMap::default(),
            area: 9.5,
            power: 2.2,
        };
        candidate.input_caps.insert(3, 0.30);
        candidate.input_caps.insert(4, 0.05);
        candidate.outputs.insert(
            7,
            (
                LocalEdgeState {
                    arrival: 0.9,
                    transition: 0.11,
                },
                LocalEdgeState {
                    arrival: 1.3,
                    transition: 0.10,
                },
            ),
        );

        let mut adjoint = CircuitAdjointV1 {
            arc_delay_price: FxHashMap::default(),
            arrival_price: FxHashMap::default(),
            transition_price: FxHashMap::default(),
            load_price: FxHashMap::default(),
            input_cap_price: FxHashMap::default(),
            area_price: 0.2,
            power_price: 0.5,
            timing_normalizer: 1.0,
        };
        let class3 = egraph_serialize::ClassId::from("physical.3".to_owned());
        let class4 = egraph_serialize::ClassId::from("physical.4".to_owned());
        let class7 = egraph_serialize::ClassId::from("physical.7".to_owned());
        adjoint.load_price.insert(class3, 0.7);
        adjoint.load_price.insert(class4, -0.4);
        adjoint
            .arrival_price
            .insert((class7.clone(), SignalEdge::Rise), 1.1);
        adjoint
            .arrival_price
            .insert((class7.clone(), SignalEdge::Fall), 1.3);
        adjoint
            .transition_price
            .insert((class7.clone(), SignalEdge::Rise), -0.2);
        adjoint
            .transition_price
            .insert((class7, SignalEdge::Fall), 0.4);

        let score = boundary_dual_score_region(&candidate, &incumbent, &adjoint);
        let component_sum: f64 = boundary_dual_components_region(&candidate, &incumbent, &adjoint)
            .iter()
            .map(|component| component.signed_contribution)
            .sum();
        assert!(
            (score - component_sum).abs() <= 1.0e-12,
            "score={score} components={component_sum}"
        );
    }
}
