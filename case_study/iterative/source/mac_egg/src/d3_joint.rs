//! D3 occurrence-aware joint topology+sizing choice construction.
//!
//! This module deliberately stops at legal, complete physical realizations.
//! It does not predict post-sizing basin value and it has no acceptance API.

use crate::adaptive_region::{AdaptiveRegionSpace, RegionEclass, RegionEnode};
use crate::language::StdCellType;
use crate::netlist::Netlist;
use crate::physical_topology_egraph::{
    PhysicalizedTopology, TopologyExpr, TopologySelection, physicalize_topology,
};
use crate::structural_realization::expression_text;
use petgraph::graph::NodeIndex;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;
use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JointKind {
    NoOp,
    TopologyOnly,
    SizingOnly,
    JointTopologySizing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SizingPattern {
    Inherited,
    AllSmall,
    AllLarge,
    AdjointBest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingMode {
    Unpriced,
    AreaPower,
    TimingOnly,
    FullAdjoint,
}

impl PricingMode {
    pub const ALL: [Self; 4] = [
        Self::Unpriced,
        Self::AreaPower,
        Self::TimingOnly,
        Self::FullAdjoint,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Unpriced => "a0_unpriced",
            Self::AreaPower => "a1_area_power",
            Self::TimingOnly => "a2_timing_only",
            Self::FullAdjoint => "a3_full_adjoint",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct JointVariant {
    pub variant_id: String,
    pub eclass_id: String,
    pub root_anchor: usize,
    pub topology_enode_id: String,
    pub topology_signature: String,
    pub sized_signature: String,
    pub pattern: SizingPattern,
    pub kind: JointKind,
    pub topology_changed: bool,
    pub sizing_changed: bool,
    pub physical_cell_count: usize,
    pub local_equivalence_pass: bool,
    #[serde(skip)]
    pub expression: TopologyExpr,
}

#[derive(Clone, Debug, Serialize)]
pub struct JointAssignment {
    pub variants: Vec<JointVariant>,
    pub changed_occurrences: usize,
    pub topology_changed_occurrences: usize,
    pub sizing_changed_occurrences: usize,
    pub kind: JointKind,
    pub additive_price: f64,
    pub joint_price: f64,
}

#[derive(Clone, Debug)]
pub struct JointSolveConfig {
    pub trust_region: usize,
    pub beam_width: usize,
    pub output_limit: usize,
}

#[derive(Clone, Debug, Default)]
pub struct DriveFamilies {
    by_family: FxHashMap<String, Vec<(usize, String)>>,
}

fn drive_parts(op: &str) -> Option<(String, usize, String)> {
    let marker = op.rfind('x')?;
    let bytes = op.as_bytes();
    let mut end = marker + 1;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == marker + 1 {
        return None;
    }
    let drive = op[marker + 1..end].parse().ok()?;
    Some((op[..marker].to_owned(), drive, op[end..].to_owned()))
}

impl DriveFamilies {
    pub fn from_cell_names<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut result = Self::default();
        for name in names {
            let name = name.as_ref();
            let Some((prefix, drive, suffix)) = drive_parts(name) else {
                continue;
            };
            result
                .by_family
                .entry(format!("{prefix}x{suffix}"))
                .or_default()
                .push((drive, name.to_owned()));
        }
        for values in result.by_family.values_mut() {
            values.sort_by_key(|(drive, name)| (*drive, name.clone()));
            values.dedup_by(|first, second| first.1 == second.1);
        }
        result
    }

    pub fn domain(&self, op: &str) -> Vec<String> {
        let Some((prefix, _, suffix)) = drive_parts(op) else {
            return vec![op.to_owned()];
        };
        self.by_family
            .get(&format!("{prefix}x{suffix}"))
            .map(|values| values.iter().map(|(_, name)| name.clone()).collect())
            .unwrap_or_else(|| vec![op.to_owned()])
    }

    fn extreme(&self, op: &str, large: bool) -> String {
        let values = self.domain(op);
        if large {
            values.last().cloned().unwrap_or_else(|| op.to_owned())
        } else {
            values.first().cloned().unwrap_or_else(|| op.to_owned())
        }
    }
}

fn cell_count(expression: &TopologyExpr) -> usize {
    match expression {
        TopologyExpr::Anchor(_) => 0,
        TopologyExpr::Cell { children, .. } => 1 + children.iter().map(cell_count).sum::<usize>(),
    }
}

fn cell_paths(expression: &TopologyExpr) -> Vec<Vec<usize>> {
    fn walk(expression: &TopologyExpr, path: &mut Vec<usize>, output: &mut Vec<Vec<usize>>) {
        if let TopologyExpr::Cell { children, .. } = expression {
            output.push(path.clone());
            for (index, child) in children.iter().enumerate() {
                path.push(index);
                walk(child, path, output);
                path.pop();
            }
        }
    }
    let mut output = Vec::new();
    walk(expression, &mut Vec::new(), &mut output);
    output
}

fn op_at_path<'a>(expression: &'a TopologyExpr, path: &[usize]) -> Option<&'a str> {
    let mut current = expression;
    for index in path {
        let TopologyExpr::Cell { children, .. } = current else {
            return None;
        };
        current = children.get(*index)?;
    }
    match current {
        TopologyExpr::Cell { op, .. } => Some(op),
        TopologyExpr::Anchor(_) => None,
    }
}

fn with_op_at_path(expression: &TopologyExpr, path: &[usize], replacement: &str) -> TopologyExpr {
    if path.is_empty() {
        return match expression {
            TopologyExpr::Cell { children, .. } => {
                TopologyExpr::cell(replacement, children.clone())
            }
            TopologyExpr::Anchor(_) => expression.clone(),
        };
    }
    match expression {
        TopologyExpr::Anchor(_) => expression.clone(),
        TopologyExpr::Cell { op, children } => {
            let mut changed = children.clone();
            let index = path[0];
            if let Some(child) = changed.get_mut(index) {
                *child = with_op_at_path(child, &path[1..], replacement);
            }
            TopologyExpr::cell(op.clone(), changed)
        }
    }
}

fn resize_all(expression: &TopologyExpr, families: &DriveFamilies, large: bool) -> TopologyExpr {
    match expression {
        TopologyExpr::Anchor(anchor) => TopologyExpr::Anchor(*anchor),
        TopologyExpr::Cell { op, children } => TopologyExpr::cell(
            families.extreme(op, large),
            children
                .iter()
                .map(|child| resize_all(child, families, large))
                .collect(),
        ),
    }
}

fn incumbent_expression(source: &Netlist<StdCellType, ()>, anchor: usize) -> TopologyExpr {
    let node = NodeIndex::new(anchor);
    TopologyExpr::cell(
        source.graph[node].to_string(),
        source.inputs(node).map(TopologyExpr::Anchor).collect(),
    )
}

fn variant_kind(topology_changed: bool, sizing_changed: bool) -> JointKind {
    match (topology_changed, sizing_changed) {
        (false, false) => JointKind::NoOp,
        (true, false) => JointKind::TopologyOnly,
        (false, true) => JointKind::SizingOnly,
        (true, true) => JointKind::JointTopologySizing,
    }
}

fn boolean_family_with_experimental_full(
    op: &str,
    experimental_full: bool,
) -> Option<&'static str> {
    // Boolean equivalence must not depend on whether a Liberty drive is
    // encoded as x1, xp5, xp25R, and so on.  `drive_parts` intentionally
    // parses integer sizing domains, but is too narrow for this purpose.
    let base = op.split("_ASAP").next().unwrap_or(op);
    let prefix = base.rfind('x').map_or(base, |marker| &base[..marker]);
    // ASAP7 header-buffer cells are Boolean buffers.  They occur in real
    // Genus-mapped windows even though their family name is HB rather than
    // BUF; treating them as unsupported caused a proof false-negative.
    if experimental_full && prefix.starts_with("HB") {
        return Some("buf");
    }
    if experimental_full {
        match prefix {
            "AND4" => return Some("and4"),
            "NAND4" => return Some("nand4"),
            "OR4" => return Some("or4"),
            "NOR4" => return Some("nor4"),
            "A2O1A1I" => return Some("a2o1a1i"),
            "AO211" => return Some("ao211"),
            "AOI211" => return Some("aoi211"),
            "AOI31" => return Some("aoi31"),
            "AOI221" => return Some("aoi221"),
            _ => {}
        }
    }
    match prefix {
        "AND2" => Some("and2"),
        "AND3" => Some("and3"),
        "NAND2" => Some("nand2"),
        "NAND3" => Some("nand3"),
        "OR2" => Some("or2"),
        "OR3" => Some("or3"),
        "NOR2" => Some("nor2"),
        "NOR3" => Some("nor3"),
        "INV" => Some("inv"),
        "BUF" => Some("buf"),
        "XOR2" => Some("xor2"),
        "XNOR2" => Some("xnor2"),
        "MAJ" => Some("maj"),
        "MAJI" => Some("maji"),
        "AO21" => Some("ao21"),
        "AOI21" => Some("aoi21"),
        "AO22" => Some("ao22"),
        "AOI22" => Some("aoi22"),
        "OA21" => Some("oa21"),
        "OAI21" => Some("oai21"),
        "OA22" => Some("oa22"),
        "OAI22" => Some("oai22"),
        "O2A1O1I" => Some("o2a1o1i"),
        _ => None,
    }
}

