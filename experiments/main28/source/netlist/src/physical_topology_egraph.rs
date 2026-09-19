//! Occurrence-anchored physicalization for topology-changing V0.
//!
//! A logical e-class is deliberately not used as physical identity.  Every
//! original mapped-netlist occurrence is an independent anchor.  A selected
//! realization is a tree-shaped cell fragment whose boundary references other
//! original anchors.  Original anchors are memoized (preserving real fanout),
//! while cells created inside a fragment are materialized once per explicit
//! expression occurrence and receive a stable `anchor/path` identity.
//!
//! V0 intentionally provides no implicit physical merging, CSE, or duplication
//! optimization.  It is the legality/physical-semantics layer on which a
//! structural search can be built; sizing remains a later, existing stage.

use crate::language::{LanguageType, StdCellType};
use crate::netlist::Netlist;
use petgraph::algo::toposort;
use petgraph::graph::NodeIndex;
use rustc_hash::{FxHashMap, FxHashSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopologyExpr {
    /// Use the output of another original physical occurrence as a boundary.
    Anchor(NodeIndex),
    /// Materialize an explicit standard-cell occurrence.
    Cell {
        op: String,
        children: Vec<TopologyExpr>,
    },
}

impl TopologyExpr {
    pub fn cell(op: impl Into<String>, children: Vec<Self>) -> Self {
        Self::Cell {
            op: op.into(),
            children,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PhysicalOccurrenceId {
    Original(usize),
    Generated { anchor: usize, path: Vec<usize> },
}

#[derive(Clone, Debug)]
pub struct PhysicalizedTopology {
    pub netlist: Netlist<StdCellType, ()>,
    pub physical_ids: FxHashMap<NodeIndex, PhysicalOccurrenceId>,
    pub original_to_materialized: FxHashMap<NodeIndex, NodeIndex>,
    pub created: Vec<PhysicalOccurrenceId>,
    pub deleted_originals: Vec<NodeIndex>,
}

/// A complete deterministic realization map.  Entries may be omitted only for
/// fixed primary inputs, constants, and primary-output wrapper nodes.  Missing
/// reachable standard-cell anchors use their exact original one-cell form.
#[derive(Clone, Debug, Default)]
pub struct TopologySelection {
    pub choices: FxHashMap<NodeIndex, TopologyExpr>,
}

#[derive(Clone, Debug)]
pub struct StructuralMove {
    pub anchor: NodeIndex,
    pub rule: &'static str,
    pub old_cell_count: usize,
    pub new_cell_count: usize,
    pub realization: TopologyExpr,
}

fn unary_child(source: &Netlist<StdCellType, ()>, anchor: NodeIndex) -> Option<NodeIndex> {
    let children: Vec<_> = source.inputs(anchor).collect();
    (children.len() == 1).then_some(children[0])
}

fn anchors(source: &Netlist<StdCellType, ()>, anchor: NodeIndex) -> Vec<TopologyExpr> {
    source.inputs(anchor).map(TopologyExpr::Anchor).collect()
}

/// Enumerate the conservative topology-changing subset mirrored from
/// `6t_inv_rules.json`.  It contains only explicit inverter introduction,
/// inverter absorption, and double-inverter removal; no commutation, expand,
/// sharing, or duplication rule is admitted.
pub fn enumerate_safe_inv_moves(source: &Netlist<StdCellType, ()>) -> Vec<StructuralMove> {
    let mut moves = Vec::new();
    let introduce = [
        ("AND2x2_ASAP7_6t_L", "NAND2x1_ASAP7_6t_L", "AND2x2_NAND2x1"),
        ("AND3x1_ASAP7_6t_L", "NAND3x1_ASAP7_6t_L", "AND3x1_NAND3x1"),
        ("AO21x1_ASAP7_6t_L", "AOI21x1_ASAP7_6t_L", "AO21x1_AOI21x1"),
        ("OR2x2_ASAP7_6t_L", "NOR2x1_ASAP7_6t_L", "OR2x2_NOR2x1"),
        ("OR3x1_ASAP7_6t_L", "NOR3x1_ASAP7_6t_L", "OR3x1_NOR3x1"),
        ("XOR2x2_ASAP7_6t_L", "XNOR2x2_ASAP7_6t_L", "XOR2x2_XNOR2x2"),
    ];
    let absorb = [
        ("AND2x2_ASAP7_6t_L", "NAND2x1_ASAP7_6t_L", "NAND2x1_AND2x2"),
        ("AND3x1_ASAP7_6t_L", "NAND3x1_ASAP7_6t_L", "NAND3x1_AND3x1"),
        ("AO21x1_ASAP7_6t_L", "AOI21x1_ASAP7_6t_L", "AOI21x1_AO21x1"),
        ("OR2x2_ASAP7_6t_L", "NOR2x1_ASAP7_6t_L", "NOR2x1_OR2x2"),
        ("OR3x1_ASAP7_6t_L", "NOR3x1_ASAP7_6t_L", "NOR3x1_OR3x1"),
        ("XOR2x2_ASAP7_6t_L", "XNOR2x2_ASAP7_6t_L", "XNOR2x2_XOR2x2"),
    ];
    for anchor in source.graph.node_indices() {
        if source.leaves.contains(&anchor) || source.roots.contains(&anchor) {
            continue;
        }
        let op = source.graph[anchor].to_string();
        for (outer, inner, rule) in introduce {
            if op == outer {
                moves.push(StructuralMove {
                    anchor,
                    rule,
                    old_cell_count: 1,
                    new_cell_count: 2,
                    realization: TopologyExpr::cell(
                        "INVx1_ASAP7_6t_L",
                        vec![TopologyExpr::cell(inner, anchors(source, anchor))],
                    ),
                });
            }
        }
        if op != "INVx1_ASAP7_6t_L" {
            continue;
        }
        let Some(child) = unary_child(source, anchor) else {
            continue;
        };
        let child_op = source.graph[child].to_string();
        if child_op == "INVx1_ASAP7_6t_L" {
            if let Some(grandchild) = unary_child(source, child) {
                moves.push(StructuralMove {
                    anchor,
                    rule: "Double_INVx1",
                    old_cell_count: 2,
                    new_cell_count: 0,
                    realization: TopologyExpr::Anchor(grandchild),
                });
            }
        }
        for (inner, combined, rule) in absorb {
            if child_op == inner {
                moves.push(StructuralMove {
                    anchor,
                    rule,
                    old_cell_count: 2,
                    new_cell_count: 1,
                    realization: TopologyExpr::cell(combined, anchors(source, child)),
                });
            }
        }
    }
    moves.sort_by_key(|movement| (movement.anchor.index(), movement.rule));
    moves
}

impl TopologySelection {
    pub fn choose(&mut self, anchor: NodeIndex, realization: TopologyExpr) {
        self.choices.insert(anchor, realization);
    }
}

struct Materializer<'a> {
    source: &'a Netlist<StdCellType, ()>,
    selection: &'a TopologySelection,
    result: Netlist<StdCellType, ()>,
    memo: FxHashMap<NodeIndex, NodeIndex>,
    active: FxHashSet<NodeIndex>,
    physical_ids: FxHashMap<NodeIndex, PhysicalOccurrenceId>,
    created: Vec<PhysicalOccurrenceId>,
}

impl<'a> Materializer<'a> {
    fn connect(&mut self, parent: NodeIndex, children: &[NodeIndex]) {
        for child in children {
            self.result.graph.add_edge(parent, *child, ());
        }
    }

    fn original_expr(&self, anchor: NodeIndex) -> TopologyExpr {
        TopologyExpr::cell(
            self.source.graph[anchor].to_string(),
            self.source
                .inputs(anchor)
                .map(TopologyExpr::Anchor)
                .collect(),
        )
    }

    fn materialize_expr(
        &mut self,
        owner: NodeIndex,
        expr: &TopologyExpr,
        path: &mut Vec<usize>,
    ) -> Result<NodeIndex, String> {
        match expr {
            TopologyExpr::Anchor(anchor) => self.materialize_anchor(*anchor),
            TopologyExpr::Cell { op, children } => {
                let mut child_nodes = Vec::with_capacity(children.len());
                for (index, child) in children.iter().enumerate() {
                    path.push(index);
                    child_nodes.push(self.materialize_expr(owner, child, path)?);
                    path.pop();
                }
                let nid = self.result.graph.add_node(StdCellType::from_op(op));
                self.connect(nid, &child_nodes);
                let physical_id = if path.is_empty() {
                    PhysicalOccurrenceId::Original(owner.index())
                } else {
                    PhysicalOccurrenceId::Generated {
                        anchor: owner.index(),
                        path: path.clone(),
                    }
                };
                if matches!(physical_id, PhysicalOccurrenceId::Generated { .. }) {
                    self.created.push(physical_id.clone());
                }
                self.physical_ids.insert(nid, physical_id);
                Ok(nid)
            }
        }
    }

    fn materialize_anchor(&mut self, anchor: NodeIndex) -> Result<NodeIndex, String> {
        if let Some(existing) = self.memo.get(&anchor) {
            return Ok(*existing);
        }
        if !self.active.insert(anchor) {
            return Err(format!(
                "topology selection contains a dependency cycle at original anchor {}",
                anchor.index()
            ));
        }

        let weight = self
            .source
            .graph
            .node_weight(anchor)
            .ok_or_else(|| format!("unknown original anchor {}", anchor.index()))?
            .clone();
        let is_leaf = self.source.leaves.contains(&anchor) || weight.is_constant();
        let is_root = self.source.roots.contains(&anchor);
        let materialized = if is_leaf {
            if self.selection.choices.contains_key(&anchor) {
                return Err(format!(
                    "fixed leaf/constant anchor {} cannot have a structural choice",
                    anchor.index()
                ));
            }
            let nid = self.result.graph.add_node(weight);
            self.result.leaves.push(nid);
            self.physical_ids
                .insert(nid, PhysicalOccurrenceId::Original(anchor.index()));
            nid
        } else if is_root {
            if self.selection.choices.contains_key(&anchor) {
                return Err(format!(
                    "primary-output wrapper anchor {} cannot have a structural choice",
                    anchor.index()
                ));
            }
            let children: Vec<_> = self.source.inputs(anchor).collect();
            if children.len() != 1 {
                return Err(format!(
                    "primary-output wrapper {} has {} inputs, expected one",
                    anchor.index(),
                    children.len()
                ));
            }
            let child = self.materialize_anchor(children[0])?;
            let nid = self.result.graph.add_node(weight);
            self.result.graph.add_edge(nid, child, ());
            self.result.roots.push(nid);
            self.physical_ids
                .insert(nid, PhysicalOccurrenceId::Original(anchor.index()));
            nid
        } else {
            let expr = self
                .selection
                .choices
                .get(&anchor)
                .cloned()
                .unwrap_or_else(|| self.original_expr(anchor));
            self.materialize_expr(anchor, &expr, &mut Vec::new())?
        };

        self.active.remove(&anchor);
        self.memo.insert(anchor, materialized);
        Ok(materialized)
    }
}

/// Materialize only the region reachable from primary outputs.  This enforces
/// root reachability, recursive child closure, no implicit anchor merge, cycle
/// rejection, and dead-region removal by construction.
pub fn physicalize_topology(
    source: &Netlist<StdCellType, ()>,
    selection: &TopologySelection,
) -> Result<PhysicalizedTopology, String> {
    toposort(&source.graph, None)
        .map_err(|cycle| format!("source netlist contains a cycle: {cycle:?}"))?;
    let mut materializer = Materializer {
        source,
        selection,
        result: Netlist::default(),
        memo: FxHashMap::default(),
        active: FxHashSet::default(),
        physical_ids: FxHashMap::default(),
        created: Vec::new(),
    };
    for root in &source.roots {
        materializer.materialize_anchor(*root)?;
    }
    toposort(&materializer.result.graph, None)
        .map_err(|cycle| format!("physicalized topology contains a cycle: {cycle:?}"))?;

    let materialized_originals: FxHashSet<_> = materializer.memo.keys().copied().collect();
    let mut deleted_originals: Vec<_> = source
        .graph
        .node_indices()
        .filter(|anchor| !materialized_originals.contains(anchor))
        .collect();
    deleted_originals.sort_by_key(|nid| nid.index());
    materializer.created.sort();

    Ok(PhysicalizedTopology {
        netlist: materializer.result,
        physical_ids: materializer.physical_ids,
        original_to_materialized: materializer.memo,
        created: materializer.created,
        deleted_originals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::liberty::{get_direction_of_pins, read_liberty};
    use crate::io::stdcell::{
        read_verilog_with_lib_to_netlist, write_verilog_from_netlist_with_lib,
    };
    use crate::language::StdCellType;
    use crate::netlist_to_egg_roots_with_provenance;
    use crate::physical_scale_egraph::build_occurrence_preserving_original_space;
    use egraph_serialize::ClassId;
    use extraction_gym::ExtendedEGraph;
    use serde_json::Value;

    fn symbol(name: &str) -> StdCellType {
        StdCellType::from_op(name)
    }

    fn add_node(netlist: &mut Netlist<StdCellType, ()>, op: &str) -> NodeIndex {
        netlist.graph.add_node(symbol(op))
    }

    fn connect(netlist: &mut Netlist<StdCellType, ()>, parent: NodeIndex, children: &[NodeIndex]) {
        for child in children {
            netlist.graph.add_edge(parent, *child, ());
        }
    }

    fn two_input_single_gate(op: &str) -> (Netlist<StdCellType, ()>, NodeIndex, [NodeIndex; 2]) {
        let mut netlist = Netlist::default();
        let a = add_node(&mut netlist, "a");
        let b = add_node(&mut netlist, "b");
        netlist.leaves.extend([a, b]);
        let gate = add_node(&mut netlist, op);
        connect(&mut netlist, gate, &[a, b]);
        let output = add_node(&mut netlist, "y");
        connect(&mut netlist, output, &[gate]);
        netlist.roots.push(output);
        (netlist, gate, [a, b])
    }

    fn evaluate(
        netlist: &Netlist<StdCellType, ()>,
    ) -> (
        ExtendedEGraph,
        extraction_gym::extract::ExtractionResult,
        [f64; 3],
    ) {
        let provenance = netlist_to_egg_roots_with_provenance::<_, ()>(netlist).unwrap();
        let space = build_occurrence_preserving_original_space(netlist, &provenance).unwrap();
        let json = serde_json::to_value(&space.egraph).unwrap();
        let ext = ExtendedEGraph::from_base_to_extention(
            space.egraph,
            &Value::String("test/asap7sc6t_SELECT_LVT_TT_nldm.lib".into()),
            &json,
        );
        let cost = space.original_extraction.dag_cost_nldm_v2_on_pruned(&ext);
        (
            ext,
            space.original_extraction,
            [
                cost.components[0].into_inner(),
                cost.components[1].into_inner(),
                cost.components[2].into_inner(),
            ],
        )
    }

    #[test]
    fn topology_t0_case_a_one_gate_to_two_gates() {
        let (source, gate, [a, b]) = two_input_single_gate("NAND2x1_ASAP7_6t_L");
        let mut selection = TopologySelection::default();
        selection.choose(
            gate,
            TopologyExpr::cell(
                "INVx1_ASAP7_6t_L",
                vec![TopologyExpr::cell(
                    "AND2x2_ASAP7_6t_L",
                    vec![TopologyExpr::Anchor(a), TopologyExpr::Anchor(b)],
                )],
            ),
        );
        let physical = physicalize_topology(&source, &selection).unwrap();
        assert_eq!(physical.created.len(), 1);
        assert_eq!(
            physical.netlist.graph.node_count(),
            source.graph.node_count() + 1
        );
        assert_eq!(physical.netlist.roots.len(), 1);
        assert!(toposort(&physical.netlist.graph, None).is_ok());
    }

    #[test]
    fn topology_t0_case_b_two_gates_to_one_removes_dead_gate() {
        let (mut source, and_gate, [a, b]) = two_input_single_gate("AND2x2_ASAP7_6t_L");
        let output = source.roots[0];
        source
            .graph
            .remove_edge(source.graph.find_edge(output, and_gate).unwrap());
        let inv = add_node(&mut source, "INVx1_ASAP7_6t_L");
        connect(&mut source, inv, &[and_gate]);
        connect(&mut source, output, &[inv]);
        let mut selection = TopologySelection::default();
        selection.choose(
            inv,
            TopologyExpr::cell(
                "NAND2x1_ASAP7_6t_L",
                vec![TopologyExpr::Anchor(a), TopologyExpr::Anchor(b)],
            ),
        );
        let physical = physicalize_topology(&source, &selection).unwrap();
        assert!(physical.deleted_originals.contains(&and_gate));
        assert_eq!(
            physical.netlist.graph.node_count() + 1,
            source.graph.node_count()
        );
    }

    #[test]
    fn topology_t0_case_c_equal_logic_anchors_never_merge() {
        let mut source = Netlist::default();
        let a = add_node(&mut source, "a");
        let b = add_node(&mut source, "b");
        source.leaves.extend([a, b]);
        let g0 = add_node(&mut source, "NAND2x1_ASAP7_6t_L");
        let g1 = add_node(&mut source, "NAND2x1_ASAP7_6t_L");
        connect(&mut source, g0, &[a, b]);
        connect(&mut source, g1, &[a, b]);
        for (name, gate) in [("y0", g0), ("y1", g1)] {
            let output = add_node(&mut source, name);
            connect(&mut source, output, &[gate]);
            source.roots.push(output);
        }
        let physical = physicalize_topology(&source, &TopologySelection::default()).unwrap();
        assert_ne!(
            physical.original_to_materialized[&g0],
            physical.original_to_materialized[&g1]
        );
        let nand_count = physical
            .netlist
            .graph
            .node_weights()
            .filter(|cell| cell.to_string() == "NAND2x1_ASAP7_6t_L")
            .count();
        assert_eq!(nand_count, 2);
    }

    #[test]
    fn topology_t0_case_d_shared_fanout_materializes_driver_once() {
        let (mut source, driver, _) = two_input_single_gate("NAND2x1_ASAP7_6t_L");
        let old_output = source.roots.pop().unwrap();
        source.graph.remove_node(old_output);
        for name in ["y0", "y1"] {
            let consumer = add_node(&mut source, "INVx1_ASAP7_6t_L");
            connect(&mut source, consumer, &[driver]);
            let output = add_node(&mut source, name);
            connect(&mut source, output, &[consumer]);
            source.roots.push(output);
        }
        let physical = physicalize_topology(&source, &TopologySelection::default()).unwrap();
        let materialized_driver = physical.original_to_materialized[&driver];
        assert_eq!(physical.netlist.outputs(materialized_driver).count(), 2);
        let nand_count = physical
            .netlist
            .graph
            .node_weights()
            .filter(|cell| cell.to_string() == "NAND2x1_ASAP7_6t_L")
            .count();
        assert_eq!(nand_count, 1);

        let (ext, extraction, _) = evaluate(&physical.netlist);
        let trace = extraction.evaluate_nldm_v2_with_trace(
            &ext,
            None,
            extraction_gym::extract::NldmV2Config::default(),
        );
        let driver_class = ClassId::from(format!("physical.{}", materialized_driver.index()));
        let actual_load = trace.timing[&driver_class].load;
        let mut expected_load = 0.0;
        for consumer in physical.netlist.outputs(materialized_driver) {
            let consumer_op = physical.netlist.graph[consumer].to_string();
            let nldm = &ext.cell_nldm[&consumer_op];
            let pin_index = physical
                .netlist
                .inputs(consumer)
                .position(|child| child == materialized_driver)
                .unwrap();
            let pin = &nldm.pin_order[pin_index + 1];
            expected_load += nldm.pin_info[pin].0.into_inner();
        }
        assert!((actual_load - expected_load).abs() < 1e-12);
    }

    #[test]
    fn topology_t0_case_e_cycle_is_rejected() {
        let (source, gate, _) = two_input_single_gate("NAND2x1_ASAP7_6t_L");
        let mut selection = TopologySelection::default();
        selection.choose(
            gate,
            TopologyExpr::cell("INVx1_ASAP7_6t_L", vec![TopologyExpr::Anchor(gate)]),
        );
        let error = physicalize_topology(&source, &selection).unwrap_err();
        assert!(error.contains("dependency cycle"));
    }

    #[test]
    fn topology_t0_boolean_equivalence_nand_and_inv_and() {
        for a in [false, true] {
            for b in [false, true] {
                assert_eq!(!(a && b), !(a && b));
            }
        }
    }

    #[test]
    fn topology_t0_base_recovery_and_round_trip_v2_are_exact() {
        let (source, gate, [a, b]) = two_input_single_gate("NAND2x1_ASAP7_6t_L");
        let base = physicalize_topology(&source, &TopologySelection::default()).unwrap();
        assert_eq!(base.netlist.graph.node_count(), source.graph.node_count());
        assert!(base.deleted_originals.is_empty());
        assert!(base.created.is_empty());
        let (_, _, source_ppa) = evaluate(&source);
        let (_, _, base_ppa) = evaluate(&base.netlist);
        for index in 0..3 {
            assert!((source_ppa[index] - base_ppa[index]).abs() < 1e-12);
        }

        let mut selection = TopologySelection::default();
        selection.choose(
            gate,
            TopologyExpr::cell(
                "INVx1_ASAP7_6t_L",
                vec![TopologyExpr::cell(
                    "AND2x2_ASAP7_6t_L",
                    vec![TopologyExpr::Anchor(a), TopologyExpr::Anchor(b)],
                )],
            ),
        );
        let changed = physicalize_topology(&source, &selection).unwrap();
        let (_, _, before_write) = evaluate(&changed.netlist);
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let pins = get_direction_of_pins(&liberty).unwrap();
        let output = std::env::temp_dir().join(format!(
            "egg_topology_t0_roundtrip_{}.v",
            std::process::id()
        ));
        write_verilog_from_netlist_with_lib(&output, changed.netlist, "topology_t0", pins.clone())
            .unwrap();
        let (reparsed, module_name) = read_verilog_with_lib_to_netlist(&output, pins).unwrap();
        assert_eq!(module_name, "topology_t0");
        let (_, _, after_reparse) = evaluate(&reparsed);
        for index in 0..3 {
            assert!((before_write[index] - after_reparse[index]).abs() < 1e-12);
        }
        std::fs::remove_file(output).unwrap();
    }
}
