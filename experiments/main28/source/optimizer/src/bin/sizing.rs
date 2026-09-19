//! Fast, single-process Rust implementation of the frozen A2-Next sizing stage.
//!
//! The search policy is intentionally frozen: finite-difference mirror descent,
//! R2 beam projection, local-pair projection and O1<=5 exact polish.  The speedup
//! comes from parsing Liberty/netlist once and caching the immutable circuit
//! topology instead of rebuilding it for every physical evaluation.

use anyhow::{Context, Result, bail};
#[cfg(feature = "milp-cbc")]
use coin_cbc::{Model, Sense};
use d1_series::equivalence_candidate::{
    extraction_candidate, extraction_candidate_id, unified_equivalence_enabled,
    validate_extraction_portfolio,
};
use d1_series::objective::{PpaPoint, SearchObjective};
use d1_series::shared_load_v2::{
    PortBoundaryOverrides, PortInputBoundary, PortOutputBoundary, TimingBoundary,
    verilog_scalar_constant,
};
use d1_series::v8_potential::{V8PotentialConfig, choose_potential_portfolio};
use d1_series::v8_pro::{
    CandidateObservation, V8ProPolicyConfig, choose_generator_region_representatives,
    choose_portfolio_limit, choose_stage50_survivors, evaluate_portfolio_safety,
    summarize_candidates,
};
use d1_series::v8_ultra::V8UltraConfig;
use egraph_serialize::{ClassId, Cost, Node, NodeId};
use extraction_gym::{ExtendedEGraph, Lut2D, NLDM, TimingSense};
use mac_egg::SerializedEGraph;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

const EPS: f64 = 1e-12;
const TAU_RATIO: f64 = 0.02;

#[derive(Default)]
struct RunProfile {
    gradient_sec: f64,
    local_pair_total_sec: f64,
    local_pair_solve_sec: f64,
    local_pair_calls: usize,
}

#[derive(Clone, Debug, Serialize)]
struct UltraA2Audit {
    enabled: bool,
    instance_count: usize,
    eligible_instance_count: usize,
    gate_threshold: Option<usize>,
    active_instance_cap: Option<usize>,
    timing_slots: usize,
    area_slots: usize,
    power_slots: usize,
    selected_instance_count: usize,
    selected_instances: Vec<usize>,
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
        .map(|(index, value)| value.with_context(|| format!("missing parallel result {index}"))?)
        .collect()
}

#[derive(Clone)]
struct Inst {
    name: String,
    original_cell: usize,
    inputs: Vec<String>,
    input_sources: Vec<Option<usize>>,
    input_is_primary: Vec<bool>,
    input_primary_index: Vec<Option<usize>>,
}

#[derive(Clone, Copy)]
enum BoundaryOutputSource {
    Gate(usize),
    Primary(usize),
    Constant,
}

#[derive(Clone, Serialize, Deserialize)]
struct ArcEval {
    parent: usize,
    output_edge: usize,
    source: Option<usize>,
    input_edge: usize,
    pin: usize,
    delay: f64,
    arrival: f64,
}

#[derive(Clone)]
struct Eval {
    delay: f64,
    area: f64,
    power: f64,
    score: f64,
    primary_arrival: Arc<Vec<[f64; 2]>>,
    primary_transition: Arc<Vec<[f64; 2]>>,
    arrival: Vec<[f64; 2]>,
    transition: Vec<[f64; 2]>,
    load: Vec<f64>,
    cell_area: Vec<f64>,
    cell_power: Vec<f64>,
    arcs: Vec<ArcEval>,
    arcs_by_state: Arc<Vec<[Vec<usize>; 2]>>,
    arcs_by_parent: Arc<Vec<Vec<usize>>>,
}

#[derive(Clone, Copy)]
struct DeltaMetrics {
    delay: f64,
    area: f64,
    power: f64,
    weighted_arc_delta: f64,
}

#[derive(Clone)]
struct Distribution {
    cells: Arc<Vec<usize>>,
    probabilities: Arc<Vec<f64>>,
}

#[derive(Clone)]
struct ProbabilityDeltaPlan {
    affected_topo: Vec<usize>,
    changed_sources: Vec<usize>,
    changed_primary_consumers: Vec<usize>,
    changed_primaries: Vec<usize>,
    touches_primary: bool,
}

#[derive(Clone)]
struct IterativeLut2D {
    index_1: Vec<f64>,
    index_2: Vec<f64>,
    values: Vec<f64>,
}

#[derive(Clone, Copy)]
struct IterativeLookup2D {
    xi: usize,
    yi: usize,
    wx: f64,
    wy: f64,
}

impl IterativeLut2D {
    fn from_lut(lut: &Lut2D) -> Self {
        Self {
            index_1: lut.index_1.iter().map(|value| value.into_inner()).collect(),
            index_2: lut.index_2.iter().map(|value| value.into_inner()).collect(),
            values: lut
                .values
                .iter()
                .flatten()
                .map(|value| value.into_inner())
                .collect(),
        }
    }

    #[inline]
    fn coordinates(&self, slew: f64, load: f64) -> IterativeLookup2D {
        let bracket = |axis: &[f64], value: f64| {
            if value < axis[0] {
                1
            } else if value > axis[axis.len() - 1] {
                axis.len() - 1
            } else {
                axis.iter()
                    .position(|entry| *entry >= value)
                    .unwrap_or(axis.len() - 1)
                    .max(1)
            }
        };
        let xi = bracket(&self.index_1, slew);
        let yi = bracket(&self.index_2, load);
        let (x0, x1) = (self.index_1[xi - 1], self.index_1[xi]);
        let (y0, y1) = (self.index_2[yi - 1], self.index_2[yi]);
        let wx = if x1 == x0 {
            1.0
        } else {
            (x1 - slew) / (x1 - x0)
        };
        let wy = if y1 == y0 {
            1.0
        } else {
            (y1 - load) / (y1 - y0)
        };
        IterativeLookup2D { xi, yi, wx, wy }
    }

    #[inline]
    fn lookup_at(&self, coordinates: IterativeLookup2D) -> f64 {
        let IterativeLookup2D { xi, yi, wx, wy } = coordinates;
        let width = self.index_2.len();
        let q00 = self.values[(xi - 1) * width + yi - 1];
        let q01 = self.values[(xi - 1) * width + yi];
        let q10 = self.values[xi * width + yi - 1];
        let q11 = self.values[xi * width + yi];
        q00 * wx * wy
            + q01 * wx * (1.0 - wy)
            + q10 * (1.0 - wx) * wy
            + q11 * (1.0 - wx) * (1.0 - wy)
    }

    #[inline]
    fn lookup(&self, slew: f64, load: f64) -> f64 {
        self.lookup_at(self.coordinates(slew, load))
    }

    fn same_grid(&self, other: &Self) -> bool {
        self.index_1 == other.index_1 && self.index_2 == other.index_2
    }
}

#[derive(Clone)]
struct IterativeTimingArc {
    rise_delay: IterativeLut2D,
    fall_delay: IterativeLut2D,
    rise_transition: IterativeLut2D,
    fall_transition: IterativeLut2D,
}

struct Circuit {
    source_path: PathBuf,
    source_text: String,
    instances: Vec<Inst>,
    roots: Vec<usize>,
    output_names: Vec<String>,
    output_sources: Vec<BoundaryOutputSource>,
    reachable: Vec<usize>,
    reachable_set: HashSet<usize>,
    topo: Vec<usize>,
    consumers: Vec<Vec<(usize, usize)>>,
    probability_delta_plans: Vec<ProbabilityDeltaPlan>,
    output_load_count: Vec<usize>,
    primary_output_load_count: Vec<usize>,
    fixed_primary_input: Option<Vec<PortInputBoundary>>,
    fixed_output_boundary: Option<Vec<PortOutputBoundary>>,
    fixed_output_load_ff: Option<Vec<f64>>,
    fixed_primary_output_load_ff: Option<Vec<f64>>,
    primary_consumers: Vec<Vec<(usize, usize)>>,
    cells: Arc<Vec<NLDM>>,
    cell_names: Arc<Vec<String>>,
    iterative_power_luts: Vec<Vec<IterativeLut2D>>,
    iterative_timing_luts: Vec<Vec<IterativeTimingArc>>,
    iterative_power_shared_grids: Vec<Vec<bool>>,
    iterative_timing_shared_grids: Vec<Vec<[[bool; 2]; 2]>>,
    input_caps: Vec<Vec<f64>>,
    timing_arc_pins: Vec<Vec<usize>>,
    families: Vec<Vec<usize>>,
    objective: SearchObjective,
    port_objective: Option<PortObjective>,
    boundary: TimingBoundary,
    iterative: bool,
    topology_sha256: String,
    exact_context_sha256: String,
}

/// Opt-in objective for independently optimized graph partitions.  It turns
/// each output's required time into a common global-delay coordinate and adds
/// local area/power deltas to the untouched whole-net baseline.  This is kept
/// outside SearchObjective so the production whole-net objective remains
/// bit-for-bit unchanged when EGG_PORT_OBJECTIVE_JSON is unset.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PortObjective {
    schema: String,
    global_delay_ps: f64,
    global_area: f64,
    global_power: f64,
    local_baseline_area: f64,
    local_baseline_power: f64,
    #[serde(default = "default_delay_exponent")]
    delay_exponent: f64,
    #[serde(default = "default_one")]
    area_exponent: f64,
    #[serde(default = "default_one")]
    power_exponent: f64,
    #[serde(default)]
    hard_lateness_ps: Option<f64>,
    #[serde(default)]
    max_arrival_regression_ps: Option<f64>,
    #[serde(default)]
    max_slew_regression_ps: Option<f64>,
    #[serde(default)]
    max_input_cap_regression_ff: Option<f64>,
    /// Optional surcharge on positive output-arrival regression after it is
    /// weighted by the output's reachable/reconvergent path-family exposure.
    /// This is intentionally absent from historical objective files.
    #[serde(default)]
    path_family_arrival_penalty_scale: Option<f64>,
    /// A local region normally cannot claim a whole-net delay improvement:
    /// an untouched competing path may remain at the G0 delay.  V2 callers
    /// can therefore floor negative boundary lateness at zero.  The default
    /// remains true to preserve every existing port-objective trajectory.
    #[serde(default = "default_true")]
    credit_negative_lateness: bool,
}

const fn default_delay_exponent() -> f64 {
    2.0
}

const fn default_one() -> f64 {
    1.0
}

const fn default_true() -> bool {
    true
}

impl PortObjective {
    fn from_env() -> Result<Option<Self>> {
        let Some(path) = std::env::var_os("EGG_PORT_OBJECTIVE_JSON") else {
            return Ok(None);
        };
        let path = PathBuf::from(path);
        let value: Self = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("cannot parse {}", path.display()))?;
        anyhow::ensure!(
            value.schema == "egg-port-objective-v1" || value.schema == "egg-port-objective-v2",
            "unsupported port objective schema {}",
            value.schema
        );
        for (field, number) in [
            ("global_delay_ps", value.global_delay_ps),
            ("global_area", value.global_area),
            ("global_power", value.global_power),
            ("local_baseline_area", value.local_baseline_area),
            ("local_baseline_power", value.local_baseline_power),
        ] {
            anyhow::ensure!(
                number.is_finite() && number > 0.0,
                "{field} must be positive"
            );
        }
        for (field, exponent) in [
            ("delay_exponent", value.delay_exponent),
            ("area_exponent", value.area_exponent),
            ("power_exponent", value.power_exponent),
        ] {
            anyhow::ensure!(
                exponent.is_finite() && exponent >= 0.0,
                "{field} must be finite and non-negative"
            );
        }
        anyhow::ensure!(
            value.delay_exponent + value.area_exponent + value.power_exponent > 0.0,
            "port objective needs a positive exponent"
        );
        for (field, limit) in [
            ("hard_lateness_ps", value.hard_lateness_ps),
            ("max_arrival_regression_ps", value.max_arrival_regression_ps),
            ("max_slew_regression_ps", value.max_slew_regression_ps),
            (
                "max_input_cap_regression_ff",
                value.max_input_cap_regression_ff,
            ),
            (
                "path_family_arrival_penalty_scale",
                value.path_family_arrival_penalty_scale,
            ),
        ] {
            if let Some(limit) = limit {
                anyhow::ensure!(
                    limit.is_finite() && limit >= 0.0,
                    "{field} must be finite and non-negative"
                );
            }
        }
        anyhow::ensure!(
            value.schema == "egg-port-objective-v2"
                || value.path_family_arrival_penalty_scale.is_none(),
            "path_family_arrival_penalty_scale requires egg-port-objective-v2"
        );
        Ok(Some(value))
    }
}

