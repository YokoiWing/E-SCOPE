pub mod adaptive_region;
mod analyzer;
pub mod d3_joint;
pub mod egraph_roots;
pub mod extractor;
pub mod io;
pub mod language;
pub mod mining;
pub mod netlist;
pub mod physical_scale_egraph;
pub mod physical_topology_egraph;
pub mod region_local_evaluator;
pub mod rule;
pub mod structural_realization;

use crate::egraph_roots::EGraphRoots;
use crate::language::LanguageType;
use crate::netlist::Netlist;
use egg::*;
pub use egraph_serialize::EGraph as SerializedEGraph;
use egraph_serialize::{ClassId, NodeId};
use extraction_gym::extract::ExtractionResult;
use itertools::Itertools;
use petgraph::algo::toposort;
use petgraph::graph::NodeIndex;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;
use std::fmt::Display;

#[derive(Clone, Debug)]
pub struct NetlistEggProvenance<L, A>
where
    L: Language,
    A: Analysis<L>,
    A::Data: Clone,
{
    pub roots: EGraphRoots<L, A>,
    pub netlist_node_to_egg: FxHashMap<NodeIndex, Id>,
}

pub fn egg_to_serialized_egraph<L, N>(egraph: &EGraph<L, N>, roots: &Vec<Id>) -> SerializedEGraph
where
    L: Language + Display,
    N: Analysis<L>,
{
    use egraph_serialize::*;
    let mut out = EGraph::default();
    for class in egraph.classes() {
        for (i, node) in class.nodes.iter().enumerate() {
            out.add_node(
                format!("{}.{}", class.id, i),
                Node {
                    op: node.to_string(),
                    children: node
                        .children()
                        .iter()
                        .map(|id| NodeId::from(format!("{}.0", id)))
                        .collect(),
                    eclass: ClassId::from(format!("{}", class.id)),
                    cost: Cost::new(1.0).unwrap(),
                },
            )
        }
    }
    for root in roots {
        let id = egraph[*root].id;
        out.root_eclasses.push(ClassId::from(format!("{}", id)));
    }
    out
}

pub fn netlist_to_egg_roots<N, A>(
    netlist: &Netlist<N, ()>,
) -> Result<EGraphRoots<N::Lang, A>, String>
where
    N: LanguageType,
    A: Analysis<N::Lang> + Default,
    A::Data: Clone,
{
    Ok(netlist_to_egg_roots_with_provenance(netlist)?.roots)
}

