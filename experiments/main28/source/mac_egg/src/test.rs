use anyhow::Context;
use egg::*;
#[cfg(feature = "ilp-cbc")]
use egraph_serialize::ClassId;
use extraction_gym::ExtendedEGraph;
#[cfg(feature = "ilp-cbc")]
use extraction_gym::extract::ExtractionResult;
#[cfg(feature = "ilp-cbc")]
use extraction_gym::extract::Extractor as GymExtractor;
#[cfg(feature = "ilp-cbc")]
use extraction_gym::extract::nldm_oracle_ilp::{NldmOracleCbcExtractor, NldmOracleConfig};
use mac_egg::*;
use serde_json::Value;
#[cfg(feature = "ilp-cbc")]
use std::cmp::Ordering;
use std::env;
#[cfg(feature = "ilp-cbc")]
use std::time::Instant;

#[test]
fn test_simple() {
    use egg_to_serialized_egraph;
    use rule::make_aig_rules;
    let runner = Runner::default()
        // .with_iter_limit(2)
        .with_expr(&"(and (and x y) (and x z))".parse().unwrap())
        .with_node_limit(1000000)
        // .with_expr(&"(xor3 x y z)".parse().unwrap())
        .run(&make_aig_rules());
    let s: SerializedEGraph = egg_to_serialized_egraph(&runner.egraph, &runner.roots);
    let (egraph, root) = (runner.egraph, runner.roots[0]);
    let mut extractors = extractor::extractors();
    extractors.retain(|_, ed| ed.get_use_for_bench());
    let extractor_name: String = "iterative-greedy-dag".into();
    let ed = extractors
        .get(extractor_name.as_str())
        .with_context(|| format!("Unknown extractor: {extractor_name}"))
        .unwrap();
    let es = ExtendedEGraph::from_base_to_extention_fast(s.clone());
    let result = ed.get_extractor().extract(&es, &s.root_eclasses);
    result.check(&es);
    println!("{:?}", result.choices);

    let extractor = Extractor::new(&egraph, AstSize);
    let (best_cost, best) = extractor.find_best(root);
    println!("egg result:");
    println!("{:?}", best_cost);
    println!("{:?}", best);
    use std::env;
    s.to_json_file(
        env::current_dir()
            .unwrap()
            .join("json/serialized_egraph_test.json"),
    )
    .unwrap();
    #[cfg(target_os = "linux")]
    s.to_svg_file(
        env::current_dir()
            .unwrap()
            .join("svg/serialized_egraph_test.svg"),
    )
    .unwrap();
}