#[derive(Clone)]
pub(crate) struct CellDatabase {
    cells: Arc<Vec<NLDM>>,
    cell_names: Arc<Vec<String>>,
    cell_by_name: HashMap<String, usize>,
    family_names: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct Choice(Vec<usize>);

#[derive(Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct NamedChoice(Vec<String>);

struct Tracker {
    budget: usize,
    count: usize,
    paid_count: usize,
    polish_count: usize,
    cache: HashMap<Choice, Ppa>,
    replay_limit: usize,
    replay_seen: HashSet<Choice>,
    best_choice: Choice,
    best: Eval,
    trajectory: Vec<TrajectoryPoint>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Ppa {
    delay: f64,
    area: f64,
    power: f64,
    score: f64,
}

#[derive(Default)]
struct IterativeExactCache {
    values: Mutex<HashMap<(String, NamedChoice), Ppa>>,
    hits: AtomicUsize,
    misses: AtomicUsize,
}

static ITERATIVE_EXACT_CACHE: OnceLock<IterativeExactCache> = OnceLock::new();
static ITERATIVE_EXACT_CACHE_ENABLED: OnceLock<bool> = OnceLock::new();
static ITERATIVE_SHARED_GRID_ENABLED: OnceLock<bool> = OnceLock::new();
static ITERATIVE_DYNAMIC_CONE_ENABLED: OnceLock<bool> = OnceLock::new();

fn iterative_exact_cache() -> &'static IterativeExactCache {
    ITERATIVE_EXACT_CACHE.get_or_init(IterativeExactCache::default)
}

fn iterative_exact_cache_enabled() -> bool {
    *ITERATIVE_EXACT_CACHE_ENABLED.get_or_init(|| {
        std::env::var("EGG_ITERATIVE_EXACT_STATE_CACHE")
            .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
            .unwrap_or(false)
    })
}

fn iterative_shared_grid_enabled() -> bool {
    *ITERATIVE_SHARED_GRID_ENABLED.get_or_init(|| {
        std::env::var("EGG_ITERATIVE_SHARED_GRID")
            .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
            .unwrap_or(false)
    })
}

fn iterative_dynamic_cone_enabled() -> bool {
    *ITERATIVE_DYNAMIC_CONE_ENABLED.get_or_init(|| {
        std::env::var("EGG_ITERATIVE_DYNAMIC_CONE")
            .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
            .unwrap_or(true)
    })
}

#[derive(Clone, Serialize, Deserialize)]
struct TrajectoryPoint {
    exact_evaluations: usize,
    best: Ppa,
}

#[derive(Clone, Serialize, Deserialize)]
struct BatchTask {
    input_netlist: PathBuf,
    out_dir: PathBuf,
    mode: String,
    #[serde(default)]
    resume_checkpoint: Option<PathBuf>,
}

#[derive(Clone, Serialize, Deserialize)]
struct ProfileTask {
    candidate_id: String,
    input_netlist: PathBuf,
}

struct NativeStage {
    summary: Value,
    ranking: Vec<String>,
    summaries: HashMap<String, Value>,
}

#[derive(Serialize, Deserialize)]
struct TrackerCheckpoint {
    input_sha256: String,
    exact_evaluations: usize,
    polish_evaluations: usize,
    // Cell indices are local to a batch-loaded CellDatabase and therefore can
    // change when a later progressive stage contains a smaller candidate
    // subset.  Persist names so 25 -> 50 -> 500 resume is independent of the
    // surrounding batch composition.
    cache: Vec<(NamedChoice, Ppa)>,
    best_choice: NamedChoice,
    best: Ppa,
    trajectory: Vec<TrajectoryPoint>,
}

fn ppa(ev: &Eval) -> Ppa {
    Ppa {
        delay: ev.delay,
        area: ev.area,
        power: ev.power,
        score: ev.score,
    }
}

fn eval_from_ppa(value: &Ppa) -> Eval {
    Eval {
        delay: value.delay,
        area: value.area,
        power: value.power,
        score: value.score,
        primary_arrival: Arc::new(Vec::new()),
        primary_transition: Arc::new(Vec::new()),
        arrival: Vec::new(),
        transition: Vec::new(),
        load: Vec::new(),
        cell_area: Vec::new(),
        cell_power: Vec::new(),
        arcs: Vec::new(),
        arcs_by_state: Arc::new(Vec::new()),
        arcs_by_parent: Arc::new(Vec::new()),
    }
}

fn strip_comments(text: &str) -> String {
    Regex::new(r"//[^\n]*")
        .unwrap()
        .replace_all(text, "")
        .into_owned()
}

fn port_list(text: &str, kind: &str) -> Result<Vec<String>> {
    let re = Regex::new(&format!(r"(?s)\b{}\b\s+([^;]+);", kind))?;
    let qualifiers = Regex::new(r"\b(?:wire|reg|logic|signed)\b")?;
    let mut out = Vec::new();
    for cap in re.captures_iter(text) {
        let chunk = qualifiers.replace_all(&cap[1], "");
        out.extend(
            chunk
                .replace('\n', " ")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        );
    }
    Ok(out)
}

fn load_cells(lib_content: &str, names: &BTreeSet<String>) -> Result<(Vec<String>, Vec<NLDM>)> {
    let ordered: Vec<_> = names.iter().cloned().collect();
    let jobs = std::env::var("EGG_NLDM_PARSE_JOBS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(16)
        .max(1);
    // Each Liberty cell is immutable and independent.  Parse cells in
    // parallel, then restore lexical order so every downstream cell id and
    // floating accumulation order remains identical to the historical batch.
    let cells = parallel_map_ordered(&ordered, jobs, |index, name| {
        let mut graph = SerializedEGraph::default();
        graph.add_node(
            NodeId::from(format!("n{index}")),
            Node {
                op: name.clone(),
                children: Vec::new(),
                eclass: ClassId::from(format!("c{index}")),
                cost: Cost::new(1.0).unwrap(),
            },
        );
        let serialized = serde_json::to_value(&graph)?;
        let ext = ExtendedEGraph::from_base_to_extention_with_shared_nldm(
            graph,
            &serialized,
            lib_content,
            &mut HashMap::new(),
        );
        ext.cell_nldm
            .get(name)
            .with_context(|| format!("cannot parse NLDM cell {name}"))
            .cloned()
    })?;
    Ok((ordered, cells))
}

fn scale_families(path: &Path) -> Result<BTreeMap<String, Vec<String>>> {
    let value: Value = serde_json::from_slice(&fs::read(path)?)?;
    let op = Regex::new(r"^\((\S+)")?;
    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in value["rewrites"]
        .as_array()
        .context("rules.rewrites must be array")?
    {
        let a = op
            .captures(row["searcher"].as_str().context("searcher")?)
            .context("bad searcher")?[1]
            .to_owned();
        let b = op
            .captures(row["applier"].as_str().context("applier")?)
            .context("bad applier")?[1]
            .to_owned();
        graph.entry(a.clone()).or_default().insert(b.clone());
        graph.entry(b).or_default().insert(a);
    }
    let mut result = BTreeMap::new();
    for start in graph.keys() {
        let mut seen = BTreeSet::from([start.clone()]);
        let mut stack = vec![start.clone()];
        while let Some(cell) = stack.pop() {
            for next in graph.get(&cell).into_iter().flatten() {
                if seen.insert(next.clone()) {
                    stack.push(next.clone());
                }
            }
        }
        result.insert(start.clone(), seen.into_iter().collect());
    }
    Ok(result)
}

impl CellDatabase {
    pub(crate) fn load(netlists: &[PathBuf], lib_path: &Path, rules: &Path) -> Result<Self> {
        let family_names = scale_families(rules)?;
        let inst_re = Regex::new(r"(?ms)^\s*(\w+_ASAP7_(?:6t_L|75t_R))\s+(\w+)\s*\((.*?)\)\s*;")?;
        let mut all_cells = BTreeSet::new();
        for path in netlists {
            let text = strip_comments(&fs::read_to_string(path)?);
            all_cells.extend(
                inst_re
                    .captures_iter(&text)
                    .map(|capture| capture[1].to_owned()),
            );
        }
        for family in family_names.values() {
            all_cells.extend(family.iter().cloned());
        }
        if TimingBoundary::from_env()?.internal_timing_model.is_v3() {
            all_cells.insert(
                TimingBoundary::from_env()?
                    .primary_input_driver_cell
                    .to_owned(),
            );
        }
        let lib_content = fs::read_to_string(lib_path)?;
        let (cell_names, cells) = load_cells(&lib_content, &all_cells)?;
        let cell_by_name = cell_names
            .iter()
            .enumerate()
            .map(|(index, name)| (name.clone(), index))
            .collect();
        Ok(Self {
            cells: Arc::new(cells),
            cell_names: Arc::new(cell_names),
            cell_by_name,
            family_names,
        })
    }

    pub(crate) fn nldm_cache(&self) -> HashMap<String, NLDM> {
        self.cell_names
            .iter()
            .cloned()
            .zip(self.cells.iter().cloned())
            .collect()
    }

    pub(crate) fn ensure_netlists(
        &mut self,
        netlists: &[PathBuf],
        lib_path: &Path,
    ) -> Result<usize> {
        let inst_re = Regex::new(r"(?ms)^\s*(\w+_ASAP7_(?:6t_L|75t_R))\s+(\w+)\s*\((.*?)\)\s*;")?;
        let mut missing = BTreeSet::new();
        for path in netlists {
            let text = strip_comments(&fs::read_to_string(path)?);
            for capture in inst_re.captures_iter(&text) {
                let name = capture[1].to_owned();
                if !self.cell_by_name.contains_key(&name) {
                    missing.insert(name);
                }
            }
        }
        self.ensure_cell_names(missing, lib_path)
    }

    pub(crate) fn ensure_cell_names(
        &mut self,
        names: impl IntoIterator<Item = String>,
        lib_path: &Path,
    ) -> Result<usize> {
        let missing: BTreeSet<_> = names
            .into_iter()
            .filter(|name| !self.cell_by_name.contains_key(name))
            .collect();
        if missing.is_empty() {
            return Ok(0);
        }
        let lib_content = fs::read_to_string(lib_path)?;
        let (names, cells) = load_cells(&lib_content, &missing)?;
        let database_names = Arc::make_mut(&mut self.cell_names);
        let database_cells = Arc::make_mut(&mut self.cells);
        for (name, cell) in names.into_iter().zip(cells) {
            let index = database_names.len();
            self.cell_by_name.insert(name.clone(), index);
            database_names.push(name);
            database_cells.push(cell);
        }
        Ok(missing.len())
    }
}

impl Circuit {
    #[inline]
    fn external_output_load(&self, source: usize) -> f64 {
        self.fixed_output_load_ff.as_ref().map_or_else(
            || self.output_load_count[source] as f64 * self.boundary.primary_output_load_ff,
            |loads| loads[source],
        )
    }

    #[inline]
    fn primary_external_output_load(&self, primary: usize) -> f64 {
        self.fixed_primary_output_load_ff.as_ref().map_or_else(
            || {
                self.primary_output_load_count[primary] as f64
                    * self.boundary.primary_output_load_ff
            },
            |loads| loads[primary],
        )
    }

    fn parse(netlist: &Path, lib_path: &Path, rules: &Path) -> Result<Self> {
        let database = CellDatabase::load(&[netlist.to_path_buf()], lib_path, rules)?;
        Self::parse_with_database(netlist, &database)
    }

    fn parse_with_database(netlist: &Path, database: &CellDatabase) -> Result<Self> {
        let objective = SearchObjective::from_env()?;
        Self::parse_with_database_objective(netlist, database, objective)
    }

    fn parse_with_database_objective(
        netlist: &Path,
        database: &CellDatabase,
        objective: SearchObjective,
    ) -> Result<Self> {
        Self::parse_with_database_preparation(netlist, database, objective, true)
    }

    // Full evaluation does not consume finite-difference dependency plans.
    // Only profile-batch may omit them; every sizing entry keeps preparation.
    fn parse_with_database_preparation(
        netlist: &Path,
        database: &CellDatabase,
        objective: SearchObjective,
        prepare_delta_plans: bool,
    ) -> Result<Self> {
        let source_text = fs::read_to_string(netlist)?;
        let text = strip_comments(&source_text);
        let inputs: HashSet<_> = port_list(&text, "input")?.into_iter().collect();
        let outputs = port_list(&text, "output")?;
        let assign_re = Regex::new(r"\bassign\s+(\w+)\s*=\s*([^;\s]+)\s*;")?;
        let assigns: HashMap<String, String> = assign_re
            .captures_iter(&text)
            .map(|c| (c[1].to_owned(), c[2].to_owned()))
            .collect();
        let inst_re = Regex::new(r"(?ms)^\s*(\w+_ASAP7_(?:6t_L|75t_R))\s+(\w+)\s*\((.*?)\)\s*;")?;
        let topology_cell_re = Regex::new(r"(?m)^(\s*)\w+_ASAP7_(?:6t_L|75t_R)(\s+\w+\s*\()")?;
        let topology_text = topology_cell_re.replace_all(&source_text, "${1}<CELL>${2}");
        let topology_sha256 = format!("{:x}", Sha256::digest(topology_text.as_bytes()));
        let conn_re = Regex::new(r"\.(\w+)\s*\(\s*([^()]+?)\s*\)")?;
        let mut raw = Vec::new();
        for cap in inst_re.captures_iter(&text) {
            let conns: HashMap<String, String> = conn_re
                .captures_iter(&cap[3])
                .map(|c| (c[1].to_owned(), c[2].trim().to_owned()))
                .collect();
            raw.push((cap[2].to_owned(), cap[1].to_owned(), conns));
        }
        anyhow::ensure!(
            !raw.is_empty(),
            "no mapped instances in {}",
            netlist.display()
        );
        let mut instances = Vec::new();
        let mut driver = HashMap::new();
        for (index, (name, cell_name, conns)) in raw.iter().enumerate() {
            let cell_id = database.cell_by_name[cell_name];
            let cell = &database.cells[cell_id];
            let output_pin = &cell.pin_order[0];
            let output = conns
                .get(output_pin)
                .with_context(|| format!("{name} missing {output_pin}"))?
                .clone();
            let inputs = cell
                .pin_order
                .iter()
                .skip(1)
                .map(|pin| {
                    conns
                        .get(pin)
                        .with_context(|| format!("{name} missing {pin}"))
                        .cloned()
                })
                .collect::<Result<Vec<_>>>()?;
            driver.insert(output.clone(), index);
            instances.push(Inst {
                name: name.clone(),
                original_cell: cell_id,
                inputs,
                input_sources: Vec::new(),
                input_is_primary: Vec::new(),
                input_primary_index: Vec::new(),
            });
        }
        for inst in &mut instances {
            inst.input_sources = inst
                .inputs
                .iter()
                .map(|sig| driver.get(sig).copied())
                .collect();
        }
        fn resolve(assigns: &HashMap<String, String>, mut signal: String) -> String {
            let mut seen = HashSet::new();
            while let Some(next) = assigns.get(&signal) {
                if !seen.insert(signal.clone()) {
                    break;
                }
                signal = next.clone();
            }
            signal
        }
        let input_indices: HashMap<_, _> = inputs
            .iter()
            .enumerate()
            .map(|(index, name)| (name.clone(), index))
            .collect();
        for inst in &mut instances {
            inst.input_primary_index = inst
                .inputs
                .iter()
                .map(|signal| {
                    input_indices
                        .get(&resolve(&assigns, signal.clone()))
                        .copied()
                })
                .collect();
            inst.input_is_primary = inst
                .input_primary_index
                .iter()
                .map(Option::is_some)
                .collect();
        }
        let output_sources = outputs
            .iter()
            .map(|output| {
                let resolved = resolve(&assigns, output.clone());
                driver
                    .get(&resolved)
                    .copied()
                    .map(BoundaryOutputSource::Gate)
                    .or_else(|| {
                        input_indices
                            .get(&resolved)
                            .copied()
                            .map(BoundaryOutputSource::Primary)
                    })
                    .or_else(|| {
                        verilog_scalar_constant(&resolved).map(|_| BoundaryOutputSource::Constant)
                    })
                    .with_context(|| format!("output {output} has no mapped or primary driver"))
            })
            .collect::<Result<Vec<_>>>()?;
        let roots = output_sources
            .iter()
            .filter_map(|source| match source {
                BoundaryOutputSource::Gate(index) => Some(*index),
                BoundaryOutputSource::Primary(_) | BoundaryOutputSource::Constant => None,
            })
            .collect::<Vec<_>>();
        let mut primary_output_load_count = vec![0usize; inputs.len()];
        for output in &outputs {
            if let Some(primary) = input_indices.get(&resolve(&assigns, output.clone())) {
                primary_output_load_count[*primary] += 1;
            }
        }
        let mut output_load_count = vec![0usize; instances.len()];
        for &root in &roots {
            output_load_count[root] += 1;
        }
        let port_boundary = PortBoundaryOverrides::from_env()?;
        let (
            fixed_primary_input,
            fixed_output_boundary,
            fixed_output_load_ff,
            fixed_primary_output_load_ff,
        ) = if let Some(port_boundary) = &port_boundary {
            anyhow::ensure!(
                port_boundary.inputs.len() == input_indices.len(),
                "port boundary has {} inputs but netlist has {}",
                port_boundary.inputs.len(),
                input_indices.len()
            );
            let input_by_name: HashMap<_, _> = port_boundary
                .inputs
                .iter()
                .map(|row| (row.name.as_str(), row))
                .collect();
            anyhow::ensure!(
                input_by_name.len() == port_boundary.inputs.len(),
                "duplicate input in port boundary"
            );
            let mut fixed_inputs = vec![None; input_indices.len()];
            for (name, primary) in &input_indices {
                fixed_inputs[*primary] = Some(
                    input_by_name
                        .get(name.as_str())
                        .with_context(|| format!("input {name} absent from port boundary"))?
                        .to_owned()
                        .clone(),
                );
            }
            let fixed_inputs = fixed_inputs
                .into_iter()
                .enumerate()
                .map(|(index, row)| {
                    row.with_context(|| format!("missing fixed input boundary {index}"))
                })
                .collect::<Result<Vec<_>>>()?;
            let fixed_outputs = port_boundary.ordered_outputs(&outputs)?;
            let mut gate_loads = vec![0.0; instances.len()];
            let mut primary_loads = vec![0.0; input_indices.len()];
            for (output, fixed) in outputs.iter().zip(&fixed_outputs) {
                let resolved = resolve(&assigns, output.clone());
                if let Some(source) = driver.get(&resolved) {
                    gate_loads[*source] += fixed.load_ff;
                } else if let Some(primary) = input_indices.get(&resolved) {
                    primary_loads[*primary] += fixed.load_ff;
                } else if verilog_scalar_constant(&resolved).is_some() {
                    // Constants do not have a timed/powered mapped driver.
                } else {
                    bail!("output {output} has no mapped or primary driver");
                }
            }
            (
                Some(fixed_inputs),
                Some(fixed_outputs),
                Some(gate_loads),
                Some(primary_loads),
            )
        } else {
            (None, None, None, None)
        };
        let mut reachable_set = HashSet::new();
        let mut stack = roots.clone();
        while let Some(i) = stack.pop() {
            if reachable_set.insert(i) {
                stack.extend(instances[i].input_sources.iter().flatten().copied());
            }
        }
        let mut indegree = vec![0usize; instances.len()];
        let mut fanout = vec![Vec::new(); instances.len()];
        let mut consumers = vec![Vec::new(); instances.len()];
        for &sink in &reachable_set {
            for (pin, source) in instances[sink].input_sources.iter().enumerate() {
                if let Some(source) = source.filter(|s| reachable_set.contains(s)) {
                    indegree[sink] += 1;
                    fanout[source].push(sink);
                    consumers[source].push((sink, pin));
                }
            }
        }
        let mut ready: Vec<_> = reachable_set
            .iter()
            .copied()
            .filter(|i| indegree[*i] == 0)
            .collect();
        ready.sort_by_key(|i| std::cmp::Reverse(instances[*i].name.clone()));
        let mut topo = Vec::new();
        while let Some(i) = ready.pop() {
            topo.push(i);
            fanout[i].sort_by_key(|j| instances[*j].name.clone());
            for &sink in &fanout[i] {
                indegree[sink] -= 1;
                if indegree[sink] == 0 {
                    ready.push(sink);
                    ready.sort_by_key(|j| std::cmp::Reverse(instances[*j].name.clone()));
                }
            }
        }
        anyhow::ensure!(topo.len() == reachable_set.len(), "cycle in mapped netlist");
        // Preserve mapped-Verilog occurrence order.  In the frozen Python
        // implementation O1 scans `net.instances` and may hit the exact
        // budget part-way through a scan; sorting occurrences by name changes
        // which legal move is the last evaluated candidate.  Topological
        // evaluation still uses `topo` above.
        let reachable: Vec<_> = (0..instances.len())
            .filter(|i| reachable_set.contains(i))
            .collect();
        let mut primary_consumers = vec![Vec::new(); inputs.len()];
        for &sink in &reachable {
            for (pin, primary) in instances[sink].input_primary_index.iter().enumerate() {
                if let Some(primary) = primary {
                    primary_consumers[*primary].push((sink, pin));
                }
            }
        }
        // A finite-difference probe only changes one occurrence's cell
        // distribution.  Its timing/load dependency cone is structural and
        // identical for every cell alternative and every A2 step, so compute
        // it once instead of rebuilding it for tens of thousands of probes.
        let probability_delta_plans = (0..if prepare_delta_plans {
            instances.len()
        } else {
            0
        })
            .map(|changed_inst| {
                let mut affected = vec![false; instances.len()];
                let mut pending = Vec::new();
                let mark = |index: usize, affected: &mut [bool], pending: &mut Vec<usize>| {
                    if !affected[index] {
                        affected[index] = true;
                        pending.push(index);
                    }
                };
                mark(changed_inst, &mut affected, &mut pending);
                let changed_sources = instances[changed_inst]
                    .input_sources
                    .iter()
                    .flatten()
                    .filter(|source| reachable_set.contains(source))
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                for &source in &changed_sources {
                    mark(source, &mut affected, &mut pending);
                }
                let changed_primaries = instances[changed_inst]
                    .input_primary_index
                    .iter()
                    .flatten()
                    .copied()
                    .collect::<BTreeSet<_>>();
                let changed_primary_consumers = changed_primaries
                    .iter()
                    .flat_map(|primary| {
                        primary_consumers[*primary]
                            .iter()
                            .map(|(consumer, _)| *consumer)
                    })
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                for primary in &changed_primaries {
                    for &(consumer, _) in &primary_consumers[*primary] {
                        mark(consumer, &mut affected, &mut pending);
                    }
                }
                while let Some(source) = pending.pop() {
                    for &(consumer, _) in &consumers[source] {
                        mark(consumer, &mut affected, &mut pending);
                    }
                }
                ProbabilityDeltaPlan {
                    affected_topo: topo
                        .iter()
                        .copied()
                        .filter(|index| affected[*index])
                        .collect(),
                    changed_sources,
                    changed_primary_consumers,
                    changed_primaries: changed_primaries.iter().copied().collect(),
                    touches_primary: !changed_primaries.is_empty(),
                }
            })
            .collect();
        let families: Vec<Vec<usize>> = instances
            .iter()
            .map(|inst| {
                let original = &database.cell_names[inst.original_cell];
                database
                    .family_names
                    .get(original)
                    .map(|family| {
                        family
                            .iter()
                            .filter_map(|c| database.cell_by_name.get(c).copied())
                            .collect()
                    })
                    .unwrap_or_else(|| vec![inst.original_cell])
            })
            .collect();
        let internal_power_luts = database
            .cells
            .iter()
            .map(|cell| {
                cell.pin_order
                    .iter()
                    .skip(1)
                    .map(|pin| {
                        Lut2D::from_indexed_rows(&cell.internal_power[pin])
                            .map_err(anyhow::Error::msg)
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        let iterative_power_luts: Vec<Vec<IterativeLut2D>> = internal_power_luts
            .iter()
            .map(|cell| cell.iter().map(IterativeLut2D::from_lut).collect())
            .collect();
        let iterative_timing_luts: Vec<Vec<IterativeTimingArc>> = database
            .cells
            .iter()
            .map(|cell| {
                cell.timing_arcs
                    .iter()
                    .map(|arc| IterativeTimingArc {
                        rise_delay: IterativeLut2D::from_lut(&arc.cell_rise),
                        fall_delay: IterativeLut2D::from_lut(&arc.cell_fall),
                        rise_transition: IterativeLut2D::from_lut(&arc.rise_transition),
                        fall_transition: IterativeLut2D::from_lut(&arc.fall_transition),
                    })
                    .collect()
            })
            .collect();
        let iterative_power_shared_grids = families
            .iter()
            .map(|family| {
                let reference = &iterative_power_luts[family[0]];
                (0..reference.len())
                    .map(|pin| {
                        family.iter().all(|cell| {
                            iterative_power_luts[*cell]
                                .get(pin)
                                .is_some_and(|lut| lut.same_grid(&reference[pin]))
                        })
                    })
                    .collect()
            })
            .collect();
        let iterative_timing_shared_grids = families
            .iter()
            .map(|family| {
                let reference = &iterative_timing_luts[family[0]];
                (0..reference.len())
                    .map(|arc| {
                        let reference = &reference[arc];
                        family.iter().fold([[true; 2]; 2], |mut shared, cell| {
                            let Some(candidate) = iterative_timing_luts[*cell].get(arc) else {
                                return [[false; 2]; 2];
                            };
                            shared[0][0] &= candidate.rise_delay.same_grid(&reference.rise_delay);
                            shared[0][1] &= candidate
                                .rise_transition
                                .same_grid(&reference.rise_transition);
                            shared[1][0] &= candidate.fall_delay.same_grid(&reference.fall_delay);
                            shared[1][1] &= candidate
                                .fall_transition
                                .same_grid(&reference.fall_transition);
                            shared
                        })
                    })
                    .collect()
            })
            .collect();
        let input_caps = database
            .cells
            .iter()
            .map(|cell| {
                cell.pin_order
                    .iter()
                    .skip(1)
                    .map(|pin| cell.pin_info[pin].0.into_inner())
                    .collect()
            })
            .collect();
        let timing_arc_pins = database
            .cells
            .iter()
            .map(|cell| {
                cell.timing_arcs
                    .iter()
                    .map(|arc| {
                        cell.pin_order
                            .iter()
                            .skip(1)
                            .position(|pin| pin == &arc.related_pin)
                            .context("arc pin absent")
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        let boundary = TimingBoundary::from_env()?;
        let port_objective = PortObjective::from_env()?;
        if port_objective.is_some() {
            anyhow::ensure!(
                fixed_output_boundary.is_some(),
                "EGG_PORT_OBJECTIVE_JSON requires EGG_PORT_BOUNDARY_JSON"
            );
            let outputs = fixed_output_boundary.as_ref().unwrap();
            anyhow::ensure!(
                outputs.iter().all(|output| {
                    output.required_rise_ps.is_some() && output.required_fall_ps.is_some()
                }),
                "port objective requires rise/fall required times for every output"
            );
            let needs_arrival_baseline = port_objective.as_ref().is_some_and(|objective| {
                objective.max_arrival_regression_ps.is_some()
                    || objective.path_family_arrival_penalty_scale.is_some()
            }) || outputs
                .iter()
                .any(|output| output.max_arrival_regression_ps.is_some());
            if needs_arrival_baseline {
                anyhow::ensure!(
                    outputs.iter().all(|output| {
                        output.baseline_arrival_rise_ps.is_some()
                            && output.baseline_arrival_fall_ps.is_some()
                    }),
                    "max_arrival_regression_ps requires rise/fall baseline arrivals for every output"
                );
            }
        }
        let exact_context_sha256 = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&json!({
                "topology_sha256": topology_sha256,
                "objective": objective.name(),
                "delay_cap_ps": objective.delay_cap_ps(),
                "boundary": boundary,
                "port_boundary": port_boundary,
                "port_objective": port_objective,
            }))?)
        );
        Ok(Self {
            source_path: netlist.to_path_buf(),
            source_text,
            instances,
            roots,
            output_names: outputs,
            output_sources,
            reachable,
            reachable_set,
            topo,
            consumers,
            probability_delta_plans,
            output_load_count,
            primary_output_load_count,
            fixed_primary_input,
            fixed_output_boundary,
            fixed_output_load_ff,
            fixed_primary_output_load_ff,
            primary_consumers,
            cells: database.cells.clone(),
            cell_names: database.cell_names.clone(),
            iterative_power_luts,
            iterative_timing_luts,
            iterative_power_shared_grids,
            iterative_timing_shared_grids,
            input_caps,
            timing_arc_pins,
            families,
            objective,
            port_objective,
            boundary,
            iterative: std::env::var("EGG_ITERATIVE")
                .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
                .unwrap_or(false),
            topology_sha256,
            exact_context_sha256,
        })
    }

    fn direct_primary_output_switching_power(&self) -> f64 {
        if !self.boundary.vectorless_power || !self.boundary.uniform_comb_activity {
            return 0.0;
        }
        (0..self.primary_output_load_count.len())
            .map(|primary| self.primary_external_output_load(primary))
            .sum::<f64>()
            * self.boundary.primary_input_toggle_per_cycle
            * self.boundary.power_voltage_v.powi(2)
            * 1000.0
            / self.boundary.power_period_ps
    }

    fn boundary_output_states(&self, evaluated: &Eval) -> Vec<Value> {
        self.output_names
            .iter()
            .zip(&self.output_sources)
            .enumerate()
            .map(|(ordinal, (name, source))| {
                let (arrival, transition, load) = match source {
                    BoundaryOutputSource::Gate(root) => (
                        evaluated.arrival[*root],
                        evaluated.transition[*root],
                        self.external_output_load(*root),
                    ),
                    BoundaryOutputSource::Primary(primary) => (
                        evaluated.primary_arrival[*primary],
                        evaluated.primary_transition[*primary],
                        self.primary_external_output_load(*primary),
                    ),
                    BoundaryOutputSource::Constant => (
                        [0.0; 2],
                        [0.0; 2],
                        self.fixed_output_boundary
                            .as_ref()
                            .map_or(self.boundary.primary_output_load_ff, |outputs| {
                                outputs[ordinal].load_ff
                            }),
                    ),
                };
                json!({
                    "name":name,
                    "arrival_rise_ps":arrival[0],
                    "arrival_fall_ps":arrival[1],
                    "slew_rise_ps":transition[0],
                    "slew_fall_ps":transition[1],
                    "external_load_ff":load,
                })
            })
            .collect()
    }

    // Opt-in diagnostic only: names, not HashSet-derived PI indices, identify ports.
    fn profile_boundary_response(&self, evaluated: &Eval) -> Result<Value> {
        let text = strip_comments(&self.source_text);
        let assigns: HashMap<String, String> = Regex::new(r"\bassign\s+(\w+)\s*=\s*([^;\s]+)\s*;")?
            .captures_iter(&text)
            .map(|c| (c[1].to_string(), c[2].to_string()))
            .collect();
        let resolve = |name: &str| -> Result<String> {
            let mut name = name.to_string();
            let mut seen = HashSet::new();
            while let Some(next) = assigns.get(&name) {
                anyhow::ensure!(
                    seen.insert(name.clone()),
                    "cyclic assign in boundary export"
                );
                name = next.clone();
            }
            Ok(name)
        };
        let mut caps: BTreeMap<String, f64> = port_list(&text, "input")?
            .into_iter()
            .map(|name| (name, 0.0))
            .collect();
        let choice = self.original_choice();
        for &sink in &self.reachable {
            for (pin, primary) in self.instances[sink].input_primary_index.iter().enumerate() {
                if primary.is_some() {
                    let name = resolve(&self.instances[sink].inputs[pin])?;
                    *caps
                        .get_mut(&name)
                        .context("unresolved primary input name")? +=
                        self.weighted_cap(&choice, None, sink, pin);
                }
            }
        }
        let mut seen = HashSet::new();
        for (name, source) in self.output_names.iter().zip(&self.output_sources) {
            if let BoundaryOutputSource::Primary(primary) = source {
                if seen.insert(*primary) {
                    *caps
                        .get_mut(&resolve(name)?)
                        .context("unresolved direct output PI")? +=
                        self.primary_external_output_load(*primary);
                }
            }
        }
        Ok(json!({
            "schema":"whole-port-boundary-response-v1",
            "outputs":self.boundary_output_states(evaluated),
            "inputs":caps.into_iter().map(|(name, cap)| json!({"name":name,"capacitance_ff":cap})).collect::<Vec<_>>(),
            "scope":"whole-network ports; not internal region-cut vectors"
        }))
    }

    fn output_delay(&self, arrival: &[[f64; 2]], primary_arrival: &[[f64; 2]]) -> f64 {
        self.output_sources
            .iter()
            .flat_map(|source| match source {
                BoundaryOutputSource::Gate(index) => arrival[*index],
                BoundaryOutputSource::Primary(index) => primary_arrival[*index],
                BoundaryOutputSource::Constant => [0.0; 2],
            })
            .fold(0.0, f64::max)
    }

    fn port_objective_score(
        &self,
        choice: &Choice,
        probs: Option<&[Option<Distribution>]>,
        fallback_delay: f64,
        area: f64,
        power: f64,
        arrival: &[[f64; 2]],
        transition: &[[f64; 2]],
        primary_arrival: &[[f64; 2]],
        primary_transition: &[[f64; 2]],
    ) -> f64 {
        let Some(objective) = &self.port_objective else {
            return self.objective.score(fallback_delay, area, power);
        };
        let outputs = self
            .fixed_output_boundary
            .as_ref()
            .expect("validated fixed output boundary");
        let mut max_lateness = f64::NEG_INFINITY;
        let mut max_weighted_positive_arrival_regression = 0.0_f64;
        let mut max_weighted_arrival_violation = 0.0_f64;
        let mut max_weighted_slew_violation = 0.0_f64;
        let mut max_weighted_lateness_violation = 0.0_f64;
        for (source, output) in self.output_sources.iter().zip(outputs) {
            let (port_arrival, port_transition) = match source {
                BoundaryOutputSource::Gate(index) => (arrival[*index], transition[*index]),
                BoundaryOutputSource::Primary(index) => {
                    (primary_arrival[*index], primary_transition[*index])
                }
                BoundaryOutputSource::Constant => ([0.0; 2], [0.0; 2]),
            };
            let output_lateness = (port_arrival[0]
                - output.required_rise_ps.expect("validated required rise"))
            .max(port_arrival[1] - output.required_fall_ps.expect("validated required fall"));
            max_lateness = max_lateness.max(output_lateness);
            let path_weight = output.path_family_weight.unwrap_or(1.0);
            if let Some(limit) = objective.hard_lateness_ps {
                max_weighted_lateness_violation = max_weighted_lateness_violation
                    .max(path_weight * (output_lateness - limit).max(0.0));
            }
            let mut output_arrival_regression = f64::NEG_INFINITY;
            if let Some(baseline) = output.baseline_arrival_rise_ps {
                output_arrival_regression =
                    output_arrival_regression.max(port_arrival[0] - baseline);
            }
            if let Some(baseline) = output.baseline_arrival_fall_ps {
                output_arrival_regression =
                    output_arrival_regression.max(port_arrival[1] - baseline);
            }
            if output_arrival_regression.is_finite() {
                max_weighted_positive_arrival_regression = max_weighted_positive_arrival_regression
                    .max(path_weight * output_arrival_regression.max(0.0));
                if let Some(limit) = output
                    .max_arrival_regression_ps
                    .or(objective.max_arrival_regression_ps)
                {
                    max_weighted_arrival_violation = max_weighted_arrival_violation
                        .max(path_weight * (output_arrival_regression - limit).max(0.0));
                }
            }
            let mut output_slew_regression = f64::NEG_INFINITY;
            if let Some(baseline) = output.baseline_slew_rise_ps {
                output_slew_regression = output_slew_regression.max(port_transition[0] - baseline);
            }
            if let Some(baseline) = output.baseline_slew_fall_ps {
                output_slew_regression = output_slew_regression.max(port_transition[1] - baseline);
            }
            if output_slew_regression.is_finite() {
                if let Some(limit) = output
                    .max_slew_regression_ps
                    .or(objective.max_slew_regression_ps)
                {
                    max_weighted_slew_violation = max_weighted_slew_violation
                        .max(path_weight * (output_slew_regression - limit).max(0.0));
                }
            }
        }
        let max_lateness = if max_lateness.is_finite() {
            max_lateness
        } else {
            0.0
        };
        let mut max_input_cap_violation = f64::NEG_INFINITY;
        if let Some(inputs) = &self.fixed_primary_input {
            for (primary, input) in inputs.iter().enumerate() {
                let Some(baseline_cap) = input.baseline_capacitance_ff else {
                    continue;
                };
                let cap = self.primary_consumers[primary]
                    .iter()
                    .map(|(sink, pin)| self.weighted_cap(choice, probs, *sink, *pin))
                    .sum::<f64>()
                    + self.primary_external_output_load(primary);
                if let Some(limit) = input
                    .max_cap_regression_ff
                    .or(objective.max_input_cap_regression_ff)
                {
                    max_input_cap_violation =
                        max_input_cap_violation.max(cap - baseline_cap - limit);
                }
            }
        }
        let input_cap_violation = if max_input_cap_violation.is_finite() {
            max_input_cap_violation.max(0.0)
        } else {
            0.0
        };
        let lateness_violation = max_weighted_lateness_violation;
        let arrival_violation = max_weighted_arrival_violation;
        let slew_violation = max_weighted_slew_violation;
        if lateness_violation > 0.0
            || arrival_violation > 0.0
            || slew_violation > 0.0
            || input_cap_violation > 0.0
        {
            // Feasible-first without smuggling a benchmark-dependent physical
            // weight into the product objective.  The normalized violation is
            // only used to rank infeasible repair steps toward the boundary.
            return 1_000.0
                + lateness_violation / objective.global_delay_ps
                + arrival_violation / objective.global_delay_ps
                + slew_violation / objective.global_delay_ps
                + input_cap_violation / 10.0;
        }
        let priced_lateness = if objective.credit_negative_lateness {
            max_lateness
        } else {
            max_lateness.max(0.0)
        };
        let path_family_penalty_ps = objective.path_family_arrival_penalty_scale.unwrap_or(0.0)
            * max_weighted_positive_arrival_regression;
        let predicted_delay =
            (objective.global_delay_ps + priced_lateness + path_family_penalty_ps)
                .max(f64::EPSILON);
        let predicted_area =
            (objective.global_area + area - objective.local_baseline_area).max(f64::EPSILON);
        let predicted_power =
            (objective.global_power + power - objective.local_baseline_power).max(f64::EPSILON);
        (predicted_delay / objective.global_delay_ps).powf(objective.delay_exponent)
            * (predicted_area / objective.global_area).powf(objective.area_exponent)
            * (predicted_power / objective.global_power).powf(objective.power_exponent)
    }

    fn original_choice(&self) -> Choice {
        Choice(self.instances.iter().map(|i| i.original_cell).collect())
    }

    fn named_choice(&self, choice: &Choice) -> NamedChoice {
        NamedChoice(
            choice
                .0
                .iter()
                .map(|index| self.cell_names[*index].clone())
                .collect(),
        )
    }

    fn distributions(&self, center: &Choice, epsilon: f64) -> Vec<Option<Distribution>> {
        self.distributions_for_active(center, epsilon, None)
    }

    fn distributions_for_active(
        &self,
        center: &Choice,
        epsilon: f64,
        active: Option<&BTreeSet<usize>>,
    ) -> Vec<Option<Distribution>> {
        self.instances
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let cells = self.families[i].clone();
                if cells.len() <= 1
                    || !self.reachable_set.contains(&i)
                    || active.is_some_and(|selected| !selected.contains(&i))
                {
                    return None;
                }
                let other = epsilon / (cells.len() - 1) as f64;
                let probabilities = cells
                    .iter()
                    .map(|cell| {
                        if *cell == center.0[i] {
                            1.0 - epsilon
                        } else {
                            other
                        }
                    })
                    .collect();
                Some(Distribution {
                    cells: Arc::new(cells),
                    probabilities: Arc::new(probabilities),
                })
            })
            .collect()
    }

    fn each_cell<'a>(
        &'a self,
        choice: &'a Choice,
        probs: Option<&'a [Option<Distribution>]>,
        inst: usize,
    ) -> impl Iterator<Item = (f64, usize, &'a NLDM)> + 'a {
        let distribution = probs.and_then(|p| p[inst].as_ref());
        let ids: &'a [usize] = distribution
            .map(|d| d.cells.as_slice())
            .unwrap_or_else(|| std::slice::from_ref(&choice.0[inst]));
        ids.iter()
            .copied()
            .enumerate()
            .filter_map(move |(position, cell)| {
                // `ids` and `probabilities` have the same frozen family order.
                // Pair them by position instead of linearly searching the family
                // again for every LUT/capacitance/power contribution.
                let weight = distribution.map_or_else(
                    || usize::from(choice.0[inst] == cell) as f64,
                    |value| value.probabilities[position],
                );
                (weight > 1e-15).then_some((weight, cell, &self.cells[cell]))
            })
    }

    fn weighted_delay_transition(
        &self,
        choice: &Choice,
        probs: Option<&[Option<Distribution>]>,
        inst: usize,
        arc: usize,
        output: usize,
        slew: f64,
        load: f64,
    ) -> Result<(f64, f64)> {
        let mut delay = 0.0;
        let mut transition = 0.0;
        let (shared_delay_coordinates, shared_transition_coordinates) =
            if self.iterative && iterative_shared_grid_enabled() {
                let reference = &self.iterative_timing_luts[self.families[inst][0]][arc];
                let (delay_lut, transition_lut) = match output {
                    0 => (&reference.rise_delay, &reference.rise_transition),
                    _ => (&reference.fall_delay, &reference.fall_transition),
                };
                let shared = self.iterative_timing_shared_grids[inst][arc][output];
                (
                    shared[0].then(|| delay_lut.coordinates(slew, load)),
                    shared[1].then(|| transition_lut.coordinates(slew, load)),
                )
            } else {
                (None, None)
            };
        for (weight, cell_index, cell) in self.each_cell(choice, probs, inst) {
            if self.iterative {
                let fast = &self.iterative_timing_luts[cell_index][arc];
                let (delay_lut, transition_lut) = match output {
                    0 => (&fast.rise_delay, &fast.rise_transition),
                    _ => (&fast.fall_delay, &fast.fall_transition),
                };
                delay += weight
                    * shared_delay_coordinates.map_or_else(
                        || delay_lut.lookup(slew, load),
                        |coordinates| delay_lut.lookup_at(coordinates),
                    );
                transition += weight
                    * shared_transition_coordinates.map_or_else(
                        || transition_lut.lookup(slew, load),
                        |coordinates| transition_lut.lookup_at(coordinates),
                    );
                continue;
            }
            let a = &cell.timing_arcs[arc];
            let (delay_lut, transition_lut) = match output {
                0 => (&a.cell_rise, &a.rise_transition),
                _ => (&a.cell_fall, &a.fall_transition),
            };
            let delay_value = delay_lut.lookup_f64(slew, load);
            let transition_value = transition_lut.lookup_f64(slew, load);
            delay += weight * delay_value.map_err(anyhow::Error::msg)?;
            transition += weight * transition_value.map_err(anyhow::Error::msg)?;
        }
        Ok((delay, transition))
    }

    fn weighted_cap(
        &self,
        choice: &Choice,
        probs: Option<&[Option<Distribution>]>,
        inst: usize,
        pin: usize,
    ) -> f64 {
        self.each_cell(choice, probs, inst)
            .map(|(w, cell_index, _)| w * self.input_caps[cell_index][pin])
            .sum()
    }

    fn primary_input_timing(
        &self,
        choice: &Choice,
        probs: Option<&[Option<Distribution>]>,
    ) -> Result<(Vec<[f64; 2]>, Vec<[f64; 2]>)> {
        let count = self.primary_output_load_count.len();
        if let Some(rows) = &self.fixed_primary_input {
            return Ok((
                rows.iter()
                    .map(|row| [row.arrival_rise_ps, row.arrival_fall_ps])
                    .collect(),
                rows.iter()
                    .map(|row| [row.slew_rise_ps, row.slew_fall_ps])
                    .collect(),
            ));
        }
        if !self.boundary.internal_timing_model.is_v3() {
            return Ok((
                vec![[self.boundary.primary_input_arrival_ps; 2]; count],
                vec![[self.boundary.primary_input_slew_ps; 2]; count],
            ));
        }
        let mut load = self
            .primary_output_load_count
            .iter()
            .enumerate()
            .map(|(primary, _)| self.primary_external_output_load(primary))
            .collect::<Vec<_>>();
        for &sink in &self.reachable {
            for (pin, primary) in self.instances[sink].input_primary_index.iter().enumerate() {
                if let Some(primary) = primary {
                    load[*primary] += self.weighted_cap(choice, probs, sink, pin);
                }
            }
        }
        let driver_index = self
            .cell_names
            .iter()
            .position(|name| name == self.boundary.primary_input_driver_cell)
            .with_context(|| {
                format!(
                    "missing primary-input driver cell {}",
                    self.boundary.primary_input_driver_cell
                )
            })?;
        let driver = &self.cells[driver_index];
        let mut arrival = Vec::with_capacity(count);
        let mut transition = Vec::with_capacity(count);
        for load in load {
            let timing = driver
                .primary_input_driver_timing(self.boundary.driver_input_slew_ps, load)
                .map_err(anyhow::Error::msg)?;
            arrival.push([
                self.boundary.primary_input_arrival_ps + timing.arrival_adjust_ps[0],
                self.boundary.primary_input_arrival_ps + timing.arrival_adjust_ps[1],
            ]);
            transition.push(timing.output_slew_ps);
        }
        Ok((arrival, transition))
    }

    fn primary_input_timing_delta(
        &self,
        choice: &Choice,
        probs: &[Option<Distribution>],
        base: &Eval,
        changed_primaries: &[usize],
    ) -> Result<(Vec<[f64; 2]>, Vec<[f64; 2]>)> {
        if self.fixed_primary_input.is_some() {
            return Ok((
                base.primary_arrival.as_ref().clone(),
                base.primary_transition.as_ref().clone(),
            ));
        }
        if !self.boundary.internal_timing_model.is_v3()
            || base.primary_arrival.len() != self.primary_output_load_count.len()
            || base.primary_transition.len() != self.primary_output_load_count.len()
        {
            return self.primary_input_timing(choice, Some(probs));
        }
        let driver_index = self
            .cell_names
            .iter()
            .position(|name| name == self.boundary.primary_input_driver_cell)
            .with_context(|| {
                format!(
                    "missing primary-input driver cell {}",
                    self.boundary.primary_input_driver_cell
                )
            })?;
        let driver = &self.cells[driver_index];
        let mut arrival = base.primary_arrival.as_ref().clone();
        let mut transition = base.primary_transition.as_ref().clone();
        for &primary in changed_primaries {
            let load = self.primary_external_output_load(primary)
                + self.primary_consumers[primary]
                    .iter()
                    .map(|(sink, pin)| self.weighted_cap(choice, Some(probs), *sink, *pin))
                    .sum::<f64>();
            let timing = driver
                .primary_input_driver_timing(self.boundary.driver_input_slew_ps, load)
                .map_err(anyhow::Error::msg)?;
            arrival[primary] = [
                self.boundary.primary_input_arrival_ps + timing.arrival_adjust_ps[0],
                self.boundary.primary_input_arrival_ps + timing.arrival_adjust_ps[1],
            ];
            transition[primary] = timing.output_slew_ps;
        }
        Ok((arrival, transition))
    }

    fn weighted_power(
        &self,
        choice: &Choice,
        probs: Option<&[Option<Distribution>]>,
        inst: usize,
        slews: &[f64],
        load: f64,
    ) -> Result<f64> {
        if self.boundary.vectorless_power {
            anyhow::ensure!(
                self.boundary.uniform_comb_activity,
                "A2 vectorless power currently requires EGG_POWER_ACTIVITY_MODEL=uniform"
            );
        }
        const MAX_ITERATIVE_INPUT_PINS: usize = 16;
        let mut shared_power_coordinates = [None; MAX_ITERATIVE_INPUT_PINS];
        if self.iterative && iterative_shared_grid_enabled() {
            anyhow::ensure!(
                slews.len() <= MAX_ITERATIVE_INPUT_PINS,
                "Iterative power coordinate cache needs at most {MAX_ITERATIVE_INPUT_PINS} input pins"
            );
            let reference = &self.iterative_power_luts[self.families[inst][0]];
            for (pin, slew) in slews.iter().enumerate() {
                shared_power_coordinates[pin] = self.iterative_power_shared_grids[inst][pin]
                    .then(|| reference[pin].coordinates(*slew, load));
            }
        }
        self.each_cell(choice, probs, inst)
            .map(|(w, cell_index, cell)| {
                if self.boundary.vectorless_power {
                    let mut internal_energy = 0.0;
                    let mut primary_input_capacitance = 0.0;
                    for (index, (_pin, slew)) in
                        cell.pin_order.iter().skip(1).zip(slews).enumerate()
                    {
                        let value = if self.iterative {
                            let lut = &self.iterative_power_luts[cell_index][index];
                            shared_power_coordinates[index].map_or_else(
                                || lut.lookup(*slew, load),
                                |coordinates| lut.lookup_at(coordinates),
                            )
                        } else {
                            Lut2D::from_indexed_rows(&cell.internal_power[_pin])
                                .map_err(anyhow::Error::msg)?
                                .lookup_f64(*slew, load)
                                .map_err(anyhow::Error::msg)?
                        };
                        internal_energy += value * self.boundary.primary_input_toggle_per_cycle;
                        if self.instances[inst].input_is_primary[index] {
                            primary_input_capacitance += self.input_caps[cell_index][index];
                        }
                    }
                    let activity = self.boundary.primary_input_toggle_per_cycle;
                    let frequency = 1000.0 / self.boundary.power_period_ps;
                    let switching = activity
                        * (load + primary_input_capacitance)
                        * self.boundary.power_voltage_v.powi(2)
                        * frequency;
                    let internal =
                        internal_energy * self.boundary.internal_transition_factor * frequency;
                    let leakage = cell.leakage_power.into_inner() / 1_000_000.0;
                    return Ok(w * (internal + switching + leakage));
                }
                if slews.is_empty() {
                    return Ok(w * cell.leakage_power.into_inner() / 1000.0);
                }
                let mut value = 0.0;
                for (pin, slew) in cell.pin_order.iter().skip(1).zip(slews) {
                    let rows = &cell.internal_power[pin];
                    value += Lut2D::from_indexed_rows(rows)
                        .map_err(anyhow::Error::msg)?
                        .lookup_f64(*slew, load)
                        .map_err(anyhow::Error::msg)?;
                }
                Ok(w * (value / slews.len() as f64 + cell.leakage_power.into_inner() / 1000.0))
            })
            .sum()
    }

    /// Evaluate one timing arc for several probability alternatives without
    /// repeating the cell-family metadata walk. The active lane order is
    /// deterministic, and each lane accumulates cells in the same family
    /// order as `weighted_delay_transition`.
    fn weighted_delay_transition_lanes_into(
        &self,
        choice: &Choice,
        lane_probabilities: &[Vec<Option<Distribution>>],
        active_lanes: &[usize],
        inst: usize,
        arc: usize,
        output: usize,
        slews: &[f64],
        loads: &[f64],
        delays: &mut [f64],
        transitions: &mut [f64],
    ) -> Result<()> {
        for &lane in active_lanes {
            delays[lane] = 0.0;
            transitions[lane] = 0.0;
        }
        // Keep the experimental shared-grid path isolated. It is disabled by
        // default and already has its own scalar implementation.
        if iterative_shared_grid_enabled() {
            for &lane in active_lanes {
                (delays[lane], transitions[lane]) = self.weighted_delay_transition(
                    choice,
                    Some(&lane_probabilities[lane]),
                    inst,
                    arc,
                    output,
                    slews[lane],
                    loads[lane],
                )?;
            }
            return Ok(());
        }
        let first_lane = active_lanes[0];
        let first_distribution = lane_probabilities[first_lane][inst].as_ref();
        let cells: &[usize] = first_distribution
            .map(|distribution| distribution.cells.as_slice())
            .unwrap_or_else(|| std::slice::from_ref(&choice.0[inst]));
        for (position, &cell_index) in cells.iter().enumerate() {
            let timing = &self.iterative_timing_luts[cell_index][arc];
            let (delay_lut, transition_lut) = match output {
                0 => (&timing.rise_delay, &timing.rise_transition),
                _ => (&timing.fall_delay, &timing.fall_transition),
            };
            for &lane in active_lanes {
                let distribution = lane_probabilities[lane][inst].as_ref();
                debug_assert_eq!(
                    distribution.map(|value| value.cells.as_slice()),
                    first_distribution.map(|value| value.cells.as_slice())
                );
                let weight =
                    distribution.map_or_else(|| 1.0, |value| value.probabilities[position]);
                if weight <= 1e-15 {
                    continue;
                }
                delays[lane] += weight * delay_lut.lookup(slews[lane], loads[lane]);
                transitions[lane] += weight * transition_lut.lookup(slews[lane], loads[lane]);
            }
        }
        Ok(())
    }

    /// Batch vectorless power across lanes while retaining, for every lane,
    /// the original cell-family and input-pin accumulation order.
    fn weighted_power_lanes_into(
        &self,
        choice: &Choice,
        lane_probabilities: &[Vec<Option<Distribution>>],
        active_lanes: &[usize],
        inst: usize,
        lane_slews: &[f64],
        input_count: usize,
        loads: &[f64],
        powers: &mut [f64],
    ) -> Result<()> {
        for &lane in active_lanes {
            powers[lane] = 0.0;
        }
        if !self.boundary.vectorless_power || iterative_shared_grid_enabled() {
            for &lane in active_lanes {
                let start = lane * input_count;
                powers[lane] = self.weighted_power(
                    choice,
                    Some(&lane_probabilities[lane]),
                    inst,
                    &lane_slews[start..start + input_count],
                    loads[lane],
                )?;
            }
            return Ok(());
        }
        anyhow::ensure!(
            self.boundary.uniform_comb_activity,
            "A2 vectorless power currently requires EGG_POWER_ACTIVITY_MODEL=uniform"
        );
        let first_lane = active_lanes[0];
        let first_distribution = lane_probabilities[first_lane][inst].as_ref();
        let cells: &[usize] = first_distribution
            .map(|distribution| distribution.cells.as_slice())
            .unwrap_or_else(|| std::slice::from_ref(&choice.0[inst]));
        let activity = self.boundary.primary_input_toggle_per_cycle;
        let frequency = 1000.0 / self.boundary.power_period_ps;
        let voltage_squared = self.boundary.power_voltage_v.powi(2);
        for (position, &cell_index) in cells.iter().enumerate() {
            let cell = &self.cells[cell_index];
            for &lane in active_lanes {
                let distribution = lane_probabilities[lane][inst].as_ref();
                debug_assert_eq!(
                    distribution.map(|value| value.cells.as_slice()),
                    first_distribution.map(|value| value.cells.as_slice())
                );
                let weight =
                    distribution.map_or_else(|| 1.0, |value| value.probabilities[position]);
                if weight <= 1e-15 {
                    continue;
                }
                let mut internal_energy = 0.0;
                let mut primary_input_capacitance = 0.0;
                let start = lane * input_count;
                for pin in 0..input_count {
                    internal_energy += self.iterative_power_luts[cell_index][pin]
                        .lookup(lane_slews[start + pin], loads[lane])
                        * activity;
                    if self.instances[inst].input_is_primary[pin] {
                        primary_input_capacitance += self.input_caps[cell_index][pin];
                    }
                }
                let switching = activity
                    * (loads[lane] + primary_input_capacitance)
                    * voltage_squared
                    * frequency;
                let internal =
                    internal_energy * self.boundary.internal_transition_factor * frequency;
                let leakage = cell.leakage_power.into_inner() / 1_000_000.0;
                powers[lane] += weight * (internal + switching + leakage);
            }
        }
        Ok(())
    }

    fn evaluate(&self, choice: &Choice, probs: Option<&[Option<Distribution>]>) -> Result<Eval> {
        let n = self.instances.len();
        let (primary_arrival, primary_transition) = self.primary_input_timing(choice, probs)?;
        let mut arrival = vec![[0.0_f64; 2]; n];
        let mut transition = vec![[0.0_f64; 2]; n];
        let mut load = vec![0.0; n];
        for &source in &self.reachable {
            load[source] = self.consumers[source]
                .iter()
                .map(|(sink, pin)| self.weighted_cap(choice, probs, *sink, *pin))
                .sum::<f64>()
                + self.external_output_load(source);
        }
        let mut cell_area = vec![0.0; n];
        let mut cell_power = vec![0.0; n];
        let mut arcs = Vec::new();
        let mut arcs_by_state: Vec<[Vec<usize>; 2]> =
            (0..n).map(|_| [Vec::new(), Vec::new()]).collect();
        let mut arcs_by_parent = vec![Vec::new(); n];
        for &i in &self.topo {
            let base_cell = &self.cells[choice.0[i]];
            cell_area[i] = self
                .each_cell(choice, probs, i)
                .map(|(w, _, c)| w * c.area.into_inner())
                .sum();
            let slews: Vec<_> = self.instances[i]
                .input_sources
                .iter()
                .enumerate()
                .map(|(pin, source)| {
                    source.map_or_else(
                        || {
                            if let Some(primary) = self.instances[i].input_primary_index[pin] {
                                primary_transition[primary][0].max(primary_transition[primary][1])
                            } else {
                                0.0
                            }
                        },
                        |s| transition[s][0].max(transition[s][1]),
                    )
                })
                .collect();
            cell_power[i] = self.weighted_power(choice, probs, i, &slews, load[i])?;
            let mut winner: [Option<(f64, f64)>; 2] = [None, None];
            let mut worst_transition: [Option<f64>; 2] = [None, None];
            for (arc_index, arc) in base_cell.timing_arcs.iter().enumerate() {
                let pin = self.timing_arc_pins[choice.0[i]][arc_index];
                let source =
                    self.instances[i].input_sources[pin].filter(|s| self.reachable_set.contains(s));
                let maps: &[(usize, usize)] = match arc.timing_sense {
                    TimingSense::PositiveUnate => &[(0, 0), (1, 1)],
                    TimingSense::NegativeUnate => &[(1, 0), (0, 1)],
                    TimingSense::NonUnate => &[(0, 0), (1, 0), (0, 1), (1, 1)],
                };
                for &(input_edge, output_edge) in maps {
                    let input_arrival = source.map_or_else(
                        || {
                            if let Some(primary) = self.instances[i].input_primary_index[pin] {
                                primary_arrival[primary][input_edge]
                            } else {
                                0.0
                            }
                        },
                        |s| arrival[s][input_edge],
                    );
                    let input_slew = source.map_or_else(
                        || {
                            if let Some(primary) = self.instances[i].input_primary_index[pin] {
                                primary_transition[primary][input_edge]
                            } else {
                                0.0
                            }
                        },
                        |s| transition[s][input_edge],
                    );
                    let (delay, out_transition) = self.weighted_delay_transition(
                        choice,
                        probs,
                        i,
                        arc_index,
                        output_edge,
                        input_slew,
                        load[i],
                    )?;
                    let candidate = self
                        .boundary
                        .internal_timing_model
                        .propagate(input_arrival, delay);
                    let index = arcs.len();
                    arcs.push(ArcEval {
                        parent: i,
                        output_edge,
                        source,
                        input_edge,
                        pin,
                        delay,
                        arrival: candidate,
                    });
                    arcs_by_state[i][output_edge].push(index);
                    arcs_by_parent[i].push(index);
                    if winner[output_edge].is_none() || candidate > winner[output_edge].unwrap().0 {
                        winner[output_edge] = Some((candidate, out_transition));
                    }
                    worst_transition[output_edge] = Some(
                        worst_transition[output_edge]
                            .map_or(out_transition, |current| current.max(out_transition)),
                    );
                }
            }
            for edge in 0..2 {
                if let Some((a, t)) = winner[edge] {
                    arrival[i][edge] = a;
                    transition[i][edge] = if self.boundary.internal_timing_model.is_v3() {
                        worst_transition[edge].unwrap_or(t)
                    } else {
                        t
                    };
                }
            }
        }
        let delay = self.output_delay(&arrival, &primary_arrival);
        let area: f64 = self.reachable.iter().map(|i| cell_area[*i]).sum();
        let power: f64 = self.reachable.iter().map(|i| cell_power[*i]).sum::<f64>()
            + self.direct_primary_output_switching_power();
        let area = self.boundary.internal_timing_model.area(area);
        let power = if self.boundary.internal_timing_model.is_genlib() {
            1.0
        } else {
            power
        };
        let score = self.port_objective_score(
            choice,
            probs,
            delay,
            area,
            power,
            &arrival,
            &transition,
            &primary_arrival,
            &primary_transition,
        );
        Ok(Eval {
            delay,
            area,
            power,
            score,
            primary_arrival: Arc::new(primary_arrival),
            primary_transition: Arc::new(primary_transition),
            arrival,
            transition,
            load,
            cell_area,
            cell_power,
            arcs,
            arcs_by_state: Arc::new(arcs_by_state),
            arcs_by_parent: Arc::new(arcs_by_parent),
        })
    }

    /// Re-evaluate the exact same mixed circuit after changing the probability
    /// vector of one occurrence.  Only that occurrence's input capacitance and
    /// the transitive timing fanout of it and its input drivers can change.
    /// All arithmetic inside the invalidated cone deliberately follows the
    /// same order as `evaluate`; this is an exact dependency cache, not a
    /// surrogate or an approximate gradient.
    fn evaluate_probability_delta_with_arc_groups(
        &self,
        choice: &Choice,
        probs: &[Option<Distribution>],
        base: &Eval,
        changed_inst: usize,
        weighted_arc_groups: Option<&[WeightedArcGroup]>,
    ) -> Result<(Eval, Option<f64>)> {
        if base.transition.len() != self.instances.len()
            || base.load.len() != self.instances.len()
            || base.arcs_by_parent.len() != self.instances.len()
        {
            return Ok((self.evaluate(choice, Some(probs))?, None));
        }
        let plan = &self.probability_delta_plans[changed_inst];
        if !self.iterative && self.boundary.internal_timing_model.is_v3() && plan.touches_primary {
            return Ok((self.evaluate(choice, Some(probs))?, None));
        }
        // A PI-driven occurrence can change the boundary load and hence the
        // arrival/slew presented to every consumer of that PI.  Recompute the
        // small per-PI boundary vector, then use the precomputed union of all
        // affected consumer cones.  Non-PI probes share the base vector.
        let (primary_arrival_result, primary_transition_result) =
            if self.boundary.internal_timing_model.is_v3() && plan.touches_primary {
                let (arrival, transition) =
                    self.primary_input_timing_delta(choice, probs, base, &plan.changed_primaries)?;
                (Arc::new(arrival), Arc::new(transition))
            } else if base.primary_arrival.len() == self.primary_output_load_count.len()
                && base.primary_transition.len() == self.primary_output_load_count.len()
            {
                (
                    base.primary_arrival.clone(),
                    base.primary_transition.clone(),
                )
            } else {
                let (arrival, transition) = self.primary_input_timing(choice, Some(probs))?;
                (Arc::new(arrival), Arc::new(transition))
            };
        let primary_arrival = primary_arrival_result.as_slice();
        let primary_transition = primary_transition_result.as_slice();

        let mut arrival = base.arrival.clone();
        let mut transition = base.transition.clone();
        let mut load = base.load.clone();
        // Only the changed occurrence's pin capacitances differ.  Recompute
        // each input driver's complete fanout sum to preserve summation order.
        for &source in &plan.changed_sources {
            load[source] = self.consumers[source]
                .iter()
                .map(|(sink, pin)| self.weighted_cap(choice, Some(probs), *sink, *pin))
                .sum::<f64>()
                + self.external_output_load(source);
        }
        let mut cell_area = base.cell_area.clone();
        let mut cell_power = base.cell_power.clone();
        let mut arcs = if weighted_arc_groups.is_some() {
            Vec::new()
        } else {
            base.arcs.clone()
        };
        let mut arc_delay_overrides = weighted_arc_groups.map(|_| vec![f64::NAN; base.arcs.len()]);
        let dynamic_cone = self.iterative && iterative_dynamic_cone_enabled();
        let mut dirty = vec![!dynamic_cone; self.instances.len()];
        if dynamic_cone {
            dirty[changed_inst] = true;
            for &source in &plan.changed_sources {
                dirty[source] = true;
            }
            for &consumer in &plan.changed_primary_consumers {
                dirty[consumer] = true;
            }
        }
        for &i in &plan.affected_topo {
            if !dirty[i] {
                continue;
            }
            let base_cell = &self.cells[choice.0[i]];
            cell_area[i] = self
                .each_cell(choice, Some(probs), i)
                .map(|(weight, _, cell)| weight * cell.area.into_inner())
                .sum();
            let slews: Vec<_> = self.instances[i]
                .input_sources
                .iter()
                .enumerate()
                .map(|(pin, source)| {
                    source.map_or_else(
                        || {
                            if let Some(primary) = self.instances[i].input_primary_index[pin] {
                                primary_transition[primary][0].max(primary_transition[primary][1])
                            } else {
                                0.0
                            }
                        },
                        |source| transition[source][0].max(transition[source][1]),
                    )
                })
                .collect();
            cell_power[i] = self.weighted_power(choice, Some(probs), i, &slews, load[i])?;
            let mut winner: [Option<(f64, f64)>; 2] = [None, None];
            let mut worst_transition: [Option<f64>; 2] = [None, None];
            let mut parent_arc_position = 0usize;
            for (arc_index, arc) in base_cell.timing_arcs.iter().enumerate() {
                let pin = self.timing_arc_pins[choice.0[i]][arc_index];
                let source =
                    self.instances[i].input_sources[pin].filter(|s| self.reachable_set.contains(s));
                let maps: &[(usize, usize)] = match arc.timing_sense {
                    TimingSense::PositiveUnate => &[(0, 0), (1, 1)],
                    TimingSense::NegativeUnate => &[(1, 0), (0, 1)],
                    TimingSense::NonUnate => &[(0, 0), (1, 0), (0, 1), (1, 1)],
                };
                for &(input_edge, output_edge) in maps {
                    let input_arrival = source.map_or_else(
                        || {
                            if let Some(primary) = self.instances[i].input_primary_index[pin] {
                                primary_arrival[primary][input_edge]
                            } else {
                                0.0
                            }
                        },
                        |s| arrival[s][input_edge],
                    );
                    let input_slew = source.map_or_else(
                        || {
                            if let Some(primary) = self.instances[i].input_primary_index[pin] {
                                primary_transition[primary][input_edge]
                            } else {
                                0.0
                            }
                        },
                        |s| transition[s][input_edge],
                    );
                    let (delay, out_transition) = self.weighted_delay_transition(
                        choice,
                        Some(probs),
                        i,
                        arc_index,
                        output_edge,
                        input_slew,
                        load[i],
                    )?;
                    let candidate = self
                        .boundary
                        .internal_timing_model
                        .propagate(input_arrival, delay);
                    let index = *base
                        .arcs_by_parent
                        .get(i)
                        .and_then(|indices| indices.get(parent_arc_position))
                        .context("incremental arc layout differs from full evaluation")?;
                    parent_arc_position += 1;
                    if let Some(overrides) = arc_delay_overrides.as_mut() {
                        overrides[index] = delay;
                    } else {
                        arcs[index] = ArcEval {
                            parent: i,
                            output_edge,
                            source,
                            input_edge,
                            pin,
                            delay,
                            arrival: candidate,
                        };
                    }
                    if winner[output_edge].is_none() || candidate > winner[output_edge].unwrap().0 {
                        winner[output_edge] = Some((candidate, out_transition));
                    }
                    worst_transition[output_edge] = Some(
                        worst_transition[output_edge]
                            .map_or(out_transition, |current| current.max(out_transition)),
                    );
                }
            }
            anyhow::ensure!(
                parent_arc_position == base.arcs_by_parent[i].len(),
                "incremental arc count differs from full evaluation"
            );
            for edge in 0..2 {
                if let Some((next_arrival, next_transition)) = winner[edge] {
                    arrival[i][edge] = next_arrival;
                    transition[i][edge] = if self.boundary.internal_timing_model.is_v3() {
                        worst_transition[edge].unwrap_or(next_transition)
                    } else {
                        next_transition
                    };
                }
            }
            if dynamic_cone
                && (arrival[i] != base.arrival[i] || transition[i] != base.transition[i])
            {
                for &(consumer, _) in &self.consumers[i] {
                    dirty[consumer] = true;
                }
            }
        }
        let delay = self.output_delay(&arrival, primary_arrival);
        let area: f64 = self.reachable.iter().map(|index| cell_area[*index]).sum();
        let power: f64 = self
            .reachable
            .iter()
            .map(|index| cell_power[*index])
            .sum::<f64>()
            + self.direct_primary_output_switching_power();
        let weighted_delta =
            weighted_arc_groups
                .zip(arc_delay_overrides.as_ref())
                .map(|(groups, overrides)| {
                    groups
                        .iter()
                        .map(|group| {
                            let changed_delay = group
                                .indices
                                .iter()
                                .map(|index| {
                                    if overrides[*index].is_nan() {
                                        base.arcs[*index].delay
                                    } else {
                                        overrides[*index]
                                    }
                                })
                                .fold(f64::NEG_INFINITY, f64::max);
                            group.weight * (changed_delay - group.base_delay)
                        })
                        .sum()
                });
        let area = self.boundary.internal_timing_model.area(area);
        let power = if self.boundary.internal_timing_model.is_genlib() {
            1.0
        } else {
            power
        };
        let score = self.port_objective_score(
            choice,
            Some(probs),
            delay,
            area,
            power,
            &arrival,
            &transition,
            primary_arrival,
            primary_transition,
        );
        Ok((
            Eval {
                delay,
                area,
                power,
                score,
                primary_arrival: primary_arrival_result,
                primary_transition: primary_transition_result,
                arrival,
                transition,
                load,
                cell_area,
                cell_power,
                arcs,
                arcs_by_state: base.arcs_by_state.clone(),
                arcs_by_parent: base.arcs_by_parent.clone(),
            },
            weighted_delta,
        ))
    }

    fn evaluate_probability_delta(
        &self,
        choice: &Choice,
        probs: &[Option<Distribution>],
        base: &Eval,
        changed_inst: usize,
    ) -> Result<Eval> {
        Ok(self
            .evaluate_probability_delta_with_arc_groups(choice, probs, base, changed_inst, None)?
            .0)
    }

    /// Evaluate every cell alternative for one occurrence in a single
    /// topological traversal. State is stored node-major (`node * lanes +
    /// lane`) so all lanes of the current node are contiguous. This is a
    /// single-thread data-layout optimization; it does not create workers.
    fn evaluate_probability_delta_lanes_with_arc_groups(
        &self,
        choice: &Choice,
        lane_probabilities: &[Vec<Option<Distribution>>],
        base: &Eval,
        changed_inst: usize,
        weighted_arc_groups: &[WeightedArcGroup],
    ) -> Result<Vec<DeltaMetrics>> {
        let lane_count = lane_probabilities.len();
        if lane_count == 0 {
            return Ok(Vec::new());
        }
        let node_count = self.instances.len();
        if base.transition.len() != node_count
            || base.load.len() != node_count
            || base.arcs_by_parent.len() != node_count
        {
            return lane_probabilities
                .iter()
                .map(|probabilities| {
                    let evaluated = self.evaluate(choice, Some(probabilities))?;
                    Ok(DeltaMetrics {
                        delay: evaluated.delay,
                        area: evaluated.area,
                        power: evaluated.power,
                        weighted_arc_delta: weighted_arc_delta(&evaluated, weighted_arc_groups),
                    })
                })
                .collect();
        }
        let plan = &self.probability_delta_plans[changed_inst];
        let primary_count = self.primary_output_load_count.len();
        let mut primary_arrival = vec![[0.0; 2]; primary_count * lane_count];
        let mut primary_transition = vec![[0.0; 2]; primary_count * lane_count];
        for primary in 0..primary_count {
            for lane in 0..lane_count {
                primary_arrival[primary * lane_count + lane] = base.primary_arrival[primary];
                primary_transition[primary * lane_count + lane] = base.primary_transition[primary];
            }
        }
        if self.boundary.internal_timing_model.is_v3() && plan.touches_primary {
            for (lane, probabilities) in lane_probabilities.iter().enumerate() {
                let (arrival, transition) = self.primary_input_timing_delta(
                    choice,
                    probabilities,
                    base,
                    &plan.changed_primaries,
                )?;
                for &primary in &plan.changed_primaries {
                    primary_arrival[primary * lane_count + lane] = arrival[primary];
                    primary_transition[primary * lane_count + lane] = transition[primary];
                }
            }
        }

        let mut arrival = vec![[0.0; 2]; node_count * lane_count];
        let mut transition = vec![[0.0; 2]; node_count * lane_count];
        let mut load = vec![0.0; node_count * lane_count];
        let mut cell_area = vec![0.0; node_count * lane_count];
        let mut cell_power = vec![0.0; node_count * lane_count];
        for node in 0..node_count {
            for lane in 0..lane_count {
                let index = node * lane_count + lane;
                arrival[index] = base.arrival[node];
                transition[index] = base.transition[node];
                load[index] = base.load[node];
                cell_area[index] = base.cell_area[node];
                cell_power[index] = base.cell_power[node];
            }
        }
        for &source in &plan.changed_sources {
            for (lane, probabilities) in lane_probabilities.iter().enumerate() {
                load[source * lane_count + lane] = self.consumers[source]
                    .iter()
                    .map(|(sink, pin)| self.weighted_cap(choice, Some(probabilities), *sink, *pin))
                    .sum::<f64>()
                    + self.external_output_load(source);
            }
        }
        let arc_count = base.arcs.len();
        // Arc-major layout keeps every alternative for the current arc
        // contiguous, matching the node-major propagation state above.
        let mut arc_delay_overrides = vec![f64::NAN; arc_count * lane_count];
        let dynamic_cone = iterative_dynamic_cone_enabled();
        let mut dirty = vec![!dynamic_cone; node_count * lane_count];
        if dynamic_cone {
            for lane in 0..lane_count {
                dirty[changed_inst * lane_count + lane] = true;
                for &source in &plan.changed_sources {
                    dirty[source * lane_count + lane] = true;
                }
                for &consumer in &plan.changed_primary_consumers {
                    dirty[consumer * lane_count + lane] = true;
                }
            }
        }

        let mut input_arrivals = vec![0.0; lane_count];
        let mut input_slews = vec![0.0; lane_count];
        let mut lane_loads = vec![0.0; lane_count];
        let mut lane_delays = vec![0.0; lane_count];
        let mut lane_transitions = vec![0.0; lane_count];
        let mut lane_powers = vec![0.0; lane_count];
        for &node in &plan.affected_topo {
            let active_lanes = (0..lane_count)
                .filter(|lane| dirty[node * lane_count + lane])
                .collect::<Vec<_>>();
            if active_lanes.is_empty() {
                continue;
            }
            let base_cell = &self.cells[choice.0[node]];
            let input_count = self.instances[node].input_sources.len();
            let mut lane_pin_slews = vec![0.0; lane_count * input_count];
            for &lane in &active_lanes {
                let state = node * lane_count + lane;
                let probabilities = &lane_probabilities[lane];
                cell_area[state] = self
                    .each_cell(choice, Some(probabilities), node)
                    .map(|(weight, _, cell)| weight * cell.area.into_inner())
                    .sum();
                lane_loads[lane] = load[state];
                for (pin, source) in self.instances[node].input_sources.iter().enumerate() {
                    lane_pin_slews[lane * input_count + pin] = source.map_or_else(
                        || {
                            if let Some(primary) = self.instances[node].input_primary_index[pin] {
                                let value = primary_transition[primary * lane_count + lane];
                                value[0].max(value[1])
                            } else {
                                0.0
                            }
                        },
                        |source| {
                            let value = transition[source * lane_count + lane];
                            value[0].max(value[1])
                        },
                    );
                }
            }
            self.weighted_power_lanes_into(
                choice,
                lane_probabilities,
                &active_lanes,
                node,
                &lane_pin_slews,
                input_count,
                &lane_loads,
                &mut lane_powers,
            )?;
            for &lane in &active_lanes {
                cell_power[node * lane_count + lane] = lane_powers[lane];
            }

            let mut winners: Vec<[Option<(f64, f64)>; 2]> = vec![[None; 2]; lane_count];
            let mut worst_transitions: Vec<[Option<f64>; 2]> = vec![[None; 2]; lane_count];
            let mut parent_arc_position = 0usize;
            for (arc_index, arc) in base_cell.timing_arcs.iter().enumerate() {
                let pin = self.timing_arc_pins[choice.0[node]][arc_index];
                let source = self.instances[node].input_sources[pin]
                    .filter(|source| self.reachable_set.contains(source));
                let maps: &[(usize, usize)] = match arc.timing_sense {
                    TimingSense::PositiveUnate => &[(0, 0), (1, 1)],
                    TimingSense::NegativeUnate => &[(1, 0), (0, 1)],
                    TimingSense::NonUnate => &[(0, 0), (1, 0), (0, 1), (1, 1)],
                };
                for &(input_edge, output_edge) in maps {
                    for &lane in &active_lanes {
                        input_arrivals[lane] = source.map_or_else(
                            || {
                                if let Some(primary) = self.instances[node].input_primary_index[pin]
                                {
                                    primary_arrival[primary * lane_count + lane][input_edge]
                                } else {
                                    0.0
                                }
                            },
                            |source| arrival[source * lane_count + lane][input_edge],
                        );
                        input_slews[lane] = source.map_or_else(
                            || {
                                if let Some(primary) = self.instances[node].input_primary_index[pin]
                                {
                                    primary_transition[primary * lane_count + lane][input_edge]
                                } else {
                                    0.0
                                }
                            },
                            |source| transition[source * lane_count + lane][input_edge],
                        );
                    }
                    self.weighted_delay_transition_lanes_into(
                        choice,
                        lane_probabilities,
                        &active_lanes,
                        node,
                        arc_index,
                        output_edge,
                        &input_slews,
                        &lane_loads,
                        &mut lane_delays,
                        &mut lane_transitions,
                    )?;
                    let index = *base
                        .arcs_by_parent
                        .get(node)
                        .and_then(|indices| indices.get(parent_arc_position))
                        .context("multi-lane arc layout differs from full evaluation")?;
                    parent_arc_position += 1;
                    for &lane in &active_lanes {
                        let delay = lane_delays[lane];
                        let out_transition = lane_transitions[lane];
                        let candidate = self
                            .boundary
                            .internal_timing_model
                            .propagate(input_arrivals[lane], delay);
                        arc_delay_overrides[index * lane_count + lane] = delay;
                        if winners[lane][output_edge].is_none()
                            || candidate > winners[lane][output_edge].unwrap().0
                        {
                            winners[lane][output_edge] = Some((candidate, out_transition));
                        }
                        worst_transitions[lane][output_edge] = Some(
                            worst_transitions[lane][output_edge]
                                .map_or(out_transition, |current| current.max(out_transition)),
                        );
                    }
                }
            }
            anyhow::ensure!(
                parent_arc_position == base.arcs_by_parent[node].len(),
                "multi-lane arc count differs from full evaluation"
            );
            for &lane in &active_lanes {
                let state = node * lane_count + lane;
                for edge in 0..2 {
                    if let Some((next_arrival, next_transition)) = winners[lane][edge] {
                        arrival[state][edge] = next_arrival;
                        transition[state][edge] = if self.boundary.internal_timing_model.is_v3() {
                            worst_transitions[lane][edge].unwrap_or(next_transition)
                        } else {
                            next_transition
                        };
                    }
                }
                if dynamic_cone
                    && (arrival[state] != base.arrival[node]
                        || transition[state] != base.transition[node])
                {
                    for &(consumer, _) in &self.consumers[node] {
                        dirty[consumer * lane_count + lane] = true;
                    }
                }
            }
        }

        (0..lane_count)
            .map(|lane| {
                let delay = self
                    .output_sources
                    .iter()
                    .flat_map(|source| match source {
                        BoundaryOutputSource::Gate(root) => arrival[*root * lane_count + lane],
                        BoundaryOutputSource::Primary(primary) => {
                            primary_arrival[*primary * lane_count + lane]
                        }
                        BoundaryOutputSource::Constant => [0.0; 2],
                    })
                    .fold(0.0, f64::max);
                let area = self
                    .reachable
                    .iter()
                    .map(|node| cell_area[*node * lane_count + lane])
                    .sum::<f64>();
                let power = self
                    .reachable
                    .iter()
                    .map(|node| cell_power[*node * lane_count + lane])
                    .sum::<f64>()
                    + self.direct_primary_output_switching_power();
                let weighted_arc_delta = weighted_arc_groups
                    .iter()
                    .map(|group| {
                        let changed_delay = group
                            .indices
                            .iter()
                            .map(|index| {
                                let override_delay =
                                    arc_delay_overrides[*index * lane_count + lane];
                                if override_delay.is_nan() {
                                    base.arcs[*index].delay
                                } else {
                                    override_delay
                                }
                            })
                            .fold(f64::NEG_INFINITY, f64::max);
                        group.weight * (changed_delay - group.base_delay)
                    })
                    .sum();
                let area = self.boundary.internal_timing_model.area(area);
                let power = if self.boundary.internal_timing_model.is_genlib() {
                    1.0
                } else {
                    power
                };
                Ok(DeltaMetrics {
                    delay,
                    area,
                    power,
                    weighted_arc_delta,
                })
            })
            .collect()
    }

    fn write_choice(&self, choice: &Choice, path: &Path) -> Result<()> {
        let mut text = self.source_text.clone();
        for (index, inst) in self.instances.iter().enumerate() {
            let new = &self.cell_names[choice.0[index]];
            if new == &self.cell_names[inst.original_cell] {
                continue;
            }
            let pattern = Regex::new(&format!(
                r"(?m)^(\s*)\w+_ASAP7_(?:6t_L|75t_R)(\s+{}\s*\()",
                regex::escape(&inst.name)
            ))?;
            let replacement = format!("${{1}}{}${{2}}", new);
            let next = pattern
                .replacen(&text, 1, replacement.as_str())
                .into_owned();
            anyhow::ensure!(next != text, "cannot rewrite instance {}", inst.name);
            text = next;
        }
        fs::write(path, text)?;
        Ok(())
    }
}

pub(crate) fn evaluate_written_netlist_with_database_objective(
    netlist: &Path,
    database: &CellDatabase,
    objective: SearchObjective,
) -> Result<PpaPoint> {
    let circuit = Circuit::parse_with_database_objective(netlist, database, objective)?;
    let evaluated = circuit.evaluate(&circuit.original_choice(), None)?;
    Ok(PpaPoint {
        delay: evaluated.delay,
        area: evaluated.area,
        power: evaluated.power,
    })
}

impl Tracker {
    fn new(
        circuit: &Circuit,
        budget: usize,
        checkpoint: Option<TrackerCheckpoint>,
        branch_from_checkpoint: bool,
    ) -> Result<Self> {
        if let Some(checkpoint) = checkpoint {
            anyhow::ensure!(
                checkpoint.exact_evaluations < budget,
                "resume checkpoint already consumed {} evaluations, target budget is {}",
                checkpoint.exact_evaluations,
                budget
            );
            let original = circuit.original_choice();
            let by_name: HashMap<_, _> = circuit
                .cell_names
                .iter()
                .enumerate()
                .map(|(index, name)| (name.as_str(), index))
                .collect();
            let indexed = |choice: NamedChoice| -> Result<Choice> {
                anyhow::ensure!(
                    choice.0.len() == circuit.instances.len(),
                    "resume checkpoint choice length differs from the current circuit"
                );
                choice
                    .0
                    .into_iter()
                    .map(|name| {
                        by_name.get(name.as_str()).copied().with_context(|| {
                            format!("resume checkpoint references unknown cell {name}")
                        })
                    })
                    .collect::<Result<Vec<_>>>()
                    .map(Choice)
            };
            let best_choice = indexed(checkpoint.best_choice.clone())?;
            let checkpoint_best = checkpoint.best.clone();
            let checkpoint_count = checkpoint.exact_evaluations;
            let checkpoint_polish_count = checkpoint.polish_evaluations;
            let checkpoint_trajectory = checkpoint.trajectory.clone();
            let cache: HashMap<_, _> = checkpoint
                .cache
                .into_iter()
                .map(|(choice, value)| Ok((indexed(choice)?, value)))
                .collect::<Result<_>>()?;
            if branch_from_checkpoint {
                return Ok(Self {
                    budget,
                    count: checkpoint_count,
                    paid_count: 0,
                    polish_count: checkpoint_polish_count,
                    cache,
                    replay_limit: 0,
                    replay_seen: HashSet::new(),
                    best_choice,
                    best: eval_from_ppa(&checkpoint_best),
                    trajectory: checkpoint_trajectory,
                });
            }
            let base = cache
                .get(&original)
                .context("resume checkpoint is missing the original choice")?
                .clone();
            return Ok(Self {
                budget,
                count: 0,
                paid_count: 0,
                polish_count: 0,
                cache,
                replay_limit: checkpoint_count,
                replay_seen: HashSet::from([original.clone()]),
                best_choice: original,
                best: eval_from_ppa(&base),
                trajectory: vec![TrajectoryPoint {
                    exact_evaluations: 0,
                    best: base,
                }],
            });
        }
        let choice = circuit.original_choice();
        let base = circuit.evaluate(&choice, None)?;
        let mut cache = HashMap::new();
        cache.insert(choice.clone(), ppa(&base));
        let trajectory = vec![TrajectoryPoint {
            exact_evaluations: 0,
            best: ppa(&base),
        }];
        Ok(Self {
            budget,
            count: 0,
            paid_count: 0,
            polish_count: 0,
            cache,
            replay_limit: 0,
            replay_seen: HashSet::from([choice.clone()]),
            best_choice: choice,
            best: base,
            trajectory,
        })
    }
    fn exact(&mut self, circuit: &Circuit, choice: &Choice) -> Result<Eval> {
        if let Some(value) = self.cache.get(choice) {
            if self.count < self.replay_limit && self.replay_seen.insert(choice.clone()) {
                self.count += 1;
                if value.score < self.best.score - EPS {
                    self.best = eval_from_ppa(value);
                    self.best_choice = choice.clone();
                }
                self.trajectory.push(TrajectoryPoint {
                    exact_evaluations: self.count,
                    best: ppa(&self.best),
                });
            }
            return Ok(eval_from_ppa(value));
        }
        if self.count >= self.budget {
            bail!("exact budget exhausted");
        }
        let iterative_key = (circuit.iterative && iterative_exact_cache_enabled()).then(|| {
            (
                circuit.exact_context_sha256.clone(),
                circuit.named_choice(choice),
            )
        });
        if let Some(key) = iterative_key.as_ref() {
            let cached = iterative_exact_cache()
                .values
                .lock()
                .unwrap()
                .get(key)
                .cloned();
            if let Some(value) = cached {
                iterative_exact_cache().hits.fetch_add(1, Ordering::Relaxed);
                self.count += 1;
                if value.score < self.best.score - EPS {
                    self.best = eval_from_ppa(&value);
                    self.best_choice = choice.clone();
                }
                self.cache.insert(choice.clone(), value.clone());
                self.trajectory.push(TrajectoryPoint {
                    exact_evaluations: self.count,
                    best: ppa(&self.best),
                });
                return Ok(eval_from_ppa(&value));
            }
            iterative_exact_cache()
                .misses
                .fetch_add(1, Ordering::Relaxed);
        }
        let value = circuit.evaluate(choice, None)?;
        self.count += 1;
        self.paid_count += 1;
        if value.score < self.best.score - EPS {
            self.best = value.clone();
            self.best_choice = choice.clone();
        }
        self.cache.insert(choice.clone(), ppa(&value));
        if let Some(key) = iterative_key {
            iterative_exact_cache()
                .values
                .lock()
                .unwrap()
                .insert(key, ppa(&value));
        }
        self.trajectory.push(TrajectoryPoint {
            exact_evaluations: self.count,
            best: ppa(&self.best),
        });
        Ok(value)
    }

    fn checkpoint(&self, circuit: &Circuit, input_sha256: String) -> TrackerCheckpoint {
        let named = |choice: &Choice| {
            NamedChoice(
                choice
                    .0
                    .iter()
                    .map(|index| circuit.cell_names[*index].clone())
                    .collect(),
            )
        };
        let mut cache: Vec<_> = self
            .cache
            .iter()
            .map(|(choice, value)| (named(choice), value.clone()))
            .collect();
        cache.sort_by(|left, right| left.0.cmp(&right.0));
        TrackerCheckpoint {
            input_sha256,
            exact_evaluations: self.count,
            polish_evaluations: self.polish_count,
            cache,
            best_choice: named(&self.best_choice),
            best: ppa(&self.best),
            trajectory: self.trajectory.clone(),
        }
    }
}

fn softmax(values: &[f64], tau: f64) -> Vec<f64> {
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let raw: Vec<_> = values
        .iter()
        .map(|v| ((v - max) / tau).max(-700.0).exp())
        .collect();
    let sum: f64 = raw.iter().sum();
    raw.into_iter().map(|v| v / sum).collect()
}

fn adjoint(circuit: &Circuit, ev: &Eval) -> HashMap<(usize, usize, usize, usize), f64> {
    let tau = TAU_RATIO * ev.delay;
    let mut state = vec![[0.0; 2]; circuit.instances.len()];
    let roots: Vec<_> = circuit
        .roots
        .iter()
        .flat_map(|r| [(*r, 0), (*r, 1)])
        .collect();
    let values: Vec<_> = roots.iter().map(|(r, e)| ev.arrival[*r][*e]).collect();
    for ((r, e), w) in roots.into_iter().zip(softmax(&values, tau)) {
        state[r][e] += w;
    }
    let mut result = HashMap::new();
    for &parent in circuit.topo.iter().rev() {
        for output in 0..2 {
            let mass = state[parent][output];
            if mass == 0.0 {
                continue;
            }
            let indices = &ev.arcs_by_state[parent][output];
            if indices.is_empty() {
                continue;
            }
            let values: Vec<_> = indices.iter().map(|i| ev.arcs[*i].arrival).collect();
            for (&index, w) in indices.iter().zip(softmax(&values, tau)) {
                let arc = &ev.arcs[index];
                let m = mass * w;
                *result
                    .entry((parent, output, arc.pin, arc.input_edge))
                    .or_insert(0.0) += m;
                if let Some(source) = arc.source {
                    state[source][arc.input_edge] += m;
                }
            }
        }
    }
    result
}

fn v8_ultra_active_instances(
    circuit: &Circuit,
    center: &Eval,
    config: Option<&V8UltraConfig>,
    budget: usize,
) -> (Option<BTreeSet<usize>>, UltraA2Audit) {
    let eligible: Vec<_> = circuit
        .instances
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            circuit.reachable_set.contains(index) && circuit.families[*index].len() > 1
        })
        .map(|(index, _)| index)
        .collect();
    let effective_cap = config.map(|value| value.active_instance_cap_for_budget(budget));
    let disabled = || UltraA2Audit {
        enabled: false,
        instance_count: circuit.instances.len(),
        eligible_instance_count: eligible.len(),
        gate_threshold: config.map(|value| value.gate_threshold),
        active_instance_cap: effective_cap,
        timing_slots: 0,
        area_slots: 0,
        power_slots: 0,
        selected_instance_count: eligible.len(),
        selected_instances: Vec::new(),
    };
    let Some(config) = config.filter(|value| value.enabled_for_instances(circuit.instances.len()))
    else {
        return (None, disabled());
    };
    let active_instance_cap = config.active_instance_cap_for_budget(budget);
    if eligible.len() <= active_instance_cap {
        let selected: BTreeSet<_> = eligible.iter().copied().collect();
        return (
            Some(selected.clone()),
            UltraA2Audit {
                enabled: true,
                instance_count: circuit.instances.len(),
                eligible_instance_count: eligible.len(),
                gate_threshold: Some(config.gate_threshold),
                active_instance_cap: Some(active_instance_cap),
                timing_slots: eligible.len(),
                area_slots: 0,
                power_slots: 0,
                selected_instance_count: selected.len(),
                selected_instances: selected.into_iter().collect(),
            },
        );
    }

    let arc_weights = adjoint(circuit, center);
    let mut timing_score = vec![0.0; circuit.instances.len()];
    for ((parent, _, _, _), weight) in arc_weights {
        timing_score[parent] += weight.abs();
    }
    let mut area_score = vec![0.0; circuit.instances.len()];
    let mut power_score = vec![0.0; circuit.instances.len()];
    for &index in &eligible {
        let minimum_area = circuit.families[index]
            .iter()
            .map(|cell| circuit.cells[*cell].area.into_inner())
            .fold(f64::INFINITY, f64::min);
        area_score[index] = (center.cell_area[index] - minimum_area).max(0.0);
        power_score[index] = center.cell_power[index].max(0.0);
    }
    let rank = |scores: &[f64]| {
        let mut ranked = eligible.clone();
        ranked.sort_by(|left, right| {
            scores[*right]
                .total_cmp(&scores[*left])
                .then_with(|| left.cmp(right))
        });
        ranked
    };
    let timing_rank = rank(&timing_score);
    let area_rank = rank(&area_score);
    let power_rank = rank(&power_score);
    let mut selected = BTreeSet::new();
    let mut admit = |ranked: &[usize], slots: usize| {
        for &index in ranked {
            if selected.len() >= slots {
                break;
            }
            selected.insert(index);
        }
    };
    let timing_slots = config
        .timing_slots_for_cap(active_instance_cap)
        .min(active_instance_cap);
    admit(&timing_rank, timing_slots);
    let area_target =
        (timing_slots + config.area_slots_for_cap(active_instance_cap)).min(active_instance_cap);
    admit(&area_rank, area_target);
    let power_target =
        (area_target + config.power_slots_for_cap(active_instance_cap)).min(active_instance_cap);
    admit(&power_rank, power_target);
    let rankings = [&timing_rank, &area_rank, &power_rank];
    let mut cursor = 0;
    while selected.len() < active_instance_cap {
        let mut advanced = false;
        for ranked in rankings {
            while cursor < ranked.len() {
                let index = ranked[cursor];
                if selected.insert(index) {
                    advanced = true;
                    break;
                }
                cursor += 1;
            }
            if selected.len() == active_instance_cap {
                break;
            }
        }
        if !advanced {
            break;
        }
        cursor += 1;
    }
    let selected_instances: Vec<_> = selected.iter().copied().collect();
    (
        Some(selected),
        UltraA2Audit {
            enabled: true,
            instance_count: circuit.instances.len(),
            eligible_instance_count: eligible.len(),
            gate_threshold: Some(config.gate_threshold),
            active_instance_cap: Some(active_instance_cap),
            timing_slots: config.timing_slots_for_cap(active_instance_cap),
            area_slots: config.area_slots_for_cap(active_instance_cap),
            power_slots: config.power_slots_for_cap(active_instance_cap),
            selected_instance_count: selected_instances.len(),
            selected_instances,
        },
    )
}

fn arc_map(ev: &Eval) -> HashMap<(usize, usize, usize, usize), f64> {
    let mut map = HashMap::new();
    for arc in &ev.arcs {
        map.entry((arc.parent, arc.output_edge, arc.pin, arc.input_edge))
            .and_modify(|v: &mut f64| *v = v.max(arc.delay))
            .or_insert(arc.delay);
    }
    map
}

struct WeightedArcGroup {
    weight: f64,
    indices: Vec<usize>,
    base_delay: f64,
}

fn weighted_arc_groups(
    base: &Eval,
    weights: &HashMap<(usize, usize, usize, usize), f64>,
) -> Vec<WeightedArcGroup> {
    let mut indices: HashMap<(usize, usize, usize, usize), Vec<usize>> = HashMap::new();
    for (index, arc) in base.arcs.iter().enumerate() {
        indices
            .entry((arc.parent, arc.output_edge, arc.pin, arc.input_edge))
            .or_default()
            .push(index);
    }
    weights
        .iter()
        .map(|(key, weight)| {
            let group = indices
                .get(key)
                .expect("adjoint arc key absent from the base evaluation")
                .clone();
            let base_delay = group
                .iter()
                .map(|index| base.arcs[*index].delay)
                .fold(f64::NEG_INFINITY, f64::max);
            WeightedArcGroup {
                weight: *weight,
                indices: group,
                base_delay,
            }
        })
        .collect()
}

fn weighted_arc_delta(changed: &Eval, groups: &[WeightedArcGroup]) -> f64 {
    groups
        .iter()
        .map(|group| {
            let changed_delay = group
                .indices
                .iter()
                .map(|index| changed.arcs[*index].delay)
                .fold(f64::NEG_INFINITY, f64::max);
            group.weight * (changed_delay - group.base_delay)
        })
        .sum()
}

fn project(circuit: &Circuit, probabilities: &[Option<Distribution>], center: &Choice) -> Choice {
    let mut out = center.clone();
    for (i, dist) in probabilities.iter().enumerate() {
        if let Some(d) = dist {
            let best = (0..d.cells.len())
                .max_by(|a, b| {
                    d.probabilities[*a]
                        .total_cmp(&d.probabilities[*b])
                        .then_with(|| {
                            circuit.cell_names[d.cells[*a]].cmp(&circuit.cell_names[d.cells[*b]])
                        })
                })
                .unwrap();
            out.0[i] = d.cells[best];
        }
    }
    out
}

fn beam(
    circuit: &Circuit,
    probabilities: &[Option<Distribution>],
    gradients: &[Option<Vec<f64>>],
    center: &Choice,
    m: usize,
    beam: usize,
) -> (Vec<Choice>, Vec<usize>) {
    let mut priority = Vec::new();
    for (i, dist) in probabilities.iter().enumerate() {
        if let (Some(d), Some(g)) = (dist, &gradients[i]) {
            let range = g.iter().copied().fold(f64::NEG_INFINITY, f64::max)
                - g.iter().copied().fold(f64::INFINITY, f64::min);
            let center_pos = d.cells.iter().position(|c| *c == center.0[i]).unwrap();
            priority.push(((1.0 - d.probabilities[center_pos]) * range, i));
        }
    }
    priority.sort_by(|a, b| {
        b.0.total_cmp(&a.0).then_with(|| {
            circuit.instances[a.1]
                .name
                .cmp(&circuit.instances[b.1].name)
        })
    });
    let active: Vec<_> = priority.into_iter().take(m).map(|x| x.1).collect();
    let base = project(circuit, probabilities, center);
    let mut states = vec![(0.0, base)];
    for &i in &active {
        let d = probabilities[i].as_ref().unwrap();
        let mut positions: Vec<_> = (0..d.cells.len()).collect();
        positions.sort_by(|a, b| {
            d.probabilities[*b]
                .total_cmp(&d.probabilities[*a])
                .then_with(|| circuit.cell_names[d.cells[*a]].cmp(&circuit.cell_names[d.cells[*b]]))
        });
        positions.truncate(2);
        let mut unique: BTreeMap<Choice, f64> = BTreeMap::new();
        for (cost, state) in &states {
            for &p in &positions {
                let mut next = state.clone();
                next.0[i] = d.cells[p];
                let value = cost - d.probabilities[p].max(1e-300).ln();
                unique
                    .entry(next)
                    .and_modify(|old| *old = old.min(value))
                    .or_insert(value);
            }
        }
        states = unique.into_iter().map(|(c, v)| (v, c)).collect();
        states.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        states.truncate(beam);
    }
    (states.into_iter().map(|x| x.1).collect(), active)
}

fn affected(circuit: &Circuit, names: &[usize]) -> HashSet<usize> {
    let mut out: HashSet<_> = names.iter().copied().collect();
    for &i in names {
        out.extend(
            circuit.instances[i]
                .input_sources
                .iter()
                .flatten()
                .filter(|s| circuit.reachable_set.contains(s))
                .copied(),
        );
        out.extend(circuit.consumers[i].iter().map(|x| x.0));
    }
    out
}

fn local_delta(
    base: &Eval,
    changed: &Eval,
    affected: &HashSet<usize>,
    weights: &HashMap<(usize, usize, usize, usize), f64>,
    multiplier: f64,
    objective: &SearchObjective,
) -> f64 {
    let ba = arc_map(base);
    let ca = arc_map(changed);
    let da: f64 = affected
        .iter()
        .map(|i| changed.cell_area[*i] - base.cell_area[*i])
        .sum();
    let dp: f64 = affected
        .iter()
        .map(|i| changed.cell_power[*i] - base.cell_power[*i])
        .sum();
    let dd: f64 = weights
        .iter()
        .filter(|((p, _, _, _), _)| affected.contains(p))
        .map(|(k, w)| w * (ca.get(k).copied().unwrap_or(ba[k]) - ba[k]))
        .sum();
    match objective {
        SearchObjective::D2ap => da / base.area + dp / base.power + multiplier * dd,
        SearchObjective::AreaUnderDelay { .. } => da / base.area + multiplier * dd,
        SearchObjective::Programmable { .. } => {
            objective.relative_delta(base.delay, base.area, base.power, dd, da, dp)
        }
    }
}

fn local_pair(
    circuit: &Circuit,
    probabilities: &[Option<Distribution>],
    centered: &[Option<Vec<f64>>],
    center: &Choice,
    active: &[usize],
    multiplier: f64,
) -> Result<(Vec<Choice>, f64)> {
    let local_started = Instant::now();
    let base_choice = project(circuit, probabilities, center);
    // These are frozen local electrical probes, not paid complete-circuit
    // candidates.  The Python reference likewise keeps them outside Tracker.
    let base = circuit.evaluate(&base_choice, None)?;
    let weights = adjoint(circuit, &base);
    let mut vars = Vec::new();
    for &i in active {
        let d = probabilities[i].as_ref().unwrap();
        let mut p: Vec<_> = (0..d.cells.len()).collect();
        p.sort_by(|a, b| {
            d.probabilities[*b]
                .total_cmp(&d.probabilities[*a])
                .then_with(|| circuit.cell_names[d.cells[*a]].cmp(&circuit.cell_names[d.cells[*b]]))
        });
        if p.len() >= 2 {
            vars.push((i, d.cells[p[0]], d.cells[p[1]], p[0], p[1]));
        }
    }
    let mut analytic = vec![0.0; vars.len()];
    let mut local = vec![0.0; vars.len()];
    for (j, (i, reference, alternative, rp, ap)) in vars.iter().enumerate() {
        analytic[j] = centered[*i].as_ref().unwrap()[*ap] - centered[*i].as_ref().unwrap()[*rp];
        let mut changed = base_choice.clone();
        changed.0[*i] = *alternative;
        let ev = circuit.evaluate(&changed, None)?;
        local[j] = local_delta(
            &base,
            &ev,
            &affected(circuit, &[*i]),
            &weights,
            multiplier,
            &circuit.objective,
        );
        let _ = reference;
    }
    let rms = |x: &[f64]| (x.iter().map(|v| v * v).sum::<f64>() / x.len().max(1) as f64).sqrt();
    let ar = rms(&analytic);
    let lr = rms(&local);
    let unary = if ar.max(lr) / ar.min(lr).max(1e-15) > 10.0 {
        &local
    } else {
        &analytic
    };
    let active_set: HashSet<_> = vars.iter().map(|x| x.0).collect();
    let critical_count = circuit.reachable.len().max(1) / 4;
    let mut qinst = vec![0.0; circuit.instances.len()];
    for ((p, _, _, _), w) in &weights {
        qinst[*p] += *w;
    }
    let mut ordered = circuit.reachable.clone();
    ordered.sort_by(|a, b| {
        qinst[*b]
            .total_cmp(&qinst[*a])
            .then_with(|| circuit.instances[*a].name.cmp(&circuit.instances[*b].name))
    });
    let critical: HashSet<_> = ordered.into_iter().take(critical_count.max(1)).collect();
    let mut edges = HashSet::new();
    for &a in &active_set {
        for &(b, _) in &circuit.consumers[a] {
            if active_set.contains(&b) {
                edges.insert((a.min(b), a.max(b)));
            }
        }
    }
    let direct = edges.clone();
    for &(a, mid) in &direct {
        for &(b, _) in &circuit.consumers[mid] {
            if a != b && active_set.contains(&b) && (critical.contains(&a) || critical.contains(&b))
            {
                edges.insert((a.min(b), a.max(b)));
            }
        }
        for source in circuit.instances[mid].input_sources.iter().flatten() {
            let b = *source;
            if a != b && active_set.contains(&b) && (critical.contains(&a) || critical.contains(&b))
            {
                edges.insert((a.min(b), a.max(b)));
            }
        }
    }
    let pos: HashMap<_, _> = vars.iter().enumerate().map(|(j, x)| (x.0, j)).collect();
    let mut pair = HashMap::new();
    for (a, b) in edges {
        let (Some(&ia), Some(&ib)) = (pos.get(&a), pos.get(&b)) else {
            continue;
        };
        let mut changed = base_choice.clone();
        changed.0[a] = vars[ia].2;
        changed.0[b] = vars[ib].2;
        let ev = circuit.evaluate(&changed, None)?;
        pair.insert(
            (ia, ib),
            local_delta(
                &base,
                &ev,
                &affected(circuit, &[a, b]),
                &weights,
                multiplier,
                &circuit.objective,
            ) - local[ia]
                - local[ib],
        );
    }
    let n = vars.len();
    let mut proposals = Vec::new();
    let solve_started = Instant::now();
    let solver = std::env::var("EGG_A2_LOCAL_SOLVER").unwrap_or_else(|_| "enumerate".into());
    for k in [4usize, 8, 16, 32] {
        if n > 63 {
            bail!("local pair active set too large");
        }
        let best_mask = if solver == "enumerate" {
            let mut best = (f64::INFINITY, 0u64);
            for mask in 0..(1u64 << n) {
                if mask.count_ones() as usize > k {
                    continue;
                }
                let mut value = 0.0;
                for i in 0..n {
                    if mask & (1 << i) != 0 {
                        value += unary[i];
                    }
                }
                for ((i, j), v) in &pair {
                    if mask & (1 << i) != 0 && mask & (1 << j) != 0 {
                        value += v;
                    }
                }
                if value < best.0 - EPS || ((value - best.0).abs() <= EPS && mask < best.1) {
                    best = (value, mask)
                }
            }
            best.1
        } else if solver == "cbc" {
            #[cfg(feature = "milp-cbc")]
            {
                solve_pair_cbc(&unary, &pair, k)
            }
            #[cfg(not(feature = "milp-cbc"))]
            {
                bail!("EGG_A2_LOCAL_SOLVER=cbc requires --features milp-cbc")
            }
        } else {
            bail!("EGG_A2_LOCAL_SOLVER must be enumerate or cbc")
        };
        let mut choice = base_choice.clone();
        for i in 0..n {
            choice.0[vars[i].0] = if best_mask & (1 << i) != 0 {
                vars[i].2
            } else {
                vars[i].1
            };
        }
        proposals.push(choice);
    }
    let solve_sec = solve_started.elapsed().as_secs_f64();
    let _total_sec = local_started.elapsed().as_secs_f64();
    Ok((proposals, solve_sec))
}

#[cfg(feature = "milp-cbc")]
fn solve_pair_cbc(unary: &[f64], pair: &HashMap<(usize, usize), f64>, k: usize) -> u64 {
    let mut model = Model::default();
    model.set_parameter("loglevel", "0");
    model.set_obj_sense(Sense::Minimize);
    let vars: Vec<_> = (0..unary.len()).map(|_| model.add_binary()).collect();
    for (index, (&column, &coefficient)) in vars.iter().zip(unary).enumerate() {
        // Deterministically prefer the same lower numeric mask as enumeration
        // when the physical surrogate is tied within floating precision.
        let tie = EPS * 0.125 * 2f64.powi(index as i32 - unary.len() as i32);
        model.set_obj_coeff(column, coefficient + tie);
    }
    let cardinality = model.add_row();
    model.set_row_upper(cardinality, k.min(unary.len()) as f64);
    for &column in &vars {
        model.set_weight(cardinality, column, 1.0);
    }
    for (&(left, right), &coefficient) in pair {
        let product = model.add_binary();
        model.set_obj_coeff(product, coefficient);

        let upper_left = model.add_row();
        model.set_row_upper(upper_left, 0.0);
        model.set_weight(upper_left, product, 1.0);
        model.set_weight(upper_left, vars[left], -1.0);

        let upper_right = model.add_row();
        model.set_row_upper(upper_right, 0.0);
        model.set_weight(upper_right, product, 1.0);
        model.set_weight(upper_right, vars[right], -1.0);

        let lower = model.add_row();
        model.set_row_lower(lower, -1.0);
        model.set_weight(lower, product, 1.0);
        model.set_weight(lower, vars[left], -1.0);
        model.set_weight(lower, vars[right], -1.0);
    }
    let solution = model.solve();
    vars.iter()
        .enumerate()
        .fold(0u64, |mask, (index, &column)| {
            if solution.col(column) > 0.5 {
                mask | (1u64 << index)
            } else {
                mask
            }
        })
}

fn polish(
    circuit: &Circuit,
    tracker: &mut Tracker,
    mut choice: Choice,
    limit: usize,
) -> Result<(Choice, Eval)> {
    let before = tracker.count;
    let mut current = tracker.exact(circuit, &choice)?;
    tracker.polish_count += tracker.count - before;
    for _ in 0..limit {
        let mut rows = Vec::new();
        for &i in &circuit.reachable {
            for &cell in &circuit.families[i] {
                if cell == choice.0[i] {
                    continue;
                }
                let mut next = choice.clone();
                next.0[i] = cell;
                if tracker.count >= tracker.budget {
                    return Ok((choice, current));
                }
                let before = tracker.count;
                let ev = tracker.exact(circuit, &next)?;
                tracker.polish_count += tracker.count - before;
                rows.push((
                    ev.score,
                    circuit.instances[i].name.clone(),
                    circuit.cell_names[cell].clone(),
                    next,
                    ev,
                ));
            }
        }
        rows.sort_by(|a, b| {
            a.0.total_cmp(&b.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
        });
        if rows.first().is_none_or(|r| r.0 >= current.score - EPS) {
            break;
        }
        let (_, _, _, next, ev) = rows.remove(0);
        choice = next;
        current = ev;
    }
    Ok((choice, current))
}

fn run(
    circuit: &Circuit,
    budget: usize,
    max_steps: usize,
    normalization: &str,
    eta: f64,
    epsilon: f64,
    cadence: usize,
    polish_limit: usize,
    checkpoint: Option<TrackerCheckpoint>,
    branch_from_checkpoint: bool,
    ultra_config: Option<&V8UltraConfig>,
) -> Result<(Tracker, usize, usize, RunProfile, UltraA2Audit)> {
    let mut tracker = Tracker::new(circuit, budget, checkpoint, branch_from_checkpoint)?;
    let mut center = tracker.best_choice.clone();
    // A resumed checkpoint stores scalar PPA/cache data but deliberately not
    // the large arrival/transition/arc vectors.  Reconstruct those vectors
    // once before sparse criticality selection or incremental finite
    // differences; a fresh stage already has the complete Eval.
    let mut center_ev = if tracker.best.arrival.len() == circuit.instances.len() {
        tracker.best.clone()
    } else {
        circuit.evaluate(&center, None)?
    };
    let (active_instances, ultra_audit) =
        v8_ultra_active_instances(circuit, &center_ev, ultra_config, budget);
    let mut probabilities =
        circuit.distributions_for_active(&center, epsilon, active_instances.as_ref());
    let mut no_improvement = 0;
    let mut mixed_count = 0;
    let mut fd_count = 0;
    let mut profile = RunProfile::default();
    let incremental_fd = std::env::var("EGG_A2_INCREMENTAL_FD")
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(true);
    let iterative = std::env::var("EGG_ITERATIVE")
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(false);
    let iterative_batch_fd = iterative
        && std::env::var("EGG_ITERATIVE_BATCH_FD")
            .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
            .unwrap_or(true);
    let iterative_multi_lane = iterative
        && std::env::var("EGG_ITERATIVE_MULTI_LANE")
            .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
            .unwrap_or(false);
    let fd_jobs = std::env::var("EGG_A2_FD_JOBS")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()
        .context("invalid EGG_A2_FD_JOBS")?
        .unwrap_or(8)
        .max(1);
    for step in 1..=max_steps {
        let gradient_started = Instant::now();
        let mixed = circuit.evaluate(&center, Some(&probabilities))?;
        mixed_count += 1;
        let weights = adjoint(circuit, &mixed);
        let base_arcs = arc_map(&mixed);
        let iterative_arc_groups = iterative.then(|| weighted_arc_groups(&mixed, &weights));
        let h = 1e-3;
        let mut gradients: Vec<Option<Vec<f64>>> = probabilities
            .iter()
            .map(|distribution| {
                distribution
                    .as_ref()
                    .map(|value| vec![0.0; value.cells.len()])
            })
            .collect();
        let mut probes = Vec::new();
        for i in 0..probabilities.len() {
            let Some(d) = probabilities[i].as_ref() else {
                continue;
            };
            for c in 0..d.cells.len() {
                probes.push((i, c));
            }
        }
        let evaluate_perturbed = |i: usize, perturbed: &[Option<Distribution>]| -> Result<f64> {
            let (ev, cached_weighted_delta) = if incremental_fd {
                if let Some(groups) = iterative_arc_groups.as_ref() {
                    circuit.evaluate_probability_delta_with_arc_groups(
                        &center,
                        &perturbed,
                        &mixed,
                        i,
                        Some(groups),
                    )?
                } else {
                    (
                        circuit.evaluate_probability_delta(&center, &perturbed, &mixed, i)?,
                        None,
                    )
                }
            } else {
                (circuit.evaluate(&center, Some(&perturbed))?, None)
            };
            let dd = if let Some(value) = cached_weighted_delta {
                value
            } else if let Some(groups) = iterative_arc_groups.as_ref() {
                weighted_arc_delta(&ev, groups)
            } else {
                let arcs = arc_map(&ev);
                weights
                    .iter()
                    .map(|(key, weight)| {
                        weight * (arcs.get(key).copied().unwrap_or(base_arcs[key]) - base_arcs[key])
                    })
                    .sum()
            };
            if circuit.port_objective.is_some() {
                Ok((ev.score - mixed.score) / h)
            } else {
                Ok(circuit.objective.relative_delta(
                    mixed.delay,
                    mixed.area,
                    mixed.power,
                    dd,
                    ev.area - mixed.area,
                    ev.power - mixed.power,
                ) / h)
            }
        };
        let probe_values = if iterative_batch_fd {
            // All alternatives of one occurrence invalidate the same dependency
            // cone. Dispatch them as one batch so the immutable distribution
            // vector, task setup and worker scheduling are shared. Each lane is
            // still evaluated in the original cell order and uses the same
            // floating-point operations as the scalar implementation.
            let batches: Vec<_> = probabilities
                .iter()
                .enumerate()
                .filter_map(|(i, distribution)| {
                    distribution.as_ref().map(|value| (i, value.cells.len()))
                })
                .collect();
            let batch_values = parallel_map_ordered(&batches, fd_jobs, |_, &(i, alternatives)| {
                let original = probabilities[i]
                    .as_ref()
                    .context("missing batched distribution")?
                    .probabilities
                    .clone();
                if iterative_multi_lane {
                    let mut lanes = Vec::with_capacity(alternatives);
                    for c in 0..alternatives {
                        let mut perturbed = probabilities.clone();
                        let p = Arc::make_mut(
                            &mut perturbed[i]
                                .as_mut()
                                .context("missing multi-lane distribution")?
                                .probabilities,
                        );
                        for value in p.iter_mut() {
                            *value *= 1.0 - h;
                        }
                        p[c] += h;
                        lanes.push(perturbed);
                    }
                    let groups = iterative_arc_groups
                        .as_ref()
                        .context("Iterative multi-lane requires weighted arc groups")?;
                    return circuit
                        .evaluate_probability_delta_lanes_with_arc_groups(
                            &center, &lanes, &mixed, i, groups,
                        )?
                        .into_iter()
                        .map(|metrics| {
                            anyhow::ensure!(
                                circuit.port_objective.is_none(),
                                "port objective requires EGG_ITERATIVE_MULTI_LANE=0"
                            );
                            Ok(circuit.objective.relative_delta(
                                mixed.delay,
                                mixed.area,
                                mixed.power,
                                metrics.weighted_arc_delta,
                                metrics.area - mixed.area,
                                metrics.power - mixed.power,
                            ) / h)
                        })
                        .collect::<Result<Vec<_>>>();
                }
                let mut perturbed = probabilities.clone();
                let mut values = Vec::with_capacity(alternatives);
                for c in 0..alternatives {
                    let p = Arc::make_mut(
                        &mut perturbed[i]
                            .as_mut()
                            .context("missing batched distribution")?
                            .probabilities,
                    );
                    p.clone_from(original.as_ref());
                    for value in p.iter_mut() {
                        *value *= 1.0 - h;
                    }
                    p[c] += h;
                    values.push(evaluate_perturbed(i, &perturbed)?);
                }
                Ok(values)
            })?;
            batch_values.into_iter().flatten().collect()
        } else {
            parallel_map_ordered(&probes, fd_jobs, |_, &(i, c)| {
                let mut perturbed = probabilities.clone();
                let p = Arc::make_mut(&mut perturbed[i].as_mut().unwrap().probabilities);
                for value in p.iter_mut() {
                    *value *= 1.0 - h;
                }
                p[c] += h;
                evaluate_perturbed(i, &perturbed)
            })?
        };
        mixed_count += probes.len();
        fd_count += probes.len();
        for ((i, c), value) in probes.into_iter().zip(probe_values) {
            gradients[i].as_mut().unwrap()[c] = value;
        }
        profile.gradient_sec += gradient_started.elapsed().as_secs_f64();
        let mut centered = gradients.clone();
        let mut flat = Vec::new();
        for i in 0..gradients.len() {
            if let (Some(d), Some(g)) = (&probabilities[i], &gradients[i]) {
                let mean = if normalization == "n0" {
                    g.iter().sum::<f64>() / g.len() as f64
                } else {
                    g.iter()
                        .zip(d.probabilities.iter())
                        .map(|(a, p)| a * p)
                        .sum()
                };
                let vals: Vec<_> = g.iter().map(|v| v - mean).collect();
                flat.extend(vals.iter().copied());
                centered[i] = Some(vals);
            }
        }
        let global =
            (flat.iter().map(|v| v * v).sum::<f64>() / flat.len().max(1) as f64).sqrt() + 1e-12;
        for i in 0..probabilities.len() {
            if let (Some(d), Some(g)) = (&mut probabilities[i], &centered[i]) {
                let local = (g.iter().map(|v| v * v).sum::<f64>() / g.len() as f64).sqrt() + 1e-12;
                let scale = if normalization == "n0" { local } else { global };
                let logits: Vec<_> = d
                    .probabilities
                    .iter()
                    .zip(g)
                    .map(|(p, v)| p.max(1e-300).ln() - eta * v / scale)
                    .collect();
                let max = logits.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                let raw: Vec<_> = logits.iter().map(|v| (v - max).max(-700.0).exp()).collect();
                let sum: f64 = raw.iter().sum();
                d.probabilities = Arc::new(raw.into_iter().map(|v| v / sum).collect());
            }
        }
        if step % cadence != 0 {
            continue;
        }
        let projected_mixed = circuit.evaluate(&center, Some(&probabilities))?;
        mixed_count += 1;
        let (beams, active) = beam(circuit, &probabilities, &centered, &center, 16, 16);
        let mut candidates = Vec::new();
        let argmax = project(circuit, &probabilities, &center);
        if tracker.count >= tracker.budget {
            break;
        }
        let ev = tracker.exact(circuit, &argmax)?;
        candidates.push((ev.score, argmax, ev));
        for choice in beams {
            if tracker.count >= tracker.budget {
                break;
            }
            let ev = tracker.exact(circuit, &choice)?;
            candidates.push((ev.score, choice, ev));
        }
        if tracker.count < tracker.budget {
            let local_pair_started = Instant::now();
            let (local_choices, solve_sec) = local_pair(
                circuit,
                &probabilities,
                &centered,
                &center,
                &active,
                circuit
                    .objective
                    .delay_multiplier(mixed.delay, mixed.area, mixed.power),
            )?;
            profile.local_pair_total_sec += local_pair_started.elapsed().as_secs_f64();
            profile.local_pair_solve_sec += solve_sec;
            profile.local_pair_calls += 1;
            for choice in local_choices {
                if tracker.count >= tracker.budget {
                    break;
                }
                let ev = tracker.exact(circuit, &choice)?;
                candidates.push((ev.score, choice, ev));
            }
        }
        candidates.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let (_, mut choice, mut ev) = candidates.remove(0);
        let _ = projected_mixed;
        if ev.score < center_ev.score - EPS {
            if polish_limit > 0 {
                (choice, ev) = polish(circuit, &mut tracker, choice, polish_limit)?;
            }
            center = choice;
            center_ev = ev;
            no_improvement = 0;
            probabilities =
                circuit.distributions_for_active(&center, epsilon, active_instances.as_ref());
        } else {
            no_improvement += 1;
            if no_improvement >= 3 {
                break;
            }
        }
    }
    Ok((tracker, mixed_count, fd_count, profile, ultra_audit))
}

fn sha(path: &Path) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(fs::read(path)?)))
}

fn execute(
    circuit: Circuit,
    input: &Path,
    out: &Path,
    mode: &str,
    lib: &Path,
    rules: &Path,
    started: Instant,
    resume_checkpoint: Option<&Path>,
    budget_override: Option<usize>,
    branch_from_checkpoint: bool,
) -> Result<Value> {
    fs::create_dir_all(&out)?;
    let (default_budget, steps, norm, eta) = if mode == "global" {
        (5000, 50, "n2", 1.0)
    } else if mode == "inner" {
        (500, 15, "n0", 0.5)
    } else {
        bail!("mode must be global or inner")
    };
    let budget = budget_override.unwrap_or(match std::env::var("EGG_A2_EXACT_BUDGET") {
        Ok(value) => value
            .parse::<usize>()
            .with_context(|| format!("invalid EGG_A2_EXACT_BUDGET={value:?}"))?,
        Err(std::env::VarError::NotPresent) => default_budget,
        Err(error) => return Err(error.into()),
    });
    anyhow::ensure!(budget > 0, "EGG_A2_EXACT_BUDGET must be positive");
    let input_sha256 = sha(&circuit.source_path)?;
    let checkpoint = resume_checkpoint
        .map(|path| -> Result<TrackerCheckpoint> {
            let checkpoint: TrackerCheckpoint = serde_json::from_slice(&fs::read(path)?)?;
            anyhow::ensure!(
                checkpoint.input_sha256 == input_sha256,
                "resume checkpoint input SHA does not match {}",
                input.display()
            );
            Ok(checkpoint)
        })
        .transpose()?;
    let resumed_exact = checkpoint
        .as_ref()
        .map_or(0, |value| value.exact_evaluations);
    let start_eval = circuit.evaluate(&circuit.original_choice(), None)?;
    let ultra_config_source = V8UltraConfig::from_env()?;
    let ultra_config = ultra_config_source.as_ref().map(|(_, config)| config);
    let (tracker, mixed, fd, profile, ultra_audit) = run(
        &circuit,
        budget,
        steps,
        norm,
        eta,
        0.10,
        5,
        5,
        checkpoint,
        branch_from_checkpoint,
        ultra_config,
    )?;
    let best_path = out.join("best.v");
    circuit.write_choice(&tracker.best_choice, &best_path)?;
    fs::write(
        out.join("trajectory.json"),
        serde_json::to_string_pretty(&tracker.trajectory)? + "\n",
    )?;
    fs::write(
        out.join("checkpoint.json"),
        serde_json::to_vec(&tracker.checkpoint(&circuit, input_sha256.clone()))?,
    )?;
    // A resumed stage can keep the checkpoint winner without reevaluating it;
    // checkpoint PPA intentionally omits large timing vectors.  Reconstruct
    // those vectors once for the partition-local boundary receipt.
    let best_boundary_eval = if tracker.best.arrival.len() == circuit.instances.len() {
        tracker.best.clone()
    } else {
        circuit.evaluate(&tracker.best_choice, None)?
    };
    let audit_checkpoints: Vec<_> = tracker
        .trajectory
        .iter()
        .filter(|point| [0, 25, 50, 75, 100, 175, 200, budget].contains(&point.exact_evaluations))
        .cloned()
        .collect();
    let summary = json!({"implementation":"single-process Rust frozen A2-Next","mode":mode,"input_netlist":input,
        "input_sha256":input_sha256,"liberty":lib,"scale_rules":rules,
        "objective":circuit.objective.name(),"delay_cap_ps":circuit.objective.delay_cap_ps(),
        "start_feasible":circuit.objective.feasible(start_eval.delay),
        "best_feasible":circuit.objective.feasible(tracker.best.delay),
        "start_ppa":ppa(&start_eval),"best":ppa(&tracker.best),"exact_evaluations":tracker.count,
        "start_boundary_outputs":circuit.boundary_output_states(&start_eval),
        "best_boundary_outputs":circuit.boundary_output_states(&best_boundary_eval),
        "resumed_exact_evaluations":resumed_exact,
        "new_exact_evaluations":tracker.count-resumed_exact,
        "resume_checkpoint":resume_checkpoint,
        "trajectory_file":"trajectory.json","trajectory_checkpoints":audit_checkpoints,
        "o1_polish_evaluations":tracker.polish_count,
        "exact_budget":budget,"default_exact_budget":default_budget,"changed_instances":tracker.best_choice.0.iter().zip(circuit.instances.iter()).filter(|(c,i)|**c!=i.original_cell).count(),
        "mixed_forward_count":mixed,"finite_difference_count":fd,
        "v8_ultra":{"enabled":ultra_audit.enabled,
            "config_path":ultra_config_source.as_ref().map(|(path,_)|path),
            "sparse_a2":ultra_audit},
        "iterative":{"enabled":circuit.iterative,"implementation":"exact-reuse-v4-multilane",
            "batched_finite_difference":circuit.iterative&&std::env::var("EGG_ITERATIVE_BATCH_FD")
                .map(|value|value!="0"&&!value.eq_ignore_ascii_case("false")).unwrap_or(true),
            "multi_lane_propagation":circuit.iterative&&std::env::var("EGG_ITERATIVE_MULTI_LANE")
                .map(|value|value!="0"&&!value.eq_ignore_ascii_case("false")).unwrap_or(false),
            "dynamic_exact_cone":circuit.iterative&&iterative_dynamic_cone_enabled(),
            "per_pi_boundary_delta":circuit.iterative,
            "shared_lut_grid":circuit.iterative&&iterative_shared_grid_enabled(),
            "global_exact_state_cache":circuit.iterative&&iterative_exact_cache_enabled()},
        "incremental_finite_difference":std::env::var("EGG_A2_INCREMENTAL_FD")
            .map(|value|value!="0"&&!value.eq_ignore_ascii_case("false")).unwrap_or(true),
        "profile_sec":{"gradient":profile.gradient_sec,"local_pair_total":profile.local_pair_total_sec,
            "local_pair_solve":profile.local_pair_solve_sec,"local_pair_calls":profile.local_pair_calls},
        "local_pair_solver":std::env::var("EGG_A2_LOCAL_SOLVER").unwrap_or_else(|_|"enumerate".into()),
        "elapsed_sec":started.elapsed().as_secs_f64(),"error":Value::Null});
    fs::write(
        out.join("summary.json"),
        serde_json::to_string_pretty(&summary)? + "\n",
    )?;
    fs::write(
        out.join("status.json"),
        serde_json::to_string_pretty(
            &json!({"stage":"complete","exact_evaluations":tracker.count}),
        )? + "\n",
    )?;
    if std::env::var("EGG_A2_QUIET").as_deref() != Ok("1") {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    }
    Ok(summary)
}

fn value_str<'a>(row: &'a Value, key: &str) -> Result<&'a str> {
    row[key]
        .as_str()
        .with_context(|| format!("missing string field {key}"))
}

