//! Native Rust implementation of Generator V2.
//!
//! This is a semantics-preserving port of
//! `scripts/generate_multi_output_resynthesis.py`: Hybrid planner windows are
//! converted to bounded truth-table resynthesis, shared multi-output DAG and
//! explicit divisor-resubstitution proposals.  It is proposal-only; local
//! equivalence, P1, A2 and Exact-V2 retain their existing authority.

use crate::objective::MetricWeights;
use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};

const FAMILIES: [(&str, usize); 18] = [
    ("INV", 1),
    ("AND2", 2),
    ("NAND2", 2),
    ("OR2", 2),
    ("NOR2", 2),
    ("XOR2", 2),
    ("XNOR2", 2),
    ("AO21", 3),
    ("AOI21", 3),
    ("OA21", 3),
    ("OAI21", 3),
    ("MAJ", 3),
    ("MAJI", 3),
    ("AO22", 4),
    ("AOI22", 4),
    ("OA22", 4),
    ("OAI22", 4),
    ("O2A1O1I", 4),
];

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct Truth([u64; 4]);

impl Truth {
    fn mask(bits: usize) -> Self {
        let mut words = [0u64; 4];
        for (index, word) in words.iter_mut().enumerate() {
            let remaining = bits.saturating_sub(index * 64);
            *word = if remaining >= 64 {
                u64::MAX
            } else if remaining == 0 {
                0
            } else {
                (1u64 << remaining) - 1
            };
        }
        Self(words)
    }

    fn set(&mut self, bit: usize) {
        self.0[bit / 64] |= 1u64 << (bit % 64);
    }

    fn and(self, other: Self) -> Self {
        Self(std::array::from_fn(|index| self.0[index] & other.0[index]))
    }

    fn or(self, other: Self) -> Self {
        Self(std::array::from_fn(|index| self.0[index] | other.0[index]))
    }

    fn xor(self, other: Self) -> Self {
        Self(std::array::from_fn(|index| self.0[index] ^ other.0[index]))
    }

    fn not(self, bits: usize) -> Self {
        let mask = Self::mask(bits);
        Self(std::array::from_fn(|index| mask.0[index] ^ self.0[index]))
    }
}

/// Backward-compatible name for the source-independent topology expression.
/// Generator candidates now directly produce the same representation used at
/// the ordinary/generated extraction boundary.
pub use crate::topology_expr::TopologyExpr as GeneratorExpr;

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct GeneratorChoice {
    pub root_anchor: usize,
    pub expression: GeneratorExpr,
}