fn boolean_family(op: &str) -> Option<&'static str> {
    let experimental_full = std::env::var("EGG_EXPERIMENTAL_FULL_BOOLEAN_PROOF")
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(false);
    boolean_family_with_experimental_full(op, experimental_full)
}

fn eval_cell(op: &str, inputs: &[bool]) -> Option<bool> {
    eval_cell_family(boolean_family(op)?, inputs)
}

fn eval_cell_family(family: &str, inputs: &[bool]) -> Option<bool> {
    Some(match family {
        "and2" => *inputs.first()? && *inputs.get(1)?,
        "and3" => *inputs.first()? && *inputs.get(1)? && *inputs.get(2)?,
        "and4" => *inputs.first()? && *inputs.get(1)? && *inputs.get(2)? && *inputs.get(3)?,
        "nand2" => !(*inputs.first()? && *inputs.get(1)?),
        "nand3" => !(*inputs.first()? && *inputs.get(1)? && *inputs.get(2)?),
        "nand4" => !(*inputs.first()? && *inputs.get(1)? && *inputs.get(2)? && *inputs.get(3)?),
        "or2" => *inputs.first()? || *inputs.get(1)?,
        "or3" => *inputs.first()? || *inputs.get(1)? || *inputs.get(2)?,
        "or4" => *inputs.first()? || *inputs.get(1)? || *inputs.get(2)? || *inputs.get(3)?,
        "nor2" => !(*inputs.first()? || *inputs.get(1)?),
        "nor3" => !(*inputs.first()? || *inputs.get(1)? || *inputs.get(2)?),
        "nor4" => !(*inputs.first()? || *inputs.get(1)? || *inputs.get(2)? || *inputs.get(3)?),
        "inv" => !*inputs.first()?,
        "buf" => *inputs.first()?,
        "xor2" => *inputs.first()? ^ *inputs.get(1)?,
        "xnor2" => !(*inputs.first()? ^ *inputs.get(1)?),
        "maj" => {
            let (a, b, c) = (*inputs.first()?, *inputs.get(1)?, *inputs.get(2)?);
            (a && b) || (a && c) || (b && c)
        }
        "maji" => {
            let (a, b, c) = (*inputs.first()?, *inputs.get(1)?, *inputs.get(2)?);
            !((a && b) || (a && c) || (b && c))
        }
        "ao21" => (*inputs.first()? && *inputs.get(1)?) || *inputs.get(2)?,
        "aoi21" => !((*inputs.first()? && *inputs.get(1)?) || *inputs.get(2)?),
        "ao22" => (*inputs.first()? && *inputs.get(1)?) || (*inputs.get(2)? && *inputs.get(3)?),
        "aoi22" => {
            !((*inputs.first()? && *inputs.get(1)?) || (*inputs.get(2)? && *inputs.get(3)?))
        }
        // FULL-186 Liberty: (!A1*!B) + (!A2*!B) + (!C), ordered A1,A2,B,C.
        "a2o1a1i" => {
            let (a1, a2, b, c) = (
                *inputs.first()?,
                *inputs.get(1)?,
                *inputs.get(2)?,
                *inputs.get(3)?,
            );
            (!a1 && !b) || (!a2 && !b) || !c
        }
        "ao211" => (*inputs.first()? && *inputs.get(1)?) || *inputs.get(2)? || *inputs.get(3)?,
        // FULL-186 Liberty: !(A1*A2 + B + C).
        "aoi211" => {
            !((*inputs.first()? && *inputs.get(1)?) || *inputs.get(2)? || *inputs.get(3)?)
        }
        "aoi31" => !((*inputs.first()? && *inputs.get(1)? && *inputs.get(2)?) || *inputs.get(3)?),
        "aoi221" => {
            !((*inputs.first()? && *inputs.get(1)?)
                || (*inputs.get(2)? && *inputs.get(3)?)
                || *inputs.get(4)?)
        }
        "oa21" => (*inputs.first()? || *inputs.get(1)?) && *inputs.get(2)?,
        "oai21" => !((*inputs.first()? || *inputs.get(1)?) && *inputs.get(2)?),
        "oa22" => (*inputs.first()? || *inputs.get(1)?) && (*inputs.get(2)? || *inputs.get(3)?),
        "oai22" => {
            !((*inputs.first()? || *inputs.get(1)?) && (*inputs.get(2)? || *inputs.get(3)?))
        }
        "o2a1o1i" => {
            let (a1, a2, b, c) = (
                *inputs.first()?,
                *inputs.get(1)?,
                *inputs.get(2)?,
                *inputs.get(3)?,
            );
            (!a1 && !a2 && !c) || (!b && !c)
        }
        _ => return None,
    })
}