fn read_p1_by_candidate(path: &Path) -> Result<HashMap<String, Value>> {
    let rows: Vec<Value> = serde_json::from_slice(&fs::read(path)?)?;
    rows.into_iter()
        .map(|row| {
            let input = Path::new(value_str(&row, "input")?);
            let candidate_id = input
                .file_stem()
                .and_then(|value| value.to_str())
                .context("P1 input has no UTF-8 file stem")?
                .to_owned();
            Ok((candidate_id, row))
        })
        .collect()
}

fn p1_order(p1: &HashMap<String, Value>, left: &str, right: &str) -> std::cmp::Ordering {
    p1[left]["score"]
        .as_f64()
        .unwrap_or(f64::INFINITY)
        .total_cmp(&p1[right]["score"].as_f64().unwrap_or(f64::INFINITY))
        .then_with(|| left.cmp(right))
}

fn p1_point(p1: &HashMap<String, Value>, candidate_id: &str) -> Result<PpaPoint> {
    Ok(PpaPoint {
        delay: p1[candidate_id]["delay"]
            .as_f64()
            .with_context(|| format!("{candidate_id} missing P1 delay"))?,
        area: p1[candidate_id]["area"]
            .as_f64()
            .with_context(|| format!("{candidate_id} missing P1 area"))?,
        power: p1[candidate_id]["power"]
            .as_f64()
            .with_context(|| format!("{candidate_id} missing P1 power"))?,
    })
}