/// Build the traditional logical e-graph while retaining the exact egg id
/// assigned to every original netlist occurrence.  The legacy API above is a
/// wrapper, so existing callers keep the same behavior.
pub fn netlist_to_egg_roots_with_provenance<N, A>(
    netlist: &Netlist<N, ()>,
) -> Result<NetlistEggProvenance<N::Lang, A>, String>
where
    N: LanguageType,
    A: Analysis<N::Lang> + Default,
    A::Data: Clone,
{
    let mut egraph_roots: EGraphRoots<N::Lang, A> = Default::default();
    let mut nid_to_id: FxHashMap<NodeIndex, Id> = Default::default();
    let order =
        toposort(&netlist.graph, None).map_err(|e| format!("Graph contains cycle: {:?}", e))?;
    for nid in netlist.graph.node_indices() {
        if !netlist.leaves.contains(&nid) && !netlist.graph[nid].is_constant() {
            continue;
        }
        let weight = &netlist.graph[nid];
        let id = egraph_roots.egraph.add(weight.to_lang_input());
        nid_to_id.insert(nid, id);
    }
    let leaves_set = netlist
        .graph
        .node_indices()
        .filter(|nid| netlist.leaves.contains(nid) || netlist.graph[*nid].is_constant())
        .collect::<FxHashSet<_>>();
    let root_set = FxHashSet::from_iter(netlist.roots.clone());
    for &nid in order.iter().rev() {
        if leaves_set.contains(&nid) || root_set.contains(&nid) {
            continue;
        }
        if nid_to_id.contains_key(&nid) {
            return Err(format!("Node {:?} exists already", nid));
        } else {
            // let inputs: Vec<_> = netlist
            //     .graph
            //     .neighbors(nid)
            //     .map(|neighbor| nid_to_id[&neighbor])
            //     .collect();
            // let inputs: Vec<_> = inputs.into_iter().rev().collect(); // petaGraph use linked list to push edges, so we must reverse
            let inputs = netlist
                .inputs(nid)
                .map(|neighbor| nid_to_id[&neighbor])
                .collect_vec();
            let weight = &netlist.graph[nid];
            let id = egraph_roots.egraph.add(weight.to_lang_gate(inputs));
            nid_to_id.insert(nid, id);
        }
    }
    for &nid in netlist.roots.iter() {
        if nid_to_id.contains_key(&nid) {
            return Err(format!("Node {:?} exists already", nid));
        } else {
            let count = netlist.graph.neighbors(nid).count();
            if count != 1 {
                return Err(format!(
                    "Output {:?} should have exactly 1 input, but got {:?}",
                    nid, count
                ));
            }
            let inputs: Vec<_> = netlist
                .graph
                .neighbors(nid)
                .map(|neighbor| nid_to_id[&neighbor])
                .collect();
            let weight = &netlist.graph[nid];
            let id = egraph_roots.egraph.add(weight.to_lang_output(inputs[0]));
            nid_to_id.insert(nid, id);
            egraph_roots.roots.push(id);
        }
    }
    egraph_roots.egraph.rebuild();
    Ok(NetlistEggProvenance {
        roots: egraph_roots,
        netlist_node_to_egg: nid_to_id,
    })
}

pub fn choose_result_in_serialized_egraph_into_netlist<N>(
    in_egraph: &SerializedEGraph,
    result: &ExtractionResult,
) -> Result<Netlist<N, ()>, String>
where
    N: LanguageType,
{
    use egraph_serialize::*;
    let mut netlist: Netlist<N, ()> = Default::default();
    let mut todo: VecDeque<ClassId> = in_egraph.root_eclasses.clone().into();
    let mut visited: FxHashSet<ClassId> = Default::default();
    let mut class_to_nid: FxHashMap<ClassId, NodeIndex> = Default::default();

    // step 1: insert nodes
    while let Some(cid) = todo.pop_front() {
        if !visited.insert(cid.clone()) {
            continue;
        }
        if !result.choices.contains_key(&cid) {
            return Err(format!("Class {} not found in choices", cid));
        }
        let nid = &result.choices[&cid];
        let oid = netlist.graph.add_node(N::from_op(&in_egraph[nid].op));
        if in_egraph[nid].is_leaf() {
            netlist.leaves.push(oid);
        }
        class_to_nid.insert(cid.clone(), oid);

        for child in &in_egraph[nid].children {
            todo.push_back(in_egraph.nid_to_cid(child).clone());
        }
    }
    for cid in in_egraph.root_eclasses.iter() {
        netlist.roots.push(class_to_nid[cid].clone());
    }
    // step 2: insert edges
    todo = in_egraph.root_eclasses.clone().into();
    visited.clear();
    while let Some(cid) = todo.pop_front() {
        if !visited.insert(cid.clone()) {
            continue;
        }
        if !result.choices.contains_key(&cid) {
            return Err(format!("Class {} not found in choices", cid));
        }
        let nid = &result.choices[&cid];
        for child in in_egraph[nid].children.iter() {
            netlist.graph.add_edge(
                class_to_nid[&cid].clone(),
                class_to_nid[in_egraph.nid_to_cid(child)],
                (),
            );
        }

        for child in &in_egraph[nid].children {
            todo.push_back(in_egraph.nid_to_cid(child).clone());
        }
    }
    Ok(netlist)
}