#[test]
fn test_mul32() {
    use choose_result_in_serialized_egraph_into_netlist;
    use egraph_roots::EGraphRoots;
    use io::liberty::{get_direction_of_pins, read_liberty};
    use io::stdcell::{read_verilog_with_lib_to_netlist, write_verilog_from_netlist_with_lib};
    use language::StdCellLanguage;
    use language::StdCellType;
    use rule::JsonRules;

    let lib_value: String = "test/asap7sc6t_SELECT_LVT_TT_nldm.lib".into();
    let filename: String = "json/test_mul32_map_genus_inv_dmg_rules.json".into();
    let liberty = read_liberty(&lib_value).unwrap();
    let lib = get_direction_of_pins(&liberty).unwrap();
    let (netlist, name) =
        read_verilog_with_lib_to_netlist("test/mul32_map_genus.v", lib.clone()).unwrap();
    assert_eq!(name, "Multiplier");
    let egraph_roots: EGraphRoots<_, ()> = netlist_to_egg_roots(&netlist).unwrap();
    let mut rules =
        JsonRules::from_path(env::current_dir().unwrap().join("test/6t_empty_rules.json"))
            .unwrap()
            .into_egg_rules::<StdCellLanguage>()
            .unwrap();
    rules.extend(
        JsonRules::from_path(env::current_dir().unwrap().join("test/6t_inv_rules.json"))
            .unwrap()
            .into_egg_rules::<StdCellLanguage>()
            .unwrap(),
    );
    println!("总共加载了 {} 条规则", rules.len());
    rules.extend(
        JsonRules::from_path(env::current_dir().unwrap().join("test/6t_dmg_rules.json"))
            .unwrap()
            .into_egg_rules::<StdCellLanguage>()
            .unwrap(),
    );
    println!("总共加载了 {} 条规则", rules.len());
    rules.extend(
        JsonRules::from_path(env::current_dir().unwrap().join("test/6t_scale_rules.json"))
            .unwrap()
            .into_egg_rules::<StdCellLanguage>()
            .unwrap(),
    );
    println!("总共加载了 {} 条规则", rules.len());
    let runner = Runner::default()
        .with_egraph(egraph_roots.egraph)
        .with_node_limit(1000000)
        .with_iter_limit(200)
        .with_time_limit(std::time::Duration::from_secs(200))
        .run(&rules);
    runner.print_report();
    println!(
        "After rewrite: e-nodes = {}, e-classes = {}",
        runner.egraph.total_size(),
        runner.egraph.number_of_classes(), // 若版本不支持，可用: runner.iterations.last().map(|it| it.egraph_classes).unwrap_or(0)
    );
    let s = egg_to_serialized_egraph(&runner.egraph, &egraph_roots.roots);
    s.to_json_file(env::current_dir().unwrap().join(&filename))
        .unwrap();

    let json_content = std::fs::read_to_string(&filename)
        .with_context(|| format!("Failed to read {filename}"))
        .unwrap();
    let json_value: Value = serde_json::from_str(&json_content)
        .with_context(|| format!("Failed to parse {filename} as JSON"))
        .unwrap();
    let lib_value = Value::String(lib_value);
    let es = ExtendedEGraph::from_base_to_extention(s, &lib_value, &json_value);

    let mut extractors = extractor::extractors();
    extractors.retain(|_, ed| ed.get_use_for_bench());
    let extractor_name: String = "iterative-greedy-dag-SA".into();
    let ed = extractors
        .get(extractor_name.as_str())
        .with_context(|| format!("Unknown extractor: {extractor_name}"))
        .unwrap();
    let result = ed.get_extractor().extract(&es, &es.inner.root_eclasses);
    result.check(&es);
    let dag_nldm = result.dag_cost_nldm(&es);
    println!("Extracted result cost: {:?}", dag_nldm);
    let pruned_dag_nldm = result.dag_cost_nldm_on_pruned(&es);
    println!("Pruned Extracted result cost: {:?}", pruned_dag_nldm);
    let new_netlist =
        choose_result_in_serialized_egraph_into_netlist::<StdCellType>(&es.inner, &result).unwrap();
    // std::fs::write("verilog/test_mul32_map_genus_inv_dmg_rules_extract.v.netlist", format!("{:#?}", new_netlist)).unwrap();
    write_verilog_from_netlist_with_lib(
        "verilog/test_mul32_map_genus_inv_dmg_rules_extract.v",
        new_netlist,
        &name,
        lib,
    )
    .unwrap();
}

// mapping rule
// gate sizing

#[cfg(feature = "ilp-cbc")]
fn enumerate_legal_extractions(
    egraph: &ExtendedEGraph,
    roots: &[ClassId],
) -> Vec<extraction_gym::extract::ExtractionResult> {
    use indexmap::IndexSet;

    fn expand_class(
        egraph: &ExtendedEGraph,
        class_id: &ClassId,
        current: extraction_gym::extract::ExtractionResult,
        stack: &mut IndexSet<ClassId>,
    ) -> Vec<extraction_gym::extract::ExtractionResult> {
        if current.choices.contains_key(class_id) {
            return vec![current];
        }

        if !stack.insert(class_id.clone()) {
            return Vec::new();
        }

        let mut out = Vec::new();
        for node_id in &egraph.inner[class_id].nodes {
            let child_classes: Vec<ClassId> = egraph.inner[node_id]
                .children
                .iter()
                .map(|child| egraph.inner.nid_to_cid(child).clone())
                .collect();
            if child_classes.iter().any(|child| stack.contains(child)) {
                continue;
            }

            let mut next = current.clone();
            next.choose(class_id.clone(), node_id.clone());
            let mut partials = vec![next];
            for child in child_classes {
                let mut expanded = Vec::new();
                for partial in partials {
                    expanded.extend(expand_class(egraph, &child, partial, stack));
                }
                partials = expanded;
                if partials.is_empty() {
                    break;
                }
            }
            out.extend(partials);
        }

        stack.shift_remove(class_id);
        out
    }

    fn expand_roots(
        egraph: &ExtendedEGraph,
        roots: &[ClassId],
    ) -> Vec<extraction_gym::extract::ExtractionResult> {
        let mut partials = vec![extraction_gym::extract::ExtractionResult::default()];
        for root in roots {
            let mut next_partials = Vec::new();
            for partial in partials {
                next_partials.extend(expand_class(egraph, root, partial, &mut IndexSet::new()));
            }
            partials = next_partials;
            if partials.is_empty() {
                break;
            }
        }
        partials
    }

    expand_roots(egraph, roots)
}