fn v8_pro_observation<'a>(
    candidate_id: &'a str,
    plan_by_id: &HashMap<&'a str, &'a Value>,
    score: f64,
) -> Result<CandidateObservation<'a>> {
    let row = plan_by_id
        .get(candidate_id)
        .with_context(|| format!("V8-Pro observation missing plan row {candidate_id}"))?;
    let source_class = row["source_class"].as_str().unwrap_or("");
    Ok(CandidateObservation {
        candidate_id,
        topology_signature: row["topology_signature"].as_str().unwrap_or(""),
        region: row["specific_window"]
            .as_str()
            .or_else(|| row["region_id"].as_str())
            .unwrap_or(""),
        provenance_class: row["provenance_class"].as_str().unwrap_or(source_class),
        source_class,
        p1_score: score,
        legal: true,
    })
}

fn v8_objective_order(
    objective: &SearchObjective,
    p1: &HashMap<String, Value>,
    left: &str,
    right: &str,
) -> std::cmp::Ordering {
    if let Some(context) = objective.programmable_context() {
        let left_point = p1_point(p1, left).expect("validated P1 point");
        let right_point = p1_point(p1, right).expect("validated P1 point");
        context
            .compare(left_point, right_point)
            .expect("validated programmable comparison")
            .then_with(|| left.cmp(right))
    } else {
        p1_order(p1, left, right)
    }
}