/// One independently proved fragment of a larger topology transaction.
///
/// Ordinary Generator candidates leave this empty and retain the historical
/// single-cut proof.  Objective-programmable closure candidates use it to
/// compose many already-independent local rewrites without forcing their
/// unrelated Boolean boundaries into one exponential truth table.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct GeneratorProofGroup {
    pub choices: Vec<GeneratorChoice>,
    pub boundary: Vec<usize>,
    pub outputs: Vec<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GeneratorCandidate {
    pub candidate_id: String,
    pub provenance: Vec<String>,
    pub choices: Vec<GeneratorChoice>,
    pub tech_area: f64,
    pub tech_delay: f64,
    pub logic_depth: usize,
    pub cell_families: Vec<String>,
    pub outputs: Vec<usize>,
    pub window_kind: String,
    pub proof_boundary: Vec<usize>,
    pub proof_outputs: Vec<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proof_groups: Vec<GeneratorProofGroup>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_host: Option<usize>,
    pub divisors: Vec<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaced_anchor: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logical_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mapping: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_windows: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct SourceGraph {
    occurrences: Vec<SourceNode>,
}

#[derive(Clone, Debug, Deserialize)]
struct SourceNode {
    anchor: usize,
    op: String,
    inputs: Vec<usize>,
    consumers: Vec<usize>,
    is_leaf: bool,
    is_root: bool,
    #[allow(dead_code)]
    is_constant: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct PlannerRow {
    region_id: String,
    region_size: usize,
}

#[derive(Clone, Debug)]
struct TechCell {
    name: String,
    area: f64,
    delay: f64,
}

/// Immutable technology data shared by every Generator invocation in one
/// process.  Fresh-parent rounds change the graph, never the Liberty problem
/// definition; reparsing the same FULL library was therefore pure overhead.
struct TechnologyContext {
    areas: BTreeMap<String, f64>,
    delays: BTreeMap<String, f64>,
    mappings: BTreeMap<String, BTreeMap<String, TechCell>>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct TechnologyCacheKey {
    path: PathBuf,
    length: u64,
    modified_sec: u64,
    modified_nsec: u32,
}

static TECHNOLOGY_CONTEXT_CACHE: OnceLock<
    Mutex<BTreeMap<TechnologyCacheKey, Arc<TechnologyContext>>>,
> = OnceLock::new();

#[derive(Clone, Debug)]
struct Term {
    truth: Truth,
    expression: GeneratorExpr,
    text: String,
    area: f64,
    depth: usize,
    anchors: BTreeSet<usize>,
    families: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct Window {
    region_id: String,
    region: BTreeSet<usize>,
    kind: String,
    host: usize,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TermCacheKey {
    truth: Truth,
    expression: GeneratorExpr,
    area_bits: u64,
    depth: usize,
    anchors: BTreeSet<usize>,
    families: BTreeSet<String>,
}

impl From<&Term> for TermCacheKey {
    fn from(term: &Term) -> Self {
        Self {
            truth: term.truth,
            expression: term.expression.clone(),
            area_bits: term.area.to_bits(),
            depth: term.depth,
            anchors: term.anchors.clone(),
            families: term.families.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TopGateKey {
    special: TermCacheKey,
    terminals: Vec<TermCacheKey>,
    target: Truth,
    bits: usize,
    limit: usize,
}

#[derive(Default)]
struct TopGateCache {
    entries: HashMap<TopGateKey, Vec<Term>>,
    hits: usize,
    misses: usize,
}

struct WindowGenerationResult {
    candidates: Vec<GeneratorCandidate>,
    audit: Value,
    cache_hits: usize,
    cache_misses: usize,
}

fn parallel_map_ordered<T, R, F>(items: &[T], requested_jobs: usize, work: F) -> Result<Vec<R>>
where
    T: Sync,
    R: Send,
    F: Fn(usize, &T) -> Result<R> + Sync,
{
    if items.len() <= 1 || requested_jobs <= 1 {
        return items
            .iter()
            .enumerate()
            .map(|(index, item)| work(index, item))
            .collect();
    }
    let jobs = requested_jobs.min(items.len());
    let next = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            let sender = sender.clone();
            let next = &next;
            let work = &work;
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, AtomicOrdering::Relaxed);
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
        .map(|(index, value)| {
            value.with_context(|| format!("missing generator window result {index}"))?
        })
        .collect()
}

pub fn logic_name(cell: &str) -> String {
    let base = cell.split("_ASAP").next().unwrap_or(cell);
    let bytes = base.as_bytes();
    for index in (0..bytes.len()).rev() {
        if bytes[index] != b'x' {
            continue;
        }
        let suffix = &base[index + 1..];
        // Match the historical `x(?:p)?[0-9].*$`: FULL-186 contains drive
        // suffixes such as xp33R, whose trailing flavor marker is not part of
        // the Boolean family.
        let numeric = suffix.as_bytes().first().is_some_and(u8::is_ascii_digit)
            || suffix
                .strip_prefix('p')
                .and_then(|tail| tail.as_bytes().first())
                .is_some_and(u8::is_ascii_digit);
        if numeric {
            return base[..index].to_owned();
        }
    }
    base.to_owned()
}

fn expression_signature(expression: &GeneratorExpr) -> String {
    match expression {
        GeneratorExpr::Anchor { anchor } => format!("@{anchor}"),
        GeneratorExpr::Cell { op, children } => {
            let family = logic_name(op);
            let mut children: Vec<_> = children.iter().map(expression_signature).collect();
            if matches!(
                family.as_str(),
                "AND2"
                    | "AND3"
                    | "AND4"
                    | "NAND2"
                    | "NAND3"
                    | "NAND4"
                    | "OR2"
                    | "OR3"
                    | "OR4"
                    | "NOR2"
                    | "NOR3"
                    | "NOR4"
                    | "XOR2"
                    | "XNOR2"
                    | "MAJ"
            ) {
                children.sort();
            } else if matches!(family.as_str(), "AO21" | "AOI21" | "OA21" | "OAI21")
                && children.len() >= 2
            {
                children[..2].sort();
            } else if matches!(family.as_str(), "AO22" | "AOI22" | "OA22" | "OAI22")
                && children.len() >= 4
            {
                children[..2].sort();
                children[2..4].sort();
                if (children[2].as_str(), children[3].as_str())
                    < (children[0].as_str(), children[1].as_str())
                {
                    children[..4].rotate_left(2);
                }
            }
            format!("{family}({})", children.join(","))
        }
    }
}

pub fn topology_signature(candidate: &GeneratorCandidate) -> String {
    let mut choices: Vec<_> = candidate
        .choices
        .iter()
        .map(|choice| {
            format!(
                "{}={}",
                choice.root_anchor,
                expression_signature(&choice.expression)
            )
        })
        .collect();
    choices.sort();
    choices.join(";")
}

pub fn provenance_class(candidate: &GeneratorCandidate) -> &'static str {
    if matches!(
        candidate.provenance.first().map(String::as_str),
        Some(
            "cone-collapse-deletion"
                | "fanout-aware-mffc-collapse"
                | "global-equivalence-elimination"
                | "global-complement-elimination"
                | "global-resubstitution-elimination"
                | "local-cut-equivalence-elimination"
                | "local-cut-complement-elimination"
                | "local-cut-resubstitution-elimination"
                | "bounded-functional-window-resubstitution"
                | "mapped-kfeasible-window-rewrite"
        )
    ) {
        "cone-collapse-deletion"
    } else if candidate
        .provenance
        .first()
        .is_some_and(|value| value.starts_with("explicit-divisor"))
        || !candidate.divisors.is_empty()
    {
        "divisor-resubstitution"
    } else if candidate.provenance.first().map(String::as_str) == Some("multi-output-shared-dag") {
        "multi-output-shared-dag"
    } else if candidate
        .provenance
        .iter()
        .any(|value| value.starts_with("reconvergent-"))
    {
        "reconvergent-shared-host"
    } else {
        "multi-output-shared-host"
    }
}

pub fn specific_window(candidate: &GeneratorCandidate) -> String {
    if matches!(
        candidate.provenance.first().map(String::as_str),
        Some(
            "cone-collapse-deletion"
                | "fanout-aware-mffc-collapse"
                | "global-equivalence-elimination"
                | "global-complement-elimination"
                | "global-resubstitution-elimination"
                | "local-cut-equivalence-elimination"
                | "local-cut-complement-elimination"
                | "local-cut-resubstitution-elimination"
                | "bounded-functional-window-resubstitution"
                | "mapped-kfeasible-window-rewrite"
        )
    ) {
        return candidate.provenance.get(1).cloned().unwrap_or_default();
    }
    candidate
        .provenance
        .iter()
        .find(|value| {
            let Some(rest) = value.strip_prefix('S') else {
                return false;
            };
            rest.split_once('_').is_some_and(|(seed, suffix)| {
                seed.bytes().all(|byte| byte.is_ascii_digit()) && suffix.contains("_H")
            })
        })
        .cloned()
        .or_else(|| candidate.provenance.first().cloned())
        .unwrap_or_default()
}

fn read_liberty_areas(text: &str) -> Result<BTreeMap<String, f64>> {
    let cell_re = Regex::new(r"\bcell\s*\(([^)]+)\)\s*\{")?;
    let area_re = Regex::new(r"\barea\s*:\s*([0-9.eE+-]+)")?;
    let matches: Vec<_> = cell_re.captures_iter(text).collect();
    let mut result = BTreeMap::new();
    for (index, capture) in matches.iter().enumerate() {
        let start = capture.get(0).unwrap().end();
        let end = matches
            .get(index + 1)
            .map_or(text.len(), |next| next.get(0).unwrap().start());
        if let Some(area) = area_re.captures(&text[start..end]) {
            if let Ok(value) = area[1].parse() {
                result.insert(capture[1].trim().to_owned(), value);
            }
        }
    }
    Ok(result)
}

fn read_liberty_delays(text: &str) -> Result<BTreeMap<String, f64>> {
    let cell_re = Regex::new(r"\bcell\s*\(([^)]+)\)\s*\{")?;
    let table_re = Regex::new(r"(?s)(?:cell_rise|cell_fall)\s*\([^)]*\)\s*\{(.*?)\n\s*\}")?;
    let values_re = Regex::new(r"(?s)values\s*\((.*?)\)\s*;")?;
    let number_re = Regex::new(r"[-+]?(?:\d+\.?\d*|\.\d+)(?:[eE][-+]?\d+)?")?;
    let matches: Vec<_> = cell_re.captures_iter(text).collect();
    let mut result = BTreeMap::new();
    for (index, capture) in matches.iter().enumerate() {
        let start = capture.get(0).unwrap().end();
        let end = matches
            .get(index + 1)
            .map_or(text.len(), |next| next.get(0).unwrap().start());
        let mut values = Vec::new();
        for table in table_re.captures_iter(&text[start..end]) {
            for block in values_re.captures_iter(&table[1]) {
                values.extend(
                    number_re
                        .find_iter(&block[1])
                        .filter_map(|token| token.as_str().parse::<f64>().ok())
                        .filter(|value| value.is_finite()),
                );
            }
        }
        values.sort_by(f64::total_cmp);
        if let Some(value) = values.get(values.len() / 2) {
            result.insert(capture[1].trim().to_owned(), *value);
        }
    }
    Ok(result)
}

fn technology_maps(
    areas: &BTreeMap<String, f64>,
    delays: &BTreeMap<String, f64>,
) -> BTreeMap<String, BTreeMap<String, TechCell>> {
    let mut grouped: BTreeMap<String, Vec<TechCell>> = BTreeMap::new();
    for (cell, area) in areas {
        let family = logic_name(cell);
        if FAMILIES.iter().any(|(name, _)| *name == family) {
            if let Some(delay) = delays.get(cell) {
                grouped.entry(family).or_default().push(TechCell {
                    name: cell.clone(),
                    area: *area,
                    delay: *delay,
                });
            }
        }
    }
    let mut area_map = BTreeMap::new();
    let mut timing_map = BTreeMap::new();
    let mut balanced_map = BTreeMap::new();
    for (family, choices) in grouped {
        let area = choices
            .iter()
            .min_by(|left, right| {
                left.area
                    .total_cmp(&right.area)
                    .then_with(|| left.delay.total_cmp(&right.delay))
                    .then_with(|| left.name.cmp(&right.name))
            })
            .unwrap()
            .clone();
        let timing = choices
            .iter()
            .min_by(|left, right| {
                left.delay
                    .total_cmp(&right.delay)
                    .then_with(|| left.area.total_cmp(&right.area))
                    .then_with(|| left.name.cmp(&right.name))
            })
            .unwrap()
            .clone();
        let min_area = choices
            .iter()
            .map(|cell| cell.area)
            .fold(f64::INFINITY, f64::min);
        let min_delay = choices
            .iter()
            .map(|cell| cell.delay)
            .fold(f64::INFINITY, f64::min);
        let balanced = choices
            .iter()
            .min_by(|left, right| {
                let left_score =
                    left.area / min_area.max(1e-12) + left.delay / min_delay.max(1e-12);
                let right_score =
                    right.area / min_area.max(1e-12) + right.delay / min_delay.max(1e-12);
                left_score
                    .total_cmp(&right_score)
                    .then_with(|| left.area.total_cmp(&right.area))
                    .then_with(|| left.delay.total_cmp(&right.delay))
                    .then_with(|| left.name.cmp(&right.name))
            })
            .unwrap()
            .clone();
        area_map.insert(family.clone(), area);
        timing_map.insert(family.clone(), timing);
        balanced_map.insert(family, balanced);
    }
    BTreeMap::from([
        ("area".to_owned(), area_map),
        ("balanced".to_owned(), balanced_map),
        ("timing".to_owned(), timing_map),
    ])
}

fn technology_context(liberty_path: &Path) -> Result<Arc<TechnologyContext>> {
    let metadata = fs::metadata(liberty_path)
        .with_context(|| format!("read Liberty metadata {}", liberty_path.display()))?;
    let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let modified = modified.duration_since(UNIX_EPOCH).unwrap_or_default();
    let key = TechnologyCacheKey {
        path: fs::canonicalize(liberty_path).unwrap_or_else(|_| liberty_path.to_path_buf()),
        length: metadata.len(),
        modified_sec: modified.as_secs(),
        modified_nsec: modified.subsec_nanos(),
    };
    let cache = TECHNOLOGY_CONTEXT_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Some(found) = cache.lock().unwrap().get(&key).cloned() {
        return Ok(found);
    }

    let liberty = fs::read_to_string(liberty_path)?;
    let areas = read_liberty_areas(&liberty)?;
    let delays = read_liberty_delays(&liberty)?;
    let mappings = technology_maps(&areas, &delays);
    let context = Arc::new(TechnologyContext {
        areas,
        delays,
        mappings,
    });
    cache.lock().unwrap().insert(key, Arc::clone(&context));
    Ok(context)
}

fn variable_truth(index: usize, inputs: usize) -> Truth {
    let mut truth = Truth::default();
    for assignment in 0..(1usize << inputs) {
        if assignment & (1 << index) != 0 {
            truth.set(assignment);
        }
    }
    truth
}

fn apply_family(family: &str, values: &[Truth], bits: usize) -> Result<Truth> {
    let result = match family {
        "INV" => values[0].not(bits),
        "AND2" => values[0].and(values[1]),
        "NAND2" => values[0].and(values[1]).not(bits),
        "OR2" => values[0].or(values[1]),
        "NOR2" => values[0].or(values[1]).not(bits),
        "XOR2" => values[0].xor(values[1]),
        "XNOR2" => values[0].xor(values[1]).not(bits),
        "MAJ" => values[0]
            .and(values[1])
            .or(values[0].and(values[2]))
            .or(values[1].and(values[2])),
        "MAJI" => apply_family("MAJ", values, bits)?.not(bits),
        "AO21" => values[0].and(values[1]).or(values[2]),
        "AOI21" => apply_family("AO21", values, bits)?.not(bits),
        "OA21" => values[0].or(values[1]).and(values[2]),
        "OAI21" => apply_family("OA21", values, bits)?.not(bits),
        "AO22" => values[0].and(values[1]).or(values[2].and(values[3])),
        "AOI22" => apply_family("AO22", values, bits)?.not(bits),
        "OA22" => values[0].or(values[1]).and(values[2].or(values[3])),
        "OAI22" => apply_family("OA22", values, bits)?.not(bits),
        "O2A1O1I" => values[0]
            .not(bits)
            .and(values[1].not(bits))
            .and(values[3].not(bits))
            .or(values[2].not(bits).and(values[3].not(bits))),
        _ => bail!("unsupported generated family {family}"),
    };
    Ok(result)
}

fn canonical_children<'a>(family: &str, children: &mut Vec<&'a Term>) {
    let sort = |values: &mut [&Term]| values.sort_by(|left, right| left.text.cmp(&right.text));
    match family {
        "AND2" | "NAND2" | "OR2" | "NOR2" | "XOR2" | "XNOR2" | "MAJ" | "MAJI" => sort(children),
        "AO21" | "AOI21" | "OA21" | "OAI21" => sort(&mut children[..2]),
        "AO22" | "AOI22" | "OA22" | "OAI22" => {
            sort(&mut children[..2]);
            sort(&mut children[2..]);
            if (children[2].text.as_str(), children[3].text.as_str())
                < (children[0].text.as_str(), children[1].text.as_str())
            {
                children.rotate_left(2);
            }
        }
        _ => {}
    }
}

fn make_gate(
    family: &str,
    raw_children: &[&Term],
    cells: &BTreeMap<String, TechCell>,
    bits: usize,
) -> Result<Term> {
    let mut children = raw_children.to_vec();
    canonical_children(family, &mut children);
    let tech = cells.get(family).context("missing technology family")?;
    let truth = apply_family(
        family,
        &children.iter().map(|child| child.truth).collect::<Vec<_>>(),
        bits,
    )?;
    let mut anchors = BTreeSet::new();
    let mut families = BTreeSet::from([family.to_owned()]);
    for child in &children {
        anchors.extend(child.anchors.iter().copied());
        families.extend(child.families.iter().cloned());
    }
    Ok(Term {
        truth,
        expression: GeneratorExpr::Cell {
            op: tech.name.clone(),
            children: children
                .iter()
                .map(|child| child.expression.clone())
                .collect(),
        },
        text: format!(
            "{}({})",
            tech.name,
            children
                .iter()
                .map(|child| child.text.as_str())
                .collect::<Vec<_>>()
                .join(",")
        ),
        area: tech.area + children.iter().map(|child| child.area).sum::<f64>(),
        depth: 1 + children.iter().map(|child| child.depth).max().unwrap_or(0),
        anchors,
        families,
    })
}

fn for_each_product<'a>(
    items: &'a [Term],
    arity: usize,
    combinations: bool,
    mut visit: impl FnMut(&[&'a Term]) -> Result<()>,
) -> Result<()> {
    fn walk<'a>(
        items: &'a [Term],
        arity: usize,
        combinations: bool,
        start: usize,
        current: &mut Vec<&'a Term>,
        visit: &mut dyn FnMut(&[&'a Term]) -> Result<()>,
    ) -> Result<()> {
        if current.len() == arity {
            return visit(current);
        }
        for index in if combinations {
            start..items.len()
        } else {
            0..items.len()
        } {
            current.push(&items[index]);
            walk(items, arity, combinations, index, current, visit)?;
            current.pop();
        }
        Ok(())
    }
    walk(
        items,
        arity,
        combinations,
        0,
        &mut Vec::with_capacity(arity),
        &mut visit,
    )
}

fn one_gate_terms(
    terminals: &[Term],
    cells: &BTreeMap<String, TechCell>,
    bits: usize,
    limit: usize,
) -> Result<Vec<Term>> {
    let mut unique: HashMap<(Truth, String, Vec<usize>), Term> = HashMap::new();
    for (family, arity) in FAMILIES {
        if !cells.contains_key(family) {
            continue;
        }
        let combinations = matches!(
            family,
            "AND2" | "NAND2" | "OR2" | "NOR2" | "XOR2" | "XNOR2" | "MAJ" | "MAJI"
        );
        for_each_product(terminals, arity, combinations, |children| {
            let term = make_gate(family, children, cells, bits)?;
            let key = (
                term.truth,
                family.to_owned(),
                term.anchors.iter().copied().collect(),
            );
            let replace = unique.get(&key).is_none_or(|previous| {
                (term.area, term.depth, term.text.as_str())
                    < (previous.area, previous.depth, previous.text.as_str())
            });
            if replace {
                unique.insert(key, term);
            }
            Ok(())
        })?;
    }
    let mut result: Vec<_> = unique.into_values().collect();
    result.sort_by(|left, right| {
        left.area
            .total_cmp(&right.area)
            .then_with(|| left.depth.cmp(&right.depth))
            .then_with(|| left.text.cmp(&right.text))
    });
    let mut diverse = Vec::new();
    let mut seen_truth = HashSet::new();
    for term in &result {
        if seen_truth.insert(term.truth) {
            diverse.push(term.clone());
        }
    }
    if diverse.len() < limit {
        let mut used: HashSet<_> = diverse.iter().map(|term| term.text.clone()).collect();
        for term in result {
            if used.insert(term.text.clone()) {
                diverse.push(term);
            }
        }
    }
    diverse.truncate(limit);
    Ok(diverse)
}

fn top_gate_using(
    special: &Term,
    terminals: &[Term],
    target: Truth,
    cells: &BTreeMap<String, TechCell>,
    bits: usize,
    limit: usize,
) -> Result<Vec<Term>> {
    let mut matches = BTreeMap::new();
    for (family, arity) in FAMILIES {
        if !cells.contains_key(family) {
            continue;
        }
        for position in 0..arity {
            for_each_product(terminals, arity - 1, false, |rest| {
                let mut children = rest.to_vec();
                children.insert(position, special);
                if apply_family(
                    family,
                    &children.iter().map(|child| child.truth).collect::<Vec<_>>(),
                    bits,
                )? == target
                {
                    let term = make_gate(family, &children, cells, bits)?;
                    matches.entry(term.text.clone()).or_insert(term);
                }
                Ok(())
            })?;
        }
    }
    let mut result: Vec<_> = matches.into_values().collect();
    result.sort_by(|left, right| {
        left.area
            .total_cmp(&right.area)
            .then_with(|| left.depth.cmp(&right.depth))
            .then_with(|| left.text.cmp(&right.text))
    });
    result.truncate(limit);
    Ok(result)
}

impl TopGateCache {
    fn lookup(
        &mut self,
        special: &Term,
        terminals: &[Term],
        target: Truth,
        cells: &BTreeMap<String, TechCell>,
        bits: usize,
        limit: usize,
    ) -> Result<Vec<Term>> {
        // `cells` is immutable for the lifetime of this per-generation cache,
        // so it is deliberately not repeated in every key.  The former key
        // formatted the entire technology map thousands of times per round.
        let key = TopGateKey {
            special: special.into(),
            terminals: terminals.iter().map(TermCacheKey::from).collect(),
            target,
            bits,
            limit,
        };
        if let Some(result) = self.entries.get(&key) {
            self.hits += 1;
            return Ok(result.clone());
        }
        self.misses += 1;
        let result = top_gate_using(special, terminals, target, cells, bits, limit)?;
        self.entries.insert(key, result.clone());
        Ok(result)
    }
}

fn two_gate_target_terms(
    terminals: &[Term],
    target: Truth,
    cells: &BTreeMap<String, TechCell>,
    bits: usize,
    limit: usize,
    cache: &mut TopGateCache,
) -> Result<Vec<Term>> {
    let mut matches = BTreeMap::new();
    for inner in one_gate_terms(terminals, cells, bits, 64)? {
        for term in cache.lookup(&inner, terminals, target, cells, bits, 2)? {
            matches.entry(term.text.clone()).or_insert(term);
        }
    }
    let mut result: Vec<_> = matches.into_values().collect();
    result.sort_by(|left, right| {
        left.area
            .total_cmp(&right.area)
            .then_with(|| left.depth.cmp(&right.depth))
            .then_with(|| left.text.cmp(&right.text))
    });
    result.truncate(limit);
    Ok(result)
}

fn keep_cheapest_truth(map: &mut HashMap<Truth, Term>, term: Term) {
    let replace = map.get(&term.truth).is_none_or(|previous| {
        (term.area, term.depth, term.text.as_str())
            < (previous.area, previous.depth, previous.text.as_str())
    });
    if replace {
        map.insert(term.truth, term);
    }
}

fn keep_bounded_truth_pareto(
    map: &mut HashMap<Truth, Vec<Term>>,
    term: Term,
    per_truth_limit: usize,
) {
    let alternatives = map.entry(term.truth).or_default();
    if alternatives.iter().any(|previous| {
        previous.area <= term.area + 1e-12
            && previous.depth <= term.depth
            && previous.anchors.is_subset(&term.anchors)
    }) {
        return;
    }
    alternatives.retain(|previous| {
        !(term.area <= previous.area + 1e-12
            && term.depth <= previous.depth
            && term.anchors.is_subset(&previous.anchors))
    });
    alternatives.push(term);
    alternatives.sort_by(|left, right| {
        left.area
            .total_cmp(&right.area)
            .then_with(|| left.anchors.len().cmp(&right.anchors.len()))
            .then_with(|| left.depth.cmp(&right.depth))
            .then_with(|| left.text.cmp(&right.text))
    });
    alternatives.truncate(per_truth_limit);
}

fn flatten_bounded_truth_pareto(map: HashMap<Truth, Vec<Term>>, limit: usize) -> Vec<Term> {
    let mut result: Vec<_> = map.into_values().flatten().collect();
    result.sort_by(|left, right| {
        left.area
            .total_cmp(&right.area)
            .then_with(|| left.anchors.len().cmp(&right.anchors.len()))
            .then_with(|| left.depth.cmp(&right.depth))
            .then_with(|| left.text.cmp(&right.text))
    });
    result.truncate(limit);
    result
}

/// Discover a small mapped DAG over existing divisors without turning the
/// outer EGG search into a functional-resubstitution loop.
///
/// The first level is restricted to binary library families.  The second
/// level may use any supported 2/3/4-input family, but its raw-divisor pool is
/// deterministically narrowed as arity grows.  All limits are fixed across
/// objectives and designs; callers still perform an independent macro proof,
/// physical P1 evaluation and conditional A2/O1 before acceptance.
fn bounded_functional_target_terms(
    divisors: &[Term],
    target: Truth,
    cells: &BTreeMap<String, TechCell>,
    bits: usize,
    max_inner_divisors: usize,
    max_intermediate_signatures: usize,
    max_results: usize,
) -> Result<Vec<Term>> {
    let inner_divisors = &divisors[..divisors.len().min(max_inner_divisors)];
    let mut matches = BTreeMap::<String, Term>::new();

    // Zero- and one-cell resubstitution are the highest-value semantic moves:
    // they can remove a whole MFFC while preserving one nearby divisor.  The
    // arity-dependent pools are fixed complexity caps, not design tuning.
    for divisor in divisors.iter().filter(|term| term.truth == target) {
        matches
            .entry(divisor.text.clone())
            .or_insert_with(|| divisor.clone());
    }
    for (family, arity) in FAMILIES {
        if !cells.contains_key(family) {
            continue;
        }
        let raw_limit = match arity {
            1 => divisors.len().min(128),
            2 => divisors.len().min(64),
            3 => divisors.len().min(12),
            4 => divisors.len().min(6),
            _ => continue,
        };
        let raw = &divisors[..raw_limit];
        let combinations = matches!(
            family,
            "AND2" | "NAND2" | "OR2" | "NOR2" | "XOR2" | "XNOR2" | "MAJ" | "MAJI"
        );
        for_each_product(raw, arity, combinations, |children| {
            if apply_family(
                family,
                &children.iter().map(|child| child.truth).collect::<Vec<_>>(),
                bits,
            )? == target
            {
                let term = make_gate(family, children, cells, bits)?;
                matches.entry(term.text.clone()).or_insert(term);
            }
            Ok(())
        })?;
    }

    let mut intermediate_by_truth = HashMap::<Truth, Vec<Term>>::new();
    for (left_index, left) in inner_divisors.iter().enumerate() {
        for right in inner_divisors.iter().skip(left_index) {
            for (family, arity) in FAMILIES {
                if arity != 2 || !cells.contains_key(family) {
                    continue;
                }
                keep_bounded_truth_pareto(
                    &mut intermediate_by_truth,
                    make_gate(family, &[left, right], cells, bits)?,
                    3,
                );
            }
        }
    }
    let intermediates =
        flatten_bounded_truth_pareto(intermediate_by_truth, max_intermediate_signatures);

    for intermediate in &intermediates {
        for (family, arity) in FAMILIES {
            if arity < 2 || !cells.contains_key(family) {
                continue;
            }
            let raw_limit = match arity {
                2 => divisors.len().min(64),
                3 => divisors.len().min(12),
                4 => divisors.len().min(6),
                _ => continue,
            };
            let raw = &divisors[..raw_limit];
            for position in 0..arity {
                for_each_product(raw, arity - 1, false, |rest| {
                    let mut children = rest.to_vec();
                    children.insert(position, intermediate);
                    if apply_family(
                        family,
                        &children.iter().map(|child| child.truth).collect::<Vec<_>>(),
                        bits,
                    )? != target
                    {
                        return Ok(());
                    }
                    let term = make_gate(family, &children, cells, bits)?;
                    // `intermediate` is used exactly once, so every retained
                    // expression is a genuine two-level mapped DAG rather
                    // than a rediscovery of the existing direct lane.
                    if term.depth == 2 {
                        matches.entry(term.text.clone()).or_insert(term);
                    }
                    Ok(())
                })?;
            }
        }
    }

    // A second bounded layer covers the two common three-cell shapes that a
    // one-intermediate chain cannot express: two independently useful
    // branches feeding one binary cell, and a three-cell chain.  Retaining
    // one cheapest term per signature keeps this a small candidate oracle.
    for (left_index, left) in intermediates.iter().enumerate() {
        for right in intermediates.iter().skip(left_index) {
            for (family, arity) in FAMILIES {
                if arity != 2 || !cells.contains_key(family) {
                    continue;
                }
                if apply_family(family, &[left.truth, right.truth], bits)? == target {
                    let term = make_gate(family, &[left, right], cells, bits)?;
                    matches.entry(term.text.clone()).or_insert(term);
                }
            }
        }
    }

    let chain_raw = &divisors[..divisors.len().min(32)];
    let mut second_by_truth = HashMap::<Truth, Vec<Term>>::new();
    for intermediate in &intermediates {
        for raw in chain_raw {
            for (family, arity) in FAMILIES {
                if arity != 2 || !cells.contains_key(family) {
                    continue;
                }
                keep_bounded_truth_pareto(
                    &mut second_by_truth,
                    make_gate(family, &[intermediate, raw], cells, bits)?,
                    3,
                );
            }
        }
    }
    let second = flatten_bounded_truth_pareto(second_by_truth, max_intermediate_signatures);
    for intermediate in &second {
        for raw in &divisors[..divisors.len().min(64)] {
            for (family, arity) in FAMILIES {
                if arity != 2 || !cells.contains_key(family) {
                    continue;
                }
                if apply_family(family, &[intermediate.truth, raw.truth], bits)? == target {
                    let term = make_gate(family, &[intermediate, raw], cells, bits)?;
                    matches.entry(term.text.clone()).or_insert(term);
                }
            }
        }
    }

    // Fixed-width balanced joins cover the four- and five-cell shapes missed
    // by a pure chain: a two-cell term plus a one-cell term, or two two-cell
    // terms, feeding one binary mapped cell.  The explicit cell-count guard
    // keeps this a bounded recipe beam.
    let join_one = &intermediates[..intermediates.len().min(96)];
    let join_two = &second[..second.len().min(96)];
    for left in join_two {
        for right in join_one {
            for (family, arity) in FAMILIES {
                if arity != 2 || !cells.contains_key(family) {
                    continue;
                }
                if apply_family(family, &[left.truth, right.truth], bits)? == target {
                    let term = make_gate(family, &[left, right], cells, bits)?;
                    if expression_cell_count(&term.expression) <= 5 {
                        matches.entry(term.text.clone()).or_insert(term);
                    }
                }
            }
        }
    }
    for (left_index, left) in join_two.iter().enumerate() {
        for right in join_two.iter().skip(left_index) {
            for (family, arity) in FAMILIES {
                if arity != 2 || !cells.contains_key(family) {
                    continue;
                }
                if apply_family(family, &[left.truth, right.truth], bits)? == target {
                    let term = make_gate(family, &[left, right], cells, bits)?;
                    if expression_cell_count(&term.expression) <= 5 {
                        matches.entry(term.text.clone()).or_insert(term);
                    }
                }
            }
        }
    }

    // Continue a fixed-width mapped chain to cover functional replacements
    // up to five cells.  Each signature retains a few dependency-distinct
    // realizations so a cheap term that protects a large old cone cannot hide
    // a slightly larger term with better net resource deletion.
    let chain_divisors = &divisors[..divisors.len().min(48)];
    let mut frontier = second;
    for _level in 3..=5 {
        let mut next_by_truth = HashMap::<Truth, Vec<Term>>::new();
        for intermediate in &frontier {
            for raw in chain_divisors {
                for (family, arity) in FAMILIES {
                    if arity != 2 || !cells.contains_key(family) {
                        continue;
                    }
                    let term = make_gate(family, &[intermediate, raw], cells, bits)?;
                    if term.truth == target {
                        matches.entry(term.text.clone()).or_insert(term.clone());
                    }
                    keep_bounded_truth_pareto(&mut next_by_truth, term, 3);
                }
            }
        }
        frontier = flatten_bounded_truth_pareto(next_by_truth, max_intermediate_signatures);
        if frontier.is_empty() {
            break;
        }
    }
    let mut result: Vec<_> = matches.into_values().collect();
    result.sort_by(|left, right| {
        left.area
            .total_cmp(&right.area)
            .then_with(|| left.depth.cmp(&right.depth))
            .then_with(|| left.text.cmp(&right.text))
    });
    result.truncate(max_results);
    Ok(result)
}

/// Enumerate one new top cell that uses `special` exactly once and otherwise
/// uses primary cut terminals.  This is the small, technology-independent
/// dynamic-programming primitive behind the mapped-recipe atlas below.
fn for_each_top_gate_using(
    special: &Term,
    terminals: &[Term],
    cells: &BTreeMap<String, TechCell>,
    bits: usize,
    mut visit: impl FnMut(Term),
) -> Result<()> {
    for (family, arity) in FAMILIES {
        if !cells.contains_key(family) {
            continue;
        }
        for position in 0..arity {
            for_each_product(terminals, arity - 1, false, |rest| {
                let mut children = rest.to_vec();
                children.insert(position, special);
                visit(make_gate(family, &children, cells, bits)?);
                Ok(())
            })?;
        }
    }
    Ok(())
}

fn mapped_recipe_cache_key(inputs: usize, cells: &BTreeMap<String, TechCell>) -> String {
    let mut key = format!("inputs={inputs}");
    for (family, cell) in cells {
        key.push_str(&format!(
            "|{family}:{}:{:016x}:{:016x}",
            cell.name,
            cell.area.to_bits(),
            cell.delay.to_bits()
        ));
    }
    key
}

type MappedRecipeAtlas = HashMap<Truth, Vec<Term>>;
static MAPPED_RECIPE_CACHE: OnceLock<Mutex<HashMap<String, Arc<MappedRecipeAtlas>>>> =
    OnceLock::new();
type DeepRecipeKey = (String, Truth);
static MAPPED_DEEP_RECIPE_CACHE: OnceLock<Mutex<HashMap<DeepRecipeKey, Arc<Vec<Term>>>>> =
    OnceLock::new();

/// Build a PMO-style mapped recipe database for one canonical small cut.
///
/// The atlas is shared across every root and fresh-parent round with the same
/// Liberty mapping.  It retains up to three minimum-area realizations for each
/// Boolean function and covers one-, two- and three-cell trees.  The search is
/// deliberately expressed in terms of cut arity, Boolean function and
/// technology costs; it does not inspect a benchmark or objective metric name.
fn mapped_recipe_atlas(
    inputs: usize,
    cells: &BTreeMap<String, TechCell>,
) -> Result<Arc<MappedRecipeAtlas>> {
    anyhow::ensure!((1..=4).contains(&inputs));
    let key = mapped_recipe_cache_key(inputs, cells);
    let cache = MAPPED_RECIPE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(found) = cache.lock().unwrap().get(&key).cloned() {
        return Ok(found);
    }

    let bits = 1usize << inputs;
    let terminals: Vec<_> = (0..inputs)
        .map(|index| Term {
            truth: variable_truth(index, inputs),
            expression: GeneratorExpr::Anchor { anchor: index },
            text: format!("@{index}"),
            area: 0.0,
            depth: 0,
            anchors: BTreeSet::from([index]),
            families: BTreeSet::new(),
        })
        .collect();

    let mut one = HashMap::new();
    for term in one_gate_terms(&terminals, cells, bits, usize::MAX)? {
        keep_cheapest_truth(&mut one, term);
    }
    let mut one_terms: Vec<_> = one.into_values().collect();
    one_terms.sort_by(|left, right| left.text.cmp(&right.text));

    let mut two = HashMap::new();
    for inner in &one_terms {
        for_each_top_gate_using(inner, &terminals, cells, bits, |term| {
            keep_cheapest_truth(&mut two, term);
        })?;
    }
    let mut two_terms: Vec<_> = two.into_values().collect();
    two_terms.sort_by(|left, right| left.text.cmp(&right.text));

    let mut three = HashMap::new();
    // A two-cell branch followed by one top cell.
    for inner in &two_terms {
        for_each_top_gate_using(inner, &terminals, cells, bits, |term| {
            keep_cheapest_truth(&mut three, term);
        })?;
    }
    // Two independently useful one-cell branches followed by a symmetric
    // binary top cell.  This covers the non-chain three-cell trees without
    // enumerating arbitrary larger expression forests.
    for (left_index, left) in one_terms.iter().enumerate() {
        for right in one_terms.iter().skip(left_index) {
            for (family, arity) in FAMILIES {
                if arity != 2 || !cells.contains_key(family) {
                    continue;
                }
                keep_cheapest_truth(&mut three, make_gate(family, &[left, right], cells, bits)?);
            }
        }
    }

    let mut atlas: MappedRecipeAtlas = HashMap::new();
    for term in one_terms
        .into_iter()
        .chain(two_terms)
        .chain(three.into_values())
    {
        let entries = atlas.entry(term.truth).or_default();
        if entries.iter().any(|previous| previous.text == term.text) {
            continue;
        }
        entries.push(term);
        entries.sort_by(|left, right| {
            left.area
                .total_cmp(&right.area)
                .then_with(|| left.depth.cmp(&right.depth))
                .then_with(|| left.text.cmp(&right.text))
        });
        entries.truncate(3);
    }
    let atlas = Arc::new(atlas);
    cache.lock().unwrap().insert(key, Arc::clone(&atlas));
    Ok(atlas)
}

/// Target-directed extension of the canonical atlas for four- and five-cell
/// mapped expressions.  Searching only truths encountered on real cuts avoids
/// constructing an exhaustive high-depth database while keeping the beam,
/// depth and retained-result limits fixed for every design and objective.
fn mapped_deep_recipes(
    inputs: usize,
    target: Truth,
    cells: &BTreeMap<String, TechCell>,
) -> Result<Arc<Vec<Term>>> {
    anyhow::ensure!((1..=4).contains(&inputs));
    let key = (mapped_recipe_cache_key(inputs, cells), target);
    let cache = MAPPED_DEEP_RECIPE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(found) = cache.lock().unwrap().get(&key).cloned() {
        return Ok(found);
    }
    let bits = 1usize << inputs;
    let terminals: Vec<_> = (0..inputs)
        .map(|index| Term {
            truth: variable_truth(index, inputs),
            expression: GeneratorExpr::Anchor { anchor: index },
            text: format!("@{index}"),
            area: 0.0,
            depth: 0,
            anchors: BTreeSet::from([index]),
            families: BTreeSet::new(),
        })
        .collect();
    let mut recipes =
        bounded_functional_target_terms(&terminals, target, cells, bits, inputs, 256, 32)?;
    recipes.retain(|term| (4..=5).contains(&expression_cell_count(&term.expression)));
    recipes.sort_by(|left, right| {
        left.area
            .total_cmp(&right.area)
            .then_with(|| left.depth.cmp(&right.depth))
            .then_with(|| left.text.cmp(&right.text))
    });
    recipes.truncate(3);
    let recipes = Arc::new(recipes);
    cache.lock().unwrap().insert(key, Arc::clone(&recipes));
    Ok(recipes)
}

fn bind_recipe_expression(expression: &GeneratorExpr, boundary: &[usize]) -> GeneratorExpr {
    match expression {
        GeneratorExpr::Anchor { anchor } => GeneratorExpr::Anchor {
            anchor: boundary[*anchor],
        },
        GeneratorExpr::Cell { op, children } => GeneratorExpr::Cell {
            op: op.clone(),
            children: children
                .iter()
                .map(|child| bind_recipe_expression(child, boundary))
                .collect(),
        },
    }
}

fn gate_truth(name: &str, values: &[bool]) -> Option<bool> {
    let family = logic_name(name);
    for prefix in ["NAND", "AND", "NOR", "OR"] {
        if family
            .strip_prefix(prefix)
            .is_some_and(|tail| !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit()))
        {
            let base = if prefix.ends_with("AND") {
                values.iter().all(|value| *value)
            } else {
                values.iter().any(|value| *value)
            };
            return Some(if prefix.starts_with('N') { !base } else { base });
        }
    }
    Some(match family.as_str() {
        value if value.starts_with("INV") => !values[0],
        value if value.starts_with("BUF") => values[0],
        "XOR2" => values[0] ^ values[1],
        "XNOR2" => !(values[0] ^ values[1]),
        "MAJ" => values[..3].iter().filter(|value| **value).count() >= 2,
        "MAJI" => values[..3].iter().filter(|value| **value).count() < 2,
        "AO21" => (values[0] && values[1]) || values[2],
        "AOI21" => !((values[0] && values[1]) || values[2]),
        "OA21" => (values[0] || values[1]) && values[2],
        "OAI21" => !((values[0] || values[1]) && values[2]),
        "AO22" => (values[0] && values[1]) || (values[2] && values[3]),
        "AOI22" => !((values[0] && values[1]) || (values[2] && values[3])),
        "OA22" => (values[0] || values[1]) && (values[2] || values[3]),
        "OAI22" => !((values[0] || values[1]) && (values[2] || values[3])),
        "O2A1O1I" => (!values[0] && !values[1] && !values[3]) || (!values[2] && !values[3]),
        _ => return None,
    })
}

fn eval_anchor(
    anchor: usize,
    assignment: usize,
    boundary: &HashMap<usize, usize>,
    nodes: &HashMap<usize, SourceNode>,
    memo: &mut HashMap<usize, bool>,
    forbidden: Option<&BTreeSet<usize>>,
) -> Option<bool> {
    if let Some(value) = memo.get(&anchor) {
        return Some(*value);
    }
    if let Some(index) = boundary.get(&anchor) {
        let value = assignment & (1 << index) != 0;
        memo.insert(anchor, value);
        return Some(value);
    }
    if forbidden.is_some_and(|items| items.contains(&anchor)) {
        return None;
    }
    let node = nodes.get(&anchor)?;
    if node.is_leaf || node.is_root {
        return None;
    }
    let values: Vec<_> = node
        .inputs
        .iter()
        .map(|child| eval_anchor(*child, assignment, boundary, nodes, memo, forbidden))
        .collect::<Option<_>>()?;
    let value = gate_truth(&node.op, &values)?;
    memo.insert(anchor, value);
    Some(value)
}

fn truth_of(
    anchor: usize,
    boundary: &[usize],
    nodes: &HashMap<usize, SourceNode>,
    forbidden: Option<&BTreeSet<usize>>,
) -> Option<Truth> {
    let index: HashMap<_, _> = boundary
        .iter()
        .enumerate()
        .map(|(position, anchor)| (*anchor, position))
        .collect();
    let mut result = Truth::default();
    for assignment in 0..(1usize << boundary.len()) {
        if eval_anchor(
            anchor,
            assignment,
            &index,
            nodes,
            &mut HashMap::new(),
            forbidden,
        )? {
            result.set(assignment);
        }
    }
    Some(result)
}

fn incumbent_expression(
    anchor: usize,
    region: &BTreeSet<usize>,
    nodes: &HashMap<usize, SourceNode>,
    replaced: usize,
    divisor: usize,
) -> GeneratorExpr {
    if anchor == replaced {
        return GeneratorExpr::Anchor { anchor: divisor };
    }
    if !region.contains(&anchor) {
        return GeneratorExpr::Anchor { anchor };
    }
    GeneratorExpr::Cell {
        op: nodes[&anchor].op.clone(),
        children: nodes[&anchor]
            .inputs
            .iter()
            .map(|child| incumbent_expression(*child, region, nodes, replaced, divisor))
            .collect(),
    }
}

fn expression_anchors(expression: &GeneratorExpr, result: &mut BTreeSet<usize>) {
    match expression {
        GeneratorExpr::Anchor { anchor } => {
            result.insert(*anchor);
        }
        GeneratorExpr::Cell { children, .. } => {
            for child in children {
                expression_anchors(child, result);
            }
        }
    }
}

fn expression_cell_count(expression: &GeneratorExpr) -> usize {
    match expression {
        GeneratorExpr::Anchor { .. } => 0,
        GeneratorExpr::Cell { children, .. } => {
            1 + children.iter().map(expression_cell_count).sum::<usize>()
        }
    }
}

fn planner_seeds(
    plan: &[PlannerRow],
    wanted_sizes: &BTreeSet<usize>,
) -> Result<Vec<(String, usize)>> {
    let regex = Regex::new(r"_S(\d+)_(\d+)$")?;
    let mut seeds = BTreeMap::new();
    for row in plan {
        if !wanted_sizes.contains(&row.region_size) {
            continue;
        }
        if let Some(found) = regex.captures(&row.region_id) {
            seeds.insert(format!("S{}_{}", &found[1], &found[2]), found[2].parse()?);
        }
    }
    Ok(seeds.into_iter().collect())
}

fn neighborhood(seed: usize, nodes: &HashMap<usize, SourceNode>, radius: usize) -> BTreeSet<usize> {
    let mut seen = BTreeSet::from([seed]);
    let mut queue = VecDeque::from([(seed, 0usize)]);
    while let Some((anchor, depth)) = queue.pop_front() {
        if depth == radius {
            continue;
        }
        let node = &nodes[&anchor];
        for other in node.inputs.iter().chain(&node.consumers) {
            if nodes.contains_key(other) && seen.insert(*other) {
                queue.push_back((*other, depth + 1));
            }
        }
    }
    seen
}

fn descendant_paths(
    start: usize,
    nodes: &HashMap<usize, SourceNode>,
    max_depth: usize,
) -> HashMap<usize, Vec<usize>> {
    let mut result = HashMap::from([(start, vec![start])]);
    let mut queue = VecDeque::from([(start, vec![start])]);
    while let Some((anchor, path)) = queue.pop_front() {
        if path.len() - 1 == max_depth {
            continue;
        }
        for consumer in &nodes[&anchor].consumers {
            if !nodes.contains_key(consumer)
                || nodes[consumer].is_root
                || result.contains_key(consumer)
            {
                continue;
            }
            let mut next = path.clone();
            next.push(*consumer);
            result.insert(*consumer, next.clone());
            queue.push_back((*consumer, next));
        }
    }
    result
}

fn discover_windows(
    plan: &[PlannerRow],
    wanted_sizes: &BTreeSet<usize>,
    nodes: &HashMap<usize, SourceNode>,
    max_region: usize,
    max_forward_depth: usize,
) -> Result<Vec<Window>> {
    let mut found: HashMap<Vec<usize>, Window> = HashMap::new();
    for (seed_label, seed) in planner_seeds(plan, wanted_sizes)? {
        for host in neighborhood(seed, nodes, 2) {
            if nodes[&host].is_leaf || nodes[&host].is_root {
                continue;
            }
            let consumers: Vec<_> = nodes[&host]
                .consumers
                .iter()
                .filter(|consumer| nodes.contains_key(consumer) && !nodes[consumer].is_root)
                .copied()
                .collect();
            if consumers.len() < 2 {
                continue;
            }
            let star: BTreeSet<_> = std::iter::once(host)
                .chain(consumers.iter().copied())
                .collect();
            if star.len() <= max_region {
                found
                    .entry(star.iter().copied().collect())
                    .or_insert(Window {
                        region_id: format!("{seed_label}_H{host}_STAR"),
                        region: star.clone(),
                        kind: "multi-output-fanout".to_owned(),
                        host,
                    });
            }
            let paths: HashMap<_, _> = consumers
                .iter()
                .map(|consumer| {
                    (
                        *consumer,
                        descendant_paths(*consumer, nodes, max_forward_depth),
                    )
                })
                .collect();
            let mut best: Option<((usize, usize, usize), BTreeSet<usize>, usize)> = None;
            for left_index in 0..consumers.len() {
                for right_index in left_index + 1..consumers.len() {
                    let left = consumers[left_index];
                    let right = consumers[right_index];
                    let common: BTreeSet<_> = paths[&left]
                        .keys()
                        .filter(|anchor| paths[&right].contains_key(anchor))
                        .copied()
                        .collect();
                    for reconvergence in common {
                        let region: BTreeSet<_> = std::iter::once(host)
                            .chain(consumers.iter().copied())
                            .chain(paths[&left][&reconvergence].iter().copied())
                            .chain(paths[&right][&reconvergence].iter().copied())
                            .collect();
                        if region.len() > max_region {
                            continue;
                        }
                        let score = (
                            region.len(),
                            paths[&left][&reconvergence].len()
                                + paths[&right][&reconvergence].len(),
                            reconvergence,
                        );
                        if best.as_ref().is_none_or(|current| score < current.0) {
                            best = Some((score, region, reconvergence));
                        }
                    }
                }
            }
            if let Some((_, region, reconvergence)) = best {
                if region != star {
                    found
                        .entry(region.iter().copied().collect())
                        .or_insert(Window {
                            region_id: format!("{seed_label}_H{host}_RECONV{reconvergence}"),
                            region,
                            kind: "reconvergent-multi-output".to_owned(),
                            host,
                        });
                }
            }
        }
    }
    let mut result: Vec<_> = found.into_values().collect();
    result.sort_by(|left, right| {
        left.region
            .len()
            .cmp(&right.region.len())
            .then_with(|| left.region_id.cmp(&right.region_id))
    });
    Ok(result)
}

fn remap_expression(
    expression: &GeneratorExpr,
    mapping: &BTreeMap<String, TechCell>,
) -> GeneratorExpr {
    match expression {
        GeneratorExpr::Anchor { anchor } => GeneratorExpr::Anchor { anchor: *anchor },
        GeneratorExpr::Cell { op, children } => {
            let family = logic_name(op);
            GeneratorExpr::Cell {
                op: mapping
                    .get(&family)
                    .map_or_else(|| op.clone(), |cell| cell.name.clone()),
                children: children
                    .iter()
                    .map(|child| remap_expression(child, mapping))
                    .collect(),
            }
        }
    }
}

fn expression_metrics(
    expression: &GeneratorExpr,
    areas: &BTreeMap<String, f64>,
    delays: &BTreeMap<String, f64>,
) -> Result<(f64, f64, usize, BTreeSet<String>)> {
    match expression {
        GeneratorExpr::Anchor { .. } => Ok((0.0, 0.0, 0, BTreeSet::new())),
        GeneratorExpr::Cell { op, children } => {
            let child_metrics: Vec<_> = children
                .iter()
                .map(|child| expression_metrics(child, areas, delays))
                .collect::<Result<_>>()?;
            let mut families = BTreeSet::from([logic_name(op)]);
            for (_, _, _, child_families) in &child_metrics {
                families.extend(child_families.iter().cloned());
            }
            Ok((
                areas
                    .get(op)
                    .copied()
                    .with_context(|| format!("area {op}"))?
                    + child_metrics.iter().map(|item| item.0).sum::<f64>(),
                delays
                    .get(op)
                    .copied()
                    .with_context(|| format!("delay {op}"))?
                    + child_metrics.iter().map(|item| item.1).fold(0.0, f64::max),
                1 + child_metrics.iter().map(|item| item.2).max().unwrap_or(0),
                families,
            ))
        }
    }
}

fn normalized_expression(expression: &GeneratorExpr) -> Value {
    match expression {
        GeneratorExpr::Anchor { anchor } => json!({"anchor":anchor,"kind":"anchor"}),
        GeneratorExpr::Cell { op, children } => json!({
            "children":children.iter().map(normalized_expression).collect::<Vec<_>>(),
            "kind":"cell",
            "op":logic_name(op),
        }),
    }
}

fn logical_signature(choices: &[GeneratorChoice]) -> Result<String> {
    let values: Vec<_> = choices
        .iter()
        .map(|choice| {
            json!({
                "expression":normalized_expression(&choice.expression),
                "root_anchor":choice.root_anchor,
            })
        })
        .collect();
    Ok(serde_json::to_string(&values)?)
}

fn candidate_metrics(
    choices: &[GeneratorChoice],
    areas: &BTreeMap<String, f64>,
    delays: &BTreeMap<String, f64>,
) -> Result<(f64, f64, usize, BTreeSet<String>)> {
    let roots: HashMap<_, _> = choices
        .iter()
        .map(|choice| (choice.root_anchor, &choice.expression))
        .collect();
    fn path(
        expression: &GeneratorExpr,
        roots: &HashMap<usize, &GeneratorExpr>,
        delays: &BTreeMap<String, f64>,
        memo: &mut HashMap<usize, (f64, usize)>,
        active: &mut BTreeSet<usize>,
    ) -> Result<(f64, usize)> {
        match expression {
            GeneratorExpr::Anchor { anchor } => {
                let Some(root) = roots.get(anchor) else {
                    return Ok((0.0, 0));
                };
                if let Some(value) = memo.get(anchor) {
                    return Ok(*value);
                }
                anyhow::ensure!(active.insert(*anchor), "cyclic shared macro");
                let value = path(root, roots, delays, memo, active)?;
                active.remove(anchor);
                memo.insert(*anchor, value);
                Ok(value)
            }
            GeneratorExpr::Cell { op, children } => {
                let child_values: Vec<_> = children
                    .iter()
                    .map(|child| path(child, roots, delays, memo, active))
                    .collect::<Result<_>>()?;
                Ok((
                    delays
                        .get(op)
                        .copied()
                        .with_context(|| format!("delay {op}"))?
                        + child_values.iter().map(|item| item.0).fold(0.0, f64::max),
                    1 + child_values.iter().map(|item| item.1).max().unwrap_or(0),
                ))
            }
        }
    }
    let local: Vec<_> = choices
        .iter()
        .map(|choice| expression_metrics(&choice.expression, areas, delays))
        .collect::<Result<_>>()?;
    let mut memo = HashMap::new();
    let paths: Vec<_> = choices
        .iter()
        .map(|choice| {
            path(
                &choice.expression,
                &roots,
                delays,
                &mut memo,
                &mut BTreeSet::from([choice.root_anchor]),
            )
        })
        .collect::<Result<_>>()?;
    let mut families = BTreeSet::new();
    for item in &local {
        families.extend(item.3.iter().cloned());
    }
    Ok((
        local.iter().map(|item| item.0).sum(),
        paths.iter().map(|item| item.0).fold(0.0, f64::max),
        paths.iter().map(|item| item.1).max().unwrap_or(0),
        families,
    ))
}

fn expand_technology_mappings(
    candidate: &GeneratorCandidate,
    mappings: &BTreeMap<String, BTreeMap<String, TechCell>>,
    areas: &BTreeMap<String, f64>,
    delays: &BTreeMap<String, f64>,
) -> Result<Vec<GeneratorCandidate>> {
    let mut variants = Vec::new();
    let mut seen = HashSet::new();
    for mapping_name in ["area", "balanced", "timing"] {
        let mapping = &mappings[mapping_name];
        let choices: Vec<_> = candidate
            .choices
            .iter()
            .map(|choice| GeneratorChoice {
                root_anchor: choice.root_anchor,
                expression: remap_expression(&choice.expression, mapping),
            })
            .collect();
        if !seen.insert(choices.clone()) {
            continue;
        }
        let (area, delay, depth, families) = candidate_metrics(&choices, areas, delays)?;
        let mut variant = candidate.clone();
        variant.choices = choices;
        variant.logical_signature = Some(logical_signature(&variant.choices)?);
        variant.mapping = Some(mapping_name.to_owned());
        variant.tech_area = area;
        variant.tech_delay = delay;
        variant.logic_depth = depth;
        variant.cell_families = families.into_iter().collect();
        variant.candidate_id = format!("{}_{}", variant.candidate_id, mapping_name.to_uppercase());
        variants.push(variant);
    }
    Ok(variants)
}

fn candidate_order(left: &GeneratorCandidate, right: &GeneratorCandidate) -> Ordering {
    left.tech_area
        .total_cmp(&right.tech_area)
        .then_with(|| left.tech_delay.total_cmp(&right.tech_delay))
        .then_with(|| left.candidate_id.cmp(&right.candidate_id))
}

#[derive(Clone, Copy, Debug, Serialize)]
struct RealizationAllocation {
    metric_weights: MetricWeights,
    timing_pressure: f64,
    resource_pressure: f64,
    topology_quota: usize,
    alternate_realization_quota: usize,
    symmetric_pressure: bool,
}

fn realization_allocation(limit: usize, metric_weights: MetricWeights) -> RealizationAllocation {
    // Generator V2 has technology estimates for delay and cell area.  Until
    // it has a pin-activity power table, power pressure uses cell resources as
    // its monotone physical proxy instead of inventing a third heuristic
    // score.  The later full-netlist P1/A2/Exact stages still price true NLDM
    // power.
    let timing_pressure = metric_weights.delay.max(0.0);
    let resource_pressure = (metric_weights.area + metric_weights.power).max(0.0);
    let total = timing_pressure + resource_pressure;
    let (timing_share, resource_share) = if total > 0.0 && total.is_finite() {
        (timing_pressure / total, resource_pressure / total)
    } else {
        (0.5, 0.5)
    };
    let symmetry_scale = timing_pressure.abs().max(resource_pressure.abs()).max(1.0);
    let symmetric_pressure = (timing_pressure - resource_pressure).abs() <= 1e-12 * symmetry_scale;
    let topology_quota = if symmetric_pressure {
        (limit / 2).max(1)
    } else {
        ((limit as f64 * timing_share.max(resource_share)).round() as usize)
            .clamp((limit / 2).max(1), limit)
    };
    RealizationAllocation {
        metric_weights,
        timing_pressure,
        resource_pressure,
        topology_quota,
        alternate_realization_quota: limit.saturating_sub(topology_quota),
        symmetric_pressure,
    }
}

fn realization_proxy(
    candidate: &GeneratorCandidate,
    allocation: RealizationAllocation,
    min_area: f64,
    min_delay: f64,
) -> f64 {
    let total = allocation.timing_pressure + allocation.resource_pressure;
    let (timing_share, resource_share) = if total > 0.0 && total.is_finite() {
        (
            allocation.timing_pressure / total,
            allocation.resource_pressure / total,
        )
    } else {
        (0.5, 0.5)
    };
    resource_share * (candidate.tech_area / min_area.max(1e-12)).max(1e-12).ln()
        + timing_share
            * (candidate.tech_delay / min_delay.max(1e-12))
                .max(1e-12)
                .ln()
}

fn select_realizations(
    candidates: Vec<GeneratorCandidate>,
    limit: usize,
    metric_weights: Option<MetricWeights>,
) -> Vec<GeneratorCandidate> {
    let mut groups: HashMap<String, Vec<GeneratorCandidate>> = HashMap::new();
    for candidate in candidates {
        groups
            .entry(candidate.logical_signature.clone().unwrap_or_default())
            .or_default()
            .push(candidate);
    }
    let allocation = metric_weights.map(|weights| realization_allocation(limit, weights));
    // The symmetric point is not an objective-name compatibility branch.  It
    // is the exact balanced allocation already implemented by Generator V2:
    // half topology coverage and half endpoint realizations.  Keeping that
    // arithmetic path also avoids needless floating-point trajectory drift.
    let legacy_symmetric = allocation.is_none_or(|value| value.symmetric_pressure);
    let min_area = groups
        .values()
        .flatten()
        .map(|candidate| candidate.tech_area)
        .fold(f64::INFINITY, f64::min);
    let min_delay = groups
        .values()
        .flatten()
        .map(|candidate| candidate.tech_delay)
        .fold(f64::INFINITY, f64::min);
    let group_key = |items: &[GeneratorCandidate]| {
        let best = items
            .iter()
            .min_by(|left, right| {
                let ordering = if legacy_symmetric {
                    (left.tech_area * left.tech_delay)
                        .total_cmp(&(right.tech_area * right.tech_delay))
                } else {
                    let allocation = allocation.expect("non-symmetric allocation");
                    realization_proxy(left, allocation, min_area, min_delay)
                        .total_cmp(&realization_proxy(right, allocation, min_area, min_delay))
                };
                ordering
                    .then_with(|| left.logic_depth.cmp(&right.logic_depth))
                    .then_with(|| left.candidate_id.cmp(&right.candidate_id))
            })
            .unwrap();
        (
            if legacy_symmetric {
                best.tech_area * best.tech_delay
            } else {
                realization_proxy(
                    best,
                    allocation.expect("non-symmetric allocation"),
                    min_area,
                    min_delay,
                )
            },
            best.logic_depth,
            best.candidate_id.clone(),
        )
    };
    let mut ordered: Vec<_> = groups.keys().cloned().collect();
    ordered.sort_by(|left, right| {
        let left_key = group_key(&groups[left]);
        let right_key = group_key(&groups[right]);
        left_key
            .0
            .total_cmp(&right_key.0)
            .then_with(|| left_key.1.cmp(&right_key.1))
            .then_with(|| left_key.2.cmp(&right_key.2))
    });
    let budget = allocation
        .map(|value| value.topology_quota)
        .unwrap_or_else(|| (limit / 2).max(1));
    let mut selected = Vec::new();
    for category in 0..3 {
        if let Some(key) = ordered.iter().find(|key| {
            !selected.contains(*key)
                && groups[*key].iter().any(|row| match category {
                    0 => row.choices.len() > 1,
                    1 => row
                        .provenance
                        .iter()
                        .any(|value| value == "explicit-divisor-resubstitution"),
                    _ => row.window_kind.starts_with("reconvergent"),
                })
        }) {
            selected.push(key.clone());
        }
    }
    for key in &ordered {
        if !selected.contains(key) {
            selected.push(key.clone());
        }
        if selected.len() == budget {
            break;
        }
    }
    selected.truncate(budget);
    if !legacy_symmetric {
        let allocation = allocation.expect("non-symmetric allocation");
        let mut result = Vec::new();
        let mut alternatives = Vec::new();
        for key in &selected {
            let mut variants = groups[key].clone();
            variants.sort_by(|left, right| {
                realization_proxy(left, allocation, min_area, min_delay)
                    .total_cmp(&realization_proxy(right, allocation, min_area, min_delay))
                    .then_with(|| left.logic_depth.cmp(&right.logic_depth))
                    .then_with(|| left.candidate_id.cmp(&right.candidate_id))
            });
            if let Some(primary) = variants.first() {
                result.push(primary.clone());
                alternatives.extend(variants.into_iter().skip(1));
            }
        }
        alternatives.sort_by(|left, right| {
            realization_proxy(left, allocation, min_area, min_delay)
                .total_cmp(&realization_proxy(right, allocation, min_area, min_delay))
                .then_with(|| left.logic_depth.cmp(&right.logic_depth))
                .then_with(|| left.candidate_id.cmp(&right.candidate_id))
        });
        for candidate in alternatives
            .into_iter()
            .take(allocation.alternate_realization_quota)
        {
            result.push(candidate);
        }
        result.truncate(limit);
        return result;
    }
    let mut result = Vec::new();
    for key in &selected {
        result.push(
            groups[key]
                .iter()
                .min_by(|left, right| candidate_order(left, right))
                .unwrap()
                .clone(),
        );
    }
    for key in &selected {
        let timing = groups[key]
            .iter()
            .min_by(|left, right| {
                left.tech_delay
                    .total_cmp(&right.tech_delay)
                    .then_with(|| left.tech_area.total_cmp(&right.tech_area))
                    .then_with(|| left.candidate_id.cmp(&right.candidate_id))
            })
            .unwrap();
        if !result
            .iter()
            .any(|row| row.candidate_id == timing.candidate_id)
        {
            result.push(timing.clone());
        }
        if result.len() == limit {
            break;
        }
    }
    result.truncate(limit);
    result
}

fn empty_candidate(
    candidate_id: String,
    provenance: Vec<String>,
    choices: Vec<GeneratorChoice>,
    outputs: Vec<usize>,
    window: &Window,
    proof_boundary: Vec<usize>,
    proof_outputs: Vec<usize>,
) -> GeneratorCandidate {
    GeneratorCandidate {
        candidate_id,
        provenance,
        choices,
        tech_area: 0.0,
        tech_delay: 0.0,
        logic_depth: 0,
        cell_families: Vec::new(),
        outputs,
        window_kind: window.kind.clone(),
        proof_boundary,
        proof_outputs,
        proof_groups: Vec::new(),
        shared_host: None,
        divisors: Vec::new(),
        replaced_anchor: None,
        logical_signature: None,
        mapping: None,
        source_windows: Vec::new(),
    }
}

#[derive(Clone, Copy, Debug)]
pub struct GeneratorConfig {
    pub max_boundary: usize,
    pub max_divisors: usize,
    pub max_region: usize,
    pub max_forward_depth: usize,
    pub max_candidates_per_region: usize,
    pub realization_weights: Option<MetricWeights>,
}

fn collect_exclusive_fanin_cone(
    anchor: usize,
    remaining_depth: usize,
    nodes: &HashMap<usize, SourceNode>,
    cone: &mut BTreeSet<usize>,
    boundary: &mut BTreeSet<usize>,
) {
    if !cone.insert(anchor) {
        return;
    }
    let Some(node) = nodes.get(&anchor) else {
        return;
    };
    for child in &node.inputs {
        let descend = remaining_depth > 0
            && nodes.get(child).is_some_and(|child_node| {
                !child_node.is_leaf
                    && !child_node.is_constant
                    && !child_node.is_root
                    && child_node.consumers.as_slice() == [anchor]
            });
        if descend {
            collect_exclusive_fanin_cone(*child, remaining_depth - 1, nodes, cone, boundary);
        } else {
            boundary.insert(*child);
        }
    }
}

/// Return the exact set of old cells that becomes dead when `root` is
/// replaced while the listed divisor anchors remain live.  A fanin is
/// reclaimed only after every one of its consumers is already reclaimed.
/// This is the single-root MFFC reference-count rule and, unlike the legacy
/// exclusive-cone walk, correctly handles an internal node that fans out to
/// two branches which both reconverge inside the removed cone.
fn fanout_aware_reclaim_cone(
    root: usize,
    protected: &BTreeSet<usize>,
    nodes: &HashMap<usize, SourceNode>,
) -> BTreeSet<usize> {
    if protected.contains(&root) {
        return BTreeSet::new();
    }
    let mut reclaimed = BTreeSet::from([root]);
    loop {
        let mut additions = BTreeSet::new();
        for anchor in &reclaimed {
            let Some(node) = nodes.get(anchor) else {
                continue;
            };
            for child in &node.inputs {
                let Some(child_node) = nodes.get(child) else {
                    continue;
                };
                if reclaimed.contains(child)
                    || protected.contains(child)
                    || child_node.is_leaf
                    || child_node.is_constant
                    || child_node.is_root
                {
                    continue;
                }
                if child_node
                    .consumers
                    .iter()
                    .all(|consumer| reclaimed.contains(consumer))
                {
                    additions.insert(*child);
                }
            }
        }
        if additions.is_empty() {
            break;
        }
        reclaimed.extend(additions);
    }
    reclaimed
}

fn cone_boundary(cone: &BTreeSet<usize>, nodes: &HashMap<usize, SourceNode>) -> Vec<usize> {
    cone.iter()
        .flat_map(|anchor| nodes[anchor].inputs.iter().copied())
        .filter(|anchor| !cone.contains(anchor))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn cone_area(
    cone: &BTreeSet<usize>,
    nodes: &HashMap<usize, SourceNode>,
    areas: &BTreeMap<String, f64>,
) -> f64 {
    cone.iter()
        .map(|anchor| areas.get(&nodes[anchor].op).copied().unwrap_or(0.0))
        .sum()
}

fn macro_proof_supported_op(op: &str) -> bool {
    matches!(
        logic_name(op).as_str(),
        "AND2"
            | "AND3"
            | "NAND2"
            | "NAND3"
            | "OR2"
            | "OR3"
            | "NOR2"
            | "NOR3"
            | "INV"
            | "BUF"
            | "XOR2"
            | "XNOR2"
            | "MAJ"
            | "MAJI"
            | "AO21"
            | "AOI21"
            | "AO22"
            | "AOI22"
            | "OA21"
            | "OAI21"
            | "OA22"
            | "OAI22"
            | "O2A1O1I"
    )
}

fn macro_proof_expandable_to_boundary(
    anchor: usize,
    boundary: &BTreeSet<usize>,
    nodes: &HashMap<usize, SourceNode>,
    active: &mut BTreeSet<usize>,
) -> bool {
    if boundary.contains(&anchor) {
        return true;
    }
    let Some(node) = nodes.get(&anchor) else {
        return false;
    };
    if node.is_leaf
        || node.is_constant
        || node.is_root
        || !macro_proof_supported_op(&node.op)
        || !active.insert(anchor)
    {
        return false;
    }
    let result = node
        .inputs
        .iter()
        .all(|child| macro_proof_expandable_to_boundary(*child, boundary, nodes, active));
    active.remove(&anchor);
    result
}

/// Build a deterministic bounded cut around one root.  The expansion is
/// driven only by graph structure and fixed complexity caps; it is independent
/// of benchmark names and objective metric names.  The returned boundary is a
/// complete Boolean cut for every expression successfully evaluated through
/// `truth_of`.
fn bounded_fanin_cut(
    root: usize,
    nodes: &HashMap<usize, SourceNode>,
    max_internal: usize,
    max_boundary: usize,
) -> Option<(BTreeSet<usize>, Vec<usize>)> {
    let root_node = nodes.get(&root)?;
    if root_node.is_leaf || root_node.is_constant || root_node.is_root {
        return None;
    }
    let mut internal = BTreeSet::from([root]);
    let mut boundary: BTreeSet<_> = root_node.inputs.iter().copied().collect();
    if boundary.is_empty() || boundary.len() > max_boundary {
        return None;
    }
    while internal.len() < max_internal {
        let mut options = Vec::new();
        for anchor in &boundary {
            let Some(node) = nodes.get(anchor) else {
                continue;
            };
            if node.is_leaf
                || node.is_constant
                || node.is_root
                || node.inputs.is_empty()
                || !macro_proof_supported_op(&node.op)
            {
                continue;
            }
            let mut next = boundary.clone();
            next.remove(anchor);
            next.extend(
                node.inputs
                    .iter()
                    .copied()
                    .filter(|input| !internal.contains(input)),
            );
            if next.len() <= max_boundary {
                options.push((next.len(), *anchor, next));
            }
        }
        options.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        let Some((_, expanded, next)) = options.into_iter().next() else {
            break;
        };
        boundary = next;
        internal.insert(expanded);
    }
    Some((internal, boundary.into_iter().collect()))
}

/// Enumerate a deterministic bounded set of k-feasible cuts.  Unit cuts are
/// retained internally so a parent can independently decide whether to stop
/// or expand at each fanin; callers skip the root's own unit cut.  This is the
/// mapped-window primitive used by the technology recipe provider.
fn k_feasible_cuts(
    anchor: usize,
    nodes: &HashMap<usize, SourceNode>,
    max_leaves: usize,
    cut_limit: usize,
    memo: &mut HashMap<usize, Vec<Vec<usize>>>,
) -> Vec<Vec<usize>> {
    if let Some(cuts) = memo.get(&anchor) {
        return cuts.clone();
    }
    let Some(node) = nodes.get(&anchor) else {
        return vec![vec![anchor]];
    };
    let mut unique = BTreeSet::from([vec![anchor]]);
    if !node.is_leaf && !node.is_constant && !node.inputs.is_empty() {
        let mut products = vec![BTreeSet::new()];
        for input in &node.inputs {
            let child_cuts = k_feasible_cuts(*input, nodes, max_leaves, cut_limit, memo);
            let mut next = BTreeSet::new();
            for prefix in &products {
                for child_cut in &child_cuts {
                    let mut merged = prefix.clone();
                    merged.extend(child_cut.iter().copied());
                    if merged.len() <= max_leaves {
                        next.insert(merged);
                    }
                }
            }
            products = next.into_iter().collect();
            if products.is_empty() {
                break;
            }
        }
        unique.extend(
            products
                .into_iter()
                .filter(|cut| !cut.is_empty())
                .map(|cut| cut.into_iter().collect()),
        );
    }
    let mut cuts: Vec<_> = unique.into_iter().collect();
    cuts.sort_by(|left, right| left.len().cmp(&right.len()).then_with(|| left.cmp(right)));
    cuts.truncate(cut_limit);
    memo.insert(anchor, cuts.clone());
    cuts
}

/// Build a small exact semantic window that may contain both the root cone
/// and nearby sibling/reconvergent logic.  Unlike `bounded_fanin_cut`, this
/// window is allowed to expose existing off-cone nodes as divisors.  Every
/// retained node is nevertheless expandable to one common, bounded Boolean
/// boundary, so proposals remain proof-carrying and independent of simulation
/// patterns.
fn bounded_semantic_window(
    root: usize,
    nodes: &HashMap<usize, SourceNode>,
    max_internal: usize,
    max_boundary: usize,
    radius: usize,
) -> Option<(BTreeSet<usize>, Vec<usize>)> {
    let root_node = nodes.get(&root)?;
    if root_node.is_leaf || root_node.is_constant || root_node.is_root {
        return None;
    }
    let universe = neighborhood(root, nodes, radius);
    let mut internal = BTreeSet::from([root]);
    while internal.len() < max_internal {
        let boundary = cone_boundary(&internal, nodes);
        let boundary_set: BTreeSet<_> = boundary.iter().copied().collect();
        if boundary.is_empty() || boundary.len() > max_boundary {
            return None;
        }
        let mut options = Vec::new();
        for anchor in universe
            .iter()
            .copied()
            .filter(|anchor| !internal.contains(anchor))
        {
            let Some(node) = nodes.get(&anchor) else {
                continue;
            };
            if node.is_leaf
                || node.is_constant
                || node.is_root
                || node.inputs.is_empty()
                || !macro_proof_supported_op(&node.op)
                || transitively_depends_on(anchor, root, nodes, &mut BTreeSet::new())
            {
                continue;
            }
            let shared_boundary = node
                .inputs
                .iter()
                .filter(|input| boundary_set.contains(input))
                .count();
            let internal_edges = node
                .inputs
                .iter()
                .chain(&node.consumers)
                .filter(|other| internal.contains(other))
                .count();
            // Admit fanin expansion, a directly adjacent independent node,
            // or a sibling which consumes at least one current boundary.
            if !boundary_set.contains(&anchor) && shared_boundary == 0 && internal_edges == 0 {
                continue;
            }
            let mut next_internal = internal.clone();
            next_internal.insert(anchor);
            let next_boundary = cone_boundary(&next_internal, nodes);
            if next_boundary.is_empty() || next_boundary.len() > max_boundary {
                continue;
            }
            let boundary_growth = next_boundary.len().saturating_sub(boundary.len());
            let affinity = shared_boundary * 2 + internal_edges;
            options.push((
                boundary_growth,
                usize::MAX - affinity,
                anchor,
                next_boundary,
            ));
        }
        options.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.2.cmp(&right.2))
        });
        let Some((_, _, chosen, _)) = options.into_iter().next() else {
            break;
        };
        internal.insert(chosen);
    }
    let boundary = cone_boundary(&internal, nodes);
    (!boundary.is_empty() && boundary.len() <= max_boundary).then_some((internal, boundary))
}

fn transitively_depends_on(
    anchor: usize,
    target: usize,
    nodes: &HashMap<usize, SourceNode>,
    visited: &mut BTreeSet<usize>,
) -> bool {
    if anchor == target {
        return true;
    }
    if !visited.insert(anchor) {
        return false;
    }
    nodes.get(&anchor).is_some_and(|node| {
        node.inputs
            .iter()
            .any(|child| transitively_depends_on(*child, target, nodes, visited))
    })
}

/// Programmable-objective exploration lane for strict resource-deleting cone
/// collapses.  It is intentionally separate from `generate`: callers decide
/// whether the lane participates, so the frozen Generator/V7 proposal stream
/// is unchanged.  Every proposal is still re-proved by the materializer.
pub fn generate_deletion_candidates(
    graph_path: &Path,
    liberty_path: &Path,
    output_path: &Path,
    max_candidates: usize,
    enable_mapped_recipes: bool,
    enable_bounded_functional_windows: bool,
    bounded_functional_root_cap: usize,
    bounded_functional_divisor_cap: usize,
    bounded_functional_fast_root_screen: bool,
    bounded_functional_max_area_debt_cells: usize,
    bounded_functional_preferred_roots: &[usize],
) -> Result<Vec<GeneratorCandidate>> {
    anyhow::ensure!(
        !enable_bounded_functional_windows || bounded_functional_root_cap > 0,
        "bounded functional windows require a positive root cap"
    );
    anyhow::ensure!(
        !enable_bounded_functional_windows || bounded_functional_divisor_cap > 0,
        "bounded functional windows require a positive divisor cap"
    );
    anyhow::ensure!(
        !enable_bounded_functional_windows || bounded_functional_max_area_debt_cells > 0,
        "bounded functional windows require a positive area-debt cap"
    );
    let started = Instant::now();
    let graph: SourceGraph = serde_json::from_slice(&fs::read(graph_path)?)?;
    let nodes: HashMap<_, _> = graph
        .occurrences
        .into_iter()
        .map(|node| (node.anchor, node))
        .collect();
    let technology = technology_context(liberty_path)?;
    let areas = &technology.areas;
    let delays = &technology.delays;
    let mappings = &technology.mappings;
    let cells = &mappings["area"];
    let mut anchors: Vec<_> = nodes.keys().copied().collect();
    anchors.sort_unstable();
    let mut ranked = Vec::<(f64, GeneratorCandidate, Value)>::new();
    let mut roots_considered = 0usize;
    let mut roots_with_reduction = 0usize;
    for root in anchors
        .into_iter()
        .filter(|_| !bounded_functional_fast_root_screen)
    {
        let node = &nodes[&root];
        if node.is_leaf || node.is_constant || !areas.contains_key(&node.op) {
            continue;
        }
        roots_considered += 1;
        for cone_depth in [2usize, 3] {
            let mut cone = BTreeSet::new();
            let mut boundary = BTreeSet::new();
            collect_exclusive_fanin_cone(
                root,
                cone_depth.saturating_sub(1),
                &nodes,
                &mut cone,
                &mut boundary,
            );
            if cone.len() < 2 || boundary.is_empty() || boundary.len() > 4 {
                continue;
            }
            let boundary: Vec<_> = boundary.into_iter().collect();
            let Some(target) = truth_of(root, &boundary, &nodes, None) else {
                continue;
            };
            let incumbent_area: f64 = cone
                .iter()
                .map(|anchor| areas.get(&nodes[anchor].op).copied().unwrap_or(0.0))
                .sum();
            let bits = 1usize << boundary.len();
            let terminals: Vec<_> = boundary
                .iter()
                .enumerate()
                .map(|(index, anchor)| Term {
                    truth: variable_truth(index, boundary.len()),
                    expression: GeneratorExpr::Anchor { anchor: *anchor },
                    text: format!("@{anchor}"),
                    area: 0.0,
                    depth: 0,
                    anchors: BTreeSet::from([*anchor]),
                    families: BTreeSet::new(),
                })
                .collect();
            let mut cache = TopGateCache::default();
            let mut terms: Vec<_> = terminals
                .iter()
                .filter(|term| term.truth == target)
                .cloned()
                .collect();
            terms.extend(
                one_gate_terms(&terminals, cells, bits, 192)?
                    .into_iter()
                    .filter(|term| term.truth == target),
            );
            terms.extend(two_gate_target_terms(
                &terminals, target, cells, bits, 24, &mut cache,
            )?);
            let mut unique = BTreeMap::new();
            for term in terms {
                if term.area < incumbent_area - 1e-12 {
                    unique.entry(term.text.clone()).or_insert(term);
                }
            }
            let mut terms: Vec<_> = unique.into_values().collect();
            terms.sort_by(|left, right| {
                left.area
                    .total_cmp(&right.area)
                    .then_with(|| left.depth.cmp(&right.depth))
                    .then_with(|| left.text.cmp(&right.text))
            });
            if terms.is_empty() {
                continue;
            }
            roots_with_reduction += 1;
            let window = Window {
                region_id: format!("DEL_D{cone_depth}_R{root}"),
                region: cone.clone(),
                kind: "cone-collapse-deletion".to_owned(),
                host: root,
            };
            for (term_index, term) in terms.into_iter().take(3).enumerate() {
                let base = empty_candidate(
                    format!("DELETE_D{cone_depth}_R{root}_{term_index}"),
                    vec![
                        "cone-collapse-deletion".to_owned(),
                        window.region_id.clone(),
                    ],
                    vec![GeneratorChoice {
                        root_anchor: root,
                        expression: term.expression.clone(),
                    }],
                    vec![root],
                    &window,
                    boundary.clone(),
                    vec![root],
                );
                for candidate in expand_technology_mappings(&base, &mappings, &areas, &delays)? {
                    let saving = incumbent_area - candidate.tech_area;
                    if saving <= 1e-12 {
                        continue;
                    }
                    let audit = json!({
                        "candidate_id":candidate.candidate_id,
                        "root_anchor":root,
                        "cone_depth":cone_depth,
                        "reclaimed_anchors":cone,
                        "reclaimed_instances":cone.len(),
                        "proof_boundary":boundary,
                        "incumbent_cone_area":incumbent_area,
                        "replacement_area":candidate.tech_area,
                        "predicted_area_reduction":saving,
                        "replacement_depth":candidate.logic_depth,
                    });
                    ranked.push((saving, candidate, audit));
                }
            }
        }
    }

    // Technology-mapped k-feasible window rewriting.  This mirrors the core
    // mechanism of a mapped post-optimization pass more faithfully than one
    // greedy fanin cut: enumerate several exact <=4-leaf cuts, look up a
    // minimum-area mapped recipe, and price the actually reclaimed MFFC with
    // the recipe leaves protected.  It is still only a proposal provider;
    // the common Boolean proof, objective ranking and Exact guard remain the
    // authority.
    let mut mapped_window_roots_considered = 0usize;
    let mut mapped_window_cuts_searched = 0usize;
    let mut mapped_window_raw_matches = 0usize;
    let mut mapped_window_deep_raw_matches = 0usize;
    let mut mapped_window_candidates = 0usize;
    let mut mapped_window_deep_candidates = 0usize;
    let mut mapped_window_unique_deep_targets = 0usize;
    if enable_mapped_recipes && !bounded_functional_fast_root_screen {
        let mut cut_memo = HashMap::<usize, Vec<Vec<usize>>>::new();
        let mut mapped_cuts = Vec::<(usize, usize, Vec<usize>, Truth)>::new();
        for root in nodes.keys().copied().collect::<BTreeSet<_>>() {
            let node = &nodes[&root];
            if node.is_leaf
                || node.is_constant
                || node.is_root
                || !areas.contains_key(&node.op)
                || !macro_proof_supported_op(&node.op)
            {
                continue;
            }
            mapped_window_roots_considered += 1;
            let cuts = k_feasible_cuts(root, &nodes, 4, 32, &mut cut_memo);
            for (cut_index, boundary) in cuts
                .into_iter()
                .filter(|cut| !(cut.len() == 1 && cut[0] == root))
                .enumerate()
            {
                if boundary.is_empty() || boundary.len() > 4 {
                    continue;
                }
                let Some(target) = truth_of(root, &boundary, &nodes, None) else {
                    continue;
                };
                mapped_window_cuts_searched += 1;
                mapped_cuts.push((root, cut_index, boundary, target));
            }
        }

        // Deep recipes depend only on the immutable technology, cut arity and
        // truth table.  Discover that key set once, build independent entries
        // in parallel, then consume them below in the original root/cut order.
        // Candidate IDs, ranking and all downstream budgets therefore retain
        // their frozen sequential semantics.
        let unique_deep_targets: Vec<_> = mapped_cuts
            .iter()
            .map(|(_, _, boundary, target)| (boundary.len(), *target))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        mapped_window_unique_deep_targets = unique_deep_targets.len();
        for inputs in unique_deep_targets
            .iter()
            .map(|(inputs, _)| *inputs)
            .collect::<BTreeSet<_>>()
        {
            mapped_recipe_atlas(inputs, cells)?;
        }
        // Initialize the cache on the controller thread so introducing the
        // workers cannot change process-local HashMap initialization order.
        MAPPED_DEEP_RECIPE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        let recipe_jobs = std::env::var("EGG_GENERATOR_JOBS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(16)
            .max(1);
        parallel_map_ordered(&unique_deep_targets, recipe_jobs, |_, (inputs, target)| {
            mapped_deep_recipes(*inputs, *target, cells)?;
            Ok(())
        })?;

        for (root, cut_index, boundary, target) in mapped_cuts {
            let atlas = mapped_recipe_atlas(boundary.len(), cells)?;
            let mut shallow = atlas.get(&target).cloned().unwrap_or_default();
            shallow.sort_by(|left, right| {
                left.area
                    .total_cmp(&right.area)
                    .then_with(|| left.depth.cmp(&right.depth))
                    .then_with(|| left.text.cmp(&right.text))
            });
            shallow.dedup_by(|left, right| left.text == right.text);
            shallow.truncate(2);
            let deep_recipes = mapped_deep_recipes(boundary.len(), target, cells)?;
            let mut deep: Vec<_> = deep_recipes.iter().cloned().collect();
            mapped_window_deep_raw_matches += deep.len();
            let shallow_text: BTreeSet<_> =
                shallow.iter().map(|recipe| recipe.text.as_str()).collect();
            deep.retain(|recipe| !shallow_text.contains(recipe.text.as_str()));
            deep.sort_by(|left, right| {
                left.area
                    .total_cmp(&right.area)
                    .then_with(|| left.depth.cmp(&right.depth))
                    .then_with(|| left.text.cmp(&right.text))
            });
            deep.truncate(1);
            // Reserve one of the fixed three recipe slots for a 4--5 cell
            // realization.  Positive cell areas make it necessarily lose
            // an area-only sort to a shallower realization of the same
            // truth table, even when its depth/drive profile is the only
            // one compatible with an active external constraint.
            let mut recipes = shallow;
            recipes.extend(deep.iter().cloned());
            recipes.sort_by(|left, right| {
                left.area
                    .total_cmp(&right.area)
                    .then_with(|| left.depth.cmp(&right.depth))
                    .then_with(|| left.text.cmp(&right.text))
            });
            recipes.dedup_by(|left, right| left.text == right.text);
            if recipes.is_empty() {
                continue;
            }
            mapped_window_raw_matches += recipes.len();
            for (recipe_index, recipe) in recipes.iter().enumerate() {
                let expression = bind_recipe_expression(&recipe.expression, &boundary);
                let mut protected = BTreeSet::new();
                expression_anchors(&expression, &mut protected);
                let reclaimed = fanout_aware_reclaim_cone(root, &protected, &nodes);
                let reclaimed_area = cone_area(&reclaimed, &nodes, &areas);
                if reclaimed.is_empty() || recipe.area >= reclaimed_area - 1e-12 {
                    continue;
                }
                let window = Window {
                    region_id: format!("DELETE_KCUT_R{root}_{cut_index}_{recipe_index}"),
                    region: reclaimed.clone(),
                    kind: "mapped-kfeasible-window-rewrite".to_owned(),
                    host: root,
                };
                let base = empty_candidate(
                    window.region_id.clone(),
                    vec![
                        "mapped-kfeasible-window-rewrite".to_owned(),
                        window.region_id.clone(),
                    ],
                    vec![GeneratorChoice {
                        root_anchor: root,
                        expression,
                    }],
                    vec![root],
                    &window,
                    boundary.clone(),
                    vec![root],
                );
                for mut candidate in expand_technology_mappings(&base, &mappings, &areas, &delays)?
                {
                    let saving = reclaimed_area - candidate.tech_area;
                    if saving <= 1e-12 {
                        continue;
                    }
                    candidate.mapping = Some("mapped-kfeasible-window".to_owned());
                    ranked.push((
                        saving,
                        candidate.clone(),
                        json!({
                            "candidate_id":candidate.candidate_id,
                            "root_anchor":root,
                            "resubstitution_kind":"mapped-kfeasible-window",
                            "cut_index":cut_index,
                            "proof_boundary":boundary,
                            "reclaimed_anchors":reclaimed,
                            "incumbent_cone_area":reclaimed_area,
                            "replacement_area":candidate.tech_area,
                            "predicted_area_reduction":saving,
                            "replacement_depth":candidate.logic_depth,
                        }),
                    ));
                    mapped_window_candidates += 1;
                    if expression_cell_count(&recipe.expression) >= 4 {
                        mapped_window_deep_candidates += 1;
                    }
                }
            }
        }
    }

    // Fanout-aware MFFC collapse.  The legacy cone walk requires every
    // traversed fanin to have exactly one consumer.  That misses a common
    // generic case: one node fans out to two internal branches and both
    // branches reconverge below the replaced root.  Reference-count the whole
    // removable cone, then synthesize the same bounded exact cut as before.
    let mut mffc_roots_considered = 0usize;
    let mut mffc_roots_with_reduction = 0usize;
    let mut mffc_candidates = 0usize;
    for root in nodes
        .keys()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|_| !bounded_functional_fast_root_screen)
    {
        let node = &nodes[&root];
        if node.is_leaf
            || node.is_constant
            || node.is_root
            || !areas.contains_key(&node.op)
            || !macro_proof_supported_op(&node.op)
        {
            continue;
        }
        let cone = fanout_aware_reclaim_cone(root, &BTreeSet::new(), &nodes);
        if cone.len() < 2 || cone.len() > 16 {
            continue;
        }
        if !cone
            .iter()
            .all(|anchor| macro_proof_supported_op(&nodes[anchor].op))
        {
            continue;
        }
        let boundary = cone_boundary(&cone, &nodes);
        if boundary.is_empty() || boundary.len() > 6 {
            continue;
        }
        mffc_roots_considered += 1;
        let Some(target) = truth_of(root, &boundary, &nodes, None) else {
            continue;
        };
        let incumbent_area = cone_area(&cone, &nodes, &areas);
        let bits = 1usize << boundary.len();
        let terminals: Vec<_> = boundary
            .iter()
            .enumerate()
            .map(|(index, anchor)| Term {
                truth: variable_truth(index, boundary.len()),
                expression: GeneratorExpr::Anchor { anchor: *anchor },
                text: format!("@{anchor}"),
                area: 0.0,
                depth: 0,
                anchors: BTreeSet::from([*anchor]),
                families: BTreeSet::new(),
            })
            .collect();
        let mut cache = TopGateCache::default();
        let mut terms: Vec<_> = terminals
            .iter()
            .filter(|term| term.truth == target)
            .cloned()
            .collect();
        terms.extend(
            one_gate_terms(&terminals, cells, bits, 256)?
                .into_iter()
                .filter(|term| term.truth == target),
        );
        terms.extend(two_gate_target_terms(
            &terminals, target, cells, bits, 32, &mut cache,
        )?);
        let mut unique = BTreeMap::new();
        for term in terms {
            if term.area < incumbent_area - 1e-12 {
                unique.entry(term.text.clone()).or_insert(term);
            }
        }
        let mut terms: Vec<_> = unique.into_values().collect();
        terms.sort_by(|left, right| {
            left.area
                .total_cmp(&right.area)
                .then_with(|| left.depth.cmp(&right.depth))
                .then_with(|| left.text.cmp(&right.text))
        });
        if terms.is_empty() {
            continue;
        }
        mffc_roots_with_reduction += 1;
        let window = Window {
            region_id: format!("DELETE_MFFC_R{root}"),
            region: cone.clone(),
            kind: "fanout-aware-mffc-collapse".to_owned(),
            host: root,
        };
        for (term_index, term) in terms.into_iter().take(3).enumerate() {
            let base = empty_candidate(
                format!("DELETE_MFFC_R{root}_{term_index}"),
                vec![
                    "fanout-aware-mffc-collapse".to_owned(),
                    window.region_id.clone(),
                ],
                vec![GeneratorChoice {
                    root_anchor: root,
                    expression: term.expression,
                }],
                vec![root],
                &window,
                boundary.clone(),
                vec![root],
            );
            for candidate in expand_technology_mappings(&base, mappings, areas, delays)? {
                let saving = incumbent_area - candidate.tech_area;
                if saving <= 1e-12 {
                    continue;
                }
                let audit = json!({
                    "candidate_id":candidate.candidate_id,
                    "root_anchor":root,
                    "reclaimed_anchors":cone,
                    "reclaimed_instances":cone.len(),
                    "proof_boundary":boundary,
                    "incumbent_cone_area":incumbent_area,
                    "replacement_area":candidate.tech_area,
                    "predicted_area_reduction":saving,
                    "replacement_depth":candidate.logic_depth,
                });
                ranked.push((saving, candidate, audit));
                mffc_candidates += 1;
            }
        }
    }

    // Arbitrary-design local-cut resubstitution.  Unlike the global truth
    // table below, this proof depends only on a bounded local cut, not on the
    // design's total primary-input count.  Existing nearby nodes are zero-cell
    // divisors.  Every proposed expression is re-proved later by the common
    // macro-equivalence guard over this exact cut.
    let logic_anchors: Vec<_> = nodes
        .values()
        .filter(|node| !node.is_root && !node.is_constant)
        .map(|node| node.anchor)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut local_cut_roots_considered = 0usize;
    let mut local_cut_roots_with_divisors = 0usize;
    let mut local_equivalence_candidates = 0usize;
    let mut local_complement_candidates = 0usize;
    let mut local_resubstitution_candidates = 0usize;
    let mut local_complex_resubstitution_candidates = 0usize;
    let mut mapped_recipe_candidates = 0usize;
    for root in nodes
        .keys()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|_| !bounded_functional_fast_root_screen)
    {
        let node = &nodes[&root];
        if node.is_leaf
            || node.is_constant
            || node.is_root
            || !areas.contains_key(&node.op)
            || !macro_proof_supported_op(&node.op)
        {
            continue;
        }
        let Some((cut_internal, boundary)) = bounded_fanin_cut(root, &nodes, 12, 8) else {
            continue;
        };
        let Some(target) = truth_of(root, &boundary, &nodes, None) else {
            continue;
        };
        local_cut_roots_considered += 1;
        let nearby = neighborhood(root, &nodes, 4);
        let boundary_set: BTreeSet<_> = boundary.iter().copied().collect();
        let mut divisors = Vec::<(usize, Truth)>::new();
        for divisor in &logic_anchors {
            if *divisor == root
                || !nearby.contains(divisor)
                || nodes[divisor].is_root
                || transitively_depends_on(*divisor, root, &nodes, &mut BTreeSet::new())
                || !macro_proof_expandable_to_boundary(
                    *divisor,
                    &boundary_set,
                    &nodes,
                    &mut BTreeSet::new(),
                )
            {
                continue;
            }
            let Some(truth) = truth_of(*divisor, &boundary, &nodes, None) else {
                continue;
            };
            divisors.push((*divisor, truth));
        }
        // Boundary anchors are always legal divisors even when they fall just
        // outside the fixed graph-radius neighborhood.
        for (index, anchor) in boundary.iter().enumerate() {
            if !divisors.iter().any(|(divisor, _)| divisor == anchor) {
                divisors.push((*anchor, variable_truth(index, boundary.len())));
            }
        }
        divisors.sort_by_key(|(anchor, _)| *anchor);
        divisors.truncate(32);
        if divisors.is_empty() {
            continue;
        }
        local_cut_roots_with_divisors += 1;
        let bits = 1usize << boundary.len();

        // PMO-style technology-mapped small-cut recipes.  The canonical
        // database is built once per Liberty/arity and rebound to this cut's
        // actual anchors.  Exact materialization/equivalence remains the
        // authority, and the common downstream objective decides whether the
        // candidate receives any budget.
        if enable_mapped_recipes && boundary.len() <= 4 {
            let atlas = mapped_recipe_atlas(boundary.len(), cells)?;
            if let Some(recipes) = atlas.get(&target) {
                for (recipe_index, recipe) in recipes.iter().enumerate() {
                    let expression = bind_recipe_expression(&recipe.expression, &boundary);
                    let mut protected = BTreeSet::new();
                    expression_anchors(&expression, &mut protected);
                    let reclaimed = fanout_aware_reclaim_cone(root, &protected, &nodes);
                    let reclaimed_area = cone_area(&reclaimed, &nodes, &areas);
                    if reclaimed.len() < 2 || recipe.area >= reclaimed_area - 1e-12 {
                        continue;
                    }
                    let window = Window {
                        region_id: format!("DELETE_RECIPE_R{root}_{recipe_index}"),
                        region: reclaimed.clone(),
                        kind: "technology-mapped-small-cut-recipe".to_owned(),
                        host: root,
                    };
                    let base = empty_candidate(
                        window.region_id.clone(),
                        vec![
                            "technology-mapped-small-cut-recipe".to_owned(),
                            window.region_id.clone(),
                        ],
                        vec![GeneratorChoice {
                            root_anchor: root,
                            expression,
                        }],
                        vec![root],
                        &window,
                        boundary.clone(),
                        vec![root],
                    );
                    for mut candidate in
                        expand_technology_mappings(&base, &mappings, &areas, &delays)?
                    {
                        let saving = reclaimed_area - candidate.tech_area;
                        if saving <= 1e-12 {
                            continue;
                        }
                        candidate.mapping = Some("mapped-recipe".to_owned());
                        ranked.push((
                            saving,
                            candidate.clone(),
                            json!({
                                "candidate_id":candidate.candidate_id,
                                "root_anchor":root,
                                "resubstitution_kind":"technology-mapped-small-cut-recipe",
                                "recipe_cells":candidate.logic_depth,
                                "cut_internal":cut_internal,
                                "proof_boundary":boundary,
                                "reclaimed_anchors":reclaimed,
                                "incumbent_cone_area":reclaimed_area,
                                "replacement_area":candidate.tech_area,
                                "predicted_area_reduction":saving,
                            }),
                        ));
                        mapped_recipe_candidates += 1;
                    }
                }
            }
        }

        let mut exact_matches = Vec::new();
        let mut complement_matches = Vec::new();
        for (divisor, truth) in &divisors {
            let protected = BTreeSet::from([*divisor]);
            let reclaimed = fanout_aware_reclaim_cone(root, &protected, &nodes);
            let reclaimed_area = cone_area(&reclaimed, &nodes, &areas);
            if *truth == target && reclaimed_area > 1e-12 {
                exact_matches.push((reclaimed_area, *divisor, reclaimed.clone()));
            }
            if let Some(inverter) = cells.get("INV") {
                let saving = reclaimed_area - inverter.area;
                if saving > 1e-12 && truth.not(bits) == target {
                    complement_matches.push((saving, *divisor, reclaimed, inverter.clone()));
                }
            }
        }
        exact_matches.sort_by(|left, right| {
            right
                .0
                .total_cmp(&left.0)
                .then_with(|| left.1.cmp(&right.1))
        });
        complement_matches.sort_by(|left, right| {
            right
                .0
                .total_cmp(&left.0)
                .then_with(|| left.1.cmp(&right.1))
        });

        for (saving, divisor, reclaimed) in exact_matches.into_iter().take(1) {
            let window = Window {
                region_id: format!("DELETE_LCUT_EQ_R{root}_D{divisor}"),
                region: reclaimed.clone(),
                kind: "local-cut-equivalence-elimination".to_owned(),
                host: root,
            };
            let mut candidate = empty_candidate(
                window.region_id.clone(),
                vec![
                    "local-cut-equivalence-elimination".to_owned(),
                    window.region_id.clone(),
                ],
                vec![GeneratorChoice {
                    root_anchor: root,
                    expression: GeneratorExpr::Anchor { anchor: divisor },
                }],
                vec![root],
                &window,
                boundary.clone(),
                vec![root],
            );
            candidate.logical_signature = Some(logical_signature(&candidate.choices)?);
            candidate.mapping = Some("eliminate".to_owned());
            candidate.divisors = vec![divisor];
            ranked.push((
                saving,
                candidate.clone(),
                json!({
                    "candidate_id":candidate.candidate_id,
                    "root_anchor":root,
                    "equivalent_divisor":divisor,
                    "cut_internal":cut_internal,
                    "proof_boundary":boundary,
                    "reclaimed_anchors":reclaimed,
                    "predicted_area_reduction":saving,
                    "replacement_area":0.0,
                }),
            ));
            local_equivalence_candidates += 1;
        }
        for (saving, divisor, reclaimed, inverter) in complement_matches.into_iter().take(1) {
            let window = Window {
                region_id: format!("DELETE_LCUT_INV_R{root}_D{divisor}"),
                region: reclaimed.clone(),
                kind: "local-cut-complement-elimination".to_owned(),
                host: root,
            };
            let expression = GeneratorExpr::Cell {
                op: inverter.name.clone(),
                children: vec![GeneratorExpr::Anchor { anchor: divisor }],
            };
            let mut candidate = empty_candidate(
                window.region_id.clone(),
                vec![
                    "local-cut-complement-elimination".to_owned(),
                    window.region_id.clone(),
                ],
                vec![GeneratorChoice {
                    root_anchor: root,
                    expression,
                }],
                vec![root],
                &window,
                boundary.clone(),
                vec![root],
            );
            candidate.tech_area = inverter.area;
            candidate.tech_delay = inverter.delay;
            candidate.logic_depth = 1;
            candidate.cell_families = vec!["INV".to_owned()];
            candidate.logical_signature = Some(logical_signature(&candidate.choices)?);
            candidate.mapping = Some("eliminate".to_owned());
            candidate.divisors = vec![divisor];
            ranked.push((
                saving,
                candidate.clone(),
                json!({
                    "candidate_id":candidate.candidate_id,
                    "root_anchor":root,
                    "complement_divisor":divisor,
                    "cut_internal":cut_internal,
                    "proof_boundary":boundary,
                    "reclaimed_anchors":reclaimed,
                    "predicted_area_reduction":saving,
                    "replacement_area":inverter.area,
                }),
            ));
            local_complement_candidates += 1;
        }

        let mut pair_matches = Vec::new();
        for (left_index, (left, left_truth)) in divisors.iter().copied().enumerate() {
            for (right, right_truth) in divisors.iter().copied().skip(left_index + 1) {
                let protected = BTreeSet::from([left, right]);
                let reclaimed = fanout_aware_reclaim_cone(root, &protected, &nodes);
                let reclaimed_area = cone_area(&reclaimed, &nodes, &areas);
                for (family, arity) in FAMILIES {
                    if arity != 2 {
                        continue;
                    }
                    let Some(cell) = cells.get(family) else {
                        continue;
                    };
                    let saving = reclaimed_area - cell.area;
                    if saving <= 1e-12
                        || apply_family(family, &[left_truth, right_truth], bits)? != target
                    {
                        continue;
                    }
                    pair_matches.push((
                        saving,
                        family.to_owned(),
                        left,
                        right,
                        cell.clone(),
                        reclaimed.clone(),
                    ));
                }
            }
        }
        pair_matches.sort_by(|left, right| {
            right
                .0
                .total_cmp(&left.0)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.2.cmp(&right.2))
                .then_with(|| left.3.cmp(&right.3))
        });
        pair_matches
            .dedup_by(|left, right| left.1 == right.1 && left.2 == right.2 && left.3 == right.3);
        for (match_index, (saving, family, left, right, cell, reclaimed)) in
            pair_matches.into_iter().take(2).enumerate()
        {
            let window = Window {
                region_id: format!(
                    "DELETE_LCUT_RESUB_R{root}_{family}_D{left}_{right}_{match_index}"
                ),
                region: reclaimed.clone(),
                kind: "local-cut-resubstitution-elimination".to_owned(),
                host: root,
            };
            let expression = GeneratorExpr::Cell {
                op: cell.name.clone(),
                children: vec![
                    GeneratorExpr::Anchor { anchor: left },
                    GeneratorExpr::Anchor { anchor: right },
                ],
            };
            let mut candidate = empty_candidate(
                window.region_id.clone(),
                vec![
                    "local-cut-resubstitution-elimination".to_owned(),
                    window.region_id.clone(),
                ],
                vec![GeneratorChoice {
                    root_anchor: root,
                    expression,
                }],
                vec![root],
                &window,
                boundary.clone(),
                vec![root],
            );
            candidate.tech_area = cell.area;
            candidate.tech_delay = cell.delay;
            candidate.logic_depth = 1;
            candidate.cell_families = vec![family.clone()];
            candidate.logical_signature = Some(logical_signature(&candidate.choices)?);
            candidate.mapping = Some("eliminate".to_owned());
            candidate.divisors = vec![left, right];
            ranked.push((
                saving,
                candidate.clone(),
                json!({
                    "candidate_id":candidate.candidate_id,
                    "root_anchor":root,
                    "resubstitution_family":family,
                    "divisors":[left,right],
                    "cut_internal":cut_internal,
                    "proof_boundary":boundary,
                    "reclaimed_anchors":reclaimed,
                    "predicted_area_reduction":saving,
                    "replacement_area":cell.area,
                }),
            ));
            local_resubstitution_candidates += 1;
        }

        // One complex FULL-186 cell driven by existing local divisors can
        // bypass several old gates at once.  Keep a fixed, small divisor set
        // ranked by exact reclaimable area, then exhaustively prove the
        // supported 3/4-input cell functions on the same local cut.  This is
        // still one resubstitution move, not a composition of independent
        // objective-specific moves.
        let mut complex_divisors = divisors.clone();
        complex_divisors.sort_by(|left, right| {
            let reclaim = |anchor| {
                cone_area(
                    &fanout_aware_reclaim_cone(root, &BTreeSet::from([anchor]), &nodes),
                    &nodes,
                    &areas,
                )
            };
            reclaim(right.0)
                .total_cmp(&reclaim(left.0))
                .then_with(|| left.0.cmp(&right.0))
        });
        complex_divisors.truncate(10);
        let divisor_terms: Vec<_> = complex_divisors
            .iter()
            .map(|(anchor, truth)| Term {
                truth: *truth,
                expression: GeneratorExpr::Anchor { anchor: *anchor },
                text: format!("@{anchor}"),
                area: 0.0,
                depth: 0,
                anchors: BTreeSet::from([*anchor]),
                families: BTreeSet::new(),
            })
            .collect();
        let mut complex_matches = BTreeMap::<String, (f64, Term, BTreeSet<usize>)>::new();
        for (family, arity) in FAMILIES {
            if arity < 3 || !cells.contains_key(family) {
                continue;
            }
            let combinations = matches!(family, "MAJ" | "MAJI");
            for_each_product(&divisor_terms, arity, combinations, |children| {
                if apply_family(
                    family,
                    &children.iter().map(|child| child.truth).collect::<Vec<_>>(),
                    bits,
                )? != target
                {
                    return Ok(());
                }
                let term = make_gate(family, children, cells, bits)?;
                let reclaimed = fanout_aware_reclaim_cone(root, &term.anchors, &nodes);
                let saving = cone_area(&reclaimed, &nodes, &areas) - term.area;
                if saving <= 1e-12 {
                    return Ok(());
                }
                let replace = complex_matches
                    .get(&term.text)
                    .is_none_or(|previous| saving > previous.0);
                if replace {
                    complex_matches.insert(term.text.clone(), (saving, term, reclaimed));
                }
                Ok(())
            })?;
        }
        let mut complex_matches: Vec<_> = complex_matches.into_values().collect();
        complex_matches.sort_by(|left, right| {
            right
                .0
                .total_cmp(&left.0)
                .then_with(|| left.1.area.total_cmp(&right.1.area))
                .then_with(|| left.1.text.cmp(&right.1.text))
        });
        for (match_index, (saving, term, reclaimed)) in
            complex_matches.into_iter().take(2).enumerate()
        {
            let window = Window {
                region_id: format!("DELETE_LCUT_COMPLEX_R{root}_{match_index}"),
                region: reclaimed.clone(),
                kind: "local-cut-resubstitution-elimination".to_owned(),
                host: root,
            };
            let mut candidate = empty_candidate(
                window.region_id.clone(),
                vec![
                    "local-cut-resubstitution-elimination".to_owned(),
                    "complex-cell-divisor-realization".to_owned(),
                    window.region_id.clone(),
                ],
                vec![GeneratorChoice {
                    root_anchor: root,
                    expression: term.expression,
                }],
                vec![root],
                &window,
                boundary.clone(),
                vec![root],
            );
            candidate.tech_area = term.area;
            candidate.logic_depth = 1;
            candidate.cell_families = term.families.iter().cloned().collect();
            candidate.logical_signature = Some(logical_signature(&candidate.choices)?);
            candidate.mapping = Some("eliminate".to_owned());
            candidate.divisors = term.anchors.iter().copied().collect();
            ranked.push((
                saving,
                candidate.clone(),
                json!({
                    "candidate_id":candidate.candidate_id,
                    "root_anchor":root,
                    "resubstitution_kind":"complex-cell-divisor-realization",
                    "divisors":candidate.divisors,
                    "cell_families":candidate.cell_families,
                    "cut_internal":cut_internal,
                    "proof_boundary":boundary,
                    "reclaimed_anchors":reclaimed,
                    "predicted_area_reduction":saving,
                    "replacement_area":term.area,
                }),
            ));
            local_complex_resubstitution_candidates += 1;
        }
    }

    // Bounded functional-window provider.  This deliberately fills only the
    // missing middle ground between the direct local-cut lane above and a
    // full PMO-style resubstitution engine: 5/6-input exact cuts, at most 64
    // existing divisors, one bounded intermediate-signature table and a
    // two-level mapped replacement.  It emits ordinary structural macros;
    // the existing correlated composer may then pair one with an independent
    // constraint-repair move and evaluate that transaction atomically.
    let mut bounded_functional_roots_considered = 0usize;
    let mut bounded_functional_roots_searched = 0usize;
    let mut bounded_functional_raw_matches = 0usize;
    let mut bounded_functional_candidates = 0usize;
    let mut bounded_functional_resource_debt_candidates = 0usize;
    let mut bounded_functional_empty_reclaim_rejections = 0usize;
    let mut bounded_functional_area_debt_rejections = 0usize;
    let mut bounded_functional_best_raw_saving = f64::NEG_INFINITY;
    let mut bounded_functional_roots_preselected = 0usize;
    let preferred_neighborhood: BTreeSet<_> = bounded_functional_preferred_roots
        .iter()
        .flat_map(|root| neighborhood(*root, &nodes, 0))
        .collect();
    if enable_bounded_functional_windows {
        let resource_quantum = cells
            .values()
            .map(|cell| cell.area)
            .filter(|area| *area > 0.0)
            .fold(f64::INFINITY, f64::min);
        let all_roots = nodes.keys().copied().collect::<BTreeSet<_>>();
        let root_pool: Vec<_> = if bounded_functional_fast_root_screen {
            let mut cheap = Vec::<(f64, usize)>::new();
            for root in all_roots {
                let node = &nodes[&root];
                if node.is_leaf
                    || node.is_constant
                    || node.is_root
                    || !areas.contains_key(&node.op)
                    || !macro_proof_supported_op(&node.op)
                {
                    continue;
                }
                let mut seen = BTreeSet::new();
                let mut stack = vec![(root, 0usize)];
                while let Some((anchor, depth)) = stack.pop() {
                    if seen.len() >= 24 || !seen.insert(anchor) || depth >= 4 {
                        continue;
                    }
                    if let Some(local) = nodes.get(&anchor) {
                        for child in &local.inputs {
                            stack.push((*child, depth + 1));
                        }
                    }
                }
                let score = cone_area(&seen, &nodes, &areas);
                cheap.push((score, root));
            }
            cheap.sort_by(|left, right| {
                right
                    .0
                    .total_cmp(&left.0)
                    .then_with(|| left.1.cmp(&right.1))
            });
            cheap.truncate(bounded_functional_root_cap.saturating_mul(16));
            let mut roots: BTreeSet<_> = cheap.into_iter().map(|(_, root)| root).collect();
            roots.extend(preferred_neighborhood.iter().copied());
            roots.into_iter().collect()
        } else {
            all_roots.into_iter().collect()
        };
        bounded_functional_roots_preselected = root_pool.len();
        let mut opportunities = Vec::<(bool, f64, usize, BTreeSet<usize>, Vec<usize>)>::new();
        for root in root_pool {
            let node = &nodes[&root];
            if node.is_leaf
                || node.is_constant
                || node.is_root
                || !areas.contains_key(&node.op)
                || !macro_proof_supported_op(&node.op)
            {
                continue;
            }
            let Some((cut_internal, boundary)) = bounded_semantic_window(root, &nodes, 24, 8, 5)
            else {
                continue;
            };
            if !(2..=8).contains(&boundary.len()) {
                continue;
            }
            bounded_functional_roots_considered += 1;
            // Iterative has already narrowed the graph to a small cheap pool, so
            // it is now affordable—and necessary—to reject shared cones with
            // no real MFFC before applying the final root cap.
            let reclaimable = fanout_aware_reclaim_cone(root, &BTreeSet::new(), &nodes);
            if reclaimable.len() < 3 {
                continue;
            }
            let resource = cone_area(&reclaimable, &nodes, &areas);
            opportunities.push((
                preferred_neighborhood.contains(&root),
                resource,
                root,
                cut_internal,
                boundary,
            ));
        }
        opportunities.sort_by(|left, right| {
            right.0.cmp(&left.0).then_with(|| {
                right
                    .1
                    .total_cmp(&left.1)
                    .then_with(|| left.2.cmp(&right.2))
            })
        });
        opportunities.truncate(bounded_functional_root_cap);

        for (_, _, root, cut_internal, boundary) in opportunities {
            let Some(target) = truth_of(root, &boundary, &nodes, None) else {
                continue;
            };
            let bits = 1usize << boundary.len();
            let boundary_set: BTreeSet<_> = boundary.iter().copied().collect();
            let nearby = neighborhood(root, &nodes, 6);
            let mut divisor_options = Vec::<(f64, usize, Truth)>::new();
            for divisor in &logic_anchors {
                if *divisor == root
                    || !nearby.contains(divisor)
                    || nodes[divisor].is_root
                    || transitively_depends_on(*divisor, root, &nodes, &mut BTreeSet::new())
                    || !macro_proof_expandable_to_boundary(
                        *divisor,
                        &boundary_set,
                        &nodes,
                        &mut BTreeSet::new(),
                    )
                {
                    continue;
                }
                let Some(truth) = truth_of(*divisor, &boundary, &nodes, None) else {
                    continue;
                };
                let protected = BTreeSet::from([*divisor]);
                let reclaim = cone_area(
                    &fanout_aware_reclaim_cone(root, &protected, &nodes),
                    &nodes,
                    &areas,
                );
                divisor_options.push((reclaim, *divisor, truth));
            }
            for (index, anchor) in boundary.iter().enumerate() {
                if !divisor_options
                    .iter()
                    .any(|(_, divisor, _)| divisor == anchor)
                {
                    let protected = BTreeSet::from([*anchor]);
                    let reclaim = cone_area(
                        &fanout_aware_reclaim_cone(root, &protected, &nodes),
                        &nodes,
                        &areas,
                    );
                    divisor_options.push((reclaim, *anchor, variable_truth(index, boundary.len())));
                }
            }
            divisor_options.sort_by(|left, right| {
                right
                    .0
                    .total_cmp(&left.0)
                    .then_with(|| left.1.cmp(&right.1))
            });
            // One representative per functional signature is sufficient;
            // retain the anchor that protects the least reclaimable resource.
            let mut seen_truth = HashSet::new();
            divisor_options.retain(|(_, _, truth)| seen_truth.insert(*truth));
            divisor_options.truncate(bounded_functional_divisor_cap);
            if divisor_options.len() < 3 {
                continue;
            }
            bounded_functional_roots_searched += 1;
            let divisor_terms: Vec<_> = divisor_options
                .iter()
                .map(|(_, anchor, truth)| Term {
                    truth: *truth,
                    expression: GeneratorExpr::Anchor { anchor: *anchor },
                    text: format!("@{anchor}"),
                    area: 0.0,
                    depth: 0,
                    anchors: BTreeSet::from([*anchor]),
                    families: BTreeSet::new(),
                })
                .collect();
            let inner_divisor_cap = bounded_functional_divisor_cap.min(48);
            let intermediate_cap = (bounded_functional_divisor_cap * 8).clamp(32, 384);
            let mut terms = bounded_functional_target_terms(
                &divisor_terms,
                target,
                cells,
                bits,
                inner_divisor_cap,
                intermediate_cap,
                96,
            )?;
            // Also search the pure cut basis.  A divisor-based expression can
            // have a lower replacement cost yet protect most of the old cone;
            // retaining only that expression would hide a slightly larger
            // replacement that deletes substantially more logic.
            let boundary_terms: Vec<_> = boundary
                .iter()
                .enumerate()
                .map(|(index, anchor)| Term {
                    truth: variable_truth(index, boundary.len()),
                    expression: GeneratorExpr::Anchor { anchor: *anchor },
                    text: format!("@{anchor}"),
                    area: 0.0,
                    depth: 0,
                    anchors: BTreeSet::from([*anchor]),
                    families: BTreeSet::new(),
                })
                .collect();
            terms.extend(bounded_functional_target_terms(
                &boundary_terms,
                target,
                cells,
                bits,
                6,
                384,
                48,
            )?);
            let mut unique_terms = BTreeMap::new();
            for term in terms {
                unique_terms.entry(term.text.clone()).or_insert(term);
            }
            bounded_functional_raw_matches += unique_terms.len();
            let mut admissible_terms = Vec::new();
            for term in unique_terms.into_values() {
                let reclaimed = fanout_aware_reclaim_cone(root, &term.anchors, &nodes);
                let incumbent_area = cone_area(&reclaimed, &nodes, &areas);
                let saving = incumbent_area - term.area;
                bounded_functional_best_raw_saving = bounded_functional_best_raw_saving.max(saving);
                if reclaimed.is_empty() {
                    bounded_functional_empty_reclaim_rejections += 1;
                    continue;
                }
                let maximum_area_debt =
                    resource_quantum * bounded_functional_max_area_debt_cells as f64;
                if saving < -maximum_area_debt - 1e-12 {
                    bounded_functional_area_debt_rejections += 1;
                    continue;
                }
                admissible_terms.push((saving, term, reclaimed));
            }
            admissible_terms.sort_by(|left, right| {
                right
                    .0
                    .total_cmp(&left.0)
                    .then_with(|| left.1.area.total_cmp(&right.1.area))
                    .then_with(|| left.1.text.cmp(&right.1.text))
            });
            for (term_index, (_, term, reclaimed)) in
                admissible_terms.into_iter().take(4).enumerate()
            {
                let incumbent_area = cone_area(&reclaimed, &nodes, &areas);
                let window = Window {
                    region_id: format!("DELETE_BFW_R{root}_{term_index}"),
                    region: reclaimed.clone(),
                    kind: "bounded-functional-window-resubstitution".to_owned(),
                    host: root,
                };
                let base = empty_candidate(
                    window.region_id.clone(),
                    vec![
                        "bounded-functional-window-resubstitution".to_owned(),
                        window.region_id.clone(),
                    ],
                    vec![GeneratorChoice {
                        root_anchor: root,
                        expression: term.expression.clone(),
                    }],
                    vec![root],
                    &window,
                    boundary.clone(),
                    vec![root],
                );
                for mut candidate in expand_technology_mappings(&base, &mappings, &areas, &delays)?
                {
                    let saving = incumbent_area - candidate.tech_area;
                    if saving < -resource_quantum - 1e-12 {
                        continue;
                    }
                    candidate.mapping = Some("bounded-functional-window".to_owned());
                    candidate.divisors = term.anchors.iter().copied().collect();
                    ranked.push((
                        saving,
                        candidate.clone(),
                        json!({
                            "candidate_id":candidate.candidate_id,
                            "root_anchor":root,
                            "resubstitution_kind":"bounded-functional-window",
                            "cut_internal":cut_internal,
                            "proof_boundary":boundary,
                            "divisors":candidate.divisors,
                            "reclaimed_anchors":reclaimed,
                            "incumbent_cone_area":incumbent_area,
                            "replacement_area":candidate.tech_area,
                            "predicted_area_reduction":saving,
                            "replacement_depth":candidate.logic_depth,
                        }),
                    ));
                    bounded_functional_candidates += 1;
                    if saving <= 1e-12 {
                        bounded_functional_resource_debt_candidates += 1;
                    }
                }
            }
        }
    }

    // Small-control designs often have few enough primary inputs for a global
    // exact truth table.  Equivalent existing nodes then act as zero-cell
    // divisors: redirecting one root to the other removes a real instance.
    // This remains a generic gate-elimination rule and is skipped when the
    // exhaustive boundary would exceed the fixed truth-table capacity.
    let primary_inputs: Vec<_> = nodes
        .values()
        .filter(|node| node.is_leaf && !node.is_constant)
        .map(|node| node.anchor)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut global_equivalence_candidates = 0usize;
    let mut global_complement_candidates = 0usize;
    let mut global_resubstitution_candidates = 0usize;
    if !bounded_functional_fast_root_screen
        && !primary_inputs.is_empty()
        && primary_inputs.len() <= 8
    {
        let mut logic_anchors: Vec<_> = nodes
            .values()
            .filter(|node| {
                !node.is_leaf && !node.is_constant && !node.is_root && areas.contains_key(&node.op)
            })
            .map(|node| node.anchor)
            .collect();
        logic_anchors.sort_unstable();
        let truths: HashMap<_, _> = logic_anchors
            .iter()
            .filter_map(|anchor| {
                truth_of(*anchor, &primary_inputs, &nodes, None).map(|truth| (*anchor, truth))
            })
            .collect();
        for root in &logic_anchors {
            let Some(root_truth) = truths.get(root) else {
                continue;
            };
            if let Some(divisor) = logic_anchors.iter().copied().find(|divisor| {
                divisor != root
                    && truths.get(divisor) == Some(root_truth)
                    && !transitively_depends_on(*divisor, *root, &nodes, &mut BTreeSet::new())
            }) {
                let saving = areas[&nodes[root].op];
                let window = Window {
                    region_id: format!("DELETE_EQ_R{root}_D{divisor}"),
                    region: BTreeSet::from([*root]),
                    kind: "global-equivalence-elimination".to_owned(),
                    host: *root,
                };
                let mut candidate = empty_candidate(
                    window.region_id.clone(),
                    vec![
                        "global-equivalence-elimination".to_owned(),
                        window.region_id.clone(),
                    ],
                    vec![GeneratorChoice {
                        root_anchor: *root,
                        expression: GeneratorExpr::Anchor { anchor: divisor },
                    }],
                    vec![*root],
                    &window,
                    primary_inputs.clone(),
                    vec![*root],
                );
                candidate.tech_area = 0.0;
                candidate.tech_delay = 0.0;
                candidate.logical_signature = Some(logical_signature(&candidate.choices)?);
                candidate.mapping = Some("eliminate".to_owned());
                candidate.divisors = vec![divisor];
                ranked.push((
                    saving,
                    candidate.clone(),
                    json!({
                        "candidate_id":candidate.candidate_id,
                        "root_anchor":root,
                        "equivalent_divisor":divisor,
                        "proof_boundary":primary_inputs,
                        "reclaimed_instances":1,
                        "incumbent_cone_area":saving,
                        "replacement_area":0.0,
                        "predicted_area_reduction":saving,
                        "replacement_depth":0,
                    }),
                ));
                global_equivalence_candidates += 1;
            }
            let Some(inverter) = cells.get("INV") else {
                continue;
            };
            let saving = areas[&nodes[root].op] - inverter.area;
            if saving <= 1e-12 {
                continue;
            }
            let complement = root_truth.not(1usize << primary_inputs.len());
            let Some(divisor) = logic_anchors.iter().copied().find(|divisor| {
                divisor != root
                    && truths.get(divisor) == Some(&complement)
                    && !transitively_depends_on(*divisor, *root, &nodes, &mut BTreeSet::new())
            }) else {
                continue;
            };
            let window = Window {
                region_id: format!("DELETE_INV_R{root}_D{divisor}"),
                region: BTreeSet::from([*root]),
                kind: "global-complement-elimination".to_owned(),
                host: *root,
            };
            let expression = GeneratorExpr::Cell {
                op: inverter.name.clone(),
                children: vec![GeneratorExpr::Anchor { anchor: divisor }],
            };
            let mut candidate = empty_candidate(
                window.region_id.clone(),
                vec![
                    "global-complement-elimination".to_owned(),
                    window.region_id.clone(),
                ],
                vec![GeneratorChoice {
                    root_anchor: *root,
                    expression,
                }],
                vec![*root],
                &window,
                primary_inputs.clone(),
                vec![*root],
            );
            candidate.tech_area = inverter.area;
            candidate.tech_delay = inverter.delay;
            candidate.logic_depth = 1;
            candidate.cell_families = vec!["INV".to_owned()];
            candidate.logical_signature = Some(logical_signature(&candidate.choices)?);
            candidate.mapping = Some("eliminate".to_owned());
            candidate.divisors = vec![divisor];
            ranked.push((
                saving,
                candidate.clone(),
                json!({
                    "candidate_id":candidate.candidate_id,
                    "root_anchor":root,
                    "complement_divisor":divisor,
                    "proof_boundary":primary_inputs,
                    "reclaimed_instances":1,
                    "incumbent_cone_area":areas[&nodes[root].op],
                    "replacement_area":inverter.area,
                    "predicted_area_reduction":saving,
                    "replacement_depth":1,
                }),
            ));
            global_complement_candidates += 1;
        }
        let bits = 1usize << primary_inputs.len();
        for root in &logic_anchors {
            let Some(root_truth) = truths.get(root) else {
                continue;
            };
            let incumbent_area = areas[&nodes[root].op];
            let usable: Vec<_> = logic_anchors
                .iter()
                .copied()
                .filter(|divisor| {
                    divisor != root
                        && truths.contains_key(divisor)
                        && !transitively_depends_on(*divisor, *root, &nodes, &mut BTreeSet::new())
                })
                .collect();
            let mut matches = Vec::<(f64, String, usize, usize, TechCell)>::new();
            for (left_index, left) in usable.iter().copied().enumerate() {
                for right in usable.iter().copied().skip(left_index + 1) {
                    for (family, arity) in FAMILIES {
                        if arity != 2 {
                            continue;
                        }
                        let Some(cell) = cells.get(family) else {
                            continue;
                        };
                        if cell.area >= incumbent_area - 1e-12 {
                            continue;
                        }
                        if apply_family(family, &[truths[&left], truths[&right]], bits)?
                            == *root_truth
                        {
                            matches.push((
                                incumbent_area - cell.area,
                                family.to_owned(),
                                left,
                                right,
                                cell.clone(),
                            ));
                        }
                    }
                }
            }
            matches.sort_by(|left, right| {
                right
                    .0
                    .total_cmp(&left.0)
                    .then_with(|| left.1.cmp(&right.1))
                    .then_with(|| left.2.cmp(&right.2))
                    .then_with(|| left.3.cmp(&right.3))
            });
            matches.dedup_by(|left, right| {
                left.1 == right.1 && left.2 == right.2 && left.3 == right.3
            });
            for (match_index, (saving, family, left, right, cell)) in
                matches.into_iter().take(2).enumerate()
            {
                let window = Window {
                    region_id: format!(
                        "DELETE_RESUB_R{root}_{family}_D{left}_{right}_{match_index}"
                    ),
                    region: BTreeSet::from([*root]),
                    kind: "global-resubstitution-elimination".to_owned(),
                    host: *root,
                };
                let expression = GeneratorExpr::Cell {
                    op: cell.name.clone(),
                    children: vec![
                        GeneratorExpr::Anchor { anchor: left },
                        GeneratorExpr::Anchor { anchor: right },
                    ],
                };
                let mut candidate = empty_candidate(
                    window.region_id.clone(),
                    vec![
                        "global-resubstitution-elimination".to_owned(),
                        window.region_id.clone(),
                    ],
                    vec![GeneratorChoice {
                        root_anchor: *root,
                        expression,
                    }],
                    vec![*root],
                    &window,
                    primary_inputs.clone(),
                    vec![*root],
                );
                candidate.tech_area = cell.area;
                candidate.tech_delay = cell.delay;
                candidate.logic_depth = 1;
                candidate.cell_families = vec![family.clone()];
                candidate.logical_signature = Some(logical_signature(&candidate.choices)?);
                candidate.mapping = Some("eliminate".to_owned());
                candidate.divisors = vec![left, right];
                ranked.push((
                    saving,
                    candidate.clone(),
                    json!({
                        "candidate_id":candidate.candidate_id,
                        "root_anchor":root,
                        "resubstitution_family":family,
                        "divisors":[left,right],
                        "proof_boundary":primary_inputs,
                        "reclaimed_instances":1,
                        "incumbent_cone_area":incumbent_area,
                        "replacement_area":cell.area,
                        "predicted_area_reduction":saving,
                        "replacement_depth":1,
                    }),
                ));
                global_resubstitution_candidates += 1;
            }
        }
    }
    ranked.sort_by(|left, right| {
        right
            .0
            .total_cmp(&left.0)
            .then_with(|| left.1.tech_delay.total_cmp(&right.1.tech_delay))
            .then_with(|| left.1.candidate_id.cmp(&right.1.candidate_id))
    });
    let mut seen = HashSet::new();
    ranked.retain(|(_, candidate, _)| seen.insert(candidate.choices.clone()));
    ranked.truncate(max_candidates);
    let candidates: Vec<_> = ranked
        .iter()
        .map(|(_, candidate, _)| candidate.clone())
        .collect();
    let audit_rows: Vec<_> = ranked.into_iter().map(|(_, _, audit)| audit).collect();
    fs::write(
        output_path,
        serde_json::to_string_pretty(&candidates)? + "\n",
    )?;
    let audit_path = output_path.with_file_name(format!(
        "{}_audit.json",
        output_path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("deletion_plan")
    ));
    fs::write(
        audit_path,
        serde_json::to_string_pretty(&json!({
            "graph":graph_path,
            "liberty":liberty_path,
            "policy":"strict predicted Liberty-area reduction over exclusive fanin cone",
            "max_cone_depth":3,
            "max_boundary":4,
            "candidate_cap":max_candidates,
            "mapped_recipe_lane_enabled":enable_mapped_recipes && !bounded_functional_fast_root_screen,
            "bounded_functional_window_enabled":enable_bounded_functional_windows,
            "roots_considered":roots_considered,
            "roots_with_reduction":roots_with_reduction,
            "mapped_window_policy":{
                "maximum_cut_inputs":4,
                "cut_limit_per_node":32,
                "mapped_recipes_per_function":3,
                "target_directed_recipe_cells":[4,5],
                "deep_recipe_signature_beam":256,
                "pricing":"fanout-aware reclaimed area with recipe leaves protected",
            },
            "mapped_window_roots_considered":mapped_window_roots_considered,
            "mapped_window_cuts_searched":mapped_window_cuts_searched,
            "mapped_window_unique_deep_targets":mapped_window_unique_deep_targets,
            "mapped_window_raw_matches":mapped_window_raw_matches,
            "mapped_window_deep_raw_matches":mapped_window_deep_raw_matches,
            "mapped_window_candidates":mapped_window_candidates,
            "mapped_window_deep_candidates":mapped_window_deep_candidates,
            "mffc_policy":{
                "fanout_aware":true,
                "max_reclaimed_instances":16,
                "max_boundary":6,
                "max_replacement_depth":2,
            },
            "mffc_roots_considered":mffc_roots_considered,
            "mffc_roots_with_reduction":mffc_roots_with_reduction,
            "mffc_candidates":mffc_candidates,
            "local_cut_policy":{
                "max_internal":12,
                "max_boundary":8,
                "graph_radius":4,
                "max_divisors":32,
                "max_pair_matches_per_root":2,
            },
            "local_cut_roots_considered":local_cut_roots_considered,
            "local_cut_roots_with_divisors":local_cut_roots_with_divisors,
            "local_equivalence_candidates":local_equivalence_candidates,
            "local_complement_candidates":local_complement_candidates,
            "local_resubstitution_candidates":local_resubstitution_candidates,
            "local_complex_resubstitution_candidates":local_complex_resubstitution_candidates,
            "mapped_recipe_candidates":mapped_recipe_candidates,
            "bounded_functional_window_policy":{
                "semantic_boundary_inputs":[2,8],
                "maximum_window_nodes":24,
                "maximum_roots":bounded_functional_root_cap,
                "maximum_divisors_per_root":bounded_functional_divisor_cap,
                "fast_root_screen":bounded_functional_fast_root_screen,
                "functional_only":bounded_functional_fast_root_screen,
                "preferred_roots":bounded_functional_preferred_roots,
                "preferred_neighborhood_radius":0,
                "preferred_neighborhood_roots":preferred_neighborhood.len(),
                "graph_radius":6,
                "maximum_divisors":bounded_functional_divisor_cap,
                "inner_divisors":bounded_functional_divisor_cap.min(48),
                "intermediate_signatures":(bounded_functional_divisor_cap * 8).clamp(32,384),
                "pareto_realizations_per_signature":3,
                "maximum_chain_cells":5,
                "maximum_resource_debt":"bounded minimum mapped-family cell areas",
                "maximum_resource_debt_cells":bounded_functional_max_area_debt_cells,
            },
            "bounded_functional_roots_considered":bounded_functional_roots_considered,
            "bounded_functional_roots_preselected":bounded_functional_roots_preselected,
            "bounded_functional_roots_searched":bounded_functional_roots_searched,
            "bounded_functional_raw_matches":bounded_functional_raw_matches,
            "bounded_functional_candidates":bounded_functional_candidates,
            "bounded_functional_resource_debt_candidates":bounded_functional_resource_debt_candidates,
            "bounded_functional_empty_reclaim_rejections":bounded_functional_empty_reclaim_rejections,
            "bounded_functional_area_debt_rejections":bounded_functional_area_debt_rejections,
            "bounded_functional_best_raw_saving":if bounded_functional_best_raw_saving.is_finite() { Some(bounded_functional_best_raw_saving) } else { None },
            "global_primary_input_count":primary_inputs.len(),
            "global_equivalence_candidates":global_equivalence_candidates,
            "global_complement_candidates":global_complement_candidates,
            "global_resubstitution_candidates":global_resubstitution_candidates,
            "candidate_count":candidates.len(),
            "candidates":audit_rows,
            "elapsed_sec":started.elapsed().as_secs_f64(),
        }))? + "\n",
    )?;
    Ok(candidates)
}

impl Default for GeneratorConfig {
    fn default() -> Self {
        Self {
            max_boundary: 8,
            max_divisors: 8,
            max_region: 16,
            max_forward_depth: 4,
            max_candidates_per_region: 16,
            realization_weights: None,
        }
    }
}

pub fn generate(
    graph_path: &Path,
    plan_path: &Path,
    liberty_path: &Path,
    output_path: &Path,
    config: GeneratorConfig,
) -> Result<Vec<GeneratorCandidate>> {
    let started = Instant::now();
    let graph: SourceGraph = serde_json::from_slice(&fs::read(graph_path)?)?;
    let nodes: HashMap<_, _> = graph
        .occurrences
        .into_iter()
        .map(|node| (node.anchor, node))
        .collect();
    let plan: Vec<PlannerRow> = serde_json::from_slice(&fs::read(plan_path)?)?;
    let technology = technology_context(liberty_path)?;
    let areas = &technology.areas;
    let delays = &technology.delays;
    let mappings = &technology.mappings;
    let cells = &mappings["area"];
    let wanted_sizes = BTreeSet::from([4usize, 8, 16]);
    let windows = discover_windows(
        &plan,
        &wanted_sizes,
        &nodes,
        config.max_region,
        config.max_forward_depth,
    )?;
    let generator_jobs = std::env::var("EGG_GENERATOR_JOBS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(16)
        .max(1);
    let window_results = parallel_map_ordered(&windows, generator_jobs, |_, window| {
        // A cache is local to a window.  Generator choices are merged in the
        // original deterministic window order below, so parallel execution
        // cannot perturb candidate IDs or tie-breaking.
        let mut cache = TopGateCache::default();
        let outputs: Vec<_> = window
            .region
            .iter()
            .filter(|anchor| {
                nodes[anchor]
                    .consumers
                    .iter()
                    .any(|consumer| !window.region.contains(consumer))
            })
            .copied()
            .collect();
        let boundary: Vec<_> = window
            .region
            .iter()
            .flat_map(|anchor| nodes[anchor].inputs.iter().copied())
            .filter(|child| !window.region.contains(child))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut region_candidates = Vec::new();

        let host_boundary = nodes[&window.host].inputs.clone();
        if !host_boundary.is_empty() && host_boundary.len() <= config.max_boundary {
            let bits = 1usize << host_boundary.len();
            let terminals: Vec<_> = host_boundary
                .iter()
                .enumerate()
                .map(|(index, anchor)| Term {
                    truth: variable_truth(index, host_boundary.len()),
                    expression: GeneratorExpr::Anchor { anchor: *anchor },
                    text: format!("@{anchor}"),
                    area: 0.0,
                    depth: 0,
                    anchors: BTreeSet::from([*anchor]),
                    families: BTreeSet::new(),
                })
                .collect();
            if let Some(target) = truth_of(window.host, &host_boundary, &nodes, None) {
                let mut terms: Vec<_> = one_gate_terms(&terminals, cells, bits, 96)?
                    .into_iter()
                    .filter(|term| term.truth == target)
                    .collect();
                terms.extend(two_gate_target_terms(
                    &terminals, target, cells, bits, 12, &mut cache,
                )?);
                let unique: BTreeMap<_, _> = terms
                    .into_iter()
                    .map(|term| (term.text.clone(), term))
                    .collect();
                let mut terms: Vec<_> = unique.into_values().collect();
                terms.sort_by(|left, right| {
                    left.area
                        .total_cmp(&right.area)
                        .then_with(|| left.depth.cmp(&right.depth))
                        .then_with(|| left.text.cmp(&right.text))
                });
                for (index, term) in terms.into_iter().take(16).enumerate() {
                    let mut proof_boundary = host_boundary.clone();
                    proof_boundary.sort_unstable();
                    let mut candidate = empty_candidate(
                        format!("SHAREDHOST_{}_H{}_{}", window.region_id, window.host, index),
                        vec![
                            if window.kind.starts_with("reconvergent") {
                                "reconvergent-shared-host-resynthesis".to_owned()
                            } else {
                                "multi-output-shared-host-resynthesis".to_owned()
                            },
                            window.region_id.clone(),
                        ],
                        vec![GeneratorChoice {
                            root_anchor: window.host,
                            expression: term.expression,
                        }],
                        outputs.clone(),
                        window,
                        proof_boundary,
                        vec![window.host],
                    );
                    candidate.tech_area = term.area;
                    candidate.tech_delay = term.depth as f64;
                    candidate.logic_depth = term.depth;
                    candidate.cell_families = term.families.into_iter().collect();
                    candidate.shared_host = Some(window.host);
                    region_candidates.push(candidate);
                }
            }
        }

        if outputs.is_empty() || boundary.len() > config.max_boundary {
            let mapped = region_candidates
                .iter()
                .map(|candidate| expand_technology_mappings(candidate, &mappings, &areas, &delays))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect();
            let kept = select_realizations(
                mapped,
                config.max_candidates_per_region,
                config.realization_weights,
            );
            let audit = json!({
                "region_id":window.region_id,"region_size":window.region.len(),
                "window_kind":window.kind,"designated_host":window.host,
                "outputs":outputs,"boundary":boundary,"generated":region_candidates.len(),
                "kept":kept.len(),"skip":"empty outputs or boundary over limit",
            });
            return Ok(WindowGenerationResult {
                candidates: kept,
                audit,
                cache_hits: cache.hits,
                cache_misses: cache.misses,
            });
        }
        let output_truth: BTreeMap<_, _> = outputs
            .iter()
            .filter_map(|output| {
                truth_of(*output, &boundary, &nodes, None).map(|truth| (*output, truth))
            })
            .collect();
        if output_truth.len() != outputs.len() {
            let audit = json!({
                "region_id":window.region_id,"region_size":window.region.len(),
                "window_kind":window.kind,"designated_host":window.host,
                "outputs":outputs,"boundary":boundary,"generated":0,
                "skip":"unsupported incumbent function",
            });
            return Ok(WindowGenerationResult {
                candidates: Vec::new(),
                audit,
                cache_hits: cache.hits,
                cache_misses: cache.misses,
            });
        }
        let bits = 1usize << boundary.len();
        let terminals: Vec<_> = boundary
            .iter()
            .enumerate()
            .map(|(index, anchor)| Term {
                truth: variable_truth(index, boundary.len()),
                expression: GeneratorExpr::Anchor { anchor: *anchor },
                text: format!("@{anchor}"),
                area: 0.0,
                depth: 0,
                anchors: BTreeSet::from([*anchor]),
                families: BTreeSet::new(),
            })
            .collect();

        let nearby: Vec<_> = neighborhood(window.host, &nodes, 4)
            .into_iter()
            .filter(|anchor| {
                !window.region.contains(anchor)
                    && !boundary.contains(anchor)
                    && !nodes[anchor].is_leaf
                    && !nodes[anchor].is_root
            })
            .collect();
        let terminal_truths: HashSet<_> = terminals.iter().map(|term| term.truth).collect();
        let mut divisor_truths = HashSet::new();
        let mut divisors = Vec::new();
        for anchor in nearby {
            let Some(truth) = truth_of(anchor, &boundary, &nodes, Some(&window.region)) else {
                continue;
            };
            if terminal_truths.contains(&truth) || !divisor_truths.insert(truth) {
                continue;
            }
            divisors.push(Term {
                truth,
                expression: GeneratorExpr::Anchor { anchor },
                text: format!("@{anchor}"),
                area: 0.0,
                depth: 0,
                anchors: BTreeSet::from([anchor]),
                families: BTreeSet::from(["DIVISOR".to_owned()]),
            });
            if divisors.len() == config.max_divisors {
                break;
            }
        }
        let internal_truth: BTreeMap<_, _> = window
            .region
            .iter()
            .filter_map(|anchor| {
                truth_of(*anchor, &boundary, &nodes, None).map(|truth| (*anchor, truth))
            })
            .collect();
        for divisor in &divisors {
            let divisor_anchor = *divisor.anchors.iter().next().unwrap();
            for (replaced, truth) in &internal_truth {
                if *truth != divisor.truth {
                    continue;
                }
                for output in &outputs {
                    let expression = incumbent_expression(
                        *output,
                        &window.region,
                        &nodes,
                        *replaced,
                        divisor_anchor,
                    );
                    let mut anchors = BTreeSet::new();
                    expression_anchors(&expression, &mut anchors);
                    if !anchors.contains(&divisor_anchor) {
                        continue;
                    }
                    let mut candidate = empty_candidate(
                        format!(
                            "RESUB_{}_O{}_N{}_D{}",
                            window.region_id, output, replaced, divisor_anchor
                        ),
                        vec![
                            "explicit-divisor-resubstitution".to_owned(),
                            window.region_id.clone(),
                        ],
                        vec![GeneratorChoice {
                            root_anchor: *output,
                            expression,
                        }],
                        vec![*output],
                        window,
                        boundary.clone(),
                        vec![*output],
                    );
                    candidate.divisors = vec![divisor_anchor];
                    candidate.replaced_anchor = Some(*replaced);
                    region_candidates.push(candidate);
                }
            }
        }
        if !divisors.is_empty() {
            let mut resub_terminals = terminals.clone();
            resub_terminals.extend(divisors.iter().cloned());
            let divisor_anchors: BTreeSet<_> = divisors
                .iter()
                .filter_map(|term| term.anchors.iter().next().copied())
                .collect();
            let resub_terms = one_gate_terms(&resub_terminals, cells, bits, 256)?;
            for output in &outputs {
                let matches = resub_terms.iter().filter(|term| {
                    term.truth == output_truth[output]
                        && term
                            .anchors
                            .iter()
                            .any(|anchor| divisor_anchors.contains(anchor))
                });
                for (index, term) in matches.take(4).enumerate() {
                    let mut candidate = empty_candidate(
                        format!("RESUB_{}_O{}_{}", window.region_id, output, index),
                        vec![
                            "explicit-divisor-resubstitution".to_owned(),
                            window.region_id.clone(),
                        ],
                        vec![GeneratorChoice {
                            root_anchor: *output,
                            expression: term.expression.clone(),
                        }],
                        vec![*output],
                        window,
                        boundary.clone(),
                        vec![*output],
                    );
                    candidate.tech_area = term.area;
                    candidate.tech_delay = term.depth as f64;
                    candidate.logic_depth = term.depth;
                    candidate.cell_families = term.families.iter().cloned().collect();
                    candidate.divisors = term
                        .anchors
                        .difference(&boundary.iter().copied().collect())
                        .copied()
                        .collect();
                    region_candidates.push(candidate);
                }
            }
        }

        let shared_hosts: Vec<_> = window
            .region
            .iter()
            .filter(|anchor| {
                !outputs.contains(anchor)
                    && nodes[anchor]
                        .consumers
                        .iter()
                        .all(|consumer| window.region.contains(consumer))
            })
            .copied()
            .collect();
        let mut shared_terminals = terminals.clone();
        shared_terminals.extend(divisors.iter().cloned());
        let shared_functions = one_gate_terms(&shared_terminals, cells, bits, 96)?;
        if outputs.len() >= 2 {
            for host in shared_hosts.iter().take(4) {
                for shared in &shared_functions {
                    let shared_terminal = Term {
                        truth: shared.truth,
                        expression: GeneratorExpr::Anchor { anchor: *host },
                        text: format!("@{host}"),
                        area: 0.0,
                        depth: 0,
                        anchors: BTreeSet::from([*host]),
                        families: BTreeSet::from(["SHARED".to_owned()]),
                    };
                    let mut mappings_for_outputs = Vec::new();
                    for output in &outputs {
                        let found = cache.lookup(
                            &shared_terminal,
                            &shared_terminals,
                            output_truth[output],
                            cells,
                            bits,
                            1,
                        )?;
                        let Some(mapping) = found.into_iter().next() else {
                            mappings_for_outputs.clear();
                            break;
                        };
                        mappings_for_outputs.push((*output, mapping));
                    }
                    if mappings_for_outputs.len() < 2 {
                        continue;
                    }
                    let mut choices = vec![GeneratorChoice {
                        root_anchor: *host,
                        expression: shared.expression.clone(),
                    }];
                    choices.extend(mappings_for_outputs.iter().map(|(output, mapping)| {
                        GeneratorChoice {
                            root_anchor: *output,
                            expression: mapping.expression.clone(),
                        }
                    }));
                    let mut provenance = vec!["multi-output-shared-dag".to_owned()];
                    let boundary_set: BTreeSet<_> = boundary.iter().copied().collect();
                    if shared
                        .anchors
                        .iter()
                        .any(|anchor| !boundary_set.contains(anchor))
                    {
                        provenance.push("explicit-divisor-resubstitution".to_owned());
                    }
                    provenance.push(window.region_id.clone());
                    let id_index = region_candidates.len();
                    let mut candidate = empty_candidate(
                        format!("MULTIOUT_{}_H{}_{}", window.region_id, host, id_index),
                        provenance,
                        choices,
                        outputs.clone(),
                        window,
                        boundary.clone(),
                        outputs.clone(),
                    );
                    candidate.shared_host = Some(*host);
                    candidate.divisors =
                        shared.anchors.difference(&boundary_set).copied().collect();
                    region_candidates.push(candidate);
                }
            }
        }
        let mapped = region_candidates
            .iter()
            .map(|candidate| expand_technology_mappings(candidate, &mappings, &areas, &delays))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        let kept = select_realizations(
            mapped,
            config.max_candidates_per_region,
            config.realization_weights,
        );
        let audit = json!({
            "region_id":window.region_id,"region_size":window.region.len(),
            "window_kind":window.kind,"designated_host":window.host,
            "outputs":outputs,"boundary":boundary,"generated":region_candidates.len(),
            "kept":kept.len(),"divisors":divisors.iter().map(|term|term.text.clone()).collect::<Vec<_>>(),
            "shared_hosts":shared_hosts,"skip":Value::Null,
        });
        Ok(WindowGenerationResult {
            candidates: kept,
            audit,
            cache_hits: cache.hits,
            cache_misses: cache.misses,
        })
    })?;
    let mut all_candidates = Vec::new();
    let mut audit = Vec::new();
    let mut cache_hits = 0usize;
    let mut cache_misses = 0usize;
    for mut window_result in window_results {
        all_candidates.append(&mut window_result.candidates);
        audit.push(window_result.audit);
        cache_hits += window_result.cache_hits;
        cache_misses += window_result.cache_misses;
    }

    let mut unique: HashMap<Vec<GeneratorChoice>, usize> = HashMap::new();
    let mut result: Vec<GeneratorCandidate> = Vec::new();
    for mut candidate in all_candidates {
        if let Some(index) = unique.get(&candidate.choices).copied() {
            let previous = &mut result[index];
            for provenance in candidate.provenance.drain(..) {
                if !previous.provenance.contains(&provenance) {
                    previous.provenance.push(provenance);
                }
            }
            if !previous.source_windows.contains(&candidate.window_kind) {
                previous.source_windows.push(candidate.window_kind);
            }
        } else {
            candidate.source_windows = vec![candidate.window_kind.clone()];
            unique.insert(candidate.choices.clone(), result.len());
            result.push(candidate);
        }
    }
    result.sort_by(|left, right| {
        left.tech_area
            .total_cmp(&right.tech_area)
            .then_with(|| left.tech_delay.total_cmp(&right.tech_delay))
            .then_with(|| left.logic_depth.cmp(&right.logic_depth))
            .then_with(|| left.candidate_id.cmp(&right.candidate_id))
    });
    for (index, candidate) in result.iter_mut().enumerate() {
        candidate.candidate_id = format!("STRUCTGEN_V2_{index:04}_{}", candidate.candidate_id);
    }
    let mut selected_mapping_counts = BTreeMap::<String, usize>::new();
    let mut selected_logical_topologies = BTreeSet::new();
    for candidate in &result {
        *selected_mapping_counts
            .entry(
                candidate
                    .mapping
                    .clone()
                    .unwrap_or_else(|| "unknown".to_owned()),
            )
            .or_default() += 1;
        if let Some(signature) = &candidate.logical_signature {
            selected_logical_topologies.insert(signature.clone());
        }
    }
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output_path, serde_json::to_string_pretty(&result)? + "\n")?;
    let audit_path = output_path.with_file_name(format!(
        "{}_audit.json",
        output_path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("macro_plan")
    ));
    fs::write(
        audit_path,
        serde_json::to_string_pretty(&json!({
            "graph":graph_path,"plan":plan_path,"liberty":liberty_path,
            "candidate_count":result.len(),"regions":audit,
            "realization_allocation":config.realization_weights.map(|weights|
                realization_allocation(config.max_candidates_per_region, weights)),
            "selected_mapping_counts":selected_mapping_counts,
            "selected_logical_topology_count":selected_logical_topologies.len(),
            "structural_cache":{"top_gate_problem_hits":cache_hits,"top_gate_problem_misses":cache_misses},
            "generator_jobs":generator_jobs,
            "elapsed_sec":started.elapsed().as_secs_f64(),
            "implementation":"native Rust Generator V2",
        }))? + "\n",
    )?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_node(
        anchor: usize,
        op: &str,
        inputs: &[usize],
        consumers: &[usize],
        is_leaf: bool,
        is_root: bool,
    ) -> SourceNode {
        SourceNode {
            anchor,
            op: op.to_owned(),
            inputs: inputs.to_vec(),
            consumers: consumers.to_vec(),
            is_leaf,
            is_root,
            is_constant: false,
        }
    }

    fn reconvergent_test_graph(external_fanout: bool) -> HashMap<usize, SourceNode> {
        let mut nodes = HashMap::from([
            (0, source_node(0, "a", &[], &[2], true, false)),
            (1, source_node(1, "b", &[], &[2], true, false)),
            (
                2,
                source_node(
                    2,
                    "AND2x1_ASAP7_6t_L",
                    &[0, 1],
                    if external_fanout { &[3, 4, 7] } else { &[3, 4] },
                    false,
                    false,
                ),
            ),
            (
                3,
                source_node(3, "INVx1_ASAP7_6t_L", &[2], &[5], false, false),
            ),
            (
                4,
                source_node(4, "INVx1_ASAP7_6t_L", &[2], &[5], false, false),
            ),
            (
                5,
                source_node(5, "OR2x1_ASAP7_6t_L", &[3, 4], &[6], false, false),
            ),
            (6, source_node(6, "output", &[5], &[], false, true)),
        ]);
        if external_fanout {
            nodes.insert(
                7,
                source_node(7, "INVx1_ASAP7_6t_L", &[2], &[8], false, false),
            );
            nodes.insert(8, source_node(8, "output", &[7], &[], false, true));
        }
        nodes
    }

    #[test]
    fn full186_drive_suffix_is_not_part_of_logic_family() {
        assert_eq!(logic_name("INVxp33R_ASAP7_6t_L"), "INV");
        assert_eq!(logic_name("NAND2x1_ASAP7_6t_L"), "NAND2");
        assert_eq!(logic_name("AOI22xp5_ASAP7_6t_L"), "AOI22");
    }

    #[test]
    fn truth_tables_cover_all_256_assignments() {
        let bits = 256;
        let high_input = variable_truth(7, 8);
        let low_input = variable_truth(0, 8);
        let xor = apply_family("XOR2", &[high_input, low_input], bits).unwrap();
        for assignment in 0..bits {
            let expected = (((assignment >> 7) & 1) ^ (assignment & 1)) as u64;
            let actual = (xor.0[assignment / 64] >> (assignment % 64)) & 1;
            assert_eq!(actual, expected, "assignment {assignment}");
        }
    }

    #[test]
    fn mapped_recipe_atlas_covers_four_input_parity_with_three_cells() {
        let cells = FAMILIES
            .iter()
            .map(|(family, _)| {
                (
                    (*family).to_owned(),
                    TechCell {
                        name: (*family).to_owned(),
                        area: 1.0,
                        delay: 1.0,
                    },
                )
            })
            .collect();
        let atlas = mapped_recipe_atlas(4, &cells).unwrap();
        let parity = variable_truth(0, 4)
            .xor(variable_truth(1, 4))
            .xor(variable_truth(2, 4))
            .xor(variable_truth(3, 4));
        let recipes = atlas.get(&parity).expect("four-input parity recipe");
        assert!(recipes.iter().any(|term| term.area <= 3.0));
    }

    #[test]
    fn bounded_functional_window_finds_two_level_divisor_dag() {
        let cells = FAMILIES
            .iter()
            .map(|(family, _)| {
                (
                    (*family).to_owned(),
                    TechCell {
                        name: (*family).to_owned(),
                        area: 1.0,
                        delay: 1.0,
                    },
                )
            })
            .collect();
        let divisors: Vec<_> = (0..5)
            .map(|index| Term {
                truth: variable_truth(index, 5),
                expression: GeneratorExpr::Anchor { anchor: index },
                text: format!("@{index}"),
                area: 0.0,
                depth: 0,
                anchors: BTreeSet::from([index]),
                families: BTreeSet::new(),
            })
            .collect();
        let target = variable_truth(0, 5)
            .xor(variable_truth(1, 5))
            .xor(variable_truth(4, 5));
        let terms =
            bounded_functional_target_terms(&divisors, target, &cells, 1usize << 5, 32, 192, 8)
                .unwrap();
        assert!(terms.iter().any(|term| term.depth == 2 && term.area <= 2.0));

        let four_way = variable_truth(0, 5)
            .xor(variable_truth(1, 5))
            .xor(variable_truth(2, 5))
            .xor(variable_truth(3, 5));
        let terms =
            bounded_functional_target_terms(&divisors, four_way, &cells, 1usize << 5, 32, 192, 8)
                .unwrap();
        assert!(terms.iter().any(|term| term.area <= 3.0));
    }

    #[test]
    fn semantic_window_admits_independent_sibling_divisor() {
        let nodes = HashMap::from([
            (0, source_node(0, "a", &[], &[3, 4], true, false)),
            (1, source_node(1, "b", &[], &[4], true, false)),
            (2, source_node(2, "c", &[], &[3], true, false)),
            (
                3,
                source_node(3, "OR2x1_ASAP7_6t_L", &[0, 2], &[6], false, false),
            ),
            (
                4,
                source_node(4, "AND2x1_ASAP7_6t_L", &[0, 1], &[5], false, false),
            ),
            (5, source_node(5, "output", &[4], &[], false, true)),
            (6, source_node(6, "output", &[3], &[], false, true)),
        ]);
        let (internal, boundary) = bounded_semantic_window(4, &nodes, 8, 4, 3).unwrap();
        assert!(internal.contains(&4));
        assert!(
            internal.contains(&3),
            "independent sibling must be a divisor"
        );
        assert_eq!(boundary, vec![0, 1, 2]);
        assert!(!transitively_depends_on(3, 4, &nodes, &mut BTreeSet::new()));
    }

    #[test]
    fn k_feasible_cut_enumeration_keeps_multiple_expansion_choices() {
        let nodes = reconvergent_test_graph(false);
        let cuts = k_feasible_cuts(5, &nodes, 4, 32, &mut HashMap::new());
        assert!(cuts.contains(&vec![3, 4]), "direct fanin cut");
        assert!(cuts.contains(&vec![2]), "shared reconvergent cut");
        assert!(cuts.contains(&vec![0, 1]), "fully expanded PI cut");
    }

    #[test]
    fn mapped_recipe_candidates_keep_distinct_window_identity() {
        let window = Window {
            region_id: "DELETE_KCUT_R17_2_0".to_owned(),
            region: BTreeSet::from([17]),
            kind: "mapped-kfeasible-window-rewrite".to_owned(),
            host: 17,
        };
        let candidate = empty_candidate(
            "DELETE_KCUT_R17_2_0_AREA".to_owned(),
            vec![
                "mapped-kfeasible-window-rewrite".to_owned(),
                window.region_id.clone(),
            ],
            vec![GeneratorChoice {
                root_anchor: 17,
                expression: GeneratorExpr::Anchor { anchor: 3 },
            }],
            vec![17],
            &window,
            vec![3],
            vec![17],
        );
        assert_eq!(specific_window(&candidate), "DELETE_KCUT_R17_2_0");
        assert_eq!(provenance_class(&candidate), "cone-collapse-deletion");
    }

    #[test]
    fn realization_allocation_is_pressure_driven_and_d2ap_symmetric() {
        let d2ap = realization_allocation(16, MetricWeights::d2ap());
        assert!(d2ap.symmetric_pressure);
        assert_eq!(d2ap.topology_quota, 8);
        assert_eq!(d2ap.alternate_realization_quota, 8);

        let area = realization_allocation(
            16,
            MetricWeights {
                delay: 0.0,
                area: 1.0,
                power: 0.0,
            },
        );
        assert!(!area.symmetric_pressure);
        assert_eq!(area.topology_quota, 16);
        assert_eq!(area.alternate_realization_quota, 0);

        let mixed = realization_allocation(
            16,
            MetricWeights {
                delay: 3.0,
                area: 1.0,
                power: 0.0,
            },
        );
        assert_eq!(mixed.topology_quota, 12);
        assert_eq!(mixed.alternate_realization_quota, 4);
    }

    #[test]
    fn fanout_aware_mffc_reclaims_internal_reconvergence() {
        let nodes = reconvergent_test_graph(false);
        assert_eq!(
            fanout_aware_reclaim_cone(5, &BTreeSet::new(), &nodes),
            BTreeSet::from([2, 3, 4, 5])
        );
        let (_, boundary) = bounded_fanin_cut(5, &nodes, 8, 8).unwrap();
        assert_eq!(boundary, vec![0, 1]);
    }

    #[test]
    fn fanout_aware_mffc_preserves_externally_live_fanin() {
        let nodes = reconvergent_test_graph(true);
        assert_eq!(
            fanout_aware_reclaim_cone(5, &BTreeSet::new(), &nodes),
            BTreeSet::from([3, 4, 5])
        );
    }
}
