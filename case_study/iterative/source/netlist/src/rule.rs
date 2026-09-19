use crate::language::*;
use egg::{FromOp, Language, MultiPattern, Pattern, Rewrite, rewrite};
use serde::Deserialize;
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

#[derive(Debug, Deserialize)]
pub struct JsonRewrite {
    name: String,
    searcher: String,
    applier: String,
    bidirectional: Option<bool>, // None or false means one direction, not bidirectional
    multi: Option<bool>,         // None or false means only rewrite, not multi_rewrite
}

#[derive(Debug, Deserialize)]
pub struct JsonRules {
    rewrites: Vec<JsonRewrite>,
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