#[cfg(feature = "ilp-cbc")]
#[test]
fn test_nldm_oracle_ilp_matches_bruteforce_on_small_rewritten_chain() {
    let lib_path = "test/asap7sc6t_SELECT_LVT_TT_nldm.lib";
    let json_value = serde_json::json!({
        "nodes": {
            "0.0": { "op": "a", "children": [], "eclass": "0", "cost": 0.0 },
            "1.0": { "op": "INVx1_ASAP7_6t_L", "children": ["0.0"], "eclass": "1", "cost": 0.0 },
            "2.0": { "op": "INVx2_ASAP7_6t_L", "children": ["0.0"], "eclass": "1", "cost": 0.0 },
            "3.0": { "op": "INVx1_ASAP7_6t_L", "children": ["1.0"], "eclass": "2", "cost": 0.0 },
            "4.0": { "op": "INVx2_ASAP7_6t_L", "children": ["1.0"], "eclass": "2", "cost": 0.0 }
        },
        "root_eclasses": ["2"],
        "class_data": {}
    });
    let temp_path = env::temp_dir().join("nldm_oracle_small_rewrite.json");
    std::fs::write(&temp_path, serde_json::to_string(&json_value).unwrap()).unwrap();
    let s = egraph_serialize::EGraph::from_json_file(&temp_path).unwrap();

    let lib_value = Value::String(lib_path.into());
    let es = ExtendedEGraph::from_base_to_extention(s, &lib_value, &json_value);

    let roots = es.inner.root_eclasses.clone();
    let config = NldmOracleConfig::default();
    let extractor = NldmOracleCbcExtractor::default();
    let result = extractor.extract(&es, &roots);
    assert!(result.find_cycles(&es, &roots).is_empty());
    for root in &roots {
        assert!(result.choices.contains_key(root));
    }

    let extracted = extraction_gym::extract::nldm_oracle_ilp::evaluate_surrogate_choice(
        &es, &roots, &result, &config, true,
    );
    let candidates = enumerate_legal_extractions(&es, &roots);
    assert!(!candidates.is_empty());

    let extracted_cost = result.dag_cost_nldm_on_pruned(&es);
    let extracted_tuple = lex_cost_tuple(&extracted_cost);
    let mut best = (f64::INFINITY, f64::INFINITY, f64::INFINITY);
    let mut best_count = 0usize;
    for candidate in candidates {
        assert!(candidate.find_cycles(&es, &roots).is_empty());
        let cost = candidate.dag_cost_nldm_on_pruned(&es);
        let tuple = lex_cost_tuple(&cost);
        if lex_tuple_cmp(tuple, best) == Ordering::Less {
            best = tuple;
            best_count = 1;
        } else if lex_tuple_cmp(tuple, best) == Ordering::Equal {
            best_count += 1;
        }
    }

    assert!(
        lex_tuple_cmp(extracted_tuple, best) == Ordering::Equal,
        "MILP lex cost {:?} != brute-force optimum {:?}",
        extracted_tuple,
        best
    );
    assert!(best_count >= 1);
}