fn exhaustive_proof_boundary_limit() -> usize {
    std::env::var("EGG_EXPERIMENTAL_MACRO_PROOF_MAX_BOUNDARY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|limit| (1..=20).contains(limit))
        .unwrap_or(16)
}

fn expression_boundaries(expression: &TopologyExpr, output: &mut FxHashSet<usize>) {
    match expression {
        TopologyExpr::Anchor(anchor) => {
            output.insert(anchor.index());
        }
        TopologyExpr::Cell { children, .. } => {
            for child in children {
                expression_boundaries(child, output);
            }
        }
    }
}

fn eval_expression(expression: &TopologyExpr, values: &FxHashMap<usize, bool>) -> Option<bool> {
    match expression {
        TopologyExpr::Anchor(anchor) => values.get(&anchor.index()).copied(),
        TopologyExpr::Cell { op, children } => {
            let inputs: Option<Vec<_>> = children
                .iter()
                .map(|child| eval_expression(child, values))
                .collect();
            eval_cell(op, &inputs?)
        }
    }
}

fn eval_original_to_boundary(
    source: &Netlist<StdCellType, ()>,
    anchor: NodeIndex,
    boundaries: &FxHashSet<usize>,
    values: &FxHashMap<usize, bool>,
    active: &mut FxHashSet<usize>,
) -> Option<bool> {
    if boundaries.contains(&anchor.index()) {
        return values.get(&anchor.index()).copied();
    }
    if source.leaves.contains(&anchor) || source.roots.contains(&anchor) {
        return None;
    }
    if !active.insert(anchor.index()) {
        return None;
    }
    let inputs: Option<Vec<_>> = source
        .inputs(anchor)
        .map(|child| eval_original_to_boundary(source, child, boundaries, values, active))
        .collect();
    active.remove(&anchor.index());
    eval_cell(&source.graph[anchor].to_string(), &inputs?)
}

/// Exhaustively prove a local e-node realization against the original cone at
/// the exact expression boundary.  D3 V0 rejects unsupported/too-wide proofs.
pub fn local_equivalence(
    source: &Netlist<StdCellType, ()>,
    root_anchor: usize,
    expression: &TopologyExpr,
) -> bool {
    let mut boundaries = FxHashSet::default();
    expression_boundaries(expression, &mut boundaries);
    if boundaries.len() > exhaustive_proof_boundary_limit() {
        return false;
    }
    let mut ordered: Vec<_> = boundaries.iter().copied().collect();
    ordered.sort_unstable();
    for vector in 0usize..(1usize << ordered.len()) {
        let values: FxHashMap<_, _> = ordered
            .iter()
            .enumerate()
            .map(|(index, anchor)| (*anchor, ((vector >> index) & 1) != 0))
            .collect();
        let Some(candidate) = eval_expression(expression, &values) else {
            return false;
        };
        let Some(original) = eval_original_to_boundary(
            source,
            NodeIndex::new(root_anchor),
            &boundaries,
            &values,
            &mut FxHashSet::default(),
        ) else {
            return false;
        };
        if candidate != original {
            return false;
        }
    }
    true
}

fn eval_macro_expression(
    source: &Netlist<StdCellType, ()>,
    expression: &TopologyExpr,
    selected: &FxHashMap<usize, TopologyExpr>,
    boundaries: &FxHashSet<usize>,
    values: &FxHashMap<usize, bool>,
    active: &mut FxHashSet<usize>,
) -> Option<bool> {
    match expression {
        TopologyExpr::Anchor(anchor) => {
            let index = anchor.index();
            if boundaries.contains(&index) {
                return values.get(&index).copied();
            }
            if let Some(replacement) = selected.get(&index) {
                if !active.insert(index) {
                    return None;
                }
                let result = eval_macro_expression(
                    source,
                    replacement,
                    selected,
                    boundaries,
                    values,
                    active,
                );
                active.remove(&index);
                result
            } else {
                eval_original_to_boundary(
                    source,
                    *anchor,
                    boundaries,
                    values,
                    &mut FxHashSet::default(),
                )
            }
        }
        TopologyExpr::Cell { op, children } => {
            let inputs: Option<Vec<_>> = children
                .iter()
                .map(|child| {
                    eval_macro_expression(source, child, selected, boundaries, values, active)
                })
                .collect();
            eval_cell(op, &inputs?)
        }
    }
}

/// Exhaustively prove a correlated multi-root macro over a common Boolean
/// cut.  Unlike `local_equivalence`, an expression may reuse an existing
/// divisor whose own function is expanded back to the supplied cut, and
/// multiple selected roots may reference one shared selected host.
pub fn macro_equivalence(
    source: &Netlist<StdCellType, ()>,
    choices: &[(usize, TopologyExpr)],
    boundary_anchors: &[usize],
    observable_roots: &[usize],
) -> bool {
    let boundaries: FxHashSet<_> = boundary_anchors.iter().copied().collect();
    if boundaries.len() > exhaustive_proof_boundary_limit() {
        return false;
    }
    let selected: FxHashMap<_, _> = choices.iter().cloned().collect();
    let mut ordered: Vec<_> = boundaries.iter().copied().collect();
    ordered.sort_unstable();
    for vector in 0usize..(1usize << ordered.len()) {
        let values: FxHashMap<_, _> = ordered
            .iter()
            .enumerate()
            .map(|(index, anchor)| (*anchor, ((vector >> index) & 1) != 0))
            .collect();
        for root_anchor in observable_roots {
            let Some(expression) = selected.get(root_anchor) else {
                return false;
            };
            let mut active = FxHashSet::default();
            active.insert(*root_anchor);
            let Some(candidate) = eval_macro_expression(
                source,
                expression,
                &selected,
                &boundaries,
                &values,
                &mut active,
            ) else {
                return false;
            };
            let Some(original) = eval_original_to_boundary(
                source,
                NodeIndex::new(*root_anchor),
                &boundaries,
                &values,
                &mut FxHashSet::default(),
            ) else {
                return false;
            };
            if candidate != original {
                return false;
            }
        }
    }
    true
}

fn make_variant(
    source: &Netlist<StdCellType, ()>,
    class: &RegionEclass,
    enode: &RegionEnode,
    expression: TopologyExpr,
    pattern: SizingPattern,
) -> JointVariant {
    let topology_signature = enode.structural_hash.clone();
    let sized_signature = expression_text(&expression);
    let inherited_signature = expression_text(&enode.expression);
    let topology_changed = !enode.incumbent;
    let sizing_changed = sized_signature != inherited_signature;
    JointVariant {
        variant_id: format!("{}|{}|{:?}", class.eclass_id, sized_signature, pattern),
        eclass_id: class.eclass_id.clone(),
        root_anchor: class.root_anchor,
        topology_enode_id: enode.enode_id.clone(),
        topology_signature,
        sized_signature,
        pattern,
        kind: variant_kind(topology_changed, sizing_changed),
        topology_changed,
        sizing_changed,
        physical_cell_count: cell_count(&expression),
        local_equivalence_pass: local_equivalence(source, class.root_anchor, &expression),
        expression,
    }
}

/// Generate at most four complete sizing patterns per structural e-node.  The
/// callback is the current-parent local price; it is used only for coordinate
/// selection of the fourth pattern, never as an acceptance value.
pub fn joint_variants_for_class<F>(
    source: &Netlist<StdCellType, ()>,
    class: &RegionEclass,
    families: &DriveFamilies,
    mut price: F,
) -> Result<Vec<JointVariant>, String>
where
    F: FnMut(&TopologyExpr) -> Result<f64, String>,
{
    let mut variants = Vec::new();
    for enode in &class.enodes {
        let inherited = enode.expression.clone();
        let small = resize_all(&inherited, families, false);
        let large = resize_all(&inherited, families, true);
        let mut greedy = inherited.clone();
        for path in cell_paths(&greedy) {
            let Some(op) = op_at_path(&greedy, &path).map(str::to_owned) else {
                continue;
            };
            let mut best = greedy.clone();
            let mut best_price = price(&best)?;
            for candidate_op in families.domain(&op) {
                let candidate = with_op_at_path(&greedy, &path, &candidate_op);
                let candidate_price = price(&candidate)?;
                if candidate_price < best_price {
                    best = candidate;
                    best_price = candidate_price;
                }
            }
            greedy = best;
        }
        let proposed = [
            (SizingPattern::Inherited, inherited),
            (SizingPattern::AllSmall, small),
            (SizingPattern::AllLarge, large),
            (SizingPattern::AdjointBest, greedy),
        ];
        let mut seen = FxHashSet::default();
        for (pattern, expression) in proposed {
            let signature = expression_text(&expression);
            if seen.insert(signature) {
                let variant = make_variant(source, class, enode, expression, pattern);
                if variant.local_equivalence_pass {
                    variants.push(variant);
                }
            }
        }
    }
    variants.sort_by(|first, second| {
        first
            .topology_changed
            .cmp(&second.topology_changed)
            .then_with(|| first.topology_signature.cmp(&second.topology_signature))
            .then_with(|| first.sized_signature.cmp(&second.sized_signature))
    });
    Ok(variants)
}

pub fn assignment_kind(variants: &[JointVariant]) -> JointKind {
    let topology = variants.iter().any(|variant| variant.topology_changed);
    let sizing = variants.iter().any(|variant| variant.sizing_changed);
    variant_kind(topology, sizing)
}

pub fn assignment_signature(variants: &[JointVariant]) -> String {
    let mut rows: Vec<_> = variants
        .iter()
        .map(|variant| format!("{}={}", variant.eclass_id, variant.sized_signature))
        .collect();
    rows.sort();
    rows.join("|")
}

pub fn topology_signature(variants: &[JointVariant]) -> String {
    let mut rows: Vec<_> = variants
        .iter()
        .map(|variant| format!("{}={}", variant.eclass_id, variant.topology_signature))
        .collect();
    rows.sort();
    rows.join("|")
}

pub fn physicalize_joint_assignment(
    source: &Netlist<StdCellType, ()>,
    variants: &[JointVariant],
) -> Result<PhysicalizedTopology, String> {
    let mut selection = TopologySelection::default();
    let mut seen = FxHashSet::default();
    for variant in variants {
        if !seen.insert(variant.root_anchor) {
            return Err(format!(
                "more than one realization selected for occurrence:{}",
                variant.root_anchor
            ));
        }
        if variant.kind != JointKind::NoOp {
            selection.choose(
                NodeIndex::new(variant.root_anchor),
                variant.expression.clone(),
            );
        }
    }
    let physical = physicalize_topology(source, &selection)?;
    for variant in variants
        .iter()
        .filter(|variant| variant.kind != JointKind::NoOp)
    {
        if !physical
            .original_to_materialized
            .contains_key(&NodeIndex::new(variant.root_anchor))
        {
            return Err(format!(
                "selected joint occurrence:{} became unreachable",
                variant.root_anchor
            ));
        }
    }
    Ok(physical)
}

#[derive(Clone)]
struct PartialAssignment {
    variants: Vec<JointVariant>,
    additive_price: f64,
    changed: usize,
}

/// Bounded k-best discrete extraction over already priced complete sized
/// e-node realizations.  The beam is a solver frontier, not an Exact-V2 budget.
pub fn solve_joint_frontier<F>(
    source: &Netlist<StdCellType, ()>,
    space: &AdaptiveRegionSpace,
    region: &[usize],
    variants: &FxHashMap<usize, Vec<JointVariant>>,
    config: &JointSolveConfig,
    mut unary_price: F,
) -> Result<Vec<JointAssignment>, String>
where
    F: FnMut(&JointVariant) -> f64,
{
    if config.trust_region == 0 || config.beam_width == 0 || config.output_limit == 0 {
        return Err("D3 solver limits must be positive".into());
    }
    let mut ordered = region.to_vec();
    ordered.sort_unstable();
    ordered.dedup();
    if ordered.len() != region.len() {
        return Err("duplicate eclass in D3 region".into());
    }
    for anchor in &ordered {
        if space.class_by_anchor(*anchor).is_none() {
            return Err(format!("unknown D3 region occurrence:{anchor}"));
        }
        if variants.get(anchor).is_none_or(Vec::is_empty) {
            return Err(format!("empty joint domain for occurrence:{anchor}"));
        }
    }
    let mut beam = vec![PartialAssignment {
        variants: Vec::new(),
        additive_price: 0.0,
        changed: 0,
    }];
    for anchor in ordered {
        let mut expanded = Vec::new();
        for partial in &beam {
            for variant in &variants[&anchor] {
                let changed = partial.changed + usize::from(variant.kind != JointKind::NoOp);
                if changed > config.trust_region {
                    continue;
                }
                let mut next = partial.variants.clone();
                next.push(variant.clone());
                expanded.push(PartialAssignment {
                    variants: next,
                    additive_price: partial.additive_price + unary_price(variant),
                    changed,
                });
            }
        }
        expanded.sort_by(|first, second| {
            first
                .additive_price
                .partial_cmp(&second.additive_price)
                .unwrap_or(Ordering::Equal)
                .then_with(|| {
                    assignment_signature(&first.variants)
                        .cmp(&assignment_signature(&second.variants))
                })
        });
        let mut seen = FxHashSet::default();
        expanded.retain(|item| seen.insert(assignment_signature(&item.variants)));
        expanded.truncate(config.beam_width);
        beam = expanded;
    }
    let mut output = Vec::new();
    for partial in beam {
        if partial.changed == 0 || physicalize_joint_assignment(source, &partial.variants).is_err()
        {
            continue;
        }
        let topology_changed_occurrences = partial
            .variants
            .iter()
            .filter(|variant| variant.topology_changed)
            .count();
        let sizing_changed_occurrences = partial
            .variants
            .iter()
            .filter(|variant| variant.sizing_changed)
            .count();
        output.push(JointAssignment {
            kind: assignment_kind(&partial.variants),
            changed_occurrences: partial.changed,
            topology_changed_occurrences,
            sizing_changed_occurrences,
            variants: partial.variants,
            additive_price: partial.additive_price,
            joint_price: partial.additive_price,
        });
    }
    output.sort_by(|first, second| {
        first
            .additive_price
            .partial_cmp(&second.additive_price)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                assignment_signature(&first.variants).cmp(&assignment_signature(&second.variants))
            })
    });
    output.truncate(config.output_limit);
    Ok(output)
}

