#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_variables)]

use derivative::Derivative;
use egraph_serialize::{ClassId, EGraph, Node, NodeId};
use indexmap::{IndexMap, IndexSet};
use itertools::{Itertools, iproduct};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub struct TopoSortEgraphIterator<'a> {
    egraph: &'a EGraph,
    outdegree: IndexMap<NodeId, usize>,
    parents: IndexMap<ClassId, Vec<NodeId>>,
    queue: VecDeque<NodeId>,
    visited: IndexSet<NodeId>,
    found_cycle: bool,
}

impl<'a> TopoSortEgraphIterator<'a> {
    fn new(egraph: &'a EGraph) -> Self {
        let outdegree: IndexMap<NodeId, usize> = egraph
            .nodes
            .iter()
            .map(|(nid, n)| (nid.clone(), n.children.len()))
            .collect();
        let mut parents = IndexMap::<ClassId, Vec<NodeId>>::with_capacity(egraph.classes().len());
        let n2c = |nid: &NodeId| egraph.nid_to_cid(nid);
        let mut queue = VecDeque::default();
        let mut visited = IndexSet::with_capacity(egraph.nodes.len());

        for class in egraph.classes().values() {
            parents.insert(class.id.clone(), Vec::new());
        }

        for class in egraph.classes().values() {
            for node in &class.nodes {
                for c in &egraph[node].children {
                    // compute parents of this enode
                    parents[n2c(c)].push(node.clone());
                }

                // start the queue from leaves
                if egraph[node].is_leaf() {
                    if visited.insert(node.clone()) {
                        queue.push_back(node.clone());
                    }
                }
            }
        }
        Self {
            egraph,
            outdegree,
            parents,
            queue,
            visited,
            found_cycle: false,
        }
    }
}

impl<'a> Iterator for TopoSortEgraphIterator<'a> {
    type Item = Result<NodeId, String>;
    fn next(&mut self) -> Option<Self::Item> {
        let n2c = |nid: &NodeId| self.egraph.nid_to_cid(nid);
        if let Some(nid) = self.queue.pop_front() {
            for parent in self.parents[n2c(&nid)].iter() {
                if let Some(outdegree) = self.outdegree.get_mut(parent) {
                    *outdegree = outdegree.saturating_sub(1);
                    if *outdegree == 0 {
                        if self.visited.insert(parent.clone()) {
                            self.queue.push_back(parent.clone());
                        }
                    }
                }
            }
            return Some(Ok(nid));
        }
        if !self.found_cycle {
            let remaining: Vec<_> = self
                .outdegree
                .iter()
                .filter(|&(_, &d)| d > 0)
                .map(|(n, _)| n)
                .collect();
            if remaining.len() > 0 {
                self.found_cycle = true;
                return Some(Err(format!("Exists cycle: {:?}", remaining)));
            }
        }
        None
    }
}

/// Topo sorts the egraph from leaves to roots.
pub fn topo_sort_egragh(egraph: &EGraph) -> TopoSortEgraphIterator<'_> {
    TopoSortEgraphIterator::new(egraph)
}

// #[derive(Debug, Ord, PartialOrd, Eq, PartialEq, Hash, Clone, Default)]
// pub struct Cut {
//     nodes: BTreeSet<NodeId>,
// }

pub type Cut = BTreeSet<NodeId>;

// impl<const N: usize> From<[NodeId; N]> for Cut {
//     fn from(arr: [NodeId; N]) -> Self {
//         Self {
//             nodes: BTreeSet::from(arr),
//         }
//     }
// }

// #[derive(Debug, Ord, PartialOrd, Eq, PartialEq, Hash, Clone, Default)]
// pub struct CutRoot {
//     nodes: BTreeSet<NodeId>,
// }

pub type CutRoot = BTreeSet<NodeId>;

// impl From<(Cut, NodeId)> for CutRoot {
//     fn from((cut, root): (Cut, NodeId)) -> Self {
//         let mut nodes = cut.nodes;
//         nodes.insert(root);
//         Self { nodes }
//     }
// }
//
// impl From<(&Cut, &NodeId)> for CutRoot {
//     fn from((cut, root): (&Cut, &NodeId)) -> Self {
//         let mut nodes = cut.nodes.clone();
//         nodes.insert(root.clone());
//         Self { nodes }
//     }
// }

fn cut_root(mut cut: Cut, root: NodeId) -> CutRoot {
    cut.insert(root);
    cut
}

// #[derive(Debug, Ord, PartialOrd, Eq, PartialEq, Hash, Clone, Default)]
// pub struct Cone {
//     nodes: BTreeMap<ClassId, NodeId>,
// }

pub type Cone = BTreeMap<ClassId, NodeId>;

// impl<const N: usize> From<[(ClassId, NodeId); N]> for Cone {
//     fn from(arr: [(ClassId, NodeId); N]) -> Self {
//         Self {
//             nodes: BTreeMap::from(arr),
//         }
//     }
// }

#[derive(Debug)]
pub struct KCuts {
    k: usize,
    cuts: IndexMap<NodeId, Vec<Cut>>,
}