#[cfg(feature = "ilp-cbc")]
#[test]
fn test_nldm_oracle_ilp_matches_bruteforce_on_add2_inv_rules_lexicographic() {
    use egraph_roots::EGraphRoots;
    use io::liberty::{get_direction_of_pins, read_liberty};
    use io::stdcell::read_verilog_with_lib_to_netlist;
    use rule::JsonRules;

    let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
    let lib = get_direction_of_pins(&liberty).unwrap();
    let (netlist, _name) = read_verilog_with_lib_to_netlist("test/add2_map_abc.v", lib).unwrap();
    let egraph_roots: EGraphRoots<_, ()> = netlist_to_egg_roots(&netlist).unwrap();
    let rules = JsonRules::from_path(env::current_dir().unwrap().join("test/6t_inv_rules.json"))
        .unwrap()
        .into_egg_rules::<language::StdCellLanguage>()
        .unwrap();
    let runner = Runner::default()
        .with_egraph(egraph_roots.egraph)
        .with_node_limit(100000)
        .with_iter_limit(30)
        .run(&rules);
    let serialized = egg_to_serialized_egraph(&runner.egraph, &egraph_roots.roots);
    let json_value = serde_json::to_value(&serialized).unwrap();
    let lib_value = Value::String("test/asap7sc6t_SELECT_LVT_TT_nldm.lib".into());
    let es = ExtendedEGraph::from_base_to_extention(serialized, &lib_value, &json_value);
    let roots = es.inner.root_eclasses.clone();

    let extractor = NldmOracleCbcExtractor::default();
    let milp_start = Instant::now();
    let result = extractor.extract(&es, &roots);
    let milp_elapsed = milp_start.elapsed();
    result.check(&es);
    let extracted_cost = result.dag_cost_nldm_on_pruned(&es);
    let extracted_tuple = lex_cost_tuple(&extracted_cost);

    let brute_start = Instant::now();
    let candidates = enumerate_legal_extractions(&es, &roots);
    let brute_elapsed = brute_start.elapsed();
    assert_eq!(candidates.len(), 16);

    let mut best_tuple = (f64::INFINITY, f64::INFINITY, f64::INFINITY);
    let mut best_count = 0usize;
    for candidate in candidates {
        let tuple = lex_cost_tuple(&candidate.dag_cost_nldm_on_pruned(&es));
        match lex_tuple_cmp(tuple, best_tuple) {
            Ordering::Less => {
                best_tuple = tuple;
                best_count = 1;
            }
            Ordering::Equal => best_count += 1,
            Ordering::Greater => {}
        }
    }

    println!(
        "add2+inv lex brute-force candidates={}, milp_time_ms={}, brute_time_ms={}, milp={:?}, brute={:?}",
        16,
        milp_elapsed.as_millis(),
        brute_elapsed.as_millis(),
        extracted_tuple,
        best_tuple
    );

    assert_eq!(lex_tuple_cmp(extracted_tuple, best_tuple), Ordering::Equal);
    assert!(best_count >= 1);
}

#[cfg(feature = "ilp-cbc")]
fn lex_cost_tuple(cost: &extraction_gym::ExtendedCost) -> (f64, f64, f64) {
    (
        cost.components[0].into_inner(),
        cost.components[1].into_inner(),
        cost.components[2].into_inner(),
    )
}

#[cfg(feature = "ilp-cbc")]
fn lex_tuple_cmp(lhs: (f64, f64, f64), rhs: (f64, f64, f64)) -> Ordering {
    for (a, b) in [(lhs.0, rhs.0), (lhs.1, rhs.1), (lhs.2, rhs.2)] {
        if (a - b).abs() <= 1e-6 {
            continue;
        }
        return a.partial_cmp(&b).unwrap();
    }
    Ordering::Equal
}

#[cfg(feature = "ilp-cbc")]
fn delay_area_cost(cost: &extraction_gym::ExtendedCost) -> f64 {
    let d = cost.components[0].into_inner();
    let a = cost.components[1].into_inner();
    d * a
}

#[cfg(feature = "ilp-cbc")]
fn delay2_area_power_cost(cost: &extraction_gym::ExtendedCost) -> f64 {
    let d = cost.components[0].into_inner();
    let a = cost.components[1].into_inner();
    let p = cost.components[2].into_inner();
    d * d * a * p
}

#[cfg(feature = "ilp-cbc")]
fn rewrite_and_bruteforce_best(
    netlist: &netlist::Netlist<language::StdCellType, ()>,
    lib_value: &Value,
    rule_paths: &[&str],
) -> (
    SerializedEGraph,
    ExtractionResult,
    extraction_gym::ExtendedCost,
    usize,
    usize,
    usize,
    usize,
) {
    use egraph_roots::EGraphRoots;
    use rule::JsonRules;

    let egraph_roots: EGraphRoots<_, ()> = netlist_to_egg_roots(netlist).unwrap();
    let mut rules = Vec::new();
    for path in rule_paths {
        rules.extend(
            JsonRules::from_path(env::current_dir().unwrap().join(path))
                .unwrap()
                .into_egg_rules::<language::StdCellLanguage>()
                .unwrap(),
        );
    }

    let runner = Runner::default()
        .with_egraph(egraph_roots.egraph)
        .with_node_limit(100000)
        .with_iter_limit(30)
        .run(&rules);
    let serialized = egg_to_serialized_egraph(&runner.egraph, &egraph_roots.roots);
    let json_value = serde_json::to_value(&serialized).unwrap();
    let es = ExtendedEGraph::from_base_to_extention(serialized.clone(), lib_value, &json_value);
    let roots = es.inner.root_eclasses.clone();

    let candidates = enumerate_legal_extractions(&es, &roots);
    let candidate_count = candidates.len();
    let mut best_result: Option<ExtractionResult> = None;
    let mut best_cost: Option<extraction_gym::ExtendedCost> = None;
    for candidate in candidates {
        let cost = candidate.dag_cost_nldm_on_pruned(&es);
        match &best_cost {
            None => {
                best_result = Some(candidate);
                best_cost = Some(cost);
            }
            Some(current_best) => {
                let lhs = delay2_area_power_cost(&cost);
                let rhs = delay2_area_power_cost(current_best);
                let better = if (lhs - rhs).abs() <= 1e-6 {
                    lex_tuple_cmp(lex_cost_tuple(&cost), lex_cost_tuple(current_best))
                        == Ordering::Less
                } else {
                    lhs < rhs
                };
                if better {
                    best_result = Some(candidate);
                    best_cost = Some(cost);
                }
            }
        }
    }

    (
        serialized,
        best_result.unwrap(),
        best_cost.unwrap(),
        runner.egraph.total_size(),
        runner.egraph.number_of_classes(),
        roots.len(),
        candidate_count,
    )
}