fn materially_less(left: f64, right: f64) -> bool {
    left < right - 1e-12 * right.abs().max(1.0)
}

fn ppa_dominates(left: PpaPoint, right: PpaPoint) -> bool {
    let no_worse = left.delay <= right.delay + 1e-12 * right.delay.abs().max(1.0)
        && left.area <= right.area + 1e-12 * right.area.abs().max(1.0)
        && left.power <= right.power + 1e-12 * right.power.abs().max(1.0);
    no_worse
        && (materially_less(left.delay, right.delay)
            || materially_less(left.area, right.area)
            || materially_less(left.power, right.power))
}

fn planner_partition_key(row: &Value) -> String {
    for field in ["specific_window", "region_id", "candidate_id"] {
        if let Some(value) = row[field].as_str() {
            if let Some(seed) = value.split('_').find(|token| {
                token.strip_prefix('S').is_some_and(|digits| {
                    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
                })
            }) {
                return seed.to_owned();
            }
        }
    }
    "GLOBAL".to_owned()
}

/// Native frozen `union_v1` portfolio plus the opt-in V8 Pareto admission
/// experiment.  Both consume already-emitted provenance fields instead of
/// recomputing or reinterpreting structural scores; V8 adds only
/// ObjectiveSpec/constraint/PPA views.
fn build_union_portfolio_native(
    plan: &[Value],
    p1: &HashMap<String, Value>,
    limit: usize,
    objective: &SearchObjective,
    extra_reserved: &[(String, String)],
) -> Result<(Vec<String>, Value)> {
    anyhow::ensure!(limit >= 20, "union portfolio needs at least 20 slots");
    validate_extraction_portfolio(plan).map_err(anyhow::Error::msg)?;
    let mut by_id = HashMap::new();
    for row in plan {
        let extraction = extraction_candidate(row).map_err(anyhow::Error::msg)?;
        let id = extraction.candidate_id.to_owned();
        anyhow::ensure!(by_id.insert(id.clone(), row).is_none(), "duplicate {id}");
        if let Some(p1_row) = p1.get(&id) {
            let p1_input = Path::new(value_str(p1_row, "input")?);
            anyhow::ensure!(
                p1_input == extraction.implementation,
                "candidate {id} implementation differs between unified extraction ({}) and P1 ({})",
                extraction.implementation.display(),
                p1_input.display()
            );
        }
    }
    anyhow::ensure!(
        by_id.len() == p1.len() && by_id.keys().all(|id| p1.contains_key(id)),
        "union plan and P1 candidate sets differ"
    );
    let mut selected: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut add = |candidate_id: String, view: &str| {
        selected
            .entry(candidate_id)
            .or_default()
            .insert(view.to_owned());
    };
    let v8_pareto = std::env::var("EGG_OBJECTIVE_ENGINE").as_deref() == Ok("v8-pareto");
    let mut d1_ids: Vec<_> = plan
        .iter()
        .filter(|row| row["source_class"].as_str() == Some("d1-rewrite"))
        .map(|row| {
            extraction_candidate_id(row)
                .map(str::to_owned)
                .map_err(anyhow::Error::msg)
        })
        .collect::<Result<_>>()?;
    let generator_rows: Vec<_> = plan
        .iter()
        .filter(|row| {
            row["source_class"]
                .as_str()
                .is_some_and(|source| source.starts_with("generator-v2"))
        })
        .collect();
    d1_ids.sort_by(|left, right| p1_order(p1, left, right));
    for id in d1_ids.iter().take(4) {
        add(id.clone(), "d1-top4-anchor");
    }

    let mut all_ids: Vec<_> = p1.keys().cloned().collect();
    all_ids.sort_by(|left, right| {
        if v8_pareto {
            v8_objective_order(objective, p1, left, right)
        } else {
            p1_order(p1, left, right)
        }
    });

    // Experimental divide-and-conquer opportunity gate.  Existing planner
    // seed regions are used as already-proven boundary partitions and each
    // receives an independent share of the fixed Top-K.  This changes no
    // default trajectory: it is reachable only in an explicitly isolated
    // experiment.  A positive result justifies building a true timing-aware
    // graph partitioner; a negative result avoids that larger implementation.
    let partition_local = std::env::var("EGG_PARTITION_LOCAL_PORTFOLIO")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if partition_local {
        let mut partitions: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for id in &all_ids {
            partitions
                .entry(planner_partition_key(by_id[id]))
                .or_default()
                .push(id.clone());
        }
        for ids in partitions.values_mut() {
            ids.sort_by(|left, right| p1_order(p1, left, right));
        }
        let mut chosen = Vec::new();
        let mut depth = 0usize;
        while chosen.len() < limit {
            let mut added = false;
            for ids in partitions.values() {
                if let Some(id) = ids.get(depth) {
                    chosen.push(id.clone());
                    added = true;
                    if chosen.len() == limit {
                        break;
                    }
                }
            }
            if !added {
                break;
            }
            depth += 1;
        }
        chosen.sort_by(|left, right| p1_order(p1, left, right));
        let selected_audit: Vec<_> = chosen
            .iter()
            .enumerate()
            .map(|(rank, id)| {
                let row = by_id[id];
                json!({
                    "portfolio_rank":rank+1,
                    "candidate_id":id,
                    "partition":planner_partition_key(row),
                    "source_class":row["source_class"],
                    "mapping":row["mapping"],
                    "views":["partition-local-round-robin"],
                    "p1_score":p1[id]["score"],
                    "p1_delay":p1[id]["delay"],
                    "topology_signature":row["topology_signature"],
                })
            })
            .collect();
        return Ok((
            chosen,
            json!({
                "policy":"partition_local_v0: equal planner-seed-region Top-K allocation",
                "limit":limit,
                "partition_count":partitions.len(),
                "partition_sizes":partitions.iter().map(|(key, ids)| (key.clone(), ids.len())).collect::<BTreeMap<_,_>>(),
                "selected":selected_audit,
            }),
        ));
    }

    if v8_pareto {
        for id in all_ids.iter().take(8) {
            add(id.clone(), "v8-primary-objective");
        }

        if let Some(context) = objective.programmable_context() {
            if !context.spec().constraints.is_empty() {
                let mut repair_ids: Vec<_> = all_ids
                    .iter()
                    .filter(|id| {
                        !context
                            .evaluate(p1_point(p1, id).expect("validated P1 point"))
                            .expect("validated programmable evaluation")
                            .feasible
                    })
                    .cloned()
                    .collect();
                repair_ids.sort_by(|left, right| {
                    let left_eval = context
                        .evaluate(p1_point(p1, left).expect("validated P1 point"))
                        .expect("validated programmable evaluation");
                    let right_eval = context
                        .evaluate(p1_point(p1, right).expect("validated P1 point"))
                        .expect("validated programmable evaluation");
                    left_eval
                        .total_relative_violation
                        .total_cmp(&right_eval.total_relative_violation)
                        .then_with(|| {
                            left_eval
                                .max_relative_violation
                                .total_cmp(&right_eval.max_relative_violation)
                        })
                        .then_with(|| v8_objective_order(objective, p1, left, right))
                });
                for id in repair_ids.into_iter().take(6) {
                    add(id, "v8-constraint-repair");
                }
            }
        }

        let points: HashMap<_, _> = all_ids
            .iter()
            .map(|id| (id.clone(), p1_point(p1, id).expect("validated P1 point")))
            .collect();
        let pareto_ids: Vec<_> = all_ids
            .iter()
            .filter(|id| {
                !all_ids.iter().any(|other| {
                    other.as_str() != id.as_str()
                        && ppa_dominates(points[other.as_str()], points[id.as_str()])
                })
            })
            .cloned()
            .collect();
        for (metric, order) in [("delay", 0usize), ("area", 1usize), ("power", 2usize)] {
            let mut ranked = pareto_ids.clone();
            ranked.sort_by(|left_id, right_id| {
                let left = points[left_id.as_str()];
                let right = points[right_id.as_str()];
                let ordering = match order {
                    0 => left.delay.total_cmp(&right.delay),
                    1 => left.area.total_cmp(&right.area),
                    _ => left.power.total_cmp(&right.power),
                };
                ordering.then_with(|| v8_objective_order(objective, p1, left_id, right_id))
            });
            for id in ranked.into_iter().take(2) {
                add(id, &format!("v8-pareto-{metric}"));
            }
        }
    }

    let preferred_mapping = objective
        .programmable_context()
        .and_then(|context| context.relative_gradient(context.reference()).ok())
        .map(|weights| {
            if weights.area >= weights.delay && weights.area >= weights.power {
                "area"
            } else if weights.delay >= weights.power {
                "timing"
            } else {
                "balanced"
            }
        })
        .unwrap_or("timing");
    if !v8_pareto {
        let mut timing_best: BTreeMap<String, &Value> = BTreeMap::new();
        for row in &generator_rows {
            if row["mapping"].as_str() != Some(preferred_mapping) {
                continue;
            }
            let signature = value_str(row, "topology_signature")?.to_owned();
            let id = value_str(row, "candidate_id")?;
            let replace = timing_best.get(&signature).is_none_or(|current| {
                let current_id = current["candidate_id"].as_str().unwrap();
                (
                    p1[id]["delay"].as_f64().unwrap(),
                    p1[id]["score"].as_f64().unwrap(),
                    id,
                ) < (
                    p1[current_id]["delay"].as_f64().unwrap(),
                    p1[current_id]["score"].as_f64().unwrap(),
                    current_id,
                )
            });
            if replace {
                timing_best.insert(signature, row);
            }
        }
        let mut timing_rows: Vec<_> = timing_best.into_values().collect();
        timing_rows.sort_by(|left, right| {
            let left_id = left["candidate_id"].as_str().unwrap();
            let right_id = right["candidate_id"].as_str().unwrap();
            p1[left_id]["delay"]
                .as_f64()
                .unwrap()
                .total_cmp(&p1[right_id]["delay"].as_f64().unwrap())
                .then_with(|| p1_order(p1, left_id, right_id))
        });
        for row in timing_rows.into_iter().take(8) {
            add(
                value_str(row, "candidate_id")?.to_owned(),
                "timing-topology",
            );
        }
    }

    let mut strata: BTreeMap<(String, String), Vec<&Value>> = BTreeMap::new();
    for row in &generator_rows {
        strata
            .entry((
                value_str(row, "provenance_class")?.to_owned(),
                value_str(row, "specific_window")?.to_owned(),
            ))
            .or_default()
            .push(row);
    }
    let mut by_provenance: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for ((provenance, _), rows) in strata {
        let best = rows
            .into_iter()
            .min_by(|left, right| {
                p1_order(
                    p1,
                    left["candidate_id"].as_str().unwrap(),
                    right["candidate_id"].as_str().unwrap(),
                )
            })
            .context("empty provenance stratum")?;
        by_provenance.entry(provenance).or_default().push(best);
    }
    for rows in by_provenance.values_mut() {
        rows.sort_by(|left, right| {
            p1_order(
                p1,
                left["candidate_id"].as_str().unwrap(),
                right["candidate_id"].as_str().unwrap(),
            )
        });
    }
    let mut provenance_picks = Vec::new();
    let mut depth = 0usize;
    while provenance_picks.len() < 8 {
        let mut added = false;
        for rows in by_provenance.values() {
            if let Some(row) = rows.get(depth) {
                provenance_picks.push(*row);
                added = true;
                if provenance_picks.len() == 8 {
                    break;
                }
            }
        }
        if !added {
            break;
        }
        depth += 1;
    }
    for row in provenance_picks {
        add(
            value_str(row, "candidate_id")?.to_owned(),
            "generator-provenance",
        );
    }
    for (id, view) in extra_reserved {
        anyhow::ensure!(
            by_id.contains_key(id),
            "reserved candidate {id} missing from plan"
        );
        add(id.clone(), view);
    }
    drop(add);

    for id in all_ids {
        if selected.len() >= limit {
            break;
        }
        selected
            .entry(id)
            .or_default()
            .insert("global-p1-fill".to_owned());
    }
    let mut chosen: Vec<_> = selected.keys().cloned().collect();
    chosen.sort_by(|left, right| p1_order(p1, left, right));
    if chosen.len() > limit {
        let (mut reserved, filler): (Vec<_>, Vec<_>) = chosen
            .into_iter()
            .partition(|id| selected[id].iter().any(|view| view != "global-p1-fill"));
        reserved.extend(
            filler
                .into_iter()
                .take(limit.saturating_sub(reserved.len())),
        );
        chosen = reserved;
        chosen.sort_by(|left, right| p1_order(p1, left, right));
    }
    let selected_audit: Vec<_> = chosen
        .iter()
        .enumerate()
        .map(|(rank, id)| {
            let row = by_id[id];
            let extraction = extraction_candidate(row).expect("validated extraction candidate");
            json!({
                "portfolio_rank":rank+1,
                "candidate_id":extraction.candidate_id,
                "source_class":row["source_class"],
                "mapping":row["mapping"],
                "views":selected[id],
                "p1_score":p1[id]["score"],
                "p1_delay":p1[id]["delay"],
                "topology_signature":row["topology_signature"],
                "equivalence_source":extraction.equivalence
                    .map(|record|record["source"].clone()).unwrap_or(Value::Null),
                "equivalence_target":extraction.equivalence
                    .map(|record|record["target"]["kind"].clone()).unwrap_or(Value::Null),
            })
        })
        .collect();
    Ok((
        chosen,
        json!({
            "policy":if v8_pareto {
                "objective_v8: D1 Top-4 + objective Top-8 + repair Top-6 + D/A/P Pareto extremes + eight provenance representatives + objective fill"
            } else if matches!(objective, SearchObjective::D2ap) {
                "union_v1: normal D1-reorder Top-4 anchor + eight topology-distinct timing realizations + eight provenance/window representatives + global P1 fill"
            } else {
                "objective_programmable_v1: D1 Top-4 anchor + eight topology-distinct objective-preferred realizations + eight provenance/window representatives + objective P1 fill"
            },
            "preferred_generator_mapping":if v8_pareto {"pareto"} else {preferred_mapping},
            "source_counts":{"d1_rewrite":d1_ids.len(),"generator_v2":generator_rows.len()},
            "selected":selected_audit,
        }),
    ))
}