#[derive(Debug)]
pub struct KCones {
    k: usize,
    cones: FxHashMap<CutRoot, FxHashSet<Cone>>,
}

pub fn k_cuts_k_cones(egraph: &EGraph, k: usize) -> Result<(KCuts, KCones), String> {
    let mut parents = IndexMap::<ClassId, Vec<NodeId>>::with_capacity(egraph.classes().len());
    let n2c = |nid: &NodeId| egraph.nid_to_cid(nid);
    let mut analysis_pending = UniqueQueue::default();

    for class in egraph.classes().values() {
        parents.insert(class.id.clone(), Vec::new());
    }

    for class in egraph.classes().values() {
        for node in &class.nodes {
            for c in &egraph[node].children {
                // compute parents of this enode
                parents[n2c(c)].push(node.clone());
            }

            // start the analysis from leaves
            if egraph[node].is_leaf() {
                analysis_pending.insert(node.clone());
            }
        }
    }

    let mut k_cuts: IndexMap<NodeId, Vec<Cut>> = IndexMap::default();
    let mut k_cones: FxHashMap<CutRoot, FxHashSet<Cone>> = FxHashMap::default();
    let n2cn = |nid: &NodeId| (egraph.nid_to_cid(nid).clone(), nid.clone());
    while let Some(node_id) = analysis_pending.pop() {
        let cut = Cut::from([node_id.clone()]);
        k_cuts.insert(node_id.clone(), vec![cut.clone()]);
        let cone = Cone::from([n2cn(&node_id)]);
        k_cones.insert(cut_root(cut, node_id.clone()), FxHashSet::from_iter([cone]));
        let class_id = n2c(&node_id);
        let node = &egraph[&node_id];
        // 所有前置class都有访问过
        if node.children.iter().all(|child| {
            egraph[n2c(child)]
                .nodes
                .iter()
                .any(|n| k_cuts.contains_key(n))
        }) {
            let (cuts, cones) =
                calculate_cuts_cones(egraph, k, node_id.clone(), &k_cuts, &k_cones)?;
            // need update
            if cuts.len() > 0 {
                k_cuts[&node_id].extend(cuts);
                cones
                    .into_iter()
                    .for_each(|(cr, cs)| k_cones.entry(cr).or_default().extend(cs));
                analysis_pending.extend(parents[class_id].iter().cloned());
            }
        }
    }
    todo!()
}

fn calculate_cuts_cones(
    egraph: &EGraph,
    k: usize,
    node_id: NodeId,
    k_cuts: &IndexMap<NodeId, Vec<Cut>>,
    k_cones: &FxHashMap<CutRoot, FxHashSet<Cone>>,
) -> Result<(Vec<Cut>, FxHashMap<CutRoot, FxHashSet<Cone>>), String> {
    let node = &egraph[&node_id];
    let cid = egraph.nid_to_cid(&node_id);

    if node.children.is_empty() {
        return Ok((Default::default(), Default::default()));
    }

    // Get unique classes of children.
    let mut childrens_classes = node
        .children
        .iter()
        .map(|c| egraph.nid_to_cid(&c).clone())
        .collect::<Vec<ClassId>>();
    childrens_classes.sort();
    childrens_classes.dedup();

    if childrens_classes.contains(&cid) {
        return Err("Found self loop!".to_string());
    }

    for childrens_nodes in childrens_classes
        .iter()
        .map(|c| egraph[c].nodes.iter())
        .multi_cartesian_product()
    {}

    todo!()
}

fn calculate_cuts_cones_for_childrens_nodes(
    egraph: &EGraph,
    k: usize,
    node_id: NodeId,
    class_id: ClassId,
    childrens_nodes: Vec<&NodeId>,
    k_cuts: &IndexMap<NodeId, Vec<Cut>>,
    k_cones: &FxHashMap<CutRoot, FxHashSet<Cone>>,
) -> (Vec<Cut>, FxHashMap<CutRoot, FxHashSet<Cone>>) {
    if !childrens_nodes
        .iter()
        .all(|&child| k_cuts.contains_key(child))
    {
        return (Default::default(), Default::default());
    }
    let mut child_iter = childrens_nodes.iter().copied();
    if let Some(first) = child_iter.next() {
        let mut this_cuts = k_cuts[first].clone();
        let mut partial_cones: FxHashMap<CutRoot, FxHashSet<Cone>> = Default::default();
        for cut in this_cuts.iter() {
            let first_cr = cut_root(cut.clone(), first.clone());
            let this_cr = cut_root(cut.clone(), node_id.clone());
            for first_cone in k_cones[&first_cr].iter() {
                // 跳过不一致的choice
                if first_cone
                    .get(&class_id)
                    .is_some_and(|choice| choice != &node_id)
                {
                    continue;
                }

                let mut this_cone = first_cone.clone();
                this_cone.insert(class_id.clone(), node_id.clone());
                partial_cones
                    .entry(this_cr.clone())
                    .or_default()
                    .insert(this_cone);
            }
        }
        for other in child_iter {
            let mut temp_cuts: Vec<Cut> = Default::default();
            let other_cuts = k_cuts[other].clone();
            for (cut_1, cut_2) in iproduct!(&this_cuts, &other_cuts) {
                let merge_cut = cut_1 | cut_2;
                if merge_cut.len() <= k {
                    temp_cuts.push(merge_cut.clone());
                    let merge_cr = cut_root(merge_cut.clone(), node_id.clone());
                }
            }
        }
        todo!()
    } else {
        return (Default::default(), Default::default());
    }
}

