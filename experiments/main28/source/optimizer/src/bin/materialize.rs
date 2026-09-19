//! Materialize provenance-tagged multi-root structural macros.
//!
//! The generator supplies complete per-anchor expressions.  They are lifted
//! into the same occurrence-aware AdaptiveRegionSpace used by D1, then one
//! enode per selected eclass is physicalized with ordinary reachability,
//! closure and cycle checks.  Referencing another selected original anchor is
//! the explicit sharing mechanism for multi-output macros.

use anyhow::{Result, bail};
use d1_series::equivalence_candidate::{UnifiedEnode, UnifiedEquivalenceCandidate};
use d1_series::phase_i::{ImplementationDomain, RewrittenEquivalence, extract_phase_i};
use mac_egg::adaptive_region::{AdaptiveRegionSpace, RegionAssignment, RegionChoice};
use mac_egg::d3_joint::macro_equivalence;
use mac_egg::io::liberty::{Library, get_direction_of_pins, read_liberty};
use mac_egg::io::stdcell::read_verilog_with_lib_to_netlist;
use mac_egg::io::stdcell::write_verilog_from_netlist_with_lib_ref;
use mac_egg::language::StdCellType;
use mac_egg::netlist::Netlist;
use mac_egg::physical_topology_egraph::TopologyExpr;
use mac_egg::structural_realization::{RuleFamily, StructuralRealization};
use petgraph::graph::NodeIndex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

use d1_series::topology_expr::TopologyExpr as JsonExpr;

#[derive(Clone, Debug, Deserialize)]
struct JsonChoice {
    root_anchor: usize,
    expression: JsonExpr,
}

#[derive(Clone, Debug, Deserialize)]
struct JsonProofGroup {
    choices: Vec<JsonChoice>,
    boundary: Vec<usize>,
    outputs: Vec<usize>,
}

#[derive(Clone, Debug, Deserialize)]
struct JsonCandidate {
    candidate_id: String,
    provenance: Vec<String>,
    choices: Vec<JsonChoice>,
    proof_boundary: Vec<usize>,
    proof_outputs: Vec<usize>,
    #[serde(default)]
    proof_groups: Vec<JsonProofGroup>,
}

impl JsonCandidate {
    fn from_equivalence(record: &UnifiedEquivalenceCandidate) -> Result<Self> {
        fn choices(enodes: &[UnifiedEnode]) -> Result<Vec<JsonChoice>> {
            enodes
                .iter()
                .map(|enode| match enode {
                    UnifiedEnode::VirtualGenerated {
                        root_anchor,
                        expression,
                        ..
                    } => Ok(JsonChoice {
                        root_anchor: *root_anchor,
                        expression: expression.clone(),
                    }),
                    UnifiedEnode::Ordinary { .. } => {
                        bail!("referenced enodes require their resident choice space")
                    }
                })
                .collect()
        }
        Ok(Self {
            candidate_id: record.candidate_id.clone(),
            provenance: record.provenance.clone(),
            choices: choices(&record.enodes)?,
            proof_boundary: record.boundary_anchors.clone(),
            proof_outputs: record.proof_outputs.clone().ok_or_else(|| {
                anyhow::anyhow!("complete equivalence record lacks exact proof outputs")
            })?,
            proof_groups: record
                .proof_groups
                .iter()
                .map(|group| {
                    Ok(JsonProofGroup {
                        choices: choices(&group.enodes)?,
                        boundary: group.boundary_anchors.clone(),
                        outputs: group.output_anchors.clone(),
                    })
                })
                .collect::<Result<_>>()?,
        })
    }
}

#[derive(Clone, Debug, Serialize)]
struct TraceRow {
    candidate_id: String,
    provenance: Vec<String>,
    choices: Vec<RegionChoice>,
    output_netlist: String,
    legal: bool,
    local_equivalence: bool,
    error: Option<String>,
    changed_eclasses: usize,
    created_physical_instances: usize,
    deleted_physical_instances: usize,
}

pub(crate) struct MaterializedCandidate {
    pub candidate_id: String,
    pub output_netlist: PathBuf,
    pub netlist: Netlist<StdCellType, ()>,
}

