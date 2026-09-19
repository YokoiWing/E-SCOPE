//! Rule-agnostic boundary state and incumbent local replay for bounded regions.

use super::{ExtractionResult, NldmEvaluationV2, SignalEdge};
use crate::ExtendedEGraph;
use egraph_serialize::ClassId;
use rustc_hash::{FxHashMap, FxHashSet};

#[derive(Clone, Debug)]
pub struct RegionInputCap {
    pub driver: ClassId,
    pub sink: ClassId,
    pub pin_index: usize,
    pub capacitance: f64,
}

#[derive(Clone, Debug)]
pub struct RegionOutputState {
    pub class: ClassId,
    pub rise_arrival: f64,
    pub fall_arrival: f64,
    pub rise_transition: f64,
    pub fall_transition: f64,
}

#[derive(Clone, Debug)]
pub struct RegionBoundaryState {
    pub input_caps: Vec<RegionInputCap>,
    pub outputs: Vec<RegionOutputState>,
    pub area: f64,
    pub power: f64,
}

fn selected_cell<'a>(result:&ExtractionResult,ext:&'a ExtendedEGraph,class:&ClassId)->Option<(&'a egraph_serialize::Node,&'a crate::NLDM)>{
    let node_id=result.choices.get(class)?;let node=&ext.inner[node_id];let op=ext.node_ops.get(node_id).cloned().unwrap_or_else(||node.op.clone());Some((node,ext.cell_nldm.get(&op)?))
}

fn fanout(result:&ExtractionResult,ext:&ExtendedEGraph,trace:&NldmEvaluationV2)->FxHashMap<ClassId,Vec<ClassId>>{
    let mut result_map:FxHashMap<_,Vec<_>>=FxHashMap::default();
    for parent in &trace.topological_order{if let Some(node)=result.choices.get(parent){for child in &ext.inner[node].children{result_map.entry(ext.inner.nid_to_cid(child).clone()).or_default().push(parent.clone());}}}
    result_map
}

pub fn incumbent_region_boundary(
    result:&ExtractionResult,ext:&ExtendedEGraph,trace:&NldmEvaluationV2,region:&FxHashSet<ClassId>
)->Result<RegionBoundaryState,String>{
    if region.is_empty(){return Err("region must not be empty".into())}
    let fanout=fanout(result,ext,trace);let roots:FxHashSet<_>=trace.roots.iter().cloned().collect();
    let mut input_caps=Vec::new();let mut outputs=Vec::new();let mut area=0.0;let mut power=0.0;
    for class in region{
        let timing=trace.timing.get(class).ok_or_else(||format!("region class {class} is not reachable"))?;area+=timing.local_area;power+=timing.local_power;
        if let Some((node,cell))=selected_cell(result,ext,class){for (pin_index,child_node) in node.children.iter().enumerate(){let driver=ext.inner.nid_to_cid(child_node).clone();if region.contains(&driver){continue}let cap=cell.pin_order.get(pin_index+1).and_then(|pin|cell.pin_info.get(pin)).map(|x|x.0.into_inner()).unwrap_or(0.0);input_caps.push(RegionInputCap{driver,sink:class.clone(),pin_index,capacitance:cap});}}
        let external=roots.contains(class)||fanout.get(class).is_some_and(|items|items.iter().any(|sink|!region.contains(sink)));
        if external{outputs.push(RegionOutputState{class:class.clone(),rise_arrival:timing.rise.arrival,fall_arrival:timing.fall.arrival,rise_transition:timing.rise.transition,fall_transition:timing.fall.transition});}
    }
    input_caps.sort_by_key(|x|(x.driver.to_string(),x.sink.to_string(),x.pin_index));outputs.sort_by_key(|x|x.class.to_string());
    Ok(RegionBoundaryState{input_caps,outputs,area,power})
}

/// Replay incumbent region states with external predecessor states fixed.  Arc
/// values are the frozen V2 NLDM lookup results; this function verifies region
/// closure/order and boundary bookkeeping before candidate cones are admitted.
pub fn replay_incumbent_region(
    result:&ExtractionResult,ext:&ExtendedEGraph,trace:&NldmEvaluationV2,region:&FxHashSet<ClassId>
)->Result<RegionBoundaryState,String>{
    let expected=incumbent_region_boundary(result,ext,trace,region)?;
    let mut state:FxHashMap<(ClassId,SignalEdge),(f64,f64)>=FxHashMap::default();
    for class in &trace.topological_order{
        if !region.contains(class){continue}
        for output_edge in [SignalEdge::Rise,SignalEdge::Fall]{
            let arcs:Vec<_>=trace.arcs.iter().filter(|arc|arc.id.parent==*class&&arc.id.parent_edge==output_edge).collect();
            if arcs.is_empty(){let point=&trace.timing[class];let pair=match output_edge{SignalEdge::Rise=>(point.rise.arrival,point.rise.transition),SignalEdge::Fall=>(point.fall.arrival,point.fall.transition)};state.insert((class.clone(),output_edge),pair);continue}
            let mut best:Option<(f64,f64)>=None;
            for arc in arcs{let predecessor=(arc.id.predecessor.clone(),arc.id.predecessor_edge);let pred=state.get(&predecessor).copied().unwrap_or_else(||{let point=&trace.timing[&arc.id.predecessor];match arc.id.predecessor_edge{SignalEdge::Rise=>(point.rise.arrival,point.rise.transition),SignalEdge::Fall=>(point.fall.arrival,point.fall.transition)}});let candidate=(pred.0+arc.local_delay,arc.local_transition);if best.is_none()||candidate.0>best.unwrap().0{best=Some(candidate)}}
            state.insert((class.clone(),output_edge),best.unwrap());
        }
    }
    let mut replay=expected.clone();for output in &mut replay.outputs{let rise=state.get(&(output.class.clone(),SignalEdge::Rise)).ok_or("missing replay rise")?;let fall=state.get(&(output.class.clone(),SignalEdge::Fall)).ok_or("missing replay fall")?;output.rise_arrival=rise.0;output.rise_transition=rise.1;output.fall_arrival=fall.0;output.fall_transition=fall.1;}
    Ok(replay)
}
