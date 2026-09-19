//! Occurrence-preserving physical scale space for topology-preserving V0 only.
//!
//! Every original mapped-netlist instance becomes a distinct serialized
//! e-class.  Rewriting is used only to discover drive-strength alternatives;
//! the logical rewrite e-classes are never used as physical instances.
//!
//! This is intentionally *not* a general per-use extraction solution and must
//! not be used with comm/inv/dmg/expand or other topology-changing rewrites.

use crate::language::{StdCellLanguage, StdCellType};
use crate::netlist::Netlist;
use crate::{NetlistEggProvenance, SerializedEGraph};
use egg::{EGraph, Language};
use egraph_serialize::{ClassId, Cost, Node, NodeId};
use extraction_gym::extract::ExtractionResult;
use petgraph::graph::NodeIndex;
use rustc_hash::FxHashMap;

#[derive(Clone)]
pub struct OccurrencePreservingScaleSpace {
    pub egraph: SerializedEGraph,
    pub original_extraction: ExtractionResult,
    pub netlist_node_to_class: FxHashMap<NodeIndex, ClassId>,
}

fn class_id(nid: NodeIndex) -> ClassId {
    ClassId::from(format!("physical.{}", nid.index()))
}

fn node_id(nid: NodeIndex, alternative: usize) -> NodeId {
    NodeId::from(format!("physical.{}.{}", nid.index(), alternative))
}

fn physical_children(netlist: &Netlist<StdCellType, ()>, nid: NodeIndex) -> Vec<NodeId> {
    netlist.inputs(nid).map(|child| node_id(child, 0)).collect()
}

fn original_language_node(
    provenance: &NetlistEggProvenance<StdCellLanguage, ()>,
    nid: NodeIndex,
) -> Result<StdCellLanguage, String> {
    let egg_id = *provenance
        .netlist_node_to_egg
        .get(&nid)
        .ok_or_else(|| format!("netlist occurrence {:?} has no egg provenance", nid))?;
    provenance.roots.egraph[egg_id]
        .nodes
        .first()
        .cloned()
        .ok_or_else(|| format!("original egg class {egg_id} is empty"))
}

fn strip_drive_strength(op: &str) -> String {
    let bytes = op.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b'x' && bytes[i + 1].is_ascii_digit() {
            let mut end = i + 1;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            return format!("{}{}", &op[..i], &op[end..]);
        }
    }
    op.to_owned()
}

fn is_topology_preserving_scale_alternative(
    original: &StdCellLanguage,
    candidate: &StdCellLanguage,
) -> bool {
    match (original, candidate) {
        (
            StdCellLanguage::Gate(original_op, original_children),
            StdCellLanguage::Gate(candidate_op, candidate_children),
        ) => {
            original_children.as_ref() == candidate_children.as_ref()
                && strip_drive_strength(original_op.as_str())
                    == strip_drive_strength(candidate_op.as_str())
        }
        _ => original == candidate,
    }
}

/// Construct a direct occurrence-only representation of the mapped netlist.
/// It is an independent baseline containing exactly one node per physical
/// occurrence and no rewrite alternatives.
pub fn build_occurrence_preserving_original_space(
    netlist: &Netlist<StdCellType, ()>,
    provenance: &NetlistEggProvenance<StdCellLanguage, ()>,
) -> Result<OccurrencePreservingScaleSpace, String> {
    let mut egraph = SerializedEGraph::default();
    let mut extraction = ExtractionResult::default();
    let mut netlist_node_to_class = FxHashMap::default();

    for nid in netlist.graph.node_indices() {
        let cid = class_id(nid);
        let selected = node_id(nid, 0);
        let original = original_language_node(provenance, nid)?;
        egraph.add_node(
            selected.clone(),
            Node {
                op: original.to_string(),
                children: physical_children(netlist, nid),
                eclass: cid.clone(),
                cost: Cost::new(1.0).unwrap(),
            },
        );
        extraction.choose(cid.clone(), selected);
        netlist_node_to_class.insert(nid, cid);
    }
    egraph.root_eclasses = netlist.roots.iter().map(|nid| class_id(*nid)).collect();

    Ok(OccurrencePreservingScaleSpace {
        egraph,
        original_extraction: extraction,
        netlist_node_to_class,
    })
}