pub fn choose_first_node_in_each_class(
    in_egraph: &SerializedEGraph,
) -> Result<ExtractionResult, String> {
    let mut result = ExtractionResult::default();
    let mut todo: VecDeque<ClassId> = in_egraph.root_eclasses.clone().into();
    let mut visited: FxHashSet<ClassId> = Default::default();

    while let Some(cid) = todo.pop_front() {
        if !visited.insert(cid.clone()) {
            continue;
        }
        let class = in_egraph
            .classes()
            .get(&cid)
            .ok_or_else(|| format!("Class {} not found in egraph", cid))?;
        let nid = class
            .nodes
            .first()
            .ok_or_else(|| format!("Class {} has no nodes", cid))?
            .clone();
        result.choose(cid.clone(), nid.clone());
        for child in &in_egraph[&nid].children {
            todo.push_back(in_egraph.nid_to_cid(child).clone());
        }
    }

    Ok(result)
}

/// Recover the exact choices which represented the mapped netlist before rewriting.
///
/// `egg` rebuild canonicalizes both e-class ids and the child ids stored in nodes.
/// Therefore equality must be checked against a copy of the original node whose
/// children have first been mapped through `rewritten.find()`.  This is deliberate
/// provenance bookkeeping: no cost heuristic or extraction policy is involved.
pub fn recover_original_stdcell_extraction(
    original: &EGraph<language::StdCellLanguage, ()>,
    rewritten: &mut EGraph<language::StdCellLanguage, ()>,
    original_roots: &[Id],
) -> Result<ExtractionResult, String> {
    fn visit(
        original: &EGraph<language::StdCellLanguage, ()>,
        rewritten: &mut EGraph<language::StdCellLanguage, ()>,
        old_class: Id,
        result: &mut ExtractionResult,
    ) -> Result<(), String> {
        let canonical_class = rewritten.find(old_class);
        let class_id = ClassId::from(rewritten[canonical_class].id.to_string());
        let original_node = original[old_class]
            .nodes
            .first()
            .ok_or_else(|| format!("original class {old_class} is empty"))?
            .clone();

        let mut canonicalized_node = original_node.clone();
        for child in canonicalized_node.children_mut() {
            *child = rewritten.find(*child);
        }
        let node_index = rewritten[canonical_class]
            .nodes
            .iter()
            .position(|node| node == &canonicalized_node)
            .ok_or_else(|| {
                format!(
                    "original node {:?} (canonicalized as {:?}) absent from canonical class {}",
                    original_node, canonicalized_node, canonical_class
                )
            })?;
        let node_id = NodeId::from(format!("{}.{}", rewritten[canonical_class].id, node_index));

        if let Some(previous) = result.choices.get(&class_id) {
            if previous != &node_id {
                return Err(format!(
                    "original provenance is ambiguous in canonical class {}: {} versus {} while recovering {:?}; canonical class nodes={:?}",
                    class_id,
                    previous,
                    node_id,
                    canonicalized_node,
                    rewritten[canonical_class].nodes
                ));
            }
            return Ok(());
        }
        result.choose(class_id, node_id);
        for child in original_node.children() {
            visit(original, rewritten, *child, result)?;
        }
        Ok(())
    }

    let mut result = ExtractionResult::default();
    for root in original_roots {
        visit(original, rewritten, *root, &mut result)?;
    }
    Ok(result)
}

pub fn serialized_egraph_to_egg<L, A>(_egraph: &SerializedEGraph) -> (EGraph<L, A>, Vec<Id>)
where
    L: Language + Display,
    A: Analysis<L>,
{
    todo!()
}