fn native_stage(
    candidate_ids: &[String],
    p1: &HashMap<String, Value>,
    out: &Path,
    budget: usize,
    resume: Option<&Path>,
    branch_from_checkpoint: bool,
    jobs: usize,
    database: &CellDatabase,
    lib: &Path,
    rules: &Path,
    objective: &SearchObjective,
) -> Result<NativeStage> {
    let started = Instant::now();
    let iterative_cache_hits_before = iterative_exact_cache().hits.load(Ordering::Relaxed);
    let iterative_cache_misses_before = iterative_exact_cache().misses.load(Ordering::Relaxed);
    fs::create_dir_all(out)?;
    let mut logical = Vec::new();
    let mut execution = Vec::new();
    let mut representatives: BTreeMap<(String, Option<String>), String> = BTreeMap::new();
    let mut aliases = BTreeMap::new();
    for id in candidate_ids {
        let input = PathBuf::from(value_str(&p1[id], "input")?);
        let checkpoint = resume.map(|root| root.join(id).join("checkpoint.json"));
        if let Some(path) = &checkpoint {
            anyhow::ensure!(path.exists(), "missing A2 checkpoint {}", path.display());
        }
        let task = BatchTask {
            input_netlist: input.clone(),
            out_dir: out.join(id),
            mode: "inner".to_owned(),
            resume_checkpoint: checkpoint.clone(),
        };
        logical.push(task.clone());
        let key = (sha(&input)?, checkpoint.as_deref().map(sha).transpose()?);
        if let Some(representative) = representatives.get(&key) {
            aliases.insert(id.clone(), representative.clone());
        } else {
            representatives.insert(key, id.clone());
            execution.push((id.clone(), task));
        }
    }
    fs::write(
        out.join("batch_plan.json"),
        serde_json::to_string_pretty(&logical)? + "\n",
    )?;
    fs::write(
        out.join("batch_execution_plan.json"),
        serde_json::to_string_pretty(&execution.iter().map(|(_, task)| task).collect::<Vec<_>>())?
            + "\n",
    )?;
    let results = parallel_map_ordered(&execution, jobs, |_, (_, task)| {
        let task_started = Instant::now();
        let circuit = Circuit::parse_with_database_objective(
            &task.input_netlist,
            database,
            objective.clone(),
        )?;
        execute(
            circuit,
            &task.input_netlist,
            &task.out_dir,
            &task.mode,
            lib,
            rules,
            task_started,
            task.resume_checkpoint.as_deref(),
            Some(budget),
            branch_from_checkpoint,
        )
    })?;
    let mut summaries: HashMap<String, Value> = execution
        .iter()
        .map(|(id, _)| id.clone())
        .zip(results)
        .collect();
    for (id, representative) in &aliases {
        let source = out.join(representative);
        let target = out.join(id);
        fs::create_dir_all(&target)?;
        for name in [
            "best.v",
            "checkpoint.json",
            "status.json",
            "trajectory.json",
        ] {
            fs::copy(source.join(name), target.join(name))?;
        }
        let mut summary = summaries[representative].clone();
        summary["input_netlist"] = p1[id]["input"].clone();
        summary["physical_execution_alias_of"] = Value::String(representative.clone());
        fs::write(
            target.join("summary.json"),
            serde_json::to_string_pretty(&summary)? + "\n",
        )?;
        summaries.insert(id.clone(), summary);
    }
    let mut ranking = candidate_ids.to_vec();
    ranking.sort_by(|left, right| {
        summaries[left]["best"]["score"]
            .as_f64()
            .unwrap()
            .total_cmp(&summaries[right]["best"]["score"].as_f64().unwrap())
            .then_with(|| left.cmp(right))
    });
    let paid_exact: usize = candidate_ids
        .iter()
        .map(|id| summaries[id]["new_exact_evaluations"].as_u64().unwrap() as usize)
        .sum();
    let physical_paid_exact: usize = execution
        .iter()
        .map(|(id, _)| summaries[id]["new_exact_evaluations"].as_u64().unwrap() as usize)
        .sum();
    let score_map: serde_json::Map<String, Value> = ranking
        .iter()
        .map(|id| (id.clone(), summaries[id]["best"]["score"].clone()))
        .collect();
    let summary = json!({
        "budget_per_candidate":budget,
        "candidate_count":candidate_ids.len(),
        "physically_executed_candidate_count":execution.len(),
        "physical_execution_cache_hits":aliases.len(),
        "physical_execution_aliases":aliases,
        "iterative_exact_state_cache_hits":iterative_exact_cache().hits.load(Ordering::Relaxed)-iterative_cache_hits_before,
        "iterative_exact_state_cache_misses":iterative_exact_cache().misses.load(Ordering::Relaxed)-iterative_cache_misses_before,
        "paid_exact":paid_exact,
        "physical_paid_exact":physical_paid_exact,
        "ranking":ranking,
        "scores":score_map,
        "elapsed_sec":started.elapsed().as_secs_f64(),
    });
    Ok(NativeStage {
        summary,
        ranking,
        summaries,
    })
}

