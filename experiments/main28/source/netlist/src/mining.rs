#![allow(dead_code)]

use crate::SerializedEGraph;
use crate::io::liberty::Library;
use crate::io::stdcell::write_verilog_from_netlist_with_lib;
use crate::language::StdCellType;
use crate::netlist::Netlist;
use egraph_serialize::{ClassId, NodeId};
use indexmap::IndexMap;
use itertools::Itertools;
use libertyparse::PinDirection;
use petgraph::graph::NodeIndex;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct MiningConfig {
    pub min_support: usize,
    pub max_pattern_size: usize,
    pub top_k: usize,
}

impl Default for MiningConfig {
    fn default() -> Self {
        Self {
            min_support: 2,
            max_pattern_size: 3,
            top_k: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PatternExpr {
    Input,
    Gate {
        op: String,
        children: Vec<PatternExpr>,
    },
}

impl PatternExpr {
    pub fn gate_count(&self) -> usize {
        match self {
            Self::Input => 0,
            Self::Gate { children, .. } => 1 + children.iter().map(Self::gate_count).sum::<usize>(),
        }
    }

    pub fn signature(&self) -> String {
        match self {
            Self::Input => "IN".to_string(),
            Self::Gate { op, children } => {
                if children.is_empty() {
                    op.clone()
                } else {
                    format!("{}({})", op, children.iter().map(Self::signature).join(","))
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MinedPattern {
    pub rank: usize,
    pub support: usize,
    pub gate_count: usize,
    pub root_op: String,
    pub signature: String,
    pub expr: PatternExpr,
    pub verilog_path: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct MiningSummary {
    pub min_support: usize,
    pub max_pattern_size: usize,
    pub top_k: usize,
    pub egraph_nodes: usize,
    pub egraph_classes: usize,
    pub exported_patterns: Vec<MinedPattern>,
}

#[derive(Debug, Clone)]
pub struct GSpan {
    egraph: SerializedEGraph,
    library: Library,
    config: MiningConfig,
    lib_pins: IndexMap<String, (String, Vec<String>)>,
    memo: FxHashMap<(String, usize), Vec<PatternExpr>>,
}

impl GSpan {
    pub fn new(
        egraph: SerializedEGraph,
        library: Library,
        config: MiningConfig,
    ) -> Result<Self, String> {
        let lib_pins = Self::construct_lib_pins(&library)?;
        Ok(Self {
            egraph,
            library,
            config,
            lib_pins,
            memo: FxHashMap::default(),
        })
    }

    fn construct_lib_pins(
        library: &Library,
    ) -> Result<IndexMap<String, (String, Vec<String>)>, String> {
        let mut lib_pins = IndexMap::default();
        for (cell_name, pins) in library {
            let out_pins = pins
                .iter()
                .filter_map(|(name, dir)| match dir {
                    PinDirection::O => Some(name.clone()),
                    _ => None,
                })
                .collect_vec();
            if out_pins.len() != 1 {
                return Err(format!(
                    "only support single-output cells during mining, got {} outputs for {}",
                    out_pins.len(),
                    cell_name
                ));
            }
            let in_pins = pins
                .iter()
                .filter_map(|(name, dir)| match dir {
                    PinDirection::I => Some(name.clone()),
                    _ => None,
                })
                .collect_vec();
            lib_pins.insert(cell_name.clone(), (out_pins[0].clone(), in_pins));
        }
        Ok(lib_pins)
    }

    fn is_library_gate(&self, op: &str) -> bool {
        self.lib_pins.contains_key(op)
    }

    fn enumerate_patterns_for_node(
        &mut self,
        node_id: &NodeId,
        budget: usize,
        stack: &mut FxHashSet<String>,
    ) -> Vec<PatternExpr> {
        if budget == 0 {
            return Vec::new();
        }
        let node = &self.egraph[node_id];
        let node_op = node.op.clone();
        let node_children = node.children.clone();
        if !self.is_library_gate(&node_op) {
            return Vec::new();
        }

        let key = (node_id.to_string(), budget);
        if let Some(cached) = self.memo.get(&key) {
            return cached.clone();
        }

        let stack_key = key.0.clone();
        if !stack.insert(stack_key.clone()) {
            return Vec::new();
        }

        let mut by_signature: FxHashMap<String, PatternExpr> = FxHashMap::default();
        let base = PatternExpr::Gate {
            op: node_op.clone(),
            children: vec![PatternExpr::Input; node_children.len()],
        };
        by_signature.insert(base.signature(), base);

        if budget > 1 && !node_children.is_empty() {
            let mut child_options = Vec::with_capacity(node_children.len());
            for child in &node_children {
                let child_class = self.egraph.nid_to_cid(child).clone();
                let options = self.enumerate_child_options(&child_class, budget - 1, stack);
                child_options.push(options);
            }
            self.combine_child_patterns(
                &node_op,
                &child_options,
                0,
                &mut Vec::with_capacity(node_children.len()),
                &mut by_signature,
                budget,
            );
        }

        stack.remove(&stack_key);
        let result = by_signature.into_values().collect_vec();
        self.memo.insert(key, result.clone());
        result
    }

    fn enumerate_child_options(
        &mut self,
        class_id: &ClassId,
        budget: usize,
        stack: &mut FxHashSet<String>,
    ) -> Vec<PatternExpr> {
        let mut by_signature: FxHashMap<String, PatternExpr> = FxHashMap::default();
        by_signature.insert(PatternExpr::Input.signature(), PatternExpr::Input);

        if budget == 0 {
            return by_signature.into_values().collect();
        }

        let node_ids = self.egraph[class_id].nodes.clone();
        for node_id in node_ids {
            for pattern in self.enumerate_patterns_for_node(&node_id, budget, stack) {
                by_signature.insert(pattern.signature(), pattern);
            }
        }
        by_signature.into_values().collect()
    }

    fn combine_child_patterns(
        &self,
        op: &str,
        child_options: &[Vec<PatternExpr>],
        idx: usize,
        current: &mut Vec<PatternExpr>,
        out: &mut FxHashMap<String, PatternExpr>,
        budget: usize,
    ) {
        if idx == child_options.len() {
            let pattern = PatternExpr::Gate {
                op: op.to_string(),
                children: current.clone(),
            };
            let size = pattern.gate_count();
            if size >= 2 && size <= budget {
                out.insert(pattern.signature(), pattern);
            }
            return;
        }

        for option in &child_options[idx] {
            current.push(option.clone());
            let current_size = 1 + current.iter().map(PatternExpr::gate_count).sum::<usize>();
            if current_size <= budget {
                self.combine_child_patterns(op, child_options, idx + 1, current, out, budget);
            }
            current.pop();
        }
    }

    pub fn mine(&mut self) -> Vec<(PatternExpr, usize)> {
        let mut counts: FxHashMap<String, (PatternExpr, usize)> = FxHashMap::default();
        let node_ids = self.egraph.nodes.keys().cloned().collect_vec();
        for node_id in node_ids {
            let mut stack = FxHashSet::default();
            let patterns = self.enumerate_patterns_for_node(
                &node_id,
                self.config.max_pattern_size,
                &mut stack,
            );
            let mut seen = FxHashSet::default();
            for pattern in patterns {
                let gate_count = pattern.gate_count();
                if gate_count < 2 || gate_count > self.config.max_pattern_size {
                    continue;
                }
                let signature = pattern.signature();
                if !seen.insert(signature.clone()) {
                    continue;
                }
                let entry = counts.entry(signature).or_insert((pattern.clone(), 0));
                entry.1 += 1;
            }
        }

        let mut patterns = counts
            .into_values()
            .filter(|(_, support)| *support >= self.config.min_support)
            .collect_vec();
        patterns.sort_by(|(lhs_pattern, lhs_support), (rhs_pattern, rhs_support)| {
            rhs_support
                .cmp(lhs_support)
                .then_with(|| rhs_pattern.gate_count().cmp(&lhs_pattern.gate_count()))
                .then_with(|| lhs_pattern.signature().cmp(&rhs_pattern.signature()))
        });
        patterns.truncate(self.config.top_k);
        patterns
    }

    pub fn export_top_patterns<P: AsRef<Path>>(
        &mut self,
        output_dir: P,
    ) -> Result<MiningSummary, String> {
        fs::create_dir_all(output_dir.as_ref()).map_err(|e| e.to_string())?;
        let mined = self.mine();
        let mut exported_patterns = Vec::with_capacity(mined.len());

        for (idx, (expr, support)) in mined.into_iter().enumerate() {
            let rank = idx + 1;
            let root_op = match &expr {
                PatternExpr::Gate { op, .. } => op.clone(),
                PatternExpr::Input => "input".to_string(),
            };
            let signature = expr.signature();
            let file_name = format!(
                "pattern_{rank:02}_support{support}_size{}.v",
                expr.gate_count()
            );
            let verilog_path = output_dir.as_ref().join(file_name);
            let module_name = format!("pattern_{rank:02}");
            let netlist = pattern_to_netlist(&expr);
            write_verilog_from_netlist_with_lib(
                &verilog_path,
                netlist,
                &module_name,
                self.library.clone(),
            )?;

            exported_patterns.push(MinedPattern {
                rank,
                support,
                gate_count: expr.gate_count(),
                root_op,
                signature,
                expr,
                verilog_path,
            });
        }

        let summary = MiningSummary {
            min_support: self.config.min_support,
            max_pattern_size: self.config.max_pattern_size,
            top_k: self.config.top_k,
            egraph_nodes: self.egraph.nodes.len(),
            egraph_classes: self.egraph.classes().len(),
            exported_patterns,
        };
        let summary_path = output_dir.as_ref().join("summary.json");
        fs::write(
            &summary_path,
            serde_json::to_string_pretty(&summary).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        Ok(summary)
    }
}

fn pattern_to_netlist(expr: &PatternExpr) -> Netlist<StdCellType, ()> {
    fn build_expr(
        expr: &PatternExpr,
        netlist: &mut Netlist<StdCellType, ()>,
        input_counter: &mut usize,
    ) -> NodeIndex {
        match expr {
            PatternExpr::Input => {
                let name = format!("in_{}", *input_counter);
                *input_counter += 1;
                let nid = netlist.graph.add_node(StdCellType::Symbol(name.into()));
                netlist.leaves.push(nid);
                nid
            }
            PatternExpr::Gate { op, children } => {
                let oid = netlist
                    .graph
                    .add_node(StdCellType::Symbol(op.clone().into()));
                for child in children {
                    let iid = build_expr(child, netlist, input_counter);
                    netlist.graph.add_edge(oid, iid, ());
                }
                oid
            }
        }
    }

    let mut netlist: Netlist<StdCellType, ()> = Default::default();
    let mut input_counter = 0usize;
    let root_gate = build_expr(expr, &mut netlist, &mut input_counter);
    let output = netlist.graph.add_node(StdCellType::Symbol("out_0".into()));
    netlist.graph.add_edge(output, root_gate, ());
    netlist.roots.push(output);
    netlist
}

pub fn mine_patterns_to_dir<P: AsRef<Path>>(
    egraph: &SerializedEGraph,
    library: &Library,
    config: MiningConfig,
    output_dir: P,
) -> Result<MiningSummary, String> {
    let mut miner = GSpan::new(egraph.clone(), library.clone(), config)?;
    miner.export_top_patterns(output_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egg_to_serialized_egraph;
    use crate::egraph_roots::EGraphRoots;
    use crate::io::liberty::{get_direction_of_pins, read_liberty};
    use crate::io::stdcell::read_verilog_with_lib_to_netlist;
    use crate::language::StdCellLanguage;
    use crate::netlist_to_egg_roots;
    use crate::rule::JsonRules;
    use egg::Runner;
    use std::env;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn test_mining_exports_patterns_after_rewrite() {
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let lib = get_direction_of_pins(&liberty).unwrap();
        let (netlist, _name) =
            read_verilog_with_lib_to_netlist("test/add2_map_abc.v", lib.clone()).unwrap();
        let egraph_roots: EGraphRoots<_, ()> = netlist_to_egg_roots(&netlist).unwrap();
        let rules =
            JsonRules::from_path(env::current_dir().unwrap().join("test/6t_inv_rules.json"))
                .unwrap()
                .into_egg_rules::<StdCellLanguage>()
                .unwrap();
        let runner = Runner::default()
            .with_egraph(egraph_roots.egraph)
            .with_node_limit(100000)
            .with_iter_limit(30)
            .run(&rules);
        let serialized = egg_to_serialized_egraph(&runner.egraph, &egraph_roots.roots);

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let output_dir = env::temp_dir().join(format!("mac_egg_mining_test_{unique}"));
        let summary = mine_patterns_to_dir(
            &serialized,
            &lib,
            MiningConfig {
                min_support: 2,
                max_pattern_size: 3,
                top_k: 3,
            },
            &output_dir,
        )
        .unwrap();

        assert!(!summary.exported_patterns.is_empty());
        for pattern in &summary.exported_patterns {
            assert!(pattern.verilog_path.exists());
            assert!(pattern.gate_count >= 2);
            assert!(pattern.support >= 2);
        }
        assert!(output_dir.join("summary.json").exists());
    }
}
