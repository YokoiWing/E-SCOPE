use crate::io::bench::parse_bench;
use crate::io::liberty::Library;
use crate::io::verilog::module;
use crate::language::StdCellType;
use crate::netlist::Netlist;
use itertools::Itertools;
use libertyparse::PinDirection;
use petgraph::algo::toposort;
use petgraph::graph::NodeIndex;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;
use std::collections::hash_map::Entry;
use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::Path;

pub fn read_bench_to_netlist<P: AsRef<Path>>(path: P) -> Result<Netlist<StdCellType, ()>, String> {
    let content =
        fs::read_to_string(path.as_ref()).map_err(|e| format!("Error reading file: {}", e))?;
    let (_, bench_file) = parse_bench(&content).map_err(|e| format!("Parse error: {:?}", e))?;
    let mut netlist: Netlist<StdCellType, ()> = Default::default();
    let mut symbol_to_nid: FxHashMap<&str, NodeIndex> = Default::default();

    fn handle_symbol(symbol: &str) -> StdCellType {
        match symbol {
            "0" => StdCellType::Bool(false),
            "1" => StdCellType::Bool(true),
            s => StdCellType::Symbol(s.into()),
        }
    }
    for input in bench_file.inputs {
        if let Entry::Vacant(entry) = symbol_to_nid.entry(input) {
            let nid = netlist.graph.add_node(handle_symbol(input));
            entry.insert(nid);
            netlist.leaves.push(nid);
        } else {
            return Err(format!("input {:?} exists already", input).into());
        }
    }
    for gate in bench_file.gates {
        let oid = *symbol_to_nid
            .entry(gate.output)
            .or_insert_with(|| netlist.graph.add_node(handle_symbol(gate.gate_type)));
        for input in gate.inputs {
            let iid = *symbol_to_nid
                .entry(input)
                .or_insert_with(|| netlist.graph.add_node(handle_symbol(input)));
            netlist.graph.add_edge(oid, iid, ());
        }
    }
    for output in bench_file.outputs {
        let oid = netlist.graph.add_node(handle_symbol(output));
        let iid = *symbol_to_nid
            .get(output)
            .ok_or(format!("output {} not found", output))?;
        netlist.graph.add_edge(oid, iid, ());
        netlist.roots.push(oid);
    }
    Ok(netlist)
}

pub fn read_verilog_with_lib_to_netlist_with_symbols<P: AsRef<Path>>(
    verilog_path: P,
    lib: Library,
) -> Result<
    (
        Netlist<StdCellType, ()>,
        String,
        FxHashMap<NodeIndex, String>,
    ),
    String,
