use crate::language::*;
use egg::{FromOp, Language, MultiPattern, Pattern, Rewrite, rewrite};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Display;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

pub fn make_aig_rules() -> Vec<Rewrite<AigLanguage, ()>> {
    let mut rules = vec![
        rewrite!("Negation_1"; "(not true)" => "false"),
        rewrite!("Negation_2"; "(not false)" => "true"),
        rewrite!("Simplify_1"; "(and true ?x)" => "?x"),
        rewrite!("Simplify_2"; "(and false ?x)" => "false"),
        rewrite!("Simplify_3"; "(and ?x true)" => "?x"),
        rewrite!("Simplify_4"; "(and ?x false)" => "false"),
        rewrite!("Simplify_5"; "(and ?x ?x)" => "?x"),
        rewrite!("Simplify_6"; "(and ?x (not ?x))" => "false"),
    ];
    rules.extend(
        vec![
            rewrite!("Commutative"; "(and ?x ?y)" <=> "(and ?y ?x)"),
            rewrite!("Associative"; "(and ?x (and ?y ?z))" <=> "(and (and ?x ?y) ?z)"),
            rewrite!("DoubleNegation"; "(not (not ?x))" <=> "?x"),
        ]
        .concat(),
    );
    rules
}

#[derive(Clone, Debug, Deserialize)]
pub struct JsonRewrite {
    name: String,
    searcher: String,
    applier: String,
    bidirectional: Option<bool>, // None or false means one direction, not bidirectional
    multi: Option<bool>,         // None or false means only rewrite, not multi_rewrite
}

#[derive(Clone, Debug, Deserialize)]
pub struct JsonRules {
    rewrites: Vec<JsonRewrite>,
}

/// Audit record for the Phase-I rule projection which removes explicit
/// drive-strength equalities while retaining the native structural seeds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Phase1StructuralRuleAudit {
    pub input_json_rewrites: usize,
    pub excluded_drive_rewrites: usize,
    pub retained_rewrites: usize,
    pub added_direct_seed_rewrites: usize,
}

fn single_gate_pattern(pattern: &str) -> Option<(String, String)> {
    let body = pattern.strip_prefix('(')?.strip_suffix(')')?;
    if body.contains('(') || body.contains(')') {
        return None;
    }
    let mut fields = body.split_whitespace();
    let op = fields.next()?.to_owned();
    let args = fields.collect::<Vec<_>>();
    if args.is_empty() || args.iter().any(|arg| !arg.starts_with('?')) {
        return None;
    }
    Some((op, args.join(" ")))
}

fn rewrite_signature(rewrite: &JsonRewrite) -> (String, String, bool, bool) {
    (
        rewrite.searcher.clone(),
        rewrite.applier.clone(),
        rewrite.bidirectional.unwrap_or(false),
        rewrite.multi.unwrap_or(false),
    )
}

struct StringUnionFind {
    parent: BTreeMap<String, String>,
}

impl StringUnionFind {
    fn new() -> Self {
        Self {
            parent: BTreeMap::new(),
        }
    }

    fn find(&mut self, value: &str) -> String {
        let parent = self
            .parent
            .entry(value.to_owned())
            .or_insert_with(|| value.to_owned())
            .clone();
        if parent == value {
            parent
        } else {
            let root = self.find(&parent);
            self.parent.insert(value.to_owned(), root.clone());
            root
        }
    }

    fn union(&mut self, left: &str, right: &str) {
        let left_root = self.find(left);
        let right_root = self.find(right);
        if left_root == right_root {
            return;
        }
        let (root, child) = if left_root < right_root {
            (left_root, right_root)
        } else {
            (right_root, left_root)
        };
        self.parent.insert(child, root);
    }

    fn families(mut self) -> BTreeMap<String, BTreeSet<String>> {
        let members: Vec<_> = self.parent.keys().cloned().collect();
        let mut result = BTreeMap::<String, BTreeSet<String>>::new();
        for member in members {
            let root = self.find(&member);
            result.entry(root).or_default().insert(member);
        }
        result
    }
}