pub fn choose_result_in_egraph(
    in_egraph: &SerializedEGraph,
    result: &ExtractionResult,
) -> Result<SerializedEGraph, String> {
    use egraph_serialize::*;
    let mut out_egraph = EGraph::default();
    out_egraph.root_eclasses = in_egraph.root_eclasses.clone();
    let mut todo: VecDeque<ClassId> = in_egraph.root_eclasses.clone().into();
    let mut visited: FxHashSet<ClassId> = Default::default();
    while let Some(cid) = todo.pop_front() {
        if !visited.insert(cid.clone()) {
            continue;
        }
        if !result.choices.contains_key(&cid) {
            return Err(format!("Class {} not found in choices", cid));
        }
        let nid = &result.choices[&cid];
        out_egraph.add_node(
            nid.to_string(),
            Node {
                op: in_egraph[nid].op.clone(),
                children: in_egraph[nid]
                    .children
                    .iter()
                    .map(|nid| result.choices[in_egraph.nid_to_cid(nid)].clone())
                    .collect(),
                eclass: in_egraph[nid].eclass.clone(),
                cost: in_egraph[nid].cost,
            },
        );

        for child in &in_egraph[nid].children {
            todo.push_back(in_egraph.nid_to_cid(child).clone());
        }
    }
    Ok(out_egraph)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::liberty::{get_direction_of_pins, read_liberty};
    use crate::io::stdcell::{read_bench_to_netlist, read_verilog_with_lib_to_netlist};
    use std::env;

    #[test]
    fn test_netlist_to_egg_roots() {
        let netlist = read_bench_to_netlist("test/add2.bench").unwrap();
        let egraph_roots: EGraphRoots<_, ()> = netlist_to_egg_roots(&netlist).unwrap();
        let s = SerializedEGraph::from(&egraph_roots);
        s.to_json_file(
            env::current_dir()
                .unwrap()
                .join("json/test_add2_bench.json"),
        )
        .unwrap();
        #[cfg(target_os = "linux")]
        s.to_svg_file(env::current_dir().unwrap().join("svg/test_add2_bench.svg"))
            .unwrap();
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let lib = get_direction_of_pins(&liberty).unwrap();
        let (netlist, name) = read_verilog_with_lib_to_netlist("test/add2_map_abc.v", lib).unwrap();
        assert_eq!(name, "add2");
        let egraph_roots: EGraphRoots<_, ()> = netlist_to_egg_roots(&netlist).unwrap();
        let s = SerializedEGraph::from(&egraph_roots);
        s.to_json_file(
            env::current_dir()
                .unwrap()
                .join("json/test_add2_map_abc_v.json"),
        )
        .unwrap();
        #[cfg(target_os = "linux")]
        s.to_svg_file(
            env::current_dir()
                .unwrap()
                .join("svg/test_add2_map_abc_v.svg"),
        )
        .unwrap();
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let lib = get_direction_of_pins(&liberty).unwrap();
        let (netlist, name) = read_verilog_with_lib_to_netlist("test/mul4_map_abc.v", lib).unwrap();
        assert_eq!(name, "Multi4");
        let egraph_roots: EGraphRoots<_, ()> = netlist_to_egg_roots(&netlist).unwrap();
        let s = SerializedEGraph::from(&egraph_roots);
        s.to_json_file(
            env::current_dir()
                .unwrap()
                .join("json/test_mul4_map_abc_v.json"),
        )
        .unwrap();
        #[cfg(target_os = "linux")]
        s.to_svg_file(
            env::current_dir()
                .unwrap()
                .join("svg/test_mul4_map_abc_v.svg"),
        )
        .unwrap();
    }

    #[test]
    fn test_netlist_to_egg_roots_mul32() {
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let lib = get_direction_of_pins(&liberty).unwrap();
        let (netlist, name) =
            read_verilog_with_lib_to_netlist("../test/mul32_map_genus.v", lib).unwrap();
        assert_eq!(name, "Multiplier");
        let egraph_roots: EGraphRoots<_, ()> = netlist_to_egg_roots(&netlist).unwrap();
        let s = SerializedEGraph::from(&egraph_roots);
        s.to_json_file(
            env::current_dir()
                .unwrap()
                .join("json/test_mul32_map_genus_v.json"),
        )
        .unwrap();
        s.to_dot_file(
            env::current_dir()
                .unwrap()
                .join("dot/test_mul32_map_genus_v_egg.dot"),
        )
        .unwrap()

        // dot rendering is too slow, so commented
        // #[cfg(target_os = "linux")]
        // s.to_svg_file(
        //     env::current_dir()
        //         .unwrap()
        //         .join("svg/test_mul4_map_genus_v.svg"),
        // )
        // .unwrap();
    }
}