> {
    let content = fs::read_to_string(verilog_path.as_ref())
        .map_err(|e| format!("Error reading file: {}", e))?;
    let (_, parsed_module) = module(&content).map_err(|e| format!("Parse error: {:?}", e))?;
    parsed_module.verify()?;
    // std::fs::write(
    //     "dot/mul32_map_genus.v.module",
    //     format!("{:#?}", parsed_module),
    // )
    // .unwrap();
    let mut netlist: Netlist<StdCellType, ()> = Default::default();
    let mut symbol_to_nid: FxHashMap<String, NodeIndex> = Default::default();
    let mut nid_to_symbol: FxHashMap<NodeIndex, String> = Default::default();

    for input in parsed_module.inputs {
        if let Some((low, high)) = input.bit_range {
            (low..=high).for_each(|bit| {
                let symbol = format!("{}[{}]", input.name, bit);
                let nid = netlist
                    .graph
                    .add_node(StdCellType::Symbol((&symbol).into()));
                symbol_to_nid.insert(symbol, nid);
                nid_to_symbol.insert(nid, format!("{}[{}]", input.name, bit));
                netlist.leaves.push(nid);
            });
        } else {
            let nid = netlist
                .graph
                .add_node(StdCellType::Symbol(input.name.into()));
            symbol_to_nid.insert(input.name.into(), nid);
            nid_to_symbol.insert(nid, input.name.into());
            netlist.leaves.push(nid);
        }
    }

    // insert nodes first
    for gate in parsed_module.gates.iter() {
        if let Some(pins) = lib.get(gate.gate_type) {
            for connection in gate.connections.iter() {
                match pins.get(connection.gate_pin) {
                    Some(PinDirection::O) => {
                        let symbol = format!("{}", connection.bit);
                        let nid = netlist
                            .graph
                            .add_node(StdCellType::Symbol(gate.gate_type.into()));
                        symbol_to_nid.insert(symbol.clone(), nid);
                        nid_to_symbol.insert(nid, symbol);
                    }
                    _ => continue,
                }
            }
        } else {
            return Err(format!(
                "gate {} not found in lib, when adding nodes",
                gate.gate_type
            )
            .into());
        }
    }
    // insert edges after
    for gate in parsed_module.gates {
        if let Some(pins) = lib.get(gate.gate_type) {
            let mut oids: Vec<_> = Default::default();
            // Named Verilog port connections are unordered.  Retain their
            // physical pin identity here, then add graph edges in Liberty pin
            // order below.  Using textual connection order would silently
            // turn `.B(b), .A(a)` into A=b/B=a inside Timing V2.
            let mut input_by_pin: FxHashMap<String, NodeIndex> = Default::default();
            for connection in gate.connections {
                match pins.get(connection.gate_pin) {
                    Some(PinDirection::O) => {
                        let symbol = format!("{}", connection.bit);
                        oids.push(
                            *symbol_to_nid
                                .get(&symbol)
                                .ok_or(format!("output {} not found", symbol))?,
                        );
                    }
                    Some(PinDirection::I) => {
                        let symbol = format!("{}", connection.bit);
                        let iid = if let Some(nid) = symbol_to_nid.get(&symbol) {
                            *nid
                        } else if symbol == "0" || symbol == "1" {
                            let is_one = symbol == "1";
                            let nid = netlist.graph.add_node(StdCellType::Bool(is_one));
                            symbol_to_nid.insert(symbol, nid);
                            nid_to_symbol.insert(nid, format!("const_{}", usize::from(is_one)));
                            nid
                        } else {
                            return Err(format!("input {} not found", symbol));
                        };
                        if input_by_pin
                            .insert(connection.gate_pin.into(), iid)
                            .is_some()
                        {
                            return Err(format!(
                                "duplicate connection for input pin {} of gate {}",
                                connection.gate_pin, gate.gate_type
                            ));
                        }
                    }
                    Some(PinDirection::Unsupported(s)) => {
                        return Err(format!(
                            "gate {} has pin {} with unsupported direction {}",
                            gate.gate_type, connection.gate_pin, s
                        )
                        .into());
                    }
                    Some(PinDirection::Unspecified) => {
                        return Err(format!(
                            "gate {} has pin {} with unspecified direction",
                            gate.gate_type, connection.gate_pin
                        )
                        .into());
                    }
                    None => {
                        return Err(format!(
                            "pin {} not found in gate {}",
                            connection.gate_pin, gate.gate_type
                        )
                        .into());
                    }
                }
            }
            if oids.len() != 1 {
                return Err(format!(
                    "Now only support one output gate, got {} outputs for gate {}",
                    oids.len(),
                    gate.gate_type
                )
                .into());
            }
            if input_by_pin.len() != pins.len() - 1 {
                return Err(format!(
                    "Inconsistent number of inputs: got {}, should be {}",
                    input_by_pin.len(),
                    pins.len() - 1
                ));
            }
            let oid = oids[0];
            for (pin, direction) in pins {
                if *direction != PinDirection::I {
                    continue;
                }
                let iid = *input_by_pin.get(pin).ok_or_else(|| {
                    format!(
                        "input pin {} of gate {} is not connected",
                        pin, gate.gate_type
                    )
                })?;
                netlist.graph.add_edge(oid, iid, ());
            }
        } else {
            return Err(format!("gate {} not found in lib", gate.gate_type).into());
        }
    }

    // Continuous assignments are aliases in the mapped netlists emitted by
    // our writer.  Resolve their destination name to the already-created
    // source node before creating output root symbols.
    for assignment in parsed_module.assignments {
        let lhs = format!("{}", assignment.lhs);
        let rhs = format!("{}", assignment.rhs);
        let source = if let Some(nid) = symbol_to_nid.get(&rhs) {
            *nid
        } else if rhs == "0" || rhs == "1" {
            let is_one = rhs == "1";
            let nid = netlist.graph.add_node(StdCellType::Bool(is_one));
            symbol_to_nid.insert(rhs, nid);
            nid_to_symbol.insert(nid, format!("const_{}", usize::from(is_one)));
            nid
        } else {
            return Err(format!("assign source {} not found", rhs));
        };
        symbol_to_nid.insert(lhs, source);
    }

    for output in parsed_module.outputs {
        if let Some((low, high)) = output.bit_range {
            for bit in low..=high {
                let symbol = format!("{}[{}]", output.name, bit);
                let oid = netlist
                    .graph
                    .add_node(StdCellType::Symbol((&symbol).into()));
                let iid = *symbol_to_nid
                    .get(&symbol)
                    .ok_or(format!("output {} not found", symbol))?;
                netlist.graph.add_edge(oid, iid, ());
                netlist.roots.push(oid);
            }
        } else {
            let symbol = output.name;
            let oid = netlist.graph.add_node(StdCellType::Symbol(symbol.into()));
            let iid = *symbol_to_nid
                .get(symbol)
                .ok_or(format!("output {} not found", symbol))?;
            netlist.graph.add_edge(oid, iid, ());
            netlist.roots.push(oid);
        }
    }
    Ok((netlist, parsed_module.name.into(), nid_to_symbol))
}