/// Construct the same original-only occurrence space directly from the
/// mapped netlist.  The original-only representation does not need logical
/// egg provenance: every physical occurrence already carries its exact
/// standard-cell operation and ordered fanins.  This avoids constructing a
/// temporary logical e-graph when a caller only needs the incumbent physical
/// state.
pub fn build_occurrence_preserving_original_space_direct(
    netlist: &Netlist<StdCellType, ()>,
) -> OccurrencePreservingScaleSpace {
    let mut egraph = SerializedEGraph::default();
    let mut extraction = ExtractionResult::default();
    let mut netlist_node_to_class = FxHashMap::default();

    for nid in netlist.graph.node_indices() {
        let cid = class_id(nid);
        let selected = node_id(nid, 0);
        egraph.add_node(
            selected.clone(),
            Node {
                op: netlist.graph[nid].to_string(),
                children: physical_children(netlist, nid),
                eclass: cid.clone(),
                cost: Cost::new(1.0).unwrap(),
            },
        );
        extraction.choose(cid.clone(), selected);
        netlist_node_to_class.insert(nid, cid);
    }
    egraph.root_eclasses = netlist.roots.iter().map(|nid| class_id(*nid)).collect();

    OccurrencePreservingScaleSpace {
        egraph,
        original_extraction: extraction,
        netlist_node_to_class,
    }
}

/// Physicalize a topology-preserving scale-only rewritten logical e-graph.
///
/// Each original occurrence gets one unique ClassId.  Its e-nodes are cloned
/// from the occurrence's rewritten canonical class only when operation family,
/// arity, canonical child order, and pin order are unchanged.  Serialized
/// children point back to original physical child occurrences.
pub fn build_occurrence_preserving_scale_space(
    netlist: &Netlist<StdCellType, ()>,
    provenance: &NetlistEggProvenance<StdCellLanguage, ()>,
    rewritten: &mut EGraph<StdCellLanguage, ()>,
) -> Result<OccurrencePreservingScaleSpace, String> {
    let mut egraph = SerializedEGraph::default();
    let mut extraction = ExtractionResult::default();
    let mut netlist_node_to_class = FxHashMap::default();

    for nid in netlist.graph.node_indices() {
        let original_egg_id = *provenance
            .netlist_node_to_egg
            .get(&nid)
            .ok_or_else(|| format!("netlist occurrence {:?} has no egg provenance", nid))?;
        let original = original_language_node(provenance, nid)?;
        let mut canonical_original = original.clone();
        for child in canonical_original.children_mut() {
            *child = rewritten.find(*child);
        }
        let canonical_class = rewritten.find(original_egg_id);
        let logical_candidates = rewritten[canonical_class].nodes.clone();
        let mut candidates: Vec<StdCellLanguage> = logical_candidates
            .into_iter()
            .filter(|candidate| {
                is_topology_preserving_scale_alternative(&canonical_original, candidate)
            })
            .collect();
        candidates.sort_by_key(|candidate| format!("{}|{:?}", candidate, candidate));
        candidates.dedup();
        if candidates.is_empty() {
            return Err(format!(
                "physical occurrence {:?} has no topology-preserving scale alternative in canonical class {}",
                nid, canonical_class
            ));
        }

        let original_index = candidates
            .iter()
            .position(|candidate| candidate == &canonical_original)
            .ok_or_else(|| {
                format!(
                    "original node {:?} for physical occurrence {:?} is absent from canonical class {}",
                    canonical_original, nid, canonical_class
                )
            })?;
        // Keep the provenance choice at alternative 0 so independent search
        // runners can reconstruct BASE-O deterministically.
        candidates.swap(0, original_index);
        let cid = class_id(nid);
        let children = physical_children(netlist, nid);
        for (alternative, candidate) in candidates.into_iter().enumerate() {
            egraph.add_node(
                node_id(nid, alternative),
                Node {
                    op: candidate.to_string(),
                    children: children.clone(),
                    eclass: cid.clone(),
                    cost: Cost::new(1.0).unwrap(),
                },
            );
        }
        extraction.choose(cid.clone(), node_id(nid, 0));
        netlist_node_to_class.insert(nid, cid);
    }
    egraph.root_eclasses = netlist.roots.iter().map(|nid| class_id(*nid)).collect();

    Ok(OccurrencePreservingScaleSpace {
        egraph,
        original_extraction: extraction,
        netlist_node_to_class,
    })
}