#[cfg(feature = "ilp-cbc")]
#[test]
fn diagnose_add2_bruteforce_rewrite_space_vs_original() {
    use egraph_roots::EGraphRoots;
    use io::liberty::{get_direction_of_pins, read_liberty};
    use io::stdcell::read_verilog_with_lib_to_netlist;
    use rule::JsonRules;

    let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
    let lib = get_direction_of_pins(&liberty).unwrap();
    let (netlist, _name) = read_verilog_with_lib_to_netlist("test/add2_map_abc.v", lib).unwrap();
    let base_roots: EGraphRoots<_, ()> = netlist_to_egg_roots(&netlist).unwrap();
    let original_serialized = SerializedEGraph::from(&base_roots);
    let original_json = serde_json::to_value(&original_serialized).unwrap();
    let lib_value = Value::String("test/asap7sc6t_SELECT_LVT_TT_nldm.lib".into());
    let original_ext = ExtendedEGraph::from_base_to_extention(
        original_serialized.clone(),
        &lib_value,
        &original_json,
    );
    let original_result = choose_first_node_in_each_class(&original_serialized).unwrap();
    let original_tuple = lex_cost_tuple(&original_result.dag_cost_nldm_on_pruned(&original_ext));

    let cases: Vec<(&str, Vec<&str>)> = vec![
        ("empty", vec!["test/6t_empty_rules.json"]),
        ("phase1_min", vec!["test/6t_phase1_min_rules.json"]),
        ("dmg", vec!["test/6t_dmg_rules.json"]),
        ("inv", vec!["test/6t_inv_rules.json"]),
        ("comm", vec!["test/6t_comm_rules.json"]),
        ("scale", vec!["test/6t_scale_rules.json"]),
        (
            "inv_scale",
            vec!["test/6t_inv_rules.json", "test/6t_scale_rules.json"],
        ),
    ];

    println!(
        "original add2 PPA = delay={:.6}, area={:.6}, power={:.6}",
        original_tuple.0, original_tuple.1, original_tuple.2
    );

    for (case_name, rule_paths) in cases {
        let egraph = netlist_to_egg_roots(&netlist).unwrap().egraph;
        let mut rules = Vec::new();
        for path in &rule_paths {
            rules.extend(
                JsonRules::from_path(env::current_dir().unwrap().join(path))
                    .unwrap()
                    .into_egg_rules::<language::StdCellLanguage>()
                    .unwrap(),
            );
        }

        let runner = Runner::default()
            .with_egraph(egraph)
            .with_node_limit(100000)
            .with_iter_limit(30)
            .run(&rules);
        let root_ids = base_roots.roots.clone();
        let serialized = egg_to_serialized_egraph(&runner.egraph, &root_ids);
        let json_value = serde_json::to_value(&serialized).unwrap();
        let es = ExtendedEGraph::from_base_to_extention(serialized, &lib_value, &json_value);
        let roots = es.inner.root_eclasses.clone();

        let candidates = enumerate_legal_extractions(&es, &roots);
        let candidate_count = candidates.len();
        let mut best_tuple = (f64::INFINITY, f64::INFINITY, f64::INFINITY);
        for candidate in candidates {
            let tuple = lex_cost_tuple(&candidate.dag_cost_nldm_on_pruned(&es));
            if lex_tuple_cmp(tuple, best_tuple) == Ordering::Less {
                best_tuple = tuple;
            }
        }

        println!(
            "case={} rules={:?} rewrite={}n/{}c candidates={} brute_best=({:.6}, {:.6}, {:.6}) vs original=({:.6}, {:.6}, {:.6})",
            case_name,
            rule_paths,
            runner.egraph.total_size(),
            runner.egraph.number_of_classes(),
            candidate_count,
            best_tuple.0,
            best_tuple.1,
            best_tuple.2,
            original_tuple.0,
            original_tuple.1,
            original_tuple.2,
        );
    }
}

