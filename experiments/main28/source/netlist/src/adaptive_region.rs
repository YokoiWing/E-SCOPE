//! Rule-agnostic adaptive-region extraction state.
//!
//! Rewrite rules populate equality; this module only sees occurrence-anchored
//! e-classes and their complete selected-enode realizations.  Rule names and
//! rule-family provenance are deliberately absent from every scoring,
//! legality, closure, and assignment API below.

use crate::language::{LanguageType, StdCellType};
use crate::netlist::Netlist;
use crate::physical_topology_egraph::{
    PhysicalizedTopology, TopologyExpr, TopologySelection, physicalize_topology,
};
use crate::structural_realization::{
    DedupStats, RuleFamily, StructuralRealization, enumerate_structural_realizations_unattributed,
    enumerate_structural_realizations_without_drive_expansion, expr_hash, expression_text,
};
use petgraph::graph::NodeIndex;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryClosure {
    FixedBoundary,
    AutoClosure,
}

#[derive(Clone, Debug, Serialize)]
pub struct RegionEnode {
    pub eclass_id: String,
    pub enode_id: String,
    pub root_anchor: usize,
    pub structural_hash: String,
    pub expression_text: String,
    pub boundary_eclasses: Vec<String>,
    pub boundary_anchors: Vec<usize>,
    pub created_logical_nodes: usize,
    pub logic_depth: usize,
    pub incumbent: bool,
    #[serde(skip)]
    pub expression: TopologyExpr,
}