pub fn incumbent_variant(
    source: &Netlist<StdCellType, ()>,
    space: &AdaptiveRegionSpace,
    anchor: usize,
) -> Result<JointVariant, String> {
    let class = space
        .class_by_anchor(anchor)
        .ok_or_else(|| format!("unknown occurrence:{anchor}"))?;
    let enode = class
        .enodes
        .iter()
        .find(|enode| enode.incumbent)
        .ok_or_else(|| format!("occurrence:{anchor} has no incumbent enode"))?;
    Ok(make_variant(
        source,
        class,
        enode,
        incumbent_expression(source, anchor),
        SizingPattern::Inherited,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adaptive_region::build_rule_agnostic_space;
    use crate::io::liberty::{get_direction_of_pins, read_liberty};
    use crate::io::stdcell::read_verilog_with_lib_to_netlist;
    use std::path::Path;

    #[test]
    fn drive_families_are_real_and_ordered() {
        let families = DriveFamilies::from_cell_names([
            "INVx4_ASAP7_6t_L",
            "INVx1_ASAP7_6t_L",
            "INVx2_ASAP7_6t_L",
        ]);
        assert_eq!(
            families.domain("INVx2_ASAP7_6t_L"),
            vec!["INVx1_ASAP7_6t_L", "INVx2_ASAP7_6t_L", "INVx4_ASAP7_6t_L"]
        );
    }

    #[test]
    fn boolean_equivalence_understands_fractional_and_inverting_complex_cells() {
        assert_eq!(
            eval_cell("NAND2xp5R_ASAP7_6t_L", &[true, true]),
            Some(false)
        );
        assert_eq!(
            eval_cell("OAI21xp5b_ASAP7_6t_L", &[false, false, true]),
            Some(true)
        );
        assert_eq!(
            eval_cell("MAJIxp5_ASAP7_6t_L", &[true, true, false]),
            Some(false)
        );
        assert_eq!(
            boolean_family_with_experimental_full("HB1x1_ASAP7_6t_L", true),
            Some("buf")
        );
        assert_eq!(
            boolean_family_with_experimental_full("AOI31xp67_ASAP7_6t_L", true),
            Some("aoi31")
        );
        assert_eq!(
            boolean_family_with_experimental_full("AOI221xp5_ASAP7_6t_L", true),
            Some("aoi221")
        );
        assert_eq!(
            boolean_family_with_experimental_full("AND4x1_ASAP7_6t_L", true),
            Some("and4")
        );
        assert_eq!(
            boolean_family_with_experimental_full("NOR4xp25_ASAP7_6t_L", true),
            Some("nor4")
        );
        assert_eq!(
            boolean_family_with_experimental_full("A2O1A1Ixp33_ASAP7_6t_L", true),
            Some("a2o1a1i")
        );
        assert_eq!(
            boolean_family_with_experimental_full("AO211x1_ASAP7_6t_L", true),
            Some("ao211")
        );
        assert_eq!(
            boolean_family_with_experimental_full("AOI211xp25_ASAP7_6t_L", true),
            Some("aoi211")
        );
        assert_eq!(
            eval_cell_family("a2o1a1i", &[true, true, true, false]),
            Some(true)
        );
        assert_eq!(
            eval_cell_family("a2o1a1i", &[true, true, true, true]),
            Some(false)
        );
        assert_eq!(
            eval_cell_family("ao211", &[true, true, false, false]),
            Some(true)
        );
        assert_eq!(
            eval_cell_family("aoi211", &[false, true, false, false]),
            Some(true)
        );
    }

    #[test]
    fn macro_equivalence_allows_an_internal_shared_host_to_change() {
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let pins = get_direction_of_pins(&liberty).unwrap();
        let (source, _) =
            read_verilog_with_lib_to_netlist("test/topology_inv_and.v", pins).unwrap();
        let output = source.inputs(source.roots[0]).next().unwrap();
        let host = source.inputs(output).next().unwrap();
        let boundary: Vec<_> = source.inputs(host).map(|node| node.index()).collect();
        let host_nand = TopologyExpr::cell(
            "NAND2x1_ASAP7_6t_L",
            boundary
                .iter()
                .map(|anchor| TopologyExpr::Anchor(NodeIndex::new(*anchor)))
                .collect(),
        );
        let choices = vec![
            (host.index(), host_nand),
            (output.index(), TopologyExpr::Anchor(host)),
        ];
        assert!(macro_equivalence(
            &source,
            &choices,
            &boundary,
            &[output.index()]
        ));
        assert!(!macro_equivalence(
            &source,
            &choices,
            &boundary,
            &[host.index(), output.index()]
        ));
    }

    #[test]
    fn incumbent_topology_has_sizing_only_joint_domain() {
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let pins = get_direction_of_pins(&liberty).unwrap();
        let families = DriveFamilies::from_cell_names(pins.keys());
        let (source, _) =
            read_verilog_with_lib_to_netlist("test/topology_inv_and.v", pins).unwrap();
        let (space, _) = build_rule_agnostic_space(
            &source,
            &[Path::new("test/6t_adaptive_dummy_rules.json")],
            2,
            3,
        )
        .unwrap();
        let class = space.active_eclasses().next().unwrap();
        let variants = joint_variants_for_class(&source, class, &families, |_| Ok(0.0)).unwrap();
        assert!(
            variants
                .iter()
                .any(|variant| variant.kind == JointKind::SizingOnly)
        );
        assert!(
            variants
                .iter()
                .any(|variant| variant.kind == JointKind::TopologyOnly)
        );
        assert!(
            variants
                .iter()
                .any(|variant| variant.kind == JointKind::JointTopologySizing)
        );
        assert!(
            variants
                .iter()
                .all(|variant| variant.local_equivalence_pass)
        );
    }

    #[test]
    fn joint_solver_enforces_one_choice_trust_and_physical_legality() {
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let pins = get_direction_of_pins(&liberty).unwrap();
        let families = DriveFamilies::from_cell_names(pins.keys());
        let (source, _) =
            read_verilog_with_lib_to_netlist("test/topology_inv_and.v", pins).unwrap();
        let (space, _) = build_rule_agnostic_space(
            &source,
            &[Path::new("test/6t_adaptive_dummy_rules.json")],
            2,
            3,
        )
        .unwrap();
        let class = space.active_eclasses().next().unwrap();
        let region = vec![class.root_anchor];
        let domain = joint_variants_for_class(&source, class, &families, |_| Ok(0.0)).unwrap();
        let variants = [(class.root_anchor, domain)].into_iter().collect();
        let solved = solve_joint_frontier(
            &source,
            &space,
            &region,
            &variants,
            &JointSolveConfig {
                trust_region: 1,
                beam_width: 64,
                output_limit: 32,
            },
            |_| 0.0,
        )
        .unwrap();
        assert!(!solved.is_empty());
        for assignment in solved {
            assert_eq!(assignment.variants.len(), 1);
            assert!(assignment.changed_occurrences <= 1);
            physicalize_joint_assignment(&source, &assignment.variants).unwrap();
        }
    }
}