#[cfg(feature = "ilp-cbc")]
#[test]
fn diagnose_add2_iterative_comm_then_other_rewrites() {
    use egraph_roots::EGraphRoots;
    use io::liberty::{get_direction_of_pins, read_liberty};
    use io::stdcell::read_verilog_with_lib_to_netlist;

    let lib_path = "test/asap7sc6t_SELECT_LVT_TT_nldm.lib";
    let liberty = read_liberty(lib_path).unwrap();
    let lib = get_direction_of_pins(&liberty).unwrap();
    let (original_netlist, _name) =
        read_verilog_with_lib_to_netlist("test/add2_map_abc.v", lib).unwrap();
    let lib_value = Value::String(lib_path.into());

    let stage0_roots: EGraphRoots<_, ()> = netlist_to_egg_roots(&original_netlist).unwrap();
    let stage0_serialized = SerializedEGraph::from(&stage0_roots);
    let stage0_json = serde_json::to_value(&stage0_serialized).unwrap();
    let stage0_ext =
        ExtendedEGraph::from_base_to_extention(stage0_serialized.clone(), &lib_value, &stage0_json);
    let stage0_result = choose_first_node_in_each_class(&stage0_serialized).unwrap();
    let stage0_cost = stage0_result.dag_cost_nldm_on_pruned(&stage0_ext);

    println!(
        "stage=0 rule=original delay={:.6} area={:.6} power={:.6} da={:.6} d2ap={:.6}",
        stage0_cost.components[0].into_inner(),
        stage0_cost.components[1].into_inner(),
        stage0_cost.components[2].into_inner(),
        delay_area_cost(&stage0_cost),
        delay2_area_power_cost(&stage0_cost),
    );

    let (
        comm_serialized,
        comm_best_result,
        comm_cost,
        comm_nodes,
        comm_classes,
        _comm_roots,
        _comm_candidates,
    ) = rewrite_and_bruteforce_best(&original_netlist, &lib_value, &["test/6t_comm_rules.json"]);
    let mut current_netlist = choose_result_in_serialized_egraph_into_netlist::<
        language::StdCellType,
    >(&comm_serialized, &comm_best_result)
    .unwrap();
    let mut current_cost = comm_cost.clone();
    let mut current_label = "comm".to_string();
    println!(
        "stage=1 rule=comm rewrite={}n/{}c delay={:.6} area={:.6} power={:.6} da={:.6} d2ap={:.6}",
        comm_nodes,
        comm_classes,
        current_cost.components[0].into_inner(),
        current_cost.components[1].into_inner(),
        current_cost.components[2].into_inner(),
        delay_area_cost(&current_cost),
        delay2_area_power_cost(&current_cost),
    );

    for (label, rule_paths) in [
        (
            "direct_comm_scale",
            vec!["test/6t_comm_rules.json", "test/6t_scale_rules.json"],
        ),
        (
            "direct_comm_inv_scale",
            vec![
                "test/6t_comm_rules.json",
                "test/6t_inv_rules.json",
                "test/6t_scale_rules.json",
            ],
        ),
        (
            "direct_comm_inv_dmg_scale",
            vec![
                "test/6t_comm_rules.json",
                "test/6t_inv_rules.json",
                "test/6t_dmg_rules.json",
                "test/6t_scale_rules.json",
            ],
        ),
    ] {
        let (_serialized, _result, cost, nodes, classes, _roots, _candidates) =
            rewrite_and_bruteforce_best(&original_netlist, &lib_value, &rule_paths);
        println!(
            "baseline={} rewrite={}n/{}c delay={:.6} area={:.6} power={:.6} da={:.6} d2ap={:.6}",
            label,
            nodes,
            classes,
            cost.components[0].into_inner(),
            cost.components[1].into_inner(),
            cost.components[2].into_inner(),
            delay_area_cost(&cost),
            delay2_area_power_cost(&cost),
        );
    }

    let candidates: Vec<(&str, Vec<&str>)> = vec![
        ("empty", vec!["test/6t_empty_rules.json"]),
        ("inv", vec!["test/6t_inv_rules.json"]),
        ("dmg", vec!["test/6t_dmg_rules.json"]),
        ("scale", vec!["test/6t_scale_rules.json"]),
        (
            "inv_scale",
            vec!["test/6t_inv_rules.json", "test/6t_scale_rules.json"],
        ),
    ];

    for stage_idx in 2..=4 {
        let mut stage_best: Option<(
            String,
            netlist::Netlist<language::StdCellType, ()>,
            extraction_gym::ExtendedCost,
            usize,
            usize,
        )> = None;

        for (label, rule_paths) in &candidates {
            let (serialized, best_result, cost, nodes, classes, _roots, _candidates) =
                rewrite_and_bruteforce_best(&current_netlist, &lib_value, rule_paths);
            let next_netlist = choose_result_in_serialized_egraph_into_netlist::<
                language::StdCellType,
            >(&serialized, &best_result)
            .unwrap();
            println!(
                "stage={} try={} base={} rewrite={}n/{}c delay={:.6} area={:.6} power={:.6} da={:.6} d2ap={:.6}",
                stage_idx,
                label,
                current_label,
                nodes,
                classes,
                cost.components[0].into_inner(),
                cost.components[1].into_inner(),
                cost.components[2].into_inner(),
                delay_area_cost(&cost),
                delay2_area_power_cost(&cost),
            );

            let better = match &stage_best {
                None => true,
                Some((_, _, best_cost, _, _)) => {
                    let lhs = delay2_area_power_cost(&cost);
                    let rhs = delay2_area_power_cost(best_cost);
                    if (lhs - rhs).abs() <= 1e-6 {
                        lex_tuple_cmp(lex_cost_tuple(&cost), lex_cost_tuple(best_cost))
                            == Ordering::Less
                    } else {
                        lhs < rhs
                    }
                }
            };
            if better {
                stage_best = Some((label.to_string(), next_netlist, cost, nodes, classes));
            }
        }

        let (best_label, best_netlist, best_cost, best_nodes, best_classes) = stage_best.unwrap();
        let improved =
            delay2_area_power_cost(&best_cost) + 1e-6 < delay2_area_power_cost(&current_cost);
        println!(
            "stage={} choose={} rewrite={}n/{}c improved={} delay={:.6} area={:.6} power={:.6} da={:.6} d2ap={:.6}",
            stage_idx,
            best_label,
            best_nodes,
            best_classes,
            improved,
            best_cost.components[0].into_inner(),
            best_cost.components[1].into_inner(),
            best_cost.components[2].into_inner(),
            delay_area_cost(&best_cost),
            delay2_area_power_cost(&best_cost),
        );
        if !improved {
            break;
        }
        current_label = format!("{} -> {}", current_label, best_label);
        current_netlist = best_netlist;
        current_cost = best_cost;
    }
}