pub(crate) struct MaterializedBatch {
    pub module: String,
    pub candidates: Vec<MaterializedCandidate>,
}

fn parallel_ordered<T: Sync, R: Send>(
    items: &[T],
    jobs: usize,
    work: impl Fn(usize, &T) -> Result<R> + Sync,
) -> Result<Vec<R>> {
    if jobs <= 1 || items.len() <= 1 {
        return items
            .iter()
            .enumerate()
            .map(|(index, item)| work(index, item))
            .collect();
    }
    let next = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 0..jobs.min(items.len()) {
            let sender = sender.clone();
            let work = &work;
            let next = &next;
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    if sender.send((index, work(index, item))).is_err() {
                        break;
                    }
                }
            });
        }
    });
    drop(sender);
    let mut ordered: Vec<Option<Result<R>>> = (0..items.len()).map(|_| None).collect();
    for (index, value) in receiver {
        ordered[index] = Some(value);
    }
    ordered
        .into_iter()
        .enumerate()
        .map(|(index, value)| value.unwrap_or_else(|| panic!("missing result {index}")))
        .collect()
}

fn topology(expr: &JsonExpr) -> TopologyExpr {
    match expr {
        JsonExpr::Anchor { anchor } => TopologyExpr::Anchor(NodeIndex::new(*anchor)),
        JsonExpr::Cell { op, children } => {
            TopologyExpr::cell(op.clone(), children.iter().map(topology).collect())
        }
    }
}

