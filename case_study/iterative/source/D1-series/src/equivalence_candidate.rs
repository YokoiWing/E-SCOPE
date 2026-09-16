//! Unified, proof-carrying representation for topology candidates.
//!
//! Ordinary D1 candidates reference enodes already present in a bounded local
//! e-graph.  Generator V2 reaches some higher-order/contextual equivalences
//! directly and historically joined D1 only after full-netlist
//! materialization.  This module gives both sources one representation at the
//! topology-extraction boundary without inserting generated expressions into
//! the live `egg::EGraph` or changing candidate generation, identity,
//! ordering, materialization, ranking, sizing, or acceptance.

use crate::generator_v2::{GeneratorCandidate, GeneratorChoice};
use crate::topology_expr::TopologyExpr;
use serde::{Deserialize, Serialize};
use serde_json::{Value, to_value};
use std::collections::BTreeSet;
use std::path::Path;

pub const UNIFIED_EQUIVALENCE_FIELD: &str = "equivalence_candidate";
pub const UNIFIED_EQUIVALENCE_SCHEMA: &str = "egg-unified-equivalence-candidate-v1";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UnifiedEnode {
    /// An enode already created by primitive rewrites in a local e-graph.
    Ordinary { eclass_id: String, enode_id: String },
    /// A higher-order/contextual rewrite endpoint generated directly without
    /// materializing all intermediate primitive-rewrite states.
    VirtualGenerated {
        root_anchor: usize,
        eclass_id: String,
        expression: TopologyExpr,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EquivalenceTarget {
    SingleEclass {
        eclass_id: String,
    },
    EclassTransaction {
        eclass_ids: Vec<String>,
    },
    /// A relational equivalence target.  A classic e-class represents one
    /// value, so a shared multi-output realization must not be mislabeled as
    /// one ordinary e-class.
    MultiOutputGroup {
        output_anchors: Vec<usize>,
        eclass_ids: Vec<String>,
    },
    /// Kept only for explicit full-netlist sidecars/closures which have no
    /// local enode inventory.  Normal D1/Generator candidates do not use it.
    WholeNetlist,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EquivalenceSource {
    PrimitiveEgraphRewrite,
    CompiledContextualRewrite,
    MappedRewriteClosure,
    ExternalSidecar,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EquivalenceProofKind {
    EclassMembership,
    BoundaryTruthTable,
    CompositionalBoundaryProof,
    WholeNetlistProof,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UnifiedProofGroup {
    pub enodes: Vec<UnifiedEnode>,
    pub boundary_anchors: Vec<usize>,
    pub output_anchors: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UnifiedEquivalenceCandidate {
    pub schema: String,
    pub candidate_id: String,
    pub source: EquivalenceSource,
    pub target: EquivalenceTarget,
    pub enodes: Vec<UnifiedEnode>,
    pub boundary_anchors: Vec<usize>,
    pub output_anchors: Vec<usize>,
    pub proof_kind: EquivalenceProofKind,
    /// Exact proof interface, distinct from the union of target/output anchors.
    /// A shared internal host can change function and must not become a proof output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof_outputs: Option<Vec<usize>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proof_groups: Vec<UnifiedProofGroup>,
    pub provenance: Vec<String>,
}

/// The common interface consumed by topology selection and conditional sizing.
/// Both ordinary and generated enodes reach extraction through this view; the
/// already-materialized netlist remains the trajectory-preserving execution
/// form during this representation-only migration.
#[derive(Clone, Copy, Debug)]
pub struct ExtractionCandidateRef<'a> {
    pub candidate_id: &'a str,
    pub implementation: &'a Path,
    pub equivalence: Option<&'a Value>,
}

fn physical_eclass(anchor: usize) -> String {
    format!("physical.{anchor}")
}

fn generated_enode(choice: &GeneratorChoice) -> UnifiedEnode {
    UnifiedEnode::VirtualGenerated {
        root_anchor: choice.root_anchor,
        eclass_id: physical_eclass(choice.root_anchor),
        expression: choice.expression.clone(),
    }
}

fn sorted_unique(values: impl IntoIterator<Item = usize>) -> Vec<usize> {
    values
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

impl UnifiedEquivalenceCandidate {
    pub fn from_generator(candidate: &GeneratorCandidate) -> Self {
        let enodes: Vec<_> = candidate.choices.iter().map(generated_enode).collect();
        let output_anchors = sorted_unique(
            candidate
                .proof_outputs
                .iter()
                .copied()
                .chain(candidate.outputs.iter().copied())
                .chain(candidate.choices.iter().map(|choice| choice.root_anchor)),
        );
        let eclass_ids: Vec<_> = output_anchors
            .iter()
            .map(|anchor| physical_eclass(*anchor))
            .collect();
        let target = if output_anchors.len() == 1 && candidate.choices.len() == 1 {
            EquivalenceTarget::SingleEclass {
                eclass_id: eclass_ids[0].clone(),
            }
        } else {
            EquivalenceTarget::MultiOutputGroup {
                output_anchors: output_anchors.clone(),
                eclass_ids,
            }
        };
        let proof_groups: Vec<_> = candidate
            .proof_groups
            .iter()
            .map(|group| UnifiedProofGroup {
                enodes: group.choices.iter().map(generated_enode).collect(),
                boundary_anchors: group.boundary.clone(),
                output_anchors: group.outputs.clone(),
            })
            .collect();
        Self {
            schema: UNIFIED_EQUIVALENCE_SCHEMA.to_owned(),
            candidate_id: candidate.candidate_id.clone(),
            source: EquivalenceSource::CompiledContextualRewrite,
            target,
            enodes,
            boundary_anchors: candidate.proof_boundary.clone(),
            output_anchors,
            proof_outputs: Some(candidate.proof_outputs.clone()),
            proof_kind: if proof_groups.is_empty() {
                EquivalenceProofKind::BoundaryTruthTable
            } else {
                EquivalenceProofKind::CompositionalBoundaryProof
            },
            proof_groups,
            provenance: candidate.provenance.clone(),
        }
    }

    pub fn from_d1_row(row: &Value) -> Result<Self, String> {
        let candidate_id = row["candidate_id"]
            .as_str()
            .ok_or_else(|| "D1 candidate missing candidate_id".to_owned())?;
        let choices = row["choices"]
            .as_array()
            .ok_or_else(|| format!("D1 candidate {candidate_id} missing choices"))?;
        let mut enodes = Vec::with_capacity(choices.len());
        for choice in choices {
            enodes.push(UnifiedEnode::Ordinary {
                eclass_id: choice["eclass_id"]
                    .as_str()
                    .ok_or_else(|| format!("D1 candidate {candidate_id} choice missing eclass_id"))?
                    .to_owned(),
                enode_id: choice["enode_id"]
                    .as_str()
                    .ok_or_else(|| format!("D1 candidate {candidate_id} choice missing enode_id"))?
                    .to_owned(),
            });
        }
        let eclass_ids: Vec<_> = enodes
            .iter()
            .filter_map(|enode| match enode {
                UnifiedEnode::Ordinary { eclass_id, .. } => Some(eclass_id.clone()),
                UnifiedEnode::VirtualGenerated { .. } => None,
            })
            .collect();
        let target = match eclass_ids.as_slice() {
            [single] => EquivalenceTarget::SingleEclass {
                eclass_id: single.clone(),
            },
            _ => EquivalenceTarget::EclassTransaction { eclass_ids },
        };
        Ok(Self {
            schema: UNIFIED_EQUIVALENCE_SCHEMA.to_owned(),
            candidate_id: candidate_id.to_owned(),
            source: EquivalenceSource::PrimitiveEgraphRewrite,
            target,
            enodes,
            boundary_anchors: Vec::new(),
            output_anchors: Vec::new(),
            proof_kind: EquivalenceProofKind::EclassMembership,
            proof_outputs: None,
            proof_groups: Vec::new(),
            provenance: vec!["d1-rewrite".to_owned()],
        })
    }

    pub fn whole_netlist(
        candidate_id: impl Into<String>,
        source: EquivalenceSource,
        provenance: impl Into<String>,
    ) -> Self {
        Self {
            schema: UNIFIED_EQUIVALENCE_SCHEMA.to_owned(),
            candidate_id: candidate_id.into(),
            source,
            target: EquivalenceTarget::WholeNetlist,
            enodes: Vec::new(),
            boundary_anchors: Vec::new(),
            output_anchors: Vec::new(),
            proof_kind: EquivalenceProofKind::WholeNetlistProof,
            proof_outputs: None,
            proof_groups: Vec::new(),
            provenance: vec![provenance.into()],
        }
    }
}

pub fn unified_equivalence_enabled() -> bool {
    std::env::var("EGG_UNIFIED_EQUIVALENCE_CANDIDATES")
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

pub fn attach_unified_candidate(
    row: &mut Value,
    candidate: &UnifiedEquivalenceCandidate,
) -> Result<(), String> {
    if !unified_equivalence_enabled() {
        return Ok(());
    }
    let row_id = row["candidate_id"]
        .as_str()
        .ok_or_else(|| "union row missing candidate_id".to_owned())?
        .to_owned();
    if row_id != candidate.candidate_id {
        return Err(format!(
            "unified candidate id {} differs from union row {row_id}",
            candidate.candidate_id
        ));
    }
    row.as_object_mut()
        .ok_or_else(|| format!("union row {row_id} is not an object"))?
        .insert(
            UNIFIED_EQUIVALENCE_FIELD.to_owned(),
            to_value(candidate).map_err(|error| error.to_string())?,
        );
    Ok(())
}

fn extraction_candidate_with_mode(
    row: &Value,
    require_unified: bool,
) -> Result<ExtractionCandidateRef<'_>, String> {
    let legacy = row["candidate_id"]
        .as_str()
        .ok_or_else(|| "union row missing candidate_id".to_owned())?;
    let implementation = row["output_netlist"]
        .as_str()
        .ok_or_else(|| format!("union candidate {legacy} missing output_netlist"))?;
    let equivalence = row.get(UNIFIED_EQUIVALENCE_FIELD);
    if require_unified {
        let unified = equivalence
            .and_then(|record| record["candidate_id"].as_str())
            .ok_or_else(|| format!("union candidate {legacy} missing unified representation"))?;
        if legacy != unified {
            return Err(format!(
                "legacy candidate id {legacy} differs from unified id {unified}"
            ));
        }
    }
    Ok(ExtractionCandidateRef {
        candidate_id: legacy,
        implementation: Path::new(implementation),
        equivalence,
    })
}

pub fn extraction_candidate(row: &Value) -> Result<ExtractionCandidateRef<'_>, String> {
    extraction_candidate_with_mode(row, unified_equivalence_enabled())
}

/// Candidate identity consumed at the topology-extraction boundary.  The
/// legacy top-level id remains authoritative when the opt-in representation
/// is disabled.  With it enabled, both ids must exist and agree exactly.
pub fn extraction_candidate_id(row: &Value) -> Result<&str, String> {
    Ok(extraction_candidate(row)?.candidate_id)
}

fn validate_extraction_portfolio_with_mode(
    rows: &[Value],
    require_unified: bool,
) -> Result<(), String> {
    if !require_unified {
        return Ok(());
    }
    let mut seen = BTreeSet::new();
    for row in rows {
        let view = extraction_candidate_with_mode(row, true)?;
        let id = view.candidate_id;
        if !seen.insert(id.to_owned()) {
            return Err(format!("duplicate unified candidate id {id}"));
        }
        let record: UnifiedEquivalenceCandidate = serde_json::from_value(
            view.equivalence
                .expect("unified mode guarantees an equivalence record")
                .clone(),
        )
        .map_err(|error| format!("invalid unified candidate {id}: {error}"))?;
        if record.schema != UNIFIED_EQUIVALENCE_SCHEMA {
            return Err(format!("candidate {id} has unknown equivalence schema"));
        }
        if record.enodes.is_empty() && !matches!(record.target, EquivalenceTarget::WholeNetlist) {
            return Err(format!("candidate {id} has no ordinary or virtual enode"));
        }
    }
    Ok(())
}

pub fn validate_extraction_portfolio(rows: &[Value]) -> Result<(), String> {
    validate_extraction_portfolio_with_mode(rows, unified_equivalence_enabled())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator_v2::{GeneratorChoice, GeneratorProofGroup};
    use petgraph::graph::NodeIndex;
    use serde_json::json;

    fn generator_choice(root_anchor: usize, boundary: usize) -> GeneratorChoice {
        GeneratorChoice {
            root_anchor,
            expression: TopologyExpr::Cell {
                op: "NAND2x1_ASAP7_6t_L".to_owned(),
                children: vec![
                    TopologyExpr::Anchor { anchor: boundary },
                    TopologyExpr::Anchor {
                        anchor: boundary + 1,
                    },
                ],
            },
        }
    }

    fn generator_candidate(choices: Vec<GeneratorChoice>) -> GeneratorCandidate {
        let outputs = choices.iter().map(|choice| choice.root_anchor).collect();
        GeneratorCandidate {
            candidate_id: "GEN_0".to_owned(),
            provenance: vec!["truth-table-resynthesis".to_owned()],
            choices,
            tech_area: 1.0,
            tech_delay: 1.0,
            logic_depth: 1,
            cell_families: vec!["NAND2".to_owned()],
            outputs,
            window_kind: "test".to_owned(),
            proof_boundary: vec![3, 4],
            proof_outputs: Vec::new(),
            proof_groups: Vec::new(),
            shared_host: None,
            divisors: Vec::new(),
            replaced_anchor: None,
            logical_signature: None,
            mapping: None,
            source_windows: Vec::new(),
        }
    }

    #[test]
    fn single_output_generator_becomes_virtual_enode_in_one_eclass() {
        let candidate = generator_candidate(vec![generator_choice(9, 3)]);
        let unified = UnifiedEquivalenceCandidate::from_generator(&candidate);
        assert_eq!(
            unified.target,
            EquivalenceTarget::SingleEclass {
                eclass_id: "physical.9".to_owned()
            }
        );
        assert!(matches!(
            &unified.enodes[0],
            UnifiedEnode::VirtualGenerated { root_anchor: 9, .. }
        ));
        assert_eq!(unified.proof_kind, EquivalenceProofKind::BoundaryTruthTable);
    }

    #[test]
    fn ordinary_and_generator_topology_expressions_share_one_shape() {
        let ordinary = mac_egg::physical_topology_egraph::TopologyExpr::cell(
            "NAND2x1_ASAP7_6t_L",
            vec![
                mac_egg::physical_topology_egraph::TopologyExpr::Anchor(NodeIndex::new(3)),
                mac_egg::physical_topology_egraph::TopologyExpr::Anchor(NodeIndex::new(4)),
            ],
        );
        let generated = generator_choice(9, 3).expression;
        assert_eq!(TopologyExpr::from(&ordinary), generated);
    }

    #[test]
    fn multi_output_generator_retains_relational_proof_group() {
        let left = generator_choice(9, 3);
        let right = generator_choice(10, 5);
        let mut candidate = generator_candidate(vec![left.clone(), right.clone()]);
        candidate.proof_groups = vec![GeneratorProofGroup {
            choices: vec![left, right],
            boundary: vec![3, 4, 5, 6],
            outputs: vec![9, 10],
        }];
        let unified = UnifiedEquivalenceCandidate::from_generator(&candidate);
        assert!(matches!(
            unified.target,
            EquivalenceTarget::MultiOutputGroup { .. }
        ));
        assert_eq!(unified.proof_groups.len(), 1);
        assert_eq!(
            unified.proof_kind,
            EquivalenceProofKind::CompositionalBoundaryProof
        );
    }

    #[test]
    fn d1_choices_become_ordinary_enodes_without_changing_legacy_row() {
        let row = json!({
            "candidate_id":"v3_R2_S0_1_C000",
            "source_class":"d1-rewrite",
            "choices":[
                {"eclass_id":"physical.1","enode_id":"enode-a"},
                {"eclass_id":"physical.2","enode_id":"enode-b"}
            ]
        });
        let unified = UnifiedEquivalenceCandidate::from_d1_row(&row).unwrap();
        assert_eq!(unified.enodes.len(), 2);
        assert!(
            unified
                .enodes
                .iter()
                .all(|enode| matches!(enode, UnifiedEnode::Ordinary { .. }))
        );
        assert!(matches!(
            unified.target,
            EquivalenceTarget::EclassTransaction { .. }
        ));
        assert_eq!(row["candidate_id"], "v3_R2_S0_1_C000");
    }

    #[test]
    fn unified_extraction_view_preserves_legacy_identity_and_implementation() {
        let mut row = json!({
            "candidate_id":"GEN_0",
            "output_netlist":"runs/test/GEN_0.v"
        });
        let candidate = generator_candidate(vec![generator_choice(9, 3)]);
        row.as_object_mut().unwrap().insert(
            UNIFIED_EQUIVALENCE_FIELD.to_owned(),
            to_value(UnifiedEquivalenceCandidate::from_generator(&candidate)).unwrap(),
        );
        validate_extraction_portfolio_with_mode(std::slice::from_ref(&row), true).unwrap();
        let view = extraction_candidate_with_mode(&row, true).unwrap();
        assert_eq!(view.candidate_id, "GEN_0");
        assert_eq!(view.implementation, Path::new("runs/test/GEN_0.v"));
    }

    #[test]
    fn unified_extraction_rejects_identity_divergence() {
        let row = json!({
            "candidate_id":"GEN_0",
            "output_netlist":"runs/test/GEN_0.v",
            "equivalence_candidate":{
                "candidate_id":"GEN_WRONG"
            }
        });
        let error = extraction_candidate_with_mode(&row, true).unwrap_err();
        assert!(error.contains("differs from unified id"));
    }
}