impl JsonRules {
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let reader = BufReader::new(file);
        let rules = serde_json::from_reader(reader).map_err(|e| e.to_string())?;
        Ok(rules)
    }

    fn from_str(rules: &str) -> Result<Self, String> {
        serde_json::from_str(rules).map_err(|e| e.to_string())
    }

    pub fn into_egg_rules<L>(self) -> Result<Vec<Rewrite<L, ()>>, String>
    where
        L: Language + Display + Send + Sync + FromOp + 'static,
    {
        let mut rules = Vec::new();
        for rewrite in self.rewrites {
            match (rewrite.multi, rewrite.bidirectional) {
                (None, Some(true)) => {
                    rules.push(Rewrite::new(
                        rewrite.name.clone(),
                        rewrite.searcher.parse::<Pattern<_>>().unwrap(),
                        rewrite.applier.parse::<Pattern<_>>().unwrap(),
                    )?);
                    rules.push(Rewrite::new(
                        rewrite.name + "-rev",
                        rewrite.applier.parse::<Pattern<_>>().unwrap(),
                        rewrite.searcher.parse::<Pattern<_>>().unwrap(),
                    )?)
                }
                (Some(true), None) => rules.push(Rewrite::new(
                    rewrite.name,
                    rewrite.searcher.parse::<MultiPattern<_>>().unwrap(),
                    rewrite.applier.parse::<MultiPattern<_>>().unwrap(),
                )?),
                (Some(true), Some(true)) => {
                    rules.push(Rewrite::new(
                        rewrite.name.clone(),
                        rewrite.searcher.parse::<MultiPattern<_>>().unwrap(),
                        rewrite.applier.parse::<MultiPattern<_>>().unwrap(),
                    )?);
                    rules.push(Rewrite::new(
                        rewrite.name + "-rev",
                        rewrite.applier.parse::<MultiPattern<_>>().unwrap(),
                        rewrite.searcher.parse::<MultiPattern<_>>().unwrap(),
                    )?)
                }
                (_, _) => {
                    rules.push(Rewrite::new(
                        rewrite.name,
                        rewrite.searcher.parse::<Pattern<_>>().unwrap(),
                        rewrite.applier.parse::<Pattern<_>>().unwrap(),
                    )?);
                }
            }
        }
        Ok(rules)
    }

    /// Remove the authoritative scale-rule equalities before Phase-I
    /// saturation.  Structural rules whose single-cell searcher uses a
    /// canonical family member receive forward-only aliases for every member
    /// of that exact scale-rule family.  This lets an incumbent of any legal
    /// drive trigger the same structural seed without placing its sibling
    /// drives into the e-graph.  Reverse structural application continues to
    /// construct only the rule's original, fixed seed cell.
    pub fn into_phase1_structural_seed_rules<L>(
        self,
        scale_rules: &JsonRules,
    ) -> Result<(Vec<Rewrite<L, ()>>, Phase1StructuralRuleAudit), String>
    where
        L: Language + Display + Send + Sync + FromOp + 'static,
    {
        let scale_signatures: BTreeSet<_> =
            scale_rules.rewrites.iter().map(rewrite_signature).collect();
        let mut union_find = StringUnionFind::new();
        for rewrite in &scale_rules.rewrites {
            let Some((left, left_args)) = single_gate_pattern(&rewrite.searcher) else {
                return Err(format!(
                    "scale searcher is not a single gate: {}",
                    rewrite.searcher
                ));
            };
            let Some((right, right_args)) = single_gate_pattern(&rewrite.applier) else {
                return Err(format!(
                    "scale applier is not a single gate: {}",
                    rewrite.applier
                ));
            };
            if left_args != right_args {
                return Err(format!(
                    "scale rule changes ordered pin variables: {} -> {}",
                    rewrite.searcher, rewrite.applier
                ));
            }
            union_find.union(&left, &right);
        }
        let families = union_find.families();
        let mut member_family = BTreeMap::new();
        for members in families.values() {
            for member in members {
                member_family.insert(member.clone(), members.clone());
            }
        }

        let mut projected = Vec::new();
        let mut audit = Phase1StructuralRuleAudit {
            input_json_rewrites: self.rewrites.len(),
            ..Phase1StructuralRuleAudit::default()
        };
        for rewrite in self.rewrites {
            if scale_signatures.contains(&rewrite_signature(&rewrite)) {
                audit.excluded_drive_rewrites += 1;
                continue;
            }
            audit.retained_rewrites += 1;
            if !rewrite.multi.unwrap_or(false) {
                if let Some((op, args)) = single_gate_pattern(&rewrite.searcher) {
                    if let Some(members) = member_family.get(&op) {
                        for member in members {
                            if member == &op {
                                continue;
                            }
                            projected.push(JsonRewrite {
                                name: format!("{}-direct-seed-from-{}", rewrite.name, member),
                                searcher: format!("({member} {args})"),
                                applier: rewrite.applier.clone(),
                                // A reverse alias would actively construct this
                                // drive variant and defeat the ablation.
                                bidirectional: None,
                                multi: None,
                            });
                            audit.added_direct_seed_rewrites += 1;
                        }
                    }
                }
            }
            projected.push(rewrite);
        }
        Ok((
            JsonRules {
                rewrites: projected,
            }
            .into_egg_rules()?,
            audit,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egraph_roots::EGraphRoots;
    use crate::io::liberty::{get_direction_of_pins, read_liberty};
    use crate::io::stdcell::read_verilog_with_lib_to_netlist;
    use crate::{SerializedEGraph, egg_to_serialized_egraph, netlist_to_egg_roots};
    use egg::Runner;
    use std::env;

    #[test]
    fn test_rules_from_str() {
        let json_data = r#"
        {
            "rewrites": [
                {
                    "name": "Negation_1",
                    "searcher": "(not true)",
                    "applier": "false"
                }, 
                {
                    "name": "Commutative",
                    "searcher": "(and ?x ?y)",
                    "applier": "(and ?y ?x)",
                    "bidirectional": true
                },
                {
                    "name": "Multipattern",
                    "searcher": "?v1 = (and ?x ?y), ?v2 = (and ?x ?y)",
                    "applier": "?v1 = (and ?y ?x), ?v2 = (and ?y ?x)",
                    "multi": true
                }
            ]
        }"#;
        let rules = JsonRules::from_str(json_data).unwrap();
        println!("{:#?}", rules);
        let rules = rules.into_egg_rules::<AigLanguage>().unwrap();
        println!("{:#?}", rules);
    }

    #[test]
    fn test_rules_from_path() {
        let rules =
            JsonRules::from_path(env::current_dir().unwrap().join("test/aig_rules.json")).unwrap();
        println!("{:#?}", rules);
        let rules = rules.into_egg_rules::<AigLanguage>().unwrap();
        println!("{:#?}", rules);
    }

    #[test]
    fn phase1_projection_excludes_drive_equalities_and_keeps_structural_seeds() {
        let scale = JsonRules::from_str(
            r#"{"rewrites":[{"name":"scale-a-b","searcher":"(A ?x)","applier":"(B ?x)","bidirectional":true}]}"#,
        )
        .unwrap();
        let full = JsonRules::from_str(
            r#"{"rewrites":[
                {"name":"scale-a-b","searcher":"(A ?x)","applier":"(B ?x)","bidirectional":true},
                {"name":"struct-a","searcher":"(A ?x)","applier":"(C ?x)","bidirectional":true},
                {"name":"pin-order","searcher":"(D ?x ?y)","applier":"(D ?y ?x)","bidirectional":true}
            ]}"#,
        )
        .unwrap();
        let (rules, audit) = full
            .into_phase1_structural_seed_rules::<StdCellLanguage>(&scale)
            .unwrap();
        assert_eq!(audit.input_json_rewrites, 3);
        assert_eq!(audit.excluded_drive_rewrites, 1);
        assert_eq!(audit.retained_rewrites, 2);
        assert_eq!(audit.added_direct_seed_rewrites, 1);
        // struct-a and pin-order remain bidirectional (four egg rules), while
        // B receives one forward-only path to the same C structural seed.
        assert_eq!(rules.len(), 5);
        let names: Vec<_> = rules.iter().map(|rule| rule.name.to_string()).collect();
        assert!(
            names
                .iter()
                .any(|name| name == "struct-a-direct-seed-from-B")
        );
        assert!(!names.iter().any(|name| name.starts_with("scale-a-b")));
    }

    #[test]
    fn test_add2_map_6t_comm_rules() {
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
        s.to_json_file(
            env::current_dir()
                .unwrap()
                .join("json/test_add2_map_abc_v_6t_comm_rules.json"),
        )
        .unwrap();
        #[cfg(target_os = "linux")]
        s.to_svg_file(
            env::current_dir()
                .unwrap()
                .join("svg/test_add2_map_abc_v_6t_comm_rules.svg"),
        )
        .unwrap();
    }

    #[test]
    fn test_add2_map_6t_inv_rules() {
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let lib = get_direction_of_pins(&liberty).unwrap();
        let (netlist, name) = read_verilog_with_lib_to_netlist("test/add2_map_abc.v", lib).unwrap();
        assert_eq!(name, "add2");
        let egraph_roots: EGraphRoots<_, ()> = netlist_to_egg_roots(&netlist).unwrap();
        let rules =
            JsonRules::from_path(env::current_dir().unwrap().join("test/6t_inv_rules.json"))
                .unwrap()
                .into_egg_rules::<StdCellLanguage>()
                .unwrap();
        let runner = Runner::default()
            .with_egraph(egraph_roots.egraph)
            .with_node_limit(1000000)
            .run(&rules);
        let s = egg_to_serialized_egraph(&runner.egraph, &egraph_roots.roots);
        s.to_json_file(
            env::current_dir()
                .unwrap()
                .join("json/test_add2_map_abc_v_6t_inv_rules.json"),
        )
        .unwrap();
        #[cfg(target_os = "linux")]
        s.to_svg_file(
            env::current_dir()
                .unwrap()
                .join("svg/test_add2_map_abc_v_6t_inv_rules.svg"),
        )
        .unwrap();
    }

    #[test]
    fn test_add2_map_6t_inv_dmg_rules() {
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
            JsonRules::from_path(env::current_dir().unwrap().join("test/6t_scale_rules.json"))
                .unwrap()
                .into_egg_rules::<StdCellLanguage>()
                .unwrap(),
        );
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
        s.to_json_file(
            env::current_dir()
                .unwrap()
                .join("json/test_add2_map_abc_v_6t_dmg_rules.json"),
        )
        .unwrap();
        #[cfg(target_os = "linux")]
        s.to_svg_file(
            env::current_dir()
                .unwrap()
                .join("svg/test_add2_map_abc_v_6t_dmg_rules.svg"),
        )
        .unwrap();
    }

    #[test]
    fn test_mul32_map_genus_inv_dmg_rules() {
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let lib = get_direction_of_pins(&liberty).unwrap();
        let (netlist, name) =
            read_verilog_with_lib_to_netlist("test/mul32_map_genus.v", lib).unwrap();
        // std::fs::write("dot/mul32_map_genus.v.netlist", format!("{:#?}", netlist)).unwrap();
        assert_eq!(name, "Multiplier");
        let egraph_roots: EGraphRoots<_, ()> = netlist_to_egg_roots(&netlist).unwrap();
        let mut rules =
            JsonRules::from_path(env::current_dir().unwrap().join("test/6t_inv_rules.json"))
                .unwrap()
                .into_egg_rules::<StdCellLanguage>()
                .unwrap();
        rules.extend(
            JsonRules::from_path(env::current_dir().unwrap().join("test/6t_scale_rules.json"))
                .unwrap()
                .into_egg_rules::<StdCellLanguage>()
                .unwrap(),
        );
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
        s.to_json_file(
            env::current_dir()
                .unwrap()
                .join("json/test_mul32_map_genus_inv_dmg_rules.json"),
        )
        .unwrap();
        // #[cfg(target_os = "linux")]
        // s.to_svg_file(
        //     env::current_dir()
        //         .unwrap()
        //         .join("svg/test_mul32_map_genus_inv_dmg_rules.svg"),
        // )
        // .unwrap();
    }
}