#[derive(Clone, Debug, Serialize)]
pub struct RegionEclass {
    pub eclass_id: String,
    pub root_anchor: usize,
    pub incumbent_enode: String,
    pub enodes: Vec<RegionEnode>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AdaptiveRegionSpace {
    pub eclasses: Vec<RegionEclass>,
    pub dependency_edges: Vec<[String; 2]>,
    pub rule_provenance_affects_extraction: bool,
    #[serde(skip)]
    by_anchor: FxHashMap<usize, usize>,
    #[serde(skip)]
    enode_lookup: FxHashMap<String, (usize, usize)>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RegionChoice {
    pub eclass_id: String,
    pub enode_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct RegionAssignment {
    pub choices: Vec<RegionChoice>,
    pub changed_eclasses: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct RegionEnumeration {
    pub requested_eclasses: Vec<String>,
    pub effective_eclasses: Vec<String>,
    pub closure: BoundaryClosure,
    pub theoretical_combinations: usize,
    pub legal_assignments: Vec<RegionAssignment>,
    pub invalid_assignments: usize,
    pub truncated: bool,
    /// Set only by the label-blind census API when it has proved that the
    /// number of legal assignments exceeds its requested cap.
    pub legal_assignment_limit_reached: bool,
}

fn eclass_id(anchor: usize) -> String {
    format!("occurrence:{anchor}")
}

fn incumbent_expr(source: &Netlist<StdCellType, ()>, anchor: NodeIndex) -> TopologyExpr {
    TopologyExpr::cell(
        source.graph[anchor].to_string(),
        source.inputs(anchor).map(TopologyExpr::Anchor).collect(),
    )
}

fn reachable_cell_anchors(source: &Netlist<StdCellType, ()>) -> FxHashSet<usize> {
    let mut reachable = FxHashSet::default();
    let mut pending = source.roots.clone();
    while let Some(anchor) = pending.pop() {
        if !reachable.insert(anchor.index()) {
            continue;
        }
        pending.extend(source.inputs(anchor));
    }
    reachable.retain(|index| {
        let anchor = NodeIndex::new(*index);
        !source.leaves.contains(&anchor)
            && !source.roots.contains(&anchor)
            && !source.graph[anchor].is_constant()
    });
    reachable
}

impl AdaptiveRegionSpace {
    fn rebuild_indices(&mut self) {
        self.by_anchor.clear();
        self.enode_lookup.clear();
        for (class_index, class) in self.eclasses.iter().enumerate() {
            self.by_anchor.insert(class.root_anchor, class_index);
            for (enode_index, enode) in class.enodes.iter().enumerate() {
                self.enode_lookup
                    .insert(enode.enode_id.clone(), (class_index, enode_index));
            }
        }
    }

    pub fn from_realizations(
        source: &Netlist<StdCellType, ()>,
        realizations: Vec<StructuralRealization>,
    ) -> Result<Self, String> {
        Self::from_realizations_with_scale_aliases(source, realizations, false)
    }

    /// Build an occurrence space while optionally retaining one-cell
    /// drive-strength alternatives.
    ///
    /// The topology-first production path continues to call
    /// `from_realizations`, which excludes sizing-only aliases.  Explicit
    /// pin/drive source experiments need the stronger contract because a
    /// consumer-absorbing multi-root macro can contain both structural roots
    /// and deliberately selected output-drive roots.  Keeping those roots is
    /// opt-in at the caller and does not change ordinary D1/V8 trajectories.
    pub fn from_realizations_with_scale_aliases(
        source: &Netlist<StdCellType, ()>,
        realizations: Vec<StructuralRealization>,
        retain_scale_only_aliases: bool,
    ) -> Result<Self, String> {
        let reachable = reachable_cell_anchors(source);
        let mut alternatives: FxHashMap<usize, Vec<StructuralRealization>> = FxHashMap::default();
        for realization in realizations {
            if reachable.contains(&realization.root_anchor)
                && (retain_scale_only_aliases || !realization.is_scale_only_alias(source))
            {
                alternatives
                    .entry(realization.root_anchor)
                    .or_default()
                    .push(realization);
            }
        }

        let mut anchors: Vec<_> = reachable.into_iter().collect();
        anchors.sort_unstable();
        let mut eclasses = Vec::with_capacity(anchors.len());
        for anchor_index in anchors {
            let anchor = NodeIndex::new(anchor_index);
            let incumbent_expression = incumbent_expr(source, anchor);
            let incumbent_hash =
                format!("root={anchor_index}|{}", expr_hash(&incumbent_expression));
            let incumbent_enode = format!("{incumbent_hash}|incumbent");
            let mut boundary_anchors: Vec<_> =
                source.inputs(anchor).map(|item| item.index()).collect();
            boundary_anchors.sort_unstable();
            boundary_anchors.dedup();
            let mut enodes = vec![RegionEnode {
                eclass_id: eclass_id(anchor_index),
                enode_id: incumbent_enode.clone(),
                root_anchor: anchor_index,
                structural_hash: incumbent_hash,
                expression_text: expression_text(&incumbent_expression),
                boundary_eclasses: boundary_anchors
                    .iter()
                    .map(|item| eclass_id(*item))
                    .collect(),
                boundary_anchors,
                created_logical_nodes: 1,
                logic_depth: 1,
                incumbent: true,
                expression: incumbent_expression,
            }];
            let mut seen = FxHashSet::default();
            if let Some(mut items) = alternatives.remove(&anchor_index) {
                items.sort_by_key(|item| item.structural_hash.clone());
                for item in items {
                    if !seen.insert(item.structural_hash.clone()) {
                        continue;
                    }
                    enodes.push(RegionEnode {
                        eclass_id: eclass_id(anchor_index),
                        enode_id: item.structural_hash.clone(),
                        root_anchor: anchor_index,
                        structural_hash: item.structural_hash,
                        expression_text: item.expression_text,
                        boundary_eclasses: item
                            .boundary_anchors
                            .iter()
                            .map(|value| eclass_id(*value))
                            .collect(),
                        boundary_anchors: item.boundary_anchors,
                        created_logical_nodes: item.created_logical_nodes,
                        logic_depth: item.logic_depth,
                        incumbent: false,
                        expression: item.expression,
                    });
                }
            }
            eclasses.push(RegionEclass {
                eclass_id: eclass_id(anchor_index),
                root_anchor: anchor_index,
                incumbent_enode,
                enodes,
            });
        }

        let anchors_with_choices: FxHashSet<_> = eclasses
            .iter()
            .filter(|class| class.enodes.len() > 1)
            .map(|class| class.root_anchor)
            .collect();
        let mut edges = FxHashSet::default();
        for class in &eclasses {
            if !anchors_with_choices.contains(&class.root_anchor) {
                continue;
            }
            for enode in &class.enodes {
                for boundary in &enode.boundary_anchors {
                    if *boundary != class.root_anchor && anchors_with_choices.contains(boundary) {
                        let mut pair = [class.root_anchor, *boundary];
                        pair.sort_unstable();
                        edges.insert(pair);
                    }
                }
            }
        }
        let mut dependency_edges: Vec<_> = edges
            .into_iter()
            .map(|pair| [eclass_id(pair[0]), eclass_id(pair[1])])
            .collect();
        dependency_edges.sort();
        let mut result = Self {
            eclasses,
            dependency_edges,
            rule_provenance_affects_extraction: false,
            by_anchor: FxHashMap::default(),
            enode_lookup: FxHashMap::default(),
        };
        result.rebuild_indices();
        Ok(result)
    }

    pub fn active_eclasses(&self) -> impl Iterator<Item = &RegionEclass> {
        self.eclasses.iter().filter(|class| class.enodes.len() > 1)
    }

    pub fn class_by_anchor(&self, anchor: usize) -> Option<&RegionEclass> {
        self.by_anchor
            .get(&anchor)
            .map(|index| &self.eclasses[*index])
    }

    pub fn enode(&self, enode_id: &str) -> Option<&RegionEnode> {
        self.enode_lookup
            .get(enode_id)
            .map(|(class_index, enode_index)| &self.eclasses[*class_index].enodes[*enode_index])
    }

    pub fn assignment_selection(
        &self,
        assignment: &RegionAssignment,
    ) -> Result<TopologySelection, String> {
        let mut selection = TopologySelection::default();
        let mut seen_classes = FxHashSet::default();
        for choice in &assignment.choices {
            if !seen_classes.insert(choice.eclass_id.clone()) {
                return Err(format!("duplicate choice for {}", choice.eclass_id));
            }
            let enode = self
                .enode(&choice.enode_id)
                .ok_or_else(|| format!("unknown enode {}", choice.enode_id))?;
            if enode.eclass_id != choice.eclass_id {
                return Err(format!(
                    "enode {} belongs to {}, not {}",
                    choice.enode_id, enode.eclass_id, choice.eclass_id
                ));
            }
            if !enode.incumbent {
                selection.choose(NodeIndex::new(enode.root_anchor), enode.expression.clone());
            }
        }
        Ok(selection)
    }

    pub fn physicalize_assignment(
        &self,
        source: &Netlist<StdCellType, ()>,
        assignment: &RegionAssignment,
    ) -> Result<PhysicalizedTopology, String> {
        let selection = self.assignment_selection(assignment)?;
        let physical = physicalize_topology(source, &selection)?;
        for choice in &assignment.choices {
            let enode = self.enode(&choice.enode_id).unwrap();
            if !enode.incumbent
                && !physical
                    .original_to_materialized
                    .contains_key(&NodeIndex::new(enode.root_anchor))
            {
                return Err(format!(
                    "selected eclass {} became unreachable under another choice",
                    enode.eclass_id
                ));
            }
        }
        Ok(physical)
    }

    fn effective_region(
        &self,
        requested: &[usize],
        closure: BoundaryClosure,
        maximum_eclasses: usize,
    ) -> Result<Vec<usize>, String> {
        let mut effective: FxHashSet<_> = requested.iter().copied().collect();
        for anchor in requested {
            let class = self
                .class_by_anchor(*anchor)
                .ok_or_else(|| format!("unknown region eclass occurrence:{anchor}"))?;
            if class.enodes.len() <= 1 {
                return Err(format!("occurrence:{anchor} has no alternative enode"));
            }
        }
        if closure == BoundaryClosure::AutoClosure {
            let mut pending: Vec<_> = requested.to_vec();
            while let Some(anchor) = pending.pop() {
                let class = self.class_by_anchor(anchor).unwrap();
                for enode in &class.enodes {
                    for boundary in &enode.boundary_anchors {
                        if self
                            .class_by_anchor(*boundary)
                            .is_some_and(|candidate| candidate.enodes.len() > 1)
                            && effective.insert(*boundary)
                        {
                            if effective.len() > maximum_eclasses {
                                return Err(format!(
                                    "auto-closure exceeds maximum region size {maximum_eclasses}"
                                ));
                            }
                            pending.push(*boundary);
                        }
                    }
                }
            }
        }
        let mut output: Vec<_> = effective.into_iter().collect();
        output.sort_unstable();
        Ok(output)
    }

    pub fn enumerate_legal_region(
        &self,
        source: &Netlist<StdCellType, ()>,
        requested: &[usize],
        closure: BoundaryClosure,
        maximum_eclasses: usize,
        maximum_combinations: usize,
    ) -> Result<RegionEnumeration, String> {
        let effective = self.effective_region(requested, closure, maximum_eclasses)?;
        let classes: Vec<_> = effective
            .iter()
            .map(|anchor| self.class_by_anchor(*anchor).unwrap())
            .collect();
        let theoretical_combinations = classes
            .iter()
            .try_fold(1usize, |product, class| {
                product.checked_mul(class.enodes.len())
            })
            .unwrap_or(usize::MAX);
        let mut assignments = Vec::new();
        let mut invalid = 0usize;
        let mut visited = 0usize;
        let mut stack: Vec<(usize, Vec<RegionChoice>)> = vec![(0, Vec::new())];
        while let Some((index, choices)) = stack.pop() {
            if visited >= maximum_combinations {
                break;
            }
            if index == classes.len() {
                visited += 1;
                let changed = choices
                    .iter()
                    .filter(|choice| !self.enode(&choice.enode_id).unwrap().incumbent)
                    .count();
                if changed == 0 {
                    continue;
                }
                let assignment = RegionAssignment {
                    choices,
                    changed_eclasses: changed,
                };
                match self.physicalize_assignment(source, &assignment) {
                    Ok(_) => assignments.push(assignment),
                    Err(_) => invalid += 1,
                }
                continue;
            }
            let class = classes[index];
            for enode in class.enodes.iter().rev() {
                let mut next = choices.clone();
                next.push(RegionChoice {
                    eclass_id: class.eclass_id.clone(),
                    enode_id: enode.enode_id.clone(),
                });
                stack.push((index + 1, next));
            }
        }
        assignments.sort_by_key(|assignment| {
            (
                assignment.changed_eclasses,
                assignment
                    .choices
                    .iter()
                    .map(|choice| choice.enode_id.clone())
                    .collect::<Vec<_>>(),
            )
        });
        Ok(RegionEnumeration {
            requested_eclasses: requested.iter().map(|anchor| eclass_id(*anchor)).collect(),
            effective_eclasses: effective.iter().map(|anchor| eclass_id(*anchor)).collect(),
            closure,
            theoretical_combinations,
            legal_assignments: assignments,
            invalid_assignments: invalid,
            truncated: theoretical_combinations > maximum_combinations,
            legal_assignment_limit_reached: false,
        })
    }

    /// Enumerate the same bounded assignment sequence as
    /// `enumerate_legal_region`, but defer physical legality to the consumer.
    /// This lets a scorer physicalize each assignment once and reuse that
    /// physical netlist for both legality and cost evaluation.
    pub fn enumerate_region_assignments_unchecked(
        &self,
        requested: &[usize],
        closure: BoundaryClosure,
        maximum_eclasses: usize,
        maximum_combinations: usize,
    ) -> Result<Vec<RegionAssignment>, String> {
        let effective = self.effective_region(requested, closure, maximum_eclasses)?;
        let classes: Vec<_> = effective
            .iter()
            .map(|anchor| self.class_by_anchor(*anchor).unwrap())
            .collect();
        let mut assignments = Vec::new();
        let mut visited = 0usize;
        let mut stack: Vec<(usize, Vec<RegionChoice>)> = vec![(0, Vec::new())];
        while let Some((index, choices)) = stack.pop() {
            if visited >= maximum_combinations {
                break;
            }
            if index == classes.len() {
                visited += 1;
                let changed = choices
                    .iter()
                    .filter(|choice| !self.enode(&choice.enode_id).unwrap().incumbent)
                    .count();
                if changed > 0 {
                    assignments.push(RegionAssignment {
                        choices,
                        changed_eclasses: changed,
                    });
                }
                continue;
            }
            let class = classes[index];
            for enode in class.enodes.iter().rev() {
                let mut next = choices.clone();
                next.push(RegionChoice {
                    eclass_id: class.eclass_id.clone(),
                    enode_id: enode.enode_id.clone(),
                });
                stack.push((index + 1, next));
            }
        }
        assignments.sort_by_key(|assignment| {
            (
                assignment.changed_eclasses,
                assignment
                    .choices
                    .iter()
                    .map(|choice| choice.enode_id.clone())
                    .collect::<Vec<_>>(),
            )
        });
        Ok(assignments)
    }

    /// Legality-only census variant.  It explores complete assignments until
    /// either the region is exhausted or `legal_limit + 1` legal assignments
    /// have been observed.  The latter is sufficient to reject a bounded
    /// support without spending time materializing thousands more choices.
    pub fn enumerate_legal_region_capped(
        &self,
        source: &Netlist<StdCellType, ()>,
        requested: &[usize],
        closure: BoundaryClosure,
        maximum_eclasses: usize,
        legal_limit: usize,
    ) -> Result<RegionEnumeration, String> {
        let effective = self.effective_region(requested, closure, maximum_eclasses)?;
        let classes: Vec<_> = effective
            .iter()
            .map(|anchor| self.class_by_anchor(*anchor).unwrap())
            .collect();
        let theoretical_combinations = classes
            .iter()
            .try_fold(1usize, |product, class| {
                product.checked_mul(class.enodes.len())
            })
            .unwrap_or(usize::MAX);
        let mut assignments = Vec::new();
        let mut invalid = 0usize;
        let mut visited = 0usize;
        let mut stack: Vec<(usize, Vec<RegionChoice>)> = vec![(0, Vec::new())];
        let mut limit_reached = false;
        while let Some((index, choices)) = stack.pop() {
            if index == classes.len() {
                visited += 1;
                if visited > 4096 {
                    limit_reached = true;
                    break;
                }
                let changed = choices
                    .iter()
                    .filter(|choice| !self.enode(&choice.enode_id).unwrap().incumbent)
                    .count();
                if changed == 0 {
                    continue;
                }
                let assignment = RegionAssignment {
                    choices,
                    changed_eclasses: changed,
                };
                match self.physicalize_assignment(source, &assignment) {
                    Ok(_) => {
                        assignments.push(assignment);
                        if assignments.len() > legal_limit {
                            limit_reached = true;
                            break;
                        }
                    }
                    Err(_) => invalid += 1,
                }
                continue;
            }
            let class = classes[index];
            for enode in class.enodes.iter().rev() {
                let mut next = choices.clone();
                next.push(RegionChoice {
                    eclass_id: class.eclass_id.clone(),
                    enode_id: enode.enode_id.clone(),
                });
                stack.push((index + 1, next));
            }
        }
        assignments.sort_by_key(|assignment| {
            (
                assignment.changed_eclasses,
                assignment
                    .choices
                    .iter()
                    .map(|choice| choice.enode_id.clone())
                    .collect::<Vec<_>>(),
            )
        });
        Ok(RegionEnumeration {
            requested_eclasses: requested.iter().map(|anchor| eclass_id(*anchor)).collect(),
            effective_eclasses: effective.iter().map(|anchor| eclass_id(*anchor)).collect(),
            closure,
            theoretical_combinations,
            legal_assignments: assignments,
            invalid_assignments: invalid,
            truncated: limit_reached,
            legal_assignment_limit_reached: limit_reached,
        })
    }
}

pub fn build_rule_agnostic_space(
    source: &Netlist<StdCellType, ()>,
    rule_paths: &[&Path],
    local_cone_depth: usize,
    max_realization_gates: usize,
) -> Result<(AdaptiveRegionSpace, DedupStats), String> {
    let (realizations, stats) = enumerate_structural_realizations_unattributed(
        source,
        rule_paths,
        local_cone_depth,
        max_realization_gates,
    )?;
    Ok((
        AdaptiveRegionSpace::from_realizations(source, realizations)?,
        stats,
    ))
}

/// Build the same structural space without inserting Phase-I drive-family
/// equalities.  Exact scale families are used only to add forward structural
/// matches for the incumbent's concrete drive; sibling drives are never
/// constructed by this projection.
pub fn build_rule_agnostic_space_without_drive_expansion(
    source: &Netlist<StdCellType, ()>,
    rule_paths: &[&Path],
    scale_rule_path: &Path,
    local_cone_depth: usize,
    max_realization_gates: usize,
) -> Result<(AdaptiveRegionSpace, DedupStats), String> {
    let (realizations, stats) = enumerate_structural_realizations_without_drive_expansion(
        source,
        rule_paths,
        scale_rule_path,
        RuleFamily::Mixed,
        local_cone_depth,
        max_realization_gates,
    )?;
    Ok((
        AdaptiveRegionSpace::from_realizations(source, realizations)?,
        stats,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::liberty::{get_direction_of_pins, read_liberty};
    use crate::io::stdcell::read_verilog_with_lib_to_netlist;

    #[test]
    fn unknown_rewrite_name_enters_rule_agnostic_eclass_enode_space() {
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let pins = get_direction_of_pins(&liberty).unwrap();
        let (source, _) =
            read_verilog_with_lib_to_netlist("test/topology_inv_and.v", pins).unwrap();
        let (space, _) = build_rule_agnostic_space(
            &source,
            &[Path::new("test/6t_adaptive_dummy_rules.json")],
            2,
            3,
        )
        .unwrap();
        let candidate = space
            .active_eclasses()
            .flat_map(|class| &class.enodes)
            .find(|enode| enode.expression_text.starts_with("NAND2x1_ASAP7_6t_L("))
            .expect("dummy rewrite did not produce the expected enode");
        assert!(!candidate.incumbent);
        let enumeration = space
            .enumerate_legal_region(
                &source,
                &[candidate.root_anchor],
                BoundaryClosure::FixedBoundary,
                1,
                4096,
            )
            .unwrap();
        assert!(enumeration.legal_assignments.iter().any(|assignment| {
            assignment
                .choices
                .iter()
                .any(|choice| choice.enode_id == candidate.enode_id)
        }));
    }

    #[test]
    fn joint_region_assignment_is_one_enode_per_eclass_and_legal() {
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let pins = get_direction_of_pins(&liberty).unwrap();
        let (source, _) =
            read_verilog_with_lib_to_netlist("test/topology_inv_and.v", pins).unwrap();
        let (space, _) = build_rule_agnostic_space(
            &source,
            &[
                Path::new("test/6t_adaptive_dummy_rules.json"),
                Path::new("test/6t_inv_rules.json"),
            ],
            2,
            3,
        )
        .unwrap();
        let anchors: Vec<_> = space
            .active_eclasses()
            .map(|class| class.root_anchor)
            .take(2)
            .collect();
        assert!(!anchors.is_empty());
        let enumeration = space
            .enumerate_legal_region(
                &source,
                &anchors,
                BoundaryClosure::FixedBoundary,
                anchors.len(),
                4096,
            )
            .unwrap();
        for assignment in &enumeration.legal_assignments {
            let unique: FxHashSet<_> = assignment
                .choices
                .iter()
                .map(|choice| choice.eclass_id.as_str())
                .collect();
            assert_eq!(unique.len(), assignment.choices.len());
            space.physicalize_assignment(&source, assignment).unwrap();
        }
    }

    #[test]
    fn scale_only_realization_is_retained_only_by_explicit_constructor() {
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let pins = get_direction_of_pins(&liberty).unwrap();
        let (source, _) =
            read_verilog_with_lib_to_netlist("test/topology_inv_and.v", pins).unwrap();
        let anchor = source
            .graph
            .node_indices()
            .find(|anchor| source.graph[*anchor].to_string() == "AND2x2_ASAP7_6t_L")
            .unwrap();
        let children: Vec<_> = source.inputs(anchor).map(TopologyExpr::Anchor).collect();
        let expression = TopologyExpr::cell("AND2x4_ASAP7_6t_L", children);
        let realization = StructuralRealization {
            root_anchor: anchor.index(),
            root_eclass: eclass_id(anchor.index()),
            root_enode: "explicit-drive-x4".into(),
            expression: expression.clone(),
            expression_text: expression_text(&expression),
            boundary_anchors: source.inputs(anchor).map(|child| child.index()).collect(),
            created_logical_nodes: 1,
            logic_depth: 1,
            structural_hash: "explicit-drive-x4".into(),
            provenance: vec!["test-explicit-drive".into()],
            first_seen_family: crate::structural_realization::RuleFamily::Mixed,
        };

        let default_space =
            AdaptiveRegionSpace::from_realizations(&source, vec![realization.clone()]).unwrap();
        assert_eq!(
            default_space
                .class_by_anchor(anchor.index())
                .unwrap()
                .enodes
                .len(),
            1
        );

        let explicit_space = AdaptiveRegionSpace::from_realizations_with_scale_aliases(
            &source,
            vec![realization],
            true,
        )
        .unwrap();
        assert_eq!(
            explicit_space
                .class_by_anchor(anchor.index())
                .unwrap()
                .enodes
                .len(),
            2
        );
        assert!(explicit_space.enode("explicit-drive-x4").is_some());
    }
}