fn expression_text(expr: &JsonExpr) -> String {
    match expr {
        JsonExpr::Anchor { anchor } => format!("@{anchor}"),
        JsonExpr::Cell { op, children } => format!(
            "{}({})",
            op,
            children
                .iter()
                .map(expression_text)
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

fn metrics(expr: &JsonExpr, boundaries: &mut BTreeSet<usize>) -> (usize, usize) {
    match expr {
        JsonExpr::Anchor { anchor } => {
            boundaries.insert(*anchor);
            (0, 0)
        }
        JsonExpr::Cell { children, .. } => {
            let mut gates = 1usize;
            let mut child_depth = 0usize;
            for child in children {
                let (child_gates, depth) = metrics(child, boundaries);
                gates += child_gates;
                child_depth = child_depth.max(depth);
            }
            (gates, child_depth + 1)
        }
    }
}

fn structural_hash(root: usize, text: &str) -> String {
    format!(
        "root={root}|resynth-macro:{:x}",
        Sha256::digest(text.as_bytes())
    )
}

fn materialize_in_memory_inner(
    args: Vec<String>,
    shared_pins: Option<&Library>,
    rewritten: Option<&[UnifiedEquivalenceCandidate]>,
) -> Result<MaterializedBatch> {
    if args.len() != 3 {
        bail!("usage: materialize_structural_macro_plan INPUT PLAN.json OUT_DIR");
    }
    let input = PathBuf::from(&args[0]);
    let plan_path = PathBuf::from(&args[1]);
    let out = PathBuf::from(&args[2]);
    fs::create_dir_all(out.join("candidates"))?;
    let liberty_path = PathBuf::from(
        std::env::var("EGG_LIB_PATH")
            .unwrap_or_else(|_| "test/asap7sc6t_SELECT_LVT_TT_nldm.lib".into()),
    );
    let pins = if let Some(pins) = shared_pins {
        pins.clone()
    } else {
        let liberty = read_liberty(&liberty_path).map_err(anyhow::Error::msg)?;
        get_direction_of_pins(&liberty).map_err(anyhow::Error::msg)?
    };
    let (source, module) =
        read_verilog_with_lib_to_netlist(&input, pins.clone()).map_err(anyhow::Error::msg)?;
    // Compatibility CLI plans are decoded at the producer boundary. The native
    // controller supplies the rewritten equivalence representation directly.
    let plan: Vec<JsonCandidate> = if let Some(rewritten) = rewritten {
        rewritten
            .iter()
            .map(JsonCandidate::from_equivalence)
            .collect::<Result<_>>()?
    } else {
        serde_json::from_slice(&fs::read(&plan_path)?)?
    };

    let mut realization_by_key = BTreeMap::<(usize, String), StructuralRealization>::new();
    for candidate in &plan {
        for choice in &candidate.choices {
            let text = expression_text(&choice.expression);
            let key = (choice.root_anchor, text.clone());
            realization_by_key.entry(key).or_insert_with(|| {
                let mut boundaries = BTreeSet::new();
                let (gates, depth) = metrics(&choice.expression, &mut boundaries);
                let hash = structural_hash(choice.root_anchor, &text);
                StructuralRealization {
                    root_anchor: choice.root_anchor,
                    root_eclass: format!("occurrence:{}", choice.root_anchor),
                    root_enode: hash.clone(),
                    expression: topology(&choice.expression),
                    expression_text: text,
                    boundary_anchors: boundaries.into_iter().collect(),
                    created_logical_nodes: gates,
                    logic_depth: depth,
                    structural_hash: hash,
                    provenance: vec!["multi-output-resynthesis".into()],
                    first_seen_family: RuleFamily::Mixed,
                }
            });
        }
    }
    let retain_scale_only_aliases = std::env::var("EGG_EXPERIMENTAL_KEEP_DRIVE_CHOICES")
        .ok()
        .is_some_and(|value| value != "0");
    let space = AdaptiveRegionSpace::from_realizations_with_scale_aliases(
        &source,
        realization_by_key.into_values().collect(),
        retain_scale_only_aliases,
    )
    .map_err(anyhow::Error::msg)?;

    let jobs = std::env::var("EGG_REALIZATION_JOBS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    let results = parallel_ordered(&plan, jobs, |_, candidate| {
        let choices: Vec<_> = candidate
            .choices
            .iter()
            .map(|choice| {
                let text = expression_text(&choice.expression);
                let proposed = structural_hash(choice.root_anchor, &text);
                // AdaptiveRegionSpace intentionally removes a one-cell
                // drive-only alias.  In a multi-root macro that root remains
                // the incumbent e-node; the other roots still form the
                // structural move and may reference this shared occurrence.
                let enode_id = if space.enode(&proposed).is_some() {
                    proposed
                } else {
                    space
                        .class_by_anchor(choice.root_anchor)
                        .expect("candidate root must be reachable")
                        .incumbent_enode
                        .clone()
                };
                RegionChoice {
                    eclass_id: format!("occurrence:{}", choice.root_anchor),
                    enode_id,
                }
            })
            .collect();
        let changed_eclasses = choices
            .iter()
            .filter(|choice| {
                space
                    .enode(&choice.enode_id)
                    .is_some_and(|enode| !enode.incumbent)
            })
            .count();
        let assignment = RegionAssignment {
            changed_eclasses,
            choices: choices.clone(),
        };
        let output = out
            .join("candidates")
            .join(format!("{}.v", candidate.candidate_id));
        let macro_choices: Vec<_> = candidate
            .choices
            .iter()
            .map(|choice| (choice.root_anchor, topology(&choice.expression)))
            .collect();
        let equivalence = if candidate.proof_groups.is_empty() {
            macro_equivalence(
                &source,
                &macro_choices,
                &candidate.proof_boundary,
                &candidate.proof_outputs,
            )
        } else {
            // A closure candidate may contain many independent cuts.  Prove
            // each original cut independently, but first require the proof
            // manifest to cover every materialized root/expression exactly
            // once.  This keeps the compositional shortcut fail-closed.
            let expected: BTreeSet<_> = candidate
                .choices
                .iter()
                .map(|choice| (choice.root_anchor, expression_text(&choice.expression)))
                .collect();
            let mut covered = BTreeSet::new();
            let mut valid = expected.len() == candidate.choices.len();
            let mut proof_choice_count = 0usize;
            for group in &candidate.proof_groups {
                proof_choice_count += group.choices.len();
                let group_choices: Vec<_> = group
                    .choices
                    .iter()
                    .map(|choice| {
                        covered.insert((choice.root_anchor, expression_text(&choice.expression)));
                        (choice.root_anchor, topology(&choice.expression))
                    })
                    .collect();
                valid &=
                    macro_equivalence(&source, &group_choices, &group.boundary, &group.outputs);
            }
            valid && proof_choice_count == covered.len() && covered == expected
        };
        if !equivalence {
            return Ok((
                TraceRow {
                    candidate_id: candidate.candidate_id.clone(),
                    provenance: candidate.provenance.clone(),
                    choices,
                    output_netlist: output.display().to_string(),
                    legal: false,
                    local_equivalence: false,
                    error: Some("local Boolean equivalence failed".into()),
                    changed_eclasses,
                    created_physical_instances: 0,
                    deleted_physical_instances: 0,
                },
                None,
            ));
        }
        let (mut expanded, _, _) = extract_phase_i(
            &source,
            RewrittenEquivalence {
                space: &space,
                domain: ImplementationDomain::Fixed {
                    assignment: &assignment,
                },
            },
            None,
        )?;
        // One simultaneous proof group produces one complete assignment. Never
        // enumerate the union of all lifted virtual enodes in `space`.
        let extracted = expanded
            .pop()
            .expect("fixed domain yields one implementation");
        match space.physicalize_assignment(&source, &extracted.assignment) {
            Ok(physical) => {
                write_verilog_from_netlist_with_lib_ref(&output, &physical.netlist, &module, &pins)
                    .map_err(anyhow::Error::msg)?;
                let row = TraceRow {
                    candidate_id: candidate.candidate_id.clone(),
                    provenance: candidate.provenance.clone(),
                    choices,
                    output_netlist: output.display().to_string(),
                    legal: true,
                    local_equivalence: true,
                    error: None,
                    changed_eclasses,
                    created_physical_instances: physical.created.len(),
                    deleted_physical_instances: physical.deleted_originals.len(),
                };
                let materialized = MaterializedCandidate {
                    candidate_id: candidate.candidate_id.clone(),
                    output_netlist: output,
                    netlist: physical.netlist,
                };
                Ok((row, Some(materialized)))
            }
            Err(error) => Ok((
                TraceRow {
                    candidate_id: candidate.candidate_id.clone(),
                    provenance: candidate.provenance.clone(),
                    choices,
                    output_netlist: output.display().to_string(),
                    legal: false,
                    local_equivalence: true,
                    error: Some(error),
                    changed_eclasses,
                    created_physical_instances: 0,
                    deleted_physical_instances: 0,
                },
                None,
            )),
        }
    })?;
    let mut rows = Vec::with_capacity(results.len());
    let mut candidates = Vec::with_capacity(results.len());
    for (row, candidate) in results {
        rows.push(row);
        candidates.extend(candidate);
    }
    fs::write(
        out.join("materialization_trace.json"),
        serde_json::to_string_pretty(&rows)? + "\n",
    )?;
    let legal = rows.iter().filter(|row| row.legal).count();
    fs::write(
        out.join("summary.json"),
        serde_json::to_string_pretty(&json!({
            "input":input,
            "plan":plan_path,
            "candidates":rows.len(),
            "legal":legal,
            "representation":"occurrence-aware multi-root eclass/enode macro",
            "phase_i_input":"extended_rewritten_equivalence",
            "phase_i_domain":"fixed_atomic_implementation",
            "phase_i_expanded_implementations":rows.iter().filter(|row| row.local_equivalence).count(),
            "read_equivalence_expressions":rewritten.is_some(),
            "sharing":"explicit Anchor reference to another selected eclass",
            "retain_scale_only_aliases":retain_scale_only_aliases,
        }))? + "\n",
    )?;
    println!("materialized {legal}/{} structural macros", rows.len());
    Ok(MaterializedBatch { module, candidates })
}

pub(crate) fn materialize_in_memory_with_pins(
    args: Vec<String>,
    shared_pins: &Library,
) -> Result<MaterializedBatch> {
    materialize_in_memory_inner(args, Some(shared_pins), None)
}

pub(crate) fn materialize_equivalence_in_memory(
    parent: &Path,
    representation: &[UnifiedEquivalenceCandidate],
    out: &Path,
    shared_pins: &Library,
) -> Result<MaterializedBatch> {
    materialize_in_memory_inner(
        vec![
            parent.display().to_string(),
            "<in-memory-rewritten-equivalence>".into(),
            out.display().to_string(),
        ],
        Some(shared_pins),
        Some(representation),
    )
}

pub(crate) fn materialize_in_memory(args: Vec<String>) -> Result<MaterializedBatch> {
    materialize_in_memory_inner(args, None, None)
}

pub fn run_cli(args: Vec<String>) -> Result<()> {
    materialize_in_memory(args)?;
    Ok(())
}

fn main() -> Result<()> {
    run_cli(std::env::args().skip(1).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use d1_series::equivalence_candidate::{
        EquivalenceProofKind, EquivalenceSource, EquivalenceTarget, UNIFIED_EQUIVALENCE_SCHEMA,
        UnifiedProofGroup,
    };

    #[test]
    fn phase_i_complete_multioutput_is_atomic_and_provenance_blind() {
        let lib = read_liberty("../netlist/test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let pins = get_direction_of_pins(&lib).unwrap();
        let input = "../netlist/test/topology_inv_and.v";
        let (source, _) = read_verilog_with_lib_to_netlist(input, pins.clone()).unwrap();
        let output = source.inputs(source.roots[0]).next().unwrap();
        let host = source.inputs(output).next().unwrap();
        let boundary: Vec<_> = source.inputs(host).map(|a| a.index()).collect();
        let enodes = vec![
            UnifiedEnode::VirtualGenerated {
                root_anchor: host.index(),
                eclass_id: format!("physical.{}", host.index()),
                expression: JsonExpr::Cell {
                    op: "NAND2x1_ASAP7_6t_L".into(),
                    children: boundary
                        .iter()
                        .map(|a| JsonExpr::Anchor { anchor: *a })
                        .collect(),
                },
            },
            UnifiedEnode::VirtualGenerated {
                root_anchor: output.index(),
                eclass_id: format!("physical.{}", output.index()),
                expression: JsonExpr::Anchor {
                    anchor: host.index(),
                },
            },
        ];
        let mut record = UnifiedEquivalenceCandidate {
            schema: UNIFIED_EQUIVALENCE_SCHEMA.into(),
            candidate_id: "atomic".into(),
            source: EquivalenceSource::CompiledContextualRewrite,
            target: EquivalenceTarget::MultiOutputGroup {
                output_anchors: vec![host.index(), output.index()],
                eclass_ids: vec![
                    format!("physical.{}", host.index()),
                    format!("physical.{}", output.index()),
                ],
            },
            enodes: enodes.clone(),
            boundary_anchors: boundary.clone(),
            output_anchors: vec![host.index(), output.index()],
            proof_kind: EquivalenceProofKind::CompositionalBoundaryProof,
            proof_outputs: Some(vec![output.index()]),
            proof_groups: vec![UnifiedProofGroup {
                enodes,
                boundary_anchors: boundary,
                output_anchors: vec![output.index()],
            }],
            provenance: vec!["fast-rewrite".into()],
        };
        let root = std::env::temp_dir().join(format!(
            "egg-phase-i-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let run = |record: &UnifiedEquivalenceCandidate, label: &str| {
            let out = root.join(label);
            let batch = materialize_equivalence_in_memory(
                Path::new(input),
                std::slice::from_ref(record),
                &out,
                &pins,
            )
            .unwrap();
            assert_eq!(batch.candidates.len(), 1);
            let trace: Vec<serde_json::Value> =
                serde_json::from_slice(&fs::read(out.join("materialization_trace.json")).unwrap())
                    .unwrap();
            assert_eq!(trace.len(), 1);
            assert_eq!(trace[0]["choices"].as_array().unwrap().len(), 2);
            assert_eq!(trace[0]["local_equivalence"], true);
            fs::read(&batch.candidates[0].output_netlist).unwrap()
        };
        let original = run(&record, "compiled");
        record.source = EquivalenceSource::PrimitiveEgraphRewrite;
        record.provenance = vec!["ordinary-rewrite".into()];
        assert_eq!(original, run(&record, "relabeled"));
        // Removing half the transaction while retaining its joint proof must
        // fail closed, rather than turn the group into independent choices.
        record.enodes.pop();
        let out = root.join("split");
        let split =
            materialize_equivalence_in_memory(Path::new(input), &[record], &out, &pins).unwrap();
        assert!(split.candidates.is_empty());
    }
}