fn early_commitment_total_budget(
    portfolio_count: usize,
    survivor_count: usize,
    stage25_budget: usize,
    stage50_budget: usize,
    stage500_budget: usize,
    followup_budget: Option<usize>,
) -> Result<usize> {
    anyhow::ensure!(portfolio_count > 0, "empty early-commitment portfolio");
    anyhow::ensure!(survivor_count > 0 && survivor_count <= portfolio_count);
    anyhow::ensure!(stage25_budget <= stage50_budget && stage50_budget <= stage500_budget);
    let final_budget = followup_budget.unwrap_or(stage500_budget);
    anyhow::ensure!(final_budget >= stage500_budget);
    Ok(portfolio_count * stage25_budget
        + survivor_count * (stage50_budget - stage25_budget)
        + (stage500_budget - stage50_budget)
        + (final_budget - stage500_budget))
}

fn progressive_native_impl(
    args: &[String],
    preloaded_database: Option<&CellDatabase>,
    preloaded_database_sec: f64,
    explicit_objective: Option<SearchObjective>,
) -> Result<()> {
    anyhow::ensure!(
        args.len() == 8,
        "usage: run_frozen_a2_fast progressive PLAN P1 OUT LIB SCALE_RULES D1_SCORE JOBS"
    );
    let started = Instant::now();
    let plan_path = PathBuf::from(&args[1]);
    let p1_path = PathBuf::from(&args[2]);
    let out = PathBuf::from(&args[3]);
    let lib = PathBuf::from(&args[4]);
    let rules = PathBuf::from(&args[5]);
    let d1_score: f64 = args[6].parse()?;
    let jobs: usize = args[7].parse::<usize>()?.max(1);
    let objective = explicit_objective.unwrap_or(SearchObjective::from_env()?);
    let ultra_config_source = V8UltraConfig::from_env()?;
    let ultra_config = ultra_config_source.as_ref().map(|(_, config)| config);
    fs::create_dir_all(&out)?;
    let plan: Vec<Value> = serde_json::from_slice(&fs::read(&plan_path)?)?;
    let p1 = read_p1_by_candidate(&p1_path)?;
    let pro_config_path = std::env::var_os("EGG_V8_PRO_CONFIG").map(PathBuf::from);
    let pro_config: Option<V8ProPolicyConfig> = pro_config_path
        .as_ref()
        .map(|path| -> Result<_> {
            serde_json::from_slice(&fs::read(path)?)
                .with_context(|| format!("invalid V8-Pro config {}", path.display()))
        })
        .transpose()?;
    let plan_by_id: HashMap<&str, &Value> = plan
        .iter()
        .map(|row| {
            Ok((
                extraction_candidate_id(row).map_err(anyhow::Error::msg)?,
                row,
            ))
        })
        .collect::<Result<_>>()?;
    let pool_observations: Vec<_> = p1
        .keys()
        .map(|id| {
            v8_pro_observation(
                id,
                &plan_by_id,
                p1[id]["score"].as_f64().unwrap_or(f64::INFINITY),
            )
        })
        .collect::<Result<_>>()?;
    let pool_stats = summarize_candidates(&pool_observations);
    let partition_pareto_closure = std::env::var("EGG_PARTITION_PARETO_CLOSURE")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let mut portfolio_limit = pro_config
        .as_ref()
        .map(|config| choose_portfolio_limit(&pool_stats, &config.progressive))
        .unwrap_or(32);
    let mut extra_reserved: Vec<_> = pro_config
        .as_ref()
        .map(|config| {
            choose_generator_region_representatives(
                &pool_observations,
                config.progressive.generator_region_representatives,
            )
            .into_iter()
            .map(|id| (id, "v8-pro-generator-region".to_owned()))
            .collect()
        })
        .unwrap_or_default();
    if partition_pareto_closure {
        // Reconstruct the unmodified Top-32 from the non-closure pool first,
        // then take its exact set union with the experimental closures.  A
        // plain cap increase would also admit unrelated rank-33+ candidates
        // and make any observed gain impossible to attribute to partition
        // composition.
        let baseline_plan: Vec<_> = plan
            .iter()
            .filter(|row| {
                !row["candidate_id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("ITERATIVECLOSE_"))
            })
            .cloned()
            .collect();
        let baseline_p1: HashMap<_, _> = p1
            .iter()
            .filter(|(id, _)| !id.starts_with("ITERATIVECLOSE_"))
            .map(|(id, row)| (id.clone(), row.clone()))
            .collect();
        let (baseline_portfolio, _) = build_union_portfolio_native(
            &baseline_plan,
            &baseline_p1,
            portfolio_limit,
            &objective,
            &extra_reserved,
        )?;
        extra_reserved.extend(
            baseline_portfolio
                .into_iter()
                .map(|id| (id, "partition-pareto-baseline".to_owned())),
        );
        extra_reserved.extend(
            plan.iter()
                .filter_map(|row| row["candidate_id"].as_str())
                .filter(|id| id.starts_with("ITERATIVECLOSE_"))
                .map(|id| (id.to_owned(), "partition-pareto-closure".to_owned())),
        );
        portfolio_limit = extra_reserved
            .iter()
            .map(|(id, _)| id)
            .collect::<BTreeSet<_>>()
            .len();
    }
    let (mut portfolio, portfolio_audit) =
        build_union_portfolio_native(&plan, &p1, portfolio_limit, &objective, &extra_reserved)?;
    anyhow::ensure!(!portfolio.is_empty() && portfolio.len() <= portfolio_limit);
    let potential_config = V8PotentialConfig::from_env().map_err(anyhow::Error::msg)?;
    let mut potential_preview_pool = portfolio.clone();
    if let Some(config) = &potential_config {
        let mut remaining: Vec<_> = p1
            .keys()
            .filter(|id| !potential_preview_pool.contains(id))
            .cloned()
            .collect();
        remaining.sort_by(|left, right| p1_order(&p1, left, right));
        potential_preview_pool.extend(
            remaining
                .into_iter()
                .take(config.preview_pool_limit.saturating_sub(portfolio.len())),
        );
    }
    let selected_observations: Vec<_> = portfolio
        .iter()
        .map(|id| {
            v8_pro_observation(
                id,
                &plan_by_id,
                p1[id]["score"].as_f64().unwrap_or(f64::INFINITY),
            )
        })
        .collect::<Result<_>>()?;
    let pro_safety = pro_config.as_ref().map(|config| {
        evaluate_portfolio_safety(
            &pool_observations,
            &selected_observations,
            true,
            &config.safety,
        )
    });
    if let Some(decision) = &pro_safety {
        fs::write(
            out.join("v8_pro_policy_receipt.json"),
            serde_json::to_string_pretty(&json!({
                "method":"V8-Pro adaptive portfolio pre-Exact gate",
                "config_path":pro_config_path,
                "rule_tier":std::env::var("EGG_V8_PRO_RULE_TIER").ok(),
                "portfolio_limit":portfolio_limit,
                "decision":decision,
                "fallback_required":!decision.safe,
            }))? + "\n",
        )?;
        anyhow::ensure!(
            decision.safe,
            "V8-Pro portfolio safety gate requested rule-tier fallback: {}",
            decision.reasons.join(",")
        );
    }
    if std::env::var("EGG_V8_PHASE_ABLATION").as_deref() == Ok("phase1-only") {
        // Phase I has already performed the common full-circuit P1 evaluation
        // and Top-K admission.  Select the best feasible admitted implementation
        // without invoking any A2 sizing stage.  The selected implementation
        // may contain drive variants introduced together with a structural
        // rewrite; sizing-only expansion remains absent, matching Phase I.
        portfolio.sort_by(|left, right| v8_objective_order(&objective, &p1, left, right));
        let leader = portfolio
            .iter()
            .find(|id| {
                p1_point(&p1, id)
                    .is_ok_and(|point| objective.feasible_ppa(point.delay, point.area, point.power))
            })
            .context("Phase-I-only portfolio has no feasible candidate")?
            .clone();
        let p1_selected_point = p1_point(&p1, &leader)?;
        let source = PathBuf::from(value_str(&p1[&leader], "input")?);
        let best_dir = out.join("phase1_best").join(&leader);
        fs::create_dir_all(&best_dir)?;
        let best_path = best_dir.join("best.v");
        fs::copy(&source, &best_path)?;
        let database = preloaded_database.context("Phase-I-only shared database missing")?;
        let point = evaluate_written_netlist_with_database_objective(
            &best_path,
            database,
            objective.clone(),
        )?;
        let score = objective.score(point.delay, point.area, point.power);
        let plan_row = plan_by_id[leader.as_str()];
        let transit_pool = vec![json!({
            "candidate_id":leader,
            "source_class":plan_row["source_class"],
            "topology_signature":plan_row["topology_signature"],
            "best_path":best_path,
            "best_sha256":sha(&best_path)?,
            "exact_stage_budget":0,
            "exact_evaluations":0,
            "point":point,
            "search_score":score,
            "exact_objective_value":objective.exact_value(point)?,
            "feasible":true,
            "minimum_relative_constraint_slack":objective.minimum_relative_constraint_slack(point)?,
        })];
        let summary = json!({
            "method":"V8 Phase-I-only common extraction and full-circuit evaluation",
            "phase_ablation_mode":"phase1-only",
            "objective":objective.name(),
            "delay_cap_ps":objective.delay_cap_ps(),
            "final_feasible":true,
            "portfolio":portfolio_audit,
            "schedule":"Phase I Top-K -> best feasible P1; A2 disabled",
            "stages":[],
            "transit_pool":transit_pool,
            "leader":leader,
            "final_best_path":best_path,
            "final_score":score,
            "final_reparse_exact":{
                "delay":point.delay,"area":point.area,"power":point.power,"score":score,
            },
            "final_reparse_wall_sec":0.0,
            "selected_p1_point":p1_selected_point,
            "d1_score":d1_score,
            "ratio_to_d1":score/d1_score,
            "p1_exact":plan.len(),
            "a2_exact":0,
            "physical_a2_exact":0,
            "total_exact":plan.len(),
            "physical_total_exact":plan.len(),
            "a2_batch_jobs":jobs,
            "shared_database_load_sec":preloaded_database_sec,
            "elapsed_sec":started.elapsed().as_secs_f64(),
        });
        fs::write(
            out.join("summary.json"),
            serde_json::to_string_pretty(&summary)? + "\n",
        )?;
        return Ok(());
    }
    let inputs: Vec<_> = potential_preview_pool
        .iter()
        .map(|id| value_str(&p1[id], "input").map(PathBuf::from))
        .collect::<Result<_>>()?;
    let database_started = Instant::now();
    let owned_database;
    let database = if let Some(database) = preloaded_database {
        database
    } else {
        owned_database = CellDatabase::load(&inputs, &lib, &rules)?;
        &owned_database
    };
    // Report the actual one-time load even when the controller moved it out
    // of this stage's critical path.
    let database_sec = if preloaded_database.is_some() {
        preloaded_database_sec
    } else {
        database_started.elapsed().as_secs_f64()
    };
    let stage25_budget = pro_config
        .as_ref()
        .map(|config| config.progressive.stage25_budget)
        .unwrap_or(25);
    let stage50_budget = pro_config
        .as_ref()
        .map(|config| config.progressive.stage50_budget)
        .unwrap_or(50);
    let stage500_budget = pro_config
        .as_ref()
        .map(|config| config.progressive.stage500_budget)
        .unwrap_or(500);
    if std::env::var("EGG_V8_PHASE_ABLATION").as_deref() == Ok("early-commitment") {
        anyhow::ensure!(
            potential_config.is_none() && pro_config.is_none(),
            "early commitment currently requires the frozen non-Pro progressive schedule"
        );
        let original_portfolio = portfolio.clone();
        portfolio.sort_by(|left, right| v8_objective_order(&objective, &p1, left, right));
        let leader = portfolio
            .iter()
            .find(|id| {
                p1_point(&p1, id)
                    .is_ok_and(|point| objective.feasible_ppa(point.delay, point.area, point.power))
            })
            .context("early-commitment portfolio has no feasible pre-sizing candidate")?
            .clone();
        let selected_input = PathBuf::from(value_str(&p1[&leader], "input")?);
        let selected_circuit =
            Circuit::parse_with_database_objective(&selected_input, database, objective.clone())?;
        let selected_instance_count = selected_circuit.instances.len();
        let followup_budget = ultra_config
            .filter(|config| config.leader_followup_enabled_for_instances(selected_instance_count))
            .map(|config| config.leader_followup_exact_budget)
            .filter(|budget| *budget > stage500_budget);
        let survivor_count = original_portfolio.len().min(8);
        let matched_budget = early_commitment_total_budget(
            original_portfolio.len(),
            survivor_count,
            stage25_budget,
            stage50_budget,
            stage500_budget,
            followup_budget,
        )?;
        let stage = native_stage(
            std::slice::from_ref(&leader),
            &p1,
            &out.join("stage_early_commitment"),
            matched_budget,
            None,
            false,
            jobs,
            database,
            &lib,
            &rules,
            &objective,
        )?;
        let best_path = out
            .join("stage_early_commitment")
            .join(&leader)
            .join("best.v");
        let reparsed =
            Circuit::parse_with_database_objective(&best_path, database, objective.clone())?;
        let reparsed_eval = reparsed.evaluate(&reparsed.original_choice(), None)?;
        let final_score = stage.summaries[&leader]["best"]["score"]
            .as_f64()
            .context("early-commitment final score")?;
        anyhow::ensure!(
            (reparsed_eval.score - final_score).abs()
                <= 2e-7_f64.max(64.0 * f64::EPSILON * final_score.abs()),
            "early-commitment write/reparse Exact mismatch"
        );
        let plan_row = plan_by_id[leader.as_str()];
        let point = PpaPoint {
            delay: reparsed_eval.delay,
            area: reparsed_eval.area,
            power: reparsed_eval.power,
        };
        let transit_pool = vec![json!({
            "candidate_id":leader,
            "source_class":plan_row["source_class"],
            "topology_signature":plan_row["topology_signature"],
            "best_path":best_path,
            "best_sha256":sha(&best_path)?,
            "exact_stage_budget":matched_budget,
            "exact_evaluations":stage.summaries[&leader]["exact_evaluations"],
            "point":point,
            "search_score":final_score,
            "exact_objective_value":objective.exact_value(point)?,
            "feasible":objective.feasible_ppa(point.delay, point.area, point.power),
            "minimum_relative_constraint_slack":objective.minimum_relative_constraint_slack(point)?,
        })];
        let a2_exact = stage.summary["paid_exact"].as_u64().unwrap() as usize;
        let physical_a2_exact = stage.summary["physical_paid_exact"].as_u64().unwrap() as usize;
        let summary = json!({
            "method":"V8 early commitment with matched total progressive Exact budget",
            "phase_ablation_mode":"early-commitment",
            "unified_equivalence_representation":unified_equivalence_enabled(),
            "objective":objective.name(),
            "delay_cap_ps":objective.delay_cap_ps(),
            "final_feasible":objective.feasible_ppa(point.delay, point.area, point.power),
            "portfolio":portfolio_audit,
            "pre_sizing_candidate_pool":original_portfolio,
            "pre_sizing_selected_candidate":leader,
            "pre_sizing_selected_point":p1_point(&p1, &leader)?,
            "selection_rule":"best feasible full-circuit P1 objective before any A2 sizing",
            "full_schedule_budget_model":{
                "portfolio_count":portfolio.len(),
                "survivor_count":survivor_count,
                "stage25_budget":stage25_budget,
                "stage50_budget":stage50_budget,
                "stage500_budget":stage500_budget,
                "leader_followup_budget":followup_budget,
                "matched_total_logical_a2_budget":matched_budget,
            },
            "schedule":format!("pre-sizing P1 leader -> 1x{} matched logical Exact", matched_budget),
            "stages":[stage.summary],
            "transit_pool":transit_pool,
            "leader":leader,
            "final_best_path":best_path,
            "final_score":final_score,
            "final_reparse_exact":ppa(&reparsed_eval),
            "final_reparse_wall_sec":0.0,
            "d1_score":d1_score,
            "ratio_to_d1":final_score/d1_score,
            "p1_exact":plan.len(),
            "a2_exact":a2_exact,
            "physical_a2_exact":physical_a2_exact,
            "total_exact":plan.len()+a2_exact,
            "physical_total_exact":plan.len()+physical_a2_exact,
            "a2_batch_jobs":jobs,
            "shared_database_load_sec":database_sec,
            "elapsed_sec":started.elapsed().as_secs_f64(),
        });
        fs::write(
            out.join("summary.json"),
            serde_json::to_string_pretty(&summary)? + "\n",
        )?;
        return Ok(());
    }
    let potential_preview = if let Some(config) = &potential_config {
        Some(native_stage(
            &potential_preview_pool,
            &p1,
            &out.join("potential_preview"),
            config.preview_exact_budget,
            None,
            false,
            jobs,
            database,
            &lib,
            &rules,
            &objective,
        )?)
    } else {
        None
    };
    let potential_promotion = potential_preview
        .as_ref()
        .map(|preview| {
            choose_potential_portfolio(
                &portfolio,
                &preview.ranking,
                potential_config
                    .as_ref()
                    .expect("preview requires V8-potential config")
                    .rescued_candidate_cap,
            )
            .map_err(anyhow::Error::msg)
        })
        .transpose()?;
    if let Some(decision) = &potential_promotion {
        portfolio = decision.selected.clone();
    }
    let potential_resume = potential_preview
        .as_ref()
        .map(|_| out.join("potential_preview"));
    let stage25 = native_stage(
        &portfolio,
        &p1,
        &out.join("stage_25"),
        stage25_budget,
        potential_resume.as_deref(),
        false,
        jobs,
        &database,
        &lib,
        &rules,
        &objective,
    )?;
    let promotion_decision = if let Some(config) = &pro_config {
        let ranked_observations: Vec<_> = stage25
            .ranking
            .iter()
            .map(|id| {
                v8_pro_observation(
                    id,
                    &plan_by_id,
                    stage25.summaries[id]["best"]["score"]
                        .as_f64()
                        .unwrap_or(f64::INFINITY),
                )
            })
            .collect::<Result<_>>()?;
        Some(choose_stage50_survivors(
            &ranked_observations,
            &config.progressive,
        ))
    } else {
        None
    };
    let survivors = promotion_decision
        .as_ref()
        .map(|decision| decision.survivors.clone())
        .unwrap_or_else(|| stage25.ranking.iter().take(8).cloned().collect());
    anyhow::ensure!(
        !survivors.is_empty(),
        "V8-Pro selected no Exact-50 survivors"
    );
    let stage50 = native_stage(
        &survivors,
        &p1,
        &out.join("stage_50"),
        stage50_budget,
        Some(&out.join("stage_25")),
        false,
        jobs,
        &database,
        &lib,
        &rules,
        &objective,
    )?;
    let leader = stage50.ranking[0].clone();
    let stage500 = native_stage(
        std::slice::from_ref(&leader),
        &p1,
        &out.join("stage_500"),
        stage500_budget,
        Some(&out.join("stage_50")),
        false,
        jobs,
        &database,
        &lib,
        &rules,
        &objective,
    )?;
    let leader_instance_count =
        stage500.summaries[&leader]["v8_ultra"]["sparse_a2"]["instance_count"]
            .as_u64()
            .unwrap_or(0) as usize;
    let stage_followup = if let Some(config) = ultra_config.filter(|config| {
        config.leader_followup_enabled_for_instances(leader_instance_count)
            && config.leader_followup_exact_budget > stage500_budget
    }) {
        Some(native_stage(
            std::slice::from_ref(&leader),
            &p1,
            &out.join("stage_leader_followup"),
            config.leader_followup_exact_budget,
            Some(&out.join("stage_500")),
            true,
            jobs,
            database,
            &lib,
            &rules,
            &objective,
        )?)
    } else {
        None
    };
    // Expose one already-paid Exact endpoint per Top-32 candidate.  The outer
    // V8 controller may use an objective-equivalent, topology-distinct point
    // as a bounded plateau transit parent; it must never launch extra A2 work
    // to construct that pool.
    let stage500_score = stage500.summaries[&leader]["best"]["score"]
        .as_f64()
        .context("stage-500 leader score")?;
    let followup_selected = stage_followup.as_ref().is_some_and(|followup| {
        followup.summaries[&leader]["best"]["score"]
            .as_f64()
            .is_some_and(|score| score < stage500_score - EPS)
    });
    let transit_pool: Vec<Value> = portfolio
        .iter()
        .map(|id| -> Result<Value> {
            let (stage_name, stage_budget, summary) = if id == &leader {
                if followup_selected {
                    let followup = stage_followup.as_ref().unwrap();
                    (
                        "stage_leader_followup",
                        ultra_config.unwrap().leader_followup_exact_budget,
                        &followup.summaries[id],
                    )
                } else {
                    ("stage_500", stage500_budget, &stage500.summaries[id])
                }
            } else if let Some(summary) = stage50.summaries.get(id) {
                ("stage_50", stage50_budget, summary)
            } else {
                ("stage_25", stage25_budget, &stage25.summaries[id])
            };
            let point = PpaPoint {
                delay: summary["best"]["delay"].as_f64().context("transit delay")?,
                area: summary["best"]["area"].as_f64().context("transit area")?,
                power: summary["best"]["power"].as_f64().context("transit power")?,
            };
            let best_path = out.join(stage_name).join(id).join("best.v");
            let plan_row = plan_by_id[id.as_str()];
            Ok(json!({
                "candidate_id":id,
                "source_class":plan_row["source_class"],
                "topology_signature":plan_row["topology_signature"],
                "best_path":best_path,
                "best_sha256":sha(&best_path)?,
                "exact_stage_budget":stage_budget,
                "exact_evaluations":summary["exact_evaluations"],
                "point":point,
                "search_score":summary["best"]["score"],
                "exact_objective_value":objective.exact_value(point)?,
                "feasible":objective.feasible_ppa(point.delay, point.area, point.power),
                "minimum_relative_constraint_slack":objective.minimum_relative_constraint_slack(point)?,
            }))
        })
        .collect::<Result<_>>()?;
    // Acceptance still consumes a written and reparsed mapped netlist.  The
    // shared database changes no physical formula; it only avoids parsing the
    // same Liberty tables in a fourth process.
    let final_reparse_started = Instant::now();
    let final_stage_name = if followup_selected {
        "stage_leader_followup"
    } else {
        "stage_500"
    };
    let final_stage = if followup_selected {
        stage_followup.as_ref().unwrap()
    } else {
        &stage500
    };
    let best_path = out.join(final_stage_name).join(&leader).join("best.v");
    let reparsed =
        Circuit::parse_with_database_objective(&best_path, &database, objective.clone())?;
    let reparsed_eval = reparsed.evaluate(&reparsed.original_choice(), None)?;
    let final_score = final_stage.summaries[&leader]["best"]["score"]
        .as_f64()
        .unwrap();
    anyhow::ensure!(
        (reparsed_eval.score - final_score).abs()
            <= 2e-7_f64.max(64.0 * f64::EPSILON * final_score.abs()),
        "native write/reparse Exact mismatch"
    );
    fs::write(
        out.join("final_reparse_exact.json"),
        serde_json::to_string_pretty(&ppa(&reparsed_eval))? + "\n",
    )?;
    let final_reparse_wall_sec = final_reparse_started.elapsed().as_secs_f64();
    let mut stages = Vec::new();
    if let Some(preview) = &potential_preview {
        stages.push(preview.summary.clone());
    }
    stages.extend([stage25.summary, stage50.summary, stage500.summary]);
    if let Some(followup) = &stage_followup {
        stages.push(followup.summary.clone());
    }
    let a2_exact: usize = stages
        .iter()
        .map(|stage| stage["paid_exact"].as_u64().unwrap() as usize)
        .sum();
    let physical_a2_exact: usize = stages
        .iter()
        .map(|stage| stage["physical_paid_exact"].as_u64().unwrap() as usize)
        .sum();
    let p1_exact = plan.len();
    let summary = json!({
        "method":if potential_config.is_some() {
            "V8-potential bounded sizing-headroom preview + union_v1 progressive refinement"
        } else {
            "union_v1 progressive frozen-A2 refinement (native Rust)"
        },
        "unified_equivalence_representation":unified_equivalence_enabled(),
        "objective":objective.name(),"delay_cap_ps":objective.delay_cap_ps(),
        "final_feasible":objective.feasible_ppa(
            reparsed_eval.delay,
            reparsed_eval.area,
            reparsed_eval.power,
        ),
        "portfolio":portfolio_audit,
        "schedule":if let Some(config) = &potential_config {
            format!("potential {}x{} -> {}x{} -> top{}x{} -> leaderx{}",
                potential_preview_pool.len(),config.preview_exact_budget,
                portfolio.len(),stage25_budget,survivors.len(),stage50_budget,stage500_budget)
        } else if pro_config.is_some() {
            format!("V8-Pro {}x{} -> top{}x{} -> leaderx{} (adaptive diversity-preserving promotion)",
                portfolio.len(),stage25_budget,survivors.len(),stage50_budget,stage500_budget)
        } else {
            "32x25 -> top8x50 -> leaderx500 (deterministic exact-cache resume)".to_owned()
        },
        "v8_pro_enabled":pro_config.is_some(),
        "v8_potential_enabled":potential_config.is_some(),
        "v8_potential_config":potential_config,
        "v8_potential_preview_pool":potential_preview_pool,
        "v8_potential_promotion":potential_promotion,
        "v8_pro_config_path":pro_config_path,
        "v8_pro_safety":pro_safety,
        "v8_pro_promotion":promotion_decision,
        "v8_ultra_leader_followup_attempted":stage_followup.is_some(),
        "v8_ultra_leader_followup_selected":followup_selected,
        "v8_ultra_leader_followup_enabled":final_stage_name=="stage_leader_followup",
        "v8_ultra_leader_followup_budget":ultra_config.map(|config|config.leader_followup_exact_budget),
        "v8_ultra_leader_followup_instance_count":leader_instance_count,
        "stages":stages,
        "transit_pool":transit_pool,
        "leader":leader,
        "final_best_path":best_path,
        "final_score":final_score,
        "final_reparse_exact":ppa(&reparsed_eval),
        "final_reparse_wall_sec":final_reparse_wall_sec,
        "d1_score":d1_score,
        "ratio_to_d1":final_score/d1_score,
        "p1_exact":p1_exact,
        "a2_exact":a2_exact,
        "physical_a2_exact":physical_a2_exact,
        "total_exact":p1_exact+a2_exact,
        "physical_total_exact":p1_exact+physical_a2_exact,
        "a2_batch_jobs":jobs,
        "shared_database_load_sec":database_sec,
        "elapsed_sec":started.elapsed().as_secs_f64(),
    });
    fs::write(
        out.join("summary.json"),
        serde_json::to_string_pretty(&summary)? + "\n",
    )?;
    if std::env::var("EGG_A2_QUIET").as_deref() != Ok("1") {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    }
    Ok(())
}

pub(crate) fn progressive_native_with_database(
    args: &[String],
    database: &CellDatabase,
    database_load_sec: f64,
) -> Result<()> {
    progressive_native_impl(args, Some(database), database_load_sec, None)
}

pub(crate) fn progressive_native_with_database_objective(
    args: &[String],
    database: &CellDatabase,
    database_load_sec: f64,
    objective: SearchObjective,
) -> Result<()> {
    progressive_native_impl(args, Some(database), database_load_sec, Some(objective))
}

/// One fixed-budget conditional-sizing stage for an objective-programmable
/// local rewrite closure.  This deliberately does not invoke the normal
/// 25->50->500 schedule: every admitted topology receives exactly `budget`
/// evaluations and the caller remains responsible for strict constrained
/// acceptance and cold rebuild.
pub(crate) fn fixed_budget_native_with_database_objective(
    plan_path: &Path,
    p1_path: &Path,
    out: &Path,
    lib: &Path,
    rules: &Path,
    budget: usize,
    jobs: usize,
    database: &CellDatabase,
    objective: SearchObjective,
) -> Result<Value> {
    anyhow::ensure!(budget > 0);
    fs::create_dir_all(out)?;
    let plan: Vec<Value> = serde_json::from_slice(&fs::read(plan_path)?)?;
    let p1 = read_p1_by_candidate(p1_path)?;
    let (portfolio, portfolio_audit) =
        build_union_portfolio_native(&plan, &p1, 32, &objective, &[])?;
    anyhow::ensure!(!portfolio.is_empty() && portfolio.len() <= 32);
    let stage = native_stage(
        &portfolio,
        &p1,
        out,
        budget,
        None,
        false,
        jobs.max(1),
        database,
        lib,
        rules,
        &objective,
    )?;
    let plan_by_id: HashMap<&str, &Value> = plan
        .iter()
        .map(|row| {
            Ok((
                extraction_candidate_id(row).map_err(anyhow::Error::msg)?,
                row,
            ))
        })
        .collect::<Result<_>>()?;
    let candidates = stage
        .ranking
        .iter()
        .map(|id| -> Result<Value> {
            let summary = &stage.summaries[id];
            let point = PpaPoint {
                delay: summary["best"]["delay"]
                    .as_f64()
                    .context("fixed-budget delay")?,
                area: summary["best"]["area"]
                    .as_f64()
                    .context("fixed-budget area")?,
                power: summary["best"]["power"]
                    .as_f64()
                    .context("fixed-budget power")?,
            };
            let best_path = out.join(id).join("best.v");
            let plan_row = plan_by_id[id.as_str()];
            Ok(json!({
                "candidate_id":id,
                "source_class":plan_row["source_class"],
                "topology_signature":plan_row["topology_signature"],
                "best_path":best_path,
                "best_sha256":sha(&best_path)?,
                "point":point,
                "search_score":summary["best"]["score"],
                "exact_objective_value":objective.exact_value(point)?,
                "feasible":objective.feasible_ppa(point.delay,point.area,point.power),
                "minimum_relative_constraint_slack":objective.minimum_relative_constraint_slack(point)?,
                "exact_evaluations":summary["exact_evaluations"],
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    let result = json!({
        "method":"fixed-budget conditional sizing for local rewrite closure",
        "budget_per_candidate":budget,
        "portfolio":portfolio_audit,
        "stage":stage.summary,
        "candidates":candidates,
    });
    fs::write(
        out.join("fixed_budget_summary.json"),
        serde_json::to_string_pretty(&result)? + "\n",
    )?;
    Ok(result)
}

pub fn progressive_native(args: &[String]) -> Result<()> {
    progressive_native_impl(args, None, 0.0, None)
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args
        .first()
        .is_some_and(|argument| argument == "progressive")
    {
        return progressive_native(&args);
    }
    let lib = PathBuf::from(
        std::env::var("EGG_LIB_PATH")
            .unwrap_or_else(|_| "test/asap7sc6t_SELECT_LVT_TT_nldm.lib".into()),
    );
    let rules = PathBuf::from(
        std::env::var("EGG_SCALE_RULES").unwrap_or_else(|_| "test/6t_scale_rules.json".into()),
    );
    if args
        .first()
        .is_some_and(|argument| argument == "profile-batch")
    {
        if args.len() != 3 {
            bail!("usage: run_frozen_a2_fast profile-batch MANIFEST.json OUTPUT.json")
        }
        let started = Instant::now();
        let tasks: Vec<ProfileTask> = serde_json::from_slice(&fs::read(&args[1])?)?;
        anyhow::ensure!(!tasks.is_empty(), "profile manifest is empty");
        let inputs: Vec<_> = tasks
            .iter()
            .map(|task| task.input_netlist.clone())
            .collect();
        let database = CellDatabase::load(&inputs, &lib, &rules)?;
        let database_sec = started.elapsed().as_secs_f64();
        let requested_jobs = std::env::var("EGG_PROFILE_BATCH_JOBS")
            .or_else(|_| std::env::var("EGG_A2_BATCH_JOBS"))
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .context("invalid EGG_PROFILE_BATCH_JOBS/EGG_A2_BATCH_JOBS")?
            .unwrap_or(1)
            .max(1);
        let jobs = requested_jobs.min(tasks.len());
        let export_boundary =
            std::env::var("EGG_PROFILE_BOUNDARY_RESPONSE").is_ok_and(|value| value == "1");
        let static_only = std::env::var("EGG_PROFILE_STATIC_ONLY").is_ok_and(|value| value == "1");
        let rows = parallel_map_ordered(&tasks, jobs, |_, task| {
            let task_started = Instant::now();
            let (point, response) = if export_boundary || static_only {
                let circuit = Circuit::parse_with_database_preparation(
                    &task.input_netlist,
                    &database,
                    SearchObjective::D2ap,
                    !static_only,
                )?;
                let evaluated = circuit.evaluate(&circuit.original_choice(), None)?;
                (
                    PpaPoint {
                        delay: evaluated.delay,
                        area: evaluated.area,
                        power: evaluated.power,
                    },
                    if export_boundary {
                        Some(circuit.profile_boundary_response(&evaluated)?)
                    } else {
                        None
                    },
                )
            } else {
                (
                    evaluate_written_netlist_with_database_objective(
                        &task.input_netlist,
                        &database,
                        SearchObjective::D2ap,
                    )?,
                    None,
                )
            };
            let mut row = json!({
                "candidate_id":task.candidate_id,
                "input_netlist":task.input_netlist,
                "delay_ps":point.delay,
                "area":point.area,
                "power":point.power,
                "d2ap":point.delay*point.delay*point.area*point.power,
                "elapsed_sec":task_started.elapsed().as_secs_f64(),
            });
            if let Some(response) = response {
                row["boundary_response"] = response;
            }
            Ok(row)
        })?;
        let output = json!({
            "schema":"egg-internal-nldm-v3-profile-batch-v1",
            "implementation":"shared-database in-process Rust whole-net profile",
            "task_count":rows.len(),
            "batch_jobs":jobs,
            "static_only":static_only,
            "shared_database_load_sec":database_sec,
            "elapsed_sec":started.elapsed().as_secs_f64(),
            "profiles":rows,
        });
        let output_path = PathBuf::from(&args[2]);
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&output_path, serde_json::to_vec_pretty(&output)?)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "output":output_path,
                "tasks":tasks.len(),
                "batch_jobs":jobs,
                "shared_database_load_sec":database_sec,
                "elapsed_sec":started.elapsed().as_secs_f64(),
            }))?
        );
        return Ok(());
    }
    if args.first().is_some_and(|argument| argument == "batch") {
        if args.len() != 2 {
            bail!("usage: run_frozen_a2_fast batch MANIFEST.json")
        }
        let batch_started = Instant::now();
        let tasks: Vec<BatchTask> = serde_json::from_slice(&fs::read(&args[1])?)?;
        anyhow::ensure!(!tasks.is_empty(), "batch manifest is empty");
        let inputs: Vec<_> = tasks
            .iter()
            .map(|task| task.input_netlist.clone())
            .collect();
        let database = CellDatabase::load(&inputs, &lib, &rules)?;
        let database_sec = batch_started.elapsed().as_secs_f64();
        let requested_jobs = std::env::var("EGG_A2_BATCH_JOBS")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .context("invalid EGG_A2_BATCH_JOBS")?
            .unwrap_or(1)
            .max(1);
        let jobs = requested_jobs.min(tasks.len());
        let next = AtomicUsize::new(0);
        let (sender, receiver) = mpsc::channel();
        std::thread::scope(|scope| {
            for _ in 0..jobs {
                let sender = sender.clone();
                let tasks = &tasks;
                let database = &database;
                let lib = &lib;
                let rules = &rules;
                let next = &next;
                scope.spawn(move || {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(task) = tasks.get(index) else {
                            break;
                        };
                        let task_started = Instant::now();
                        let result = Circuit::parse_with_database(&task.input_netlist, database)
                            .and_then(|circuit| {
                                execute(
                                    circuit,
                                    &task.input_netlist,
                                    &task.out_dir,
                                    &task.mode,
                                    lib,
                                    rules,
                                    task_started,
                                    task.resume_checkpoint.as_deref(),
                                    None,
                                    false,
                                )
                            })
                            .map_err(|error| format!("task {index}: {error:#}"));
                        if sender.send((index, result)).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        drop(sender);
        let mut ordered: Vec<Option<Value>> = vec![None; tasks.len()];
        for (index, result) in receiver {
            ordered[index] = Some(result.map_err(anyhow::Error::msg)?);
        }
        let summaries: Vec<Value> = ordered
            .into_iter()
            .enumerate()
            .map(|(index, value)| value.with_context(|| format!("missing batch task {index}")))
            .collect::<Result<_>>()?;
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "implementation":"batched Rust frozen A2-Next",
                "tasks":summaries.len(),
                "batch_jobs":jobs,
                "shared_database_load_sec":database_sec,
                "elapsed_sec":batch_started.elapsed().as_secs_f64(),
                "summaries":summaries
            }))?
        );
        return Ok(());
    }
    if args.len() < 3 {
        bail!("usage: run_frozen_a2_fast NETLIST OUT_DIR global|inner [REFERENCE]")
    }
    let input = PathBuf::from(&args[0]);
    let out = PathBuf::from(&args[1]);
    let mode = &args[2];
    let started = Instant::now();
    let circuit = Circuit::parse(&input, &lib, &rules)?;
    let resume_checkpoint = std::env::var_os("EGG_A2_RESUME_CHECKPOINT").map(PathBuf::from);
    execute(
        circuit,
        &input,
        &out,
        mode,
        &lib,
        &rules,
        started,
        resume_checkpoint.as_deref(),
        None,
        false,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(left: f64, right: f64) -> bool {
        (left - right).abs() <= 1e-12 * left.abs().max(right.abs()).max(1.0)
    }

    fn add2_circuit() -> Circuit {
        Circuit::parse(
            Path::new("../netlist/test/add2_map_abc.v"),
            Path::new("../netlist/test/asap7sc6t_SELECT_LVT_TT_nldm.lib"),
            Path::new("../netlist/test/6t_scale_rules.json"),
        )
        .unwrap()
    }

    #[test]
    fn phase_ii_scale_database_retains_multiple_legal_drives() {
        let database = CellDatabase::load(
            &[PathBuf::from("../netlist/test/add2_map_abc.v")],
            Path::new("test/asap7sc6t_full_comb/asap7sc6t_FULL_COMB_LVT_TT_nldm_211010.lib"),
            Path::new("test/6t_full_comb_scale_rules.json"),
        )
        .unwrap();
        assert!(
            database
                .family_names
                .values()
                .any(|family| family.len() > 1)
        );
        assert!(
            database
                .family_names
                .get("AND2x2_ASAP7_6t_L")
                .is_some_and(|family| family.len() >= 3)
        );
    }

    #[test]
    fn static_profile_matches_full_preparation() {
        let input = Path::new("../netlist/test/add2_map_abc.v");
        let database = CellDatabase::load(
            &[input.to_path_buf()],
            Path::new("../netlist/test/asap7sc6t_SELECT_LVT_TT_nldm.lib"),
            Path::new("../netlist/test/6t_scale_rules.json"),
        )
        .unwrap();
        let full = Circuit::parse_with_database_objective(input, &database, SearchObjective::D2ap)
            .unwrap();
        let lean = Circuit::parse_with_database_preparation(
            input,
            &database,
            SearchObjective::D2ap,
            false,
        )
        .unwrap();
        assert!(!full.probability_delta_plans.is_empty());
        assert!(lean.probability_delta_plans.is_empty());
        let a = full.evaluate(&full.original_choice(), None).unwrap();
        let b = lean.evaluate(&lean.original_choice(), None).unwrap();
        assert_eq!((a.delay, a.area, a.power), (b.delay, b.area, b.power));
        assert_eq!(
            full.profile_boundary_response(&a).unwrap(),
            lean.profile_boundary_response(&b).unwrap()
        );
    }

    #[test]
    fn profile_boundary_response_is_named_and_read_only() {
        let circuit = add2_circuit();
        let eval = circuit.evaluate(&circuit.original_choice(), None).unwrap();
        let response = circuit.profile_boundary_response(&eval).unwrap();
        assert_eq!(
            response["outputs"],
            json!(circuit.boundary_output_states(&eval))
        );
        let inputs = response["inputs"].as_array().unwrap();
        assert!(!inputs.is_empty());
        assert!(
            inputs
                .iter()
                .all(|row| row["capacitance_ff"].as_f64().unwrap() >= 0.0)
        );
        let again = circuit.evaluate(&circuit.original_choice(), None).unwrap();
        assert_eq!(eval.delay, again.delay);
        assert_eq!(eval.area, again.area);
        assert_eq!(eval.power, again.power);
    }

    #[test]
    fn v8_ultra_below_threshold_preserves_full_distribution_set() {
        let circuit = add2_circuit();
        let center = circuit.original_choice();
        let evaluated = circuit.evaluate(&center, None).unwrap();
        let config = V8UltraConfig {
            gate_threshold: circuit.instances.len() + 1,
            ..V8UltraConfig::default()
        };
        let (active, audit) = v8_ultra_active_instances(&circuit, &evaluated, Some(&config), 25);
        assert!(active.is_none());
        assert!(!audit.enabled);
        let frozen = circuit.distributions(&center, 0.10);
        let ultra = circuit.distributions_for_active(&center, 0.10, active.as_ref());
        assert_eq!(
            frozen.iter().filter(|value| value.is_some()).count(),
            ultra.iter().filter(|value| value.is_some()).count()
        );
        for (left, right) in frozen.iter().zip(&ultra) {
            match (left, right) {
                (Some(left), Some(right)) => {
                    assert_eq!(left.cells, right.cells);
                    assert_eq!(left.probabilities, right.probabilities);
                }
                (None, None) => {}
                _ => panic!("V8-Ultra below-threshold distribution mismatch"),
            }
        }
    }

    #[test]
    fn v8_ultra_large_path_caps_active_instances_deterministically() {
        let circuit = add2_circuit();
        let center = circuit.original_choice();
        let evaluated = circuit.evaluate(&center, None).unwrap();
        let config = V8UltraConfig {
            gate_threshold: 1,
            active_instance_cap: 1,
            timing_share_percent: 100,
            area_share_percent: 0,
            ..V8UltraConfig::default()
        };
        let (first, first_audit) =
            v8_ultra_active_instances(&circuit, &evaluated, Some(&config), 25);
        let (second, second_audit) =
            v8_ultra_active_instances(&circuit, &evaluated, Some(&config), 25);
        assert!(first_audit.enabled);
        assert_eq!(first, second);
        assert_eq!(
            first_audit.selected_instances,
            second_audit.selected_instances
        );
        assert!(first_audit.selected_instance_count <= 1);
        assert_eq!(
            circuit
                .distributions_for_active(&center, 0.10, first.as_ref())
                .iter()
                .filter(|value| value.is_some())
                .count(),
            first_audit.selected_instance_count
        );
    }

    #[test]
    fn incremental_probability_delta_matches_full_nldm_evaluation() {
        let circuit = add2_circuit();
        let center = circuit.original_choice();
        let mut probabilities = circuit.distributions(&center, 0.05);
        let mixed = circuit.evaluate(&center, Some(&probabilities)).unwrap();
        let changed = probabilities
            .iter()
            .position(Option::is_some)
            .expect("test netlist must contain a resizable occurrence");
        let distribution = probabilities[changed].as_mut().unwrap();
        let distribution_probabilities = Arc::make_mut(&mut distribution.probabilities);
        for probability in distribution_probabilities.iter_mut() {
            *probability *= 1.0 - 1e-3;
        }
        distribution_probabilities[0] += 1e-3;
        let full = circuit.evaluate(&center, Some(&probabilities)).unwrap();
        let incremental = circuit
            .evaluate_probability_delta(&center, &probabilities, &mixed, changed)
            .unwrap();
        assert!(close(full.delay, incremental.delay));
        assert!(close(full.area, incremental.area));
        assert!(close(full.power, incremental.power));
        assert!(close(full.score, incremental.score));
        assert_eq!(full.arrival.len(), incremental.arrival.len());
        assert!(
            full.arrival
                .iter()
                .flatten()
                .zip(incremental.arrival.iter().flatten())
                .all(|(left, right)| close(*left, *right))
        );
        assert!(
            full.transition
                .iter()
                .flatten()
                .zip(incremental.transition.iter().flatten())
                .all(|(left, right)| close(*left, *right))
        );
        assert!(
            full.arcs
                .iter()
                .zip(&incremental.arcs)
                .all(|(left, right)| close(left.delay, right.delay)
                    && close(left.arrival, right.arrival))
        );
        let weights = adjoint(&circuit, &mixed);
        let base_arcs = arc_map(&mixed);
        let changed_arcs = arc_map(&incremental);
        let reference_delta: f64 = weights
            .iter()
            .map(|(key, weight)| {
                weight * (changed_arcs.get(key).copied().unwrap_or(base_arcs[key]) - base_arcs[key])
            })
            .sum();
        let dense_delta = weighted_arc_delta(&incremental, &weighted_arc_groups(&mixed, &weights));
        assert!(close(reference_delta, dense_delta));
    }

    #[test]
    fn multi_lane_probability_delta_matches_scalar_lanes() {
        let circuit = add2_circuit();
        let center = circuit.original_choice();
        let probabilities = circuit.distributions(&center, 0.05);
        let mixed = circuit.evaluate(&center, Some(&probabilities)).unwrap();
        let changed = probabilities
            .iter()
            .position(Option::is_some)
            .expect("test netlist must contain a resizable occurrence");
        let alternatives = probabilities[changed].as_ref().unwrap().cells.len();
        let mut lanes = Vec::new();
        for alternative in 0..alternatives {
            let mut perturbed = probabilities.clone();
            let values = Arc::make_mut(&mut perturbed[changed].as_mut().unwrap().probabilities);
            for value in values.iter_mut() {
                *value *= 1.0 - 1e-3;
            }
            values[alternative] += 1e-3;
            lanes.push(perturbed);
        }
        let weights = adjoint(&circuit, &mixed);
        let groups = weighted_arc_groups(&mixed, &weights);
        let multi = circuit
            .evaluate_probability_delta_lanes_with_arc_groups(
                &center, &lanes, &mixed, changed, &groups,
            )
            .unwrap();
        for (lane, metrics) in lanes.iter().zip(multi) {
            let (scalar, scalar_delta) = circuit
                .evaluate_probability_delta_with_arc_groups(
                    &center,
                    lane,
                    &mixed,
                    changed,
                    Some(&groups),
                )
                .unwrap();
            assert!(close(metrics.delay, scalar.delay));
            assert!(close(metrics.area, scalar.area));
            assert!(close(metrics.power, scalar.power));
            assert!(close(
                metrics.weighted_arc_delta,
                scalar_delta.unwrap_or_else(|| weighted_arc_delta(&scalar, &groups))
            ));
        }
    }

    #[test]
    fn early_commitment_reassigns_the_full_progressive_budget() {
        assert_eq!(
            early_commitment_total_budget(32, 8, 25, 50, 500, None).unwrap(),
            1450
        );
        assert_eq!(
            early_commitment_total_budget(32, 8, 25, 50, 500, Some(650)).unwrap(),
            1600
        );
        assert_eq!(
            early_commitment_total_budget(1, 1, 25, 50, 500, Some(650)).unwrap(),
            650
        );
    }
}