#[cfg(feature = "ilp-cbc")]
#[test]
fn diagnose_iterative_comm_then_other_rewrites_on_small_cases() {
    use io::liberty::{get_direction_of_pins, read_liberty};
    use io::stdcell::read_verilog_with_lib_to_netlist;

    let lib_path = "test/asap7sc6t_SELECT_LVT_TT_nldm.lib";
    let liberty = read_liberty(lib_path).unwrap();
    let lib = get_direction_of_pins(&liberty).unwrap();
    let lib_value = Value::String(lib_path.into());
    let cases = [
        "test/add2_map_abc.v",
        "test/slice3_map_abc.v",
        "test/slice4_map_abc.v",
    ];
    let candidates: Vec<(&str, Vec<&str>)> = vec![
        ("empty", vec!["test/6t_empty_rules.json"]),
        ("inv", vec!["test/6t_inv_rules.json"]),
        ("dmg", vec!["test/6t_dmg_rules.json"]),
        ("scale", vec!["test/6t_scale_rules.json"]),
        (
            "inv_scale",
            vec!["test/6t_inv_rules.json", "test/6t_scale_rules.json"],
        ),
    ];

    for case_path in cases {
        let (original_netlist, _name) =
            read_verilog_with_lib_to_netlist(case_path, lib.clone()).unwrap();
        let base_roots: egraph_roots::EGraphRoots<_, ()> =
            netlist_to_egg_roots(&original_netlist).unwrap();
        let base_serialized = SerializedEGraph::from(&base_roots);
        let base_json = serde_json::to_value(&base_serialized).unwrap();
        let base_ext =
            ExtendedEGraph::from_base_to_extention(base_serialized.clone(), &lib_value, &base_json);
        let base_result = choose_first_node_in_each_class(&base_serialized).unwrap();
        let base_cost = base_result.dag_cost_nldm_on_pruned(&base_ext);

        println!(
            "case={} stage=0 rule=original delay={:.6} area={:.6} power={:.6} da={:.6} d2ap={:.6}",
            case_path,
            base_cost.components[0].into_inner(),
            base_cost.components[1].into_inner(),
            base_cost.components[2].into_inner(),
            delay_area_cost(&base_cost),
            delay2_area_power_cost(&base_cost),
        );

        let (
            comm_serialized,
            comm_result,
            comm_cost,
            comm_nodes,
            comm_classes,
            _comm_roots,
            comm_candidates,
        ) = rewrite_and_bruteforce_best(
            &original_netlist,
            &lib_value,
            &["test/6t_comm_rules.json"],
        );
        let mut current_netlist = choose_result_in_serialized_egraph_into_netlist::<
            language::StdCellType,
        >(&comm_serialized, &comm_result)
        .unwrap();
        let mut current_cost = comm_cost;
        let mut current_label = "comm".to_string();

        println!(
            "case={} stage=1 rule=comm rewrite={}n/{}c candidates={} delay={:.6} area={:.6} power={:.6} da={:.6} d2ap={:.6}",
            case_path,
            comm_nodes,
            comm_classes,
            comm_candidates,
            current_cost.components[0].into_inner(),
            current_cost.components[1].into_inner(),
            current_cost.components[2].into_inner(),
            delay_area_cost(&current_cost),
            delay2_area_power_cost(&current_cost),
        );

        for stage_idx in 2..=4 {
            let mut stage_best: Option<(
                String,
                netlist::Netlist<language::StdCellType, ()>,
                extraction_gym::ExtendedCost,
                usize,
                usize,
                usize,
            )> = None;

            for (label, rule_paths) in &candidates {
                let (serialized, best_result, cost, nodes, classes, _roots, candidate_count) =
                    rewrite_and_bruteforce_best(&current_netlist, &lib_value, rule_paths);
                let next_netlist = choose_result_in_serialized_egraph_into_netlist::<
                    language::StdCellType,
                >(&serialized, &best_result)
                .unwrap();

                println!(
                    "case={} stage={} try={} base={} rewrite={}n/{}c candidates={} delay={:.6} area={:.6} power={:.6} da={:.6} d2ap={:.6}",
                    case_path,
                    stage_idx,
                    label,
                    current_label,
                    nodes,
                    classes,
                    candidate_count,
                    cost.components[0].into_inner(),
                    cost.components[1].into_inner(),
                    cost.components[2].into_inner(),
                    delay_area_cost(&cost),
                    delay2_area_power_cost(&cost),
                );

                let better = match &stage_best {
                    None => true,
                    Some((_, _, best_cost, _, _, _)) => {
                        let lhs = delay2_area_power_cost(&cost);
                        let rhs = delay2_area_power_cost(best_cost);
                        if (lhs - rhs).abs() <= 1e-6 {
                            lex_tuple_cmp(lex_cost_tuple(&cost), lex_cost_tuple(best_cost))
                                == Ordering::Less
                        } else {
                            lhs < rhs
                        }
                    }
                };
                if better {
                    stage_best = Some((
                        label.to_string(),
                        next_netlist,
                        cost,
                        nodes,
                        classes,
                        candidate_count,
                    ));
                }
            }

            let (best_label, best_netlist, best_cost, best_nodes, best_classes, best_candidates) =
                stage_best.unwrap();
            let improved =
                delay2_area_power_cost(&best_cost) + 1e-6 < delay2_area_power_cost(&current_cost);
            println!(
                "case={} stage={} choose={} rewrite={}n/{}c candidates={} improved={} delay={:.6} area={:.6} power={:.6} da={:.6} d2ap={:.6}",
                case_path,
                stage_idx,
                best_label,
                best_nodes,
                best_classes,
                best_candidates,
                improved,
                best_cost.components[0].into_inner(),
                best_cost.components[1].into_inner(),
                best_cost.components[2].into_inner(),
                delay_area_cost(&best_cost),
                delay2_area_power_cost(&best_cost),
            );

            if !improved {
                break;
            }
            current_label = format!("{} -> {}", current_label, best_label);
            current_netlist = best_netlist;
            current_cost = best_cost;
        }
    }
}
