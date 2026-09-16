//! Exact-NLDM timing trace for a complete selected DAG.
//!
//! This deliberately mirrors `dag_cost_nldm_on_pruned`: trace is diagnostic
//! data, not a replacement cost model.

use super::*;
use ordered_float::NotNan;
use egraph_serialize::{ClassId, NodeId};

#[derive(Clone, Debug)]
pub struct TimingPoint {
    pub selected_node: NodeId,
    pub local_delay: f64,
    pub arrival: f64,
    pub required: f64,
    pub slack: f64,
    pub transition: f64,
    pub load: f64,
    pub critical_predecessor: Option<ClassId>,
}

#[derive(Clone, Debug)]
pub struct NldmEvaluation {
    pub cost: ExtendedCost,
    pub timing: FxHashMap<ClassId, TimingPoint>,
    pub topological_order: Vec<ClassId>,
    pub reachable_class_count: usize,
    pub root_count: usize,
}

impl ExtractionResult {
    pub fn evaluate_nldm_with_trace(
        &self,
        ext: &ExtendedEGraph,
        delay_target: Option<f64>,
    ) -> NldmEvaluation {
        let inf = ExtendedCost::infinity();
        let mut reachable: FxHashSet<ClassId> = FxHashSet::default();
        let mut q: VecDeque<ClassId> = ext.inner.root_eclasses.clone().into();
        while let Some(cid) = q.pop_front() {
            let Some(nid) = self.choices.get(&cid) else { continue };
            if !reachable.insert(cid.clone()) { continue; }
            for ch in &ext.inner[nid].children { q.push_back(ext.inner.nid_to_cid(ch).clone()); }
        }
        if reachable.is_empty() { return NldmEvaluation { cost: inf, timing: FxHashMap::default(), topological_order: vec![], reachable_class_count: 0, root_count: 0 }; }

        let mut parents: FxHashMap<ClassId, Vec<ClassId>> = FxHashMap::default();
        let mut indegree: FxHashMap<ClassId, usize> = FxHashMap::default();
        for cid in &reachable { parents.insert(cid.clone(), vec![]); indegree.insert(cid.clone(), 0); }
        for cid in &reachable {
            let node = &ext.inner[&self.choices[cid]];
            for ch in &node.children {
                let cc = ext.inner.nid_to_cid(ch);
                if reachable.contains(cc) { parents.entry(cc.clone()).or_default().push(cid.clone()); *indegree.get_mut(cid).unwrap() += 1; }
            }
        }
        for ps in parents.values_mut() { ps.sort_by_key(|c| c.to_string()); }
        let mut stack: Vec<ClassId> = indegree.iter().filter(|(_, d)| **d == 0).map(|(c, _)| c.clone()).collect();
        stack.sort_by_key(|c| c.to_string());
        let mut arrivals: FxHashMap<ClassId, NotNan<f64>> = FxHashMap::default();
        let mut locals: FxHashMap<ClassId, ExtendedCost> = FxHashMap::default();
        let mut loads: FxHashMap<ClassId, NotNan<f64>> = FxHashMap::default();
        let mut critical: FxHashMap<ClassId, Option<ClassId>> = FxHashMap::default();
        let zero = NotNan::new(0.0).unwrap();
        let mut topological_order = Vec::with_capacity(reachable.len());
        while let Some(cid) = stack.pop() {
            topological_order.push(cid.clone());
            let nid = &self.choices[&cid]; let node = &ext.inner[nid];
            let op = ext.node_ops.get(nid).cloned().unwrap_or_else(|| node.op.clone());
            let ps = parents.get(&cid).cloned().unwrap_or_default();
            let mut local = ExtendedCost::zero(); let mut load = if ps.is_empty() { NotNan::new(1.0).unwrap() } else { zero };
            if !node.children.is_empty() {
                if let Some(nldm) = ext.cell_nldm.get(&op) {
                    let mut slews = HashMap::new();
                    for (i, ch) in node.children.iter().enumerate() { if let Some(pin) = nldm.pin_order.get(i+1) { slews.insert(pin.clone(), locals.get(ext.inner.nid_to_cid(ch)).map(|x| x.components[3]).unwrap_or(zero)); } }
                    for p in &ps {
                        let pnid = &self.choices[p]; let pnode = &ext.inner[pnid];
                        let pop = ext.node_ops.get(pnid).cloned().unwrap_or_else(|| pnode.op.clone());
                        if let Some(idx) = pnode.children.iter().position(|c| ext.inner.nid_to_cid(c) == &cid) {
                            if let Some(pnldm) = ext.cell_nldm.get(&pop) { if let Some(pin) = pnldm.pin_order.get(idx+1) { if let Some((cap,_)) = pnldm.pin_info.get(pin) { load += *cap; } } } else { load += NotNan::new(1.0).unwrap(); }
                        }
                    }
                    local.components[0] = nldm.lookup_table("delay", slews.clone(), load).unwrap_or(zero);
                    local.components[1] = nldm.area;
                    local.components[2] = nldm.lookup_table("internal_power", slews.clone(), load).unwrap_or(zero) + nldm.leakage_power / NotNan::new(1000.0).unwrap();
                    local.components[3] = nldm.lookup_table("transition", slews, load).unwrap_or(zero);
                }
            }
            let mut max_arr = zero; let mut pred = None;
            for ch in &node.children {
                let cc = ext.inner.nid_to_cid(ch);
                let a = arrivals.get(cc).copied().unwrap_or(zero);
                // Record a predecessor even for an equal/zero arrival.  The
                // first child is a deterministic tie-break and makes a
                // reconstructed critical chain terminate at a real leaf
                // rather than at a zero-delay internal node.
                if pred.is_none() || a > max_arr {
                    max_arr = a;
                    pred = Some(cc.clone());
                }
            }
            arrivals.insert(cid.clone(), max_arr + local.components[0]); locals.insert(cid.clone(), local); loads.insert(cid.clone(), load); critical.insert(cid.clone(), pred);
            for p in ps { let d = indegree.get_mut(&p).unwrap(); *d -= 1; if *d == 0 { stack.push(p); } }
        }
        if arrivals.len() != reachable.len() { return NldmEvaluation { cost: inf, timing: FxHashMap::default(), topological_order, reachable_class_count: reachable.len(), root_count: 0 }; }
        let mut all_children = FxHashSet::default(); for cid in &reachable { for ch in &ext.inner[&self.choices[cid]].children { all_children.insert(ext.inner.nid_to_cid(ch).clone()); } }
        let mut cost = ExtendedCost::zero(); for cid in &reachable { cost.components[1] += locals[cid].components[1]; cost.components[2] += locals[cid].components[2]; if !all_children.contains(cid) { cost.components[0] = cost.components[0].max(arrivals[cid]); } }
        let target = NotNan::new(delay_target.unwrap_or(cost.components[0].into_inner())).unwrap();
        let mut req: FxHashMap<ClassId, NotNan<f64>> = reachable.iter().map(|c| (c.clone(), NotNan::new(f64::INFINITY).unwrap())).collect();
        let roots: Vec<ClassId> = ext.inner.root_eclasses.iter().filter(|c| reachable.contains(*c)).cloned().collect();
        for root in &roots { req.insert(root.clone(), target); }
        // Required time is a dependency computation, not an arrival-time sort:
        // a parent is always processed before its child here.
        for cid in topological_order.iter().rev() { let r = req[cid]; for ch in &ext.inner[&self.choices[cid]].children { let cc = ext.inner.nid_to_cid(ch); if reachable.contains(cc) { let cand = r - locals[cid].components[0]; if cand < req[cc] { req.insert(cc.clone(), cand); } } } }
        let reachable_class_count = reachable.len();
        let timing = reachable.into_iter().map(|cid| { let nid = self.choices[&cid].clone(); let a=arrivals[&cid]; let r=req[&cid]; (cid.clone(), TimingPoint { selected_node:nid, local_delay:locals[&cid].components[0].into_inner(), arrival:a.into_inner(), required:r.into_inner(), slack:(r-a).into_inner(), transition:locals[&cid].components[3].into_inner(), load:loads[&cid].into_inner(), critical_predecessor:critical.remove(&cid).unwrap_or(None) }) }).collect();
        NldmEvaluation { cost, timing, topological_order, reachable_class_count, root_count: roots.len() }
    }
}