pub fn read_verilog_with_lib_to_netlist<P: AsRef<Path>>(
    verilog_path: P,
    lib: Library,
) -> Result<(Netlist<StdCellType, ()>, String), String> {
    let (netlist, module, _) = read_verilog_with_lib_to_netlist_with_symbols(verilog_path, lib)?;
    Ok((netlist, module))
}

pub fn write_verilog_from_netlist_with_lib<P: AsRef<Path>>(
    verilog_path: P,
    netlist: Netlist<StdCellType, ()>,
    module_name: &str,
    lib: Library,
) -> Result<(), String> {
    write_verilog_from_netlist_with_lib_ref(verilog_path, &netlist, module_name, &lib)
}

/// Borrowing variant used by batched local materialization.  The historical
/// writer consumed both arguments, which forced every candidate in a batch to
/// deep-clone the complete pin-direction library even though it is immutable.
/// Keeping the formatting implementation here also guarantees byte-identical
/// Verilog receipts for the old and new call paths.
pub fn write_verilog_from_netlist_with_lib_ref<P: AsRef<Path>>(
    verilog_path: P,
    netlist: &Netlist<StdCellType, ()>,
    module_name: &str,
    lib: &Library,
) -> Result<(), String> {
    let mut file = File::create(verilog_path).map_err(|e| e.to_string())?;
    let normalize = |s: String| s.replace(|c| c == '[' || c == ']', "_");
    let inputs = netlist
        .leaves
        .iter()
        .filter_map(|&n| netlist.graph[n].to_string_as_io().map(normalize))
        .collect_vec();
    let outputs = netlist
        .roots
        .iter()
        .filter_map(|&n| netlist.graph[n].to_string_as_io().map(normalize))
        .collect_vec();
    let mut io = inputs.clone();
    io.extend_from_slice(&outputs);
    let i_nodes = FxHashSet::from_iter(netlist.leaves.clone());
    let o_nodes = FxHashSet::from_iter(netlist.roots.clone());
    || -> std::io::Result<()> {
        writeln!(file, "module {}({});", module_name, io.join(", "))?;
        for input in inputs {
            writeln!(file, "  input {};", input)?;
            writeln!(file, "  wire {};", input)?;
        }
        for output in outputs {
            writeln!(file, "  output {};", output)?;
            writeln!(file, "  wire {};", output)?;
        }
        Ok(())
    }()
    .map_err(|e| e.to_string())?;
    let order =
        toposort(&netlist.graph, None).map_err(|e| format!("Graph contains cycle: {:?}", e))?;
    for &nid in order.iter().rev() {
        if i_nodes.contains(&nid) || o_nodes.contains(&nid) {
            continue;
        }
        writeln!(file, "  wire _wire_{};", nid.index()).map_err(|e| e.to_string())?;
    }
    for &nid in order.iter().rev() {
        if i_nodes.contains(&nid) || o_nodes.contains(&nid) {
            continue;
        }
        let mut in_pins: VecDeque<_> = netlist.inputs(nid).collect();
        let weight = netlist.graph[nid].clone();
        match weight {
            StdCellType::Bool(b) => {
                writeln!(
                    file,
                    "  assign _wire_{} = {};",
                    nid.index(),
                    if b { 1 } else { 0 }
                )
                .map_err(|e| e.to_string())?;
            }
            StdCellType::Symbol(s) => {
                let pins = lib
                    .get(s.into())
                    .ok_or(format!("{} is not in lib", s).to_string())?;
                let mut pin_string_vec = vec![];
                let out_pin_num = pins.values().filter(|dir| **dir == PinDirection::O).count();
                if out_pin_num != 1 {
                    return Err(format!(
                        "We only support 1 out pin now, got {} for cell {}",
                        out_pin_num, s
                    )
                    .to_string());
                }
                for (name, dir) in pins {
                    match dir {
                        PinDirection::O => {
                            pin_string_vec
                                .push(format!(".{} (_wire_{})", name, nid.index()).to_string());
                        }
                        PinDirection::I => {
                            let in_nid = in_pins.pop_front().ok_or(
                                format!(
                                    "No enough in_pin, cell {} pin {} node {}",
                                    s,
                                    name,
                                    nid.index()
                                )
                                .to_string(),
                            )?;
                            if i_nodes.contains(&in_nid) {
                                pin_string_vec.push(format!(
                                    ".{} ({})",
                                    name,
                                    normalize(netlist.graph[in_nid].to_string()).to_string()
                                ));
                            } else {
                                pin_string_vec.push(
                                    format!(".{} (_wire_{})", name, in_nid.index()).to_string(),
                                );
                            }
                        }
                        PinDirection::Unspecified => {
                            return Err(format!(
                                "Unspecified pin direction in cell {} pin {}",
                                s, name
                            )
                            .into());
                        }
                        PinDirection::Unsupported(content) => {
                            return Err(format!(
                                "Unsupported pin direction in cell {} pin {} with content {}",
                                s, name, content
                            )
                            .into());
                        }
                    }
                }
                writeln!(
                    file,
                    "  {} _nid_{}({});",
                    s,
                    nid.index(),
                    pin_string_vec.join(", ")
                )
                .map_err(|e| e.to_string())?;
            }
        }
    }
    for &oid in &netlist.roots {
        let src = netlist
            .inputs(oid)
            .next()
            .ok_or(format!("output node {} has no source", oid.index()))?;
        let out_name = normalize(netlist.graph[oid].to_string());
        let rhs = if i_nodes.contains(&src) || o_nodes.contains(&src) {
            normalize(netlist.graph[src].to_string())
        } else {
            format!("_wire_{}", src.index())
        };
        writeln!(file, "  assign {} = {};", out_name, rhs).map_err(|e| e.to_string())?;
    }
    writeln!(file, "endmodule").map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::liberty::{get_direction_of_pins, read_liberty};
    use petgraph::dot::{Config, Dot};
    use petgraph::graph::NodeIndex;
    use std::env;

    #[test]
    fn test_read_add2_bench_netlist() {
        let netlist = read_bench_to_netlist("test/add2.bench").unwrap();
        fs::write(
            env::current_dir().unwrap().join("dot/test_add2_bench.dot"),
            format!(
                "{:?}",
                Dot::with_config(&netlist.graph, &[Config::EdgeNoLabel])
            ),
        )
        .unwrap();
    }

    #[test]
    fn test_read_verilog_with_lib_to_netlist_mul32_map_genus() {
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let lib = get_direction_of_pins(&liberty).unwrap();
        let (netlist, name) =
            read_verilog_with_lib_to_netlist("test/mul32_map_genus.v", lib).unwrap();
        assert_eq!(name, "Multiplier");
        fs::write(
            env::current_dir()
                .unwrap()
                .join("dot/test_mul32_map_genus_v.dot"),
            format!(
                "{:?}",
                Dot::with_config(&netlist.graph, &[Config::EdgeNoLabel])
            ),
        )
        .unwrap();
    }

    #[test]
    fn test_read_verilog_with_lib_to_netlist_add2_abc() {
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let lib = get_direction_of_pins(&liberty).unwrap();
        let (netlist, name) = read_verilog_with_lib_to_netlist("test/add2_map_abc.v", lib).unwrap();
        assert_eq!(name, "add2");
        fs::write(
            env::current_dir()
                .unwrap()
                .join("dot/test_add2_map_abc_v.dot"),
            format!(
                "{:?}",
                Dot::with_config(&netlist.graph, &[Config::EdgeNoLabel])
            ),
        )
        .unwrap();
        assert_eq!(
            netlist.leaves,
            vec![
                NodeIndex::new(0),
                NodeIndex::new(1),
                NodeIndex::new(2),
                NodeIndex::new(3)
            ]
        );
        assert_eq!(
            netlist.roots,
            vec![NodeIndex::new(16), NodeIndex::new(17), NodeIndex::new(18)]
        );
    }

    #[test]
    fn test_write_verilog_from_netlist_with_lib_add2_abc() {
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let lib = get_direction_of_pins(&liberty).unwrap();
        let (netlist, name) =
            read_verilog_with_lib_to_netlist("test/add2_map_abc.v", lib.clone()).unwrap();
        write_verilog_from_netlist_with_lib("verilog/test_add2_map_abc.v", netlist, &name, lib)
            .unwrap()
    }

    #[test]
    fn named_input_connection_text_order_does_not_change_physical_pin_mapping() {
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        let pins = get_direction_of_pins(&liberty).unwrap();
        let dir = std::env::temp_dir();
        let normal = dir.join(format!("egg_pin_order_normal_{}.v", std::process::id()));
        let reversed = dir.join(format!("egg_pin_order_reversed_{}.v", std::process::id()));
        let prefix = "module pin_order(a, b, y); input a, b; output y; wire a, b, y; ";
        fs::write(
            &normal,
            format!("{prefix}NAND2x1_ASAP7_6t_L u0(.A(a), .B(b), .Y(y)); endmodule\n"),
        )
        .unwrap();
        fs::write(
            &reversed,
            format!("{prefix}NAND2x1_ASAP7_6t_L u0(.Y(y), .B(b), .A(a)); endmodule\n"),
        )
        .unwrap();

        let (normal_netlist, _) = read_verilog_with_lib_to_netlist(&normal, pins.clone()).unwrap();
        let (reversed_netlist, _) = read_verilog_with_lib_to_netlist(&reversed, pins).unwrap();
        let ordered_inputs = |netlist: &Netlist<StdCellType, ()>| {
            let gate = netlist
                .graph
                .node_indices()
                .find(|node| netlist.graph[*node].to_string() == "NAND2x1_ASAP7_6t_L")
                .unwrap();
            netlist
                .inputs(gate)
                .map(|node| netlist.graph[node].to_string())
                .collect_vec()
        };
        assert_eq!(ordered_inputs(&normal_netlist), vec!["a", "b"]);
        assert_eq!(ordered_inputs(&reversed_netlist), vec!["a", "b"]);

        fs::remove_file(normal).unwrap();
        fs::remove_file(reversed).unwrap();
    }
}