#[derive(Clone)]
struct UniqueQueue<T>
where
    T: Eq + std::hash::Hash + Clone,
{
    set: FxHashSet<T>, // hashbrown::
    queue: std::collections::VecDeque<T>,
}

/** A data structure to maintain a queue of global unique elements.

Notably, insert/pop operations have O(1) expected amortized runtime complexity.

Thanks @Bastacyclop for the implementation!
*/
impl<T> Default for UniqueQueue<T>
where
    T: Eq + std::hash::Hash + Clone,
{
    fn default() -> Self {
        UniqueQueue {
            set: Default::default(),
            queue: std::collections::VecDeque::new(),
        }
    }
}

impl<T> UniqueQueue<T>
where
    T: Eq + std::hash::Hash + Clone,
{
    pub fn insert(&mut self, t: T) {
        if self.set.insert(t.clone()) {
            self.queue.push_back(t);
        }
    }

    pub fn extend<I>(&mut self, iter: I)
    where
        I: IntoIterator<Item = T>,
    {
        for t in iter.into_iter() {
            self.insert(t);
        }
    }

    pub fn pop(&mut self) -> Option<T> {
        let res = self.queue.pop_front();
        res.as_ref().map(|t| self.set.remove(t));
        res
        // self.queue.pop_front()
    }

    // #[allow(dead_code)]
    // pub fn is_empty(&self) -> bool {
    //     let r = self.queue.is_empty();
    //     debug_assert_eq!(r, self.set.is_empty());
    //     r
    // }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egraph_roots::EGraphRoots;
    use crate::io::liberty::{get_direction_of_pins, read_liberty};
    use crate::io::stdcell::read_verilog_with_lib_to_netlist;
    use crate::language::StdCellLanguage;
    use crate::rule::JsonRules;
    use crate::{SerializedEGraph, egg_to_serialized_egraph, netlist_to_egg_roots};
    use egg::Runner;
    use std::env;

    #[test]
    fn test_topo_sort_egragh() {
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let lib = get_direction_of_pins(&liberty).unwrap();
        let (netlist, name) = read_verilog_with_lib_to_netlist("test/add2_map_abc.v", lib).unwrap();
        assert_eq!(name, "add2");
        let egraph_roots: EGraphRoots<_, ()> = netlist_to_egg_roots(&netlist).unwrap();
        let rules =
            JsonRules::from_path(env::current_dir().unwrap().join("test/6t_comm_rules.json"))
                .unwrap()
                .into_egg_rules::<StdCellLanguage>()
                .unwrap();
        let runner = Runner::default()
            .with_egraph(egraph_roots.egraph)
            .with_node_limit(1000000)
            .run(&rules);
        let s = egg_to_serialized_egraph(&runner.egraph, &egraph_roots.roots);
        let mut cnt = 0;
        for nid in topo_sort_egragh(&s) {
            let nid = nid.unwrap();
            let n = &s[&nid];
            println!("{}: {}, children: {:?}", nid, n.op, n.children);
            cnt += 1;
        }
        assert_eq!(cnt, s.nodes.len());
    }

    #[test]
    fn test_topo_sort_egragh_dmg() {
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let lib = get_direction_of_pins(&liberty).unwrap();
        let (netlist, name) = read_verilog_with_lib_to_netlist("test/add2_map_abc.v", lib).unwrap();
        assert_eq!(name, "add2");
        let egraph_roots: EGraphRoots<_, ()> = netlist_to_egg_roots(&netlist).unwrap();
        let mut rules =
            JsonRules::from_path(env::current_dir().unwrap().join("test/6t_inv_rules.json"))
                .unwrap()
                .into_egg_rules::<StdCellLanguage>()
                .unwrap();
        rules.extend(
            JsonRules::from_path(env::current_dir().unwrap().join("test/6t_dmg_rules.json"))
                .unwrap()
                .into_egg_rules::<StdCellLanguage>()
                .unwrap(),
        );
        let runner = Runner::default()
            .with_egraph(egraph_roots.egraph)
            .with_node_limit(1000000)
            .run(&rules);
        let s = egg_to_serialized_egraph(&runner.egraph, &egraph_roots.roots);
        let mut cnt = 0;
        for nid in topo_sort_egragh(&s) {
            let nid = nid.unwrap();
            let n = &s[&nid];
            println!("{}: {}, children: {:?}", nid, n.op, n.children);
            cnt += 1;
        }
        assert_eq!(cnt, s.nodes.len());
    }
}
