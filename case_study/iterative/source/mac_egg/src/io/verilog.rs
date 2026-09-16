use nom::{
    IResult, Parser,
    branch::alt,
    bytes::complete::{tag, take_until},
    character::complete::{
        alpha1, alphanumeric1, char, digit1, line_ending, multispace0, multispace1, not_line_ending,
    },
    combinator::{eof, map, map_res, opt, recognize, value},
    multi::{many0, many1, separated_list0, separated_list1},
    sequence::{delimited, pair, preceded, separated_pair, terminated},
};
use rustc_hash::FxHashSet;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct Wire<'a> {
    pub name: &'a str,
    pub bit_range: Option<(usize, usize)>,
}

#[derive(Debug, PartialEq)]
pub struct Bit<'a> {
    pub name: &'a str,
    pub bit_index: Option<usize>,
}

impl fmt::Display for Bit<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(bit_index) = self.bit_index {
            write!(f, "{}[{}]", self.name, bit_index)
        } else {
            write!(f, "{}", self.name)
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct Connection<'a> {
    pub gate_pin: &'a str,
    pub bit: Bit<'a>,
}

#[derive(Debug, PartialEq, Default)]
pub struct Gate<'a> {
    pub gate_type: &'a str,
    pub name: &'a str,
    pub connections: Vec<Connection<'a>>,
}

/// A continuous assignment used by writer round-trips, e.g. `assign y = n;`.
#[derive(Debug, PartialEq)]
pub struct Assignment<'a> {
    pub lhs: Bit<'a>,
    pub rhs: Bit<'a>,
}

#[derive(Debug, PartialEq, Default)]
pub struct Module<'a> {
    pub name: &'a str,
    pub ports: Vec<&'a str>,
    pub inputs: Vec<Wire<'a>>,
    pub outputs: Vec<Wire<'a>>,
    pub wires: Vec<Wire<'a>>,
    pub gates: Vec<Gate<'a>>,
    pub assignments: Vec<Assignment<'a>>,
}

fn one_line_comment(input: &str) -> IResult<&str, &str> {
    delimited(tag("//"), not_line_ending, alt((line_ending, eof))).parse(input)
}

fn multi_line_comment(input: &str) -> IResult<&str, &str> {
    delimited(tag("/*"), take_until("*/"), tag("*/")).parse(input)
}
fn skip_white_spaces0(input: &str) -> IResult<&str, &str> {
    value(
        "",
        many0(alt((multispace1, one_line_comment, multi_line_comment))),
    )
    .parse(input)
}
fn skip_white_spaces1(input: &str) -> IResult<&str, &str> {
    value(
        "",
        many1(alt((multispace1, one_line_comment, multi_line_comment))),
    )
    .parse(input)
}

fn identifier(input: &str) -> IResult<&str, &str> {
    recognize(pair(
        alt((alpha1, tag("_"))),
        many0(alt((alphanumeric1, tag("_")))),
    ))
    .parse(input)
}

fn bit_range(input: &str) -> IResult<&str, Option<(usize, usize)>> {
    alt((
        delimited(
            (char('['), multispace0),
            map(
                separated_pair(
                    map_res(digit1, usize::from_str),
                    (multispace0, char(':'), multispace0),
                    map_res(digit1, usize::from_str),
                ),
                |(left, right)| {
                    if left > right {
                        Some((right, left))
                    } else {
                        Some((left, right))
                    }
                },
            ),
            (multispace0, char(']')),
        ),
        value(None, multispace0),
    ))
    .parse(input)
}

fn module_ports(input: &str) -> IResult<&str, Vec<&str>> {
    delimited(
        char('('),
        separated_list0(
            char(','),
            delimited(skip_white_spaces0, identifier, skip_white_spaces0),
        ),
        char(')'),
    )
    .parse(input)
}

fn module_declaration(input: &str) -> IResult<&str, (&str, Vec<&str>)> {
    terminated(
        separated_pair(identifier, skip_white_spaces0, module_ports),
        (skip_white_spaces0, char(';')),
    )
    .parse(input)
}

#[derive(Debug, PartialEq, Default)]
struct Wires<'a> {
    bit_range: Option<(usize, usize)>,
    names: Vec<&'a str>,
}

fn wires_declaration(input: &str) -> IResult<&str, Wires<'_>> {
    map(
        separated_pair(
            bit_range,
            skip_white_spaces0,
            separated_list1(
                char(','),
                delimited(skip_white_spaces0, identifier, skip_white_spaces0),
            ),
        ),
        |(bit_range, names)| Wires { bit_range, names },
    )
    .parse(input)
}

fn input_declaration(input: &str) -> IResult<&str, Wires<'_>> {
    delimited(
        (tag("input"), skip_white_spaces0),
        wires_declaration,
        (skip_white_spaces0, char(';')),
    )
    .parse(input)
}

fn output_declaration(input: &str) -> IResult<&str, Wires<'_>> {
    delimited(
        (tag("output"), skip_white_spaces0),
        wires_declaration,
        (skip_white_spaces0, char(';')),
    )
    .parse(input)
}

fn wire_declaration(input: &str) -> IResult<&str, Wires<'_>> {
    delimited(
        (tag("wire"), skip_white_spaces0),
        wires_declaration,
        (skip_white_spaces0, char(';')),
    )
    .parse(input)
}

fn identifier_with_possible_bit_choice(input: &str) -> IResult<&str, Bit<'_>> {
    map(
        (
            alt((
                value("0", tag("1'b0")),
                value("1", tag("1'b1")),
                value("0", tag("0")),
                value("1", tag("1")),
                identifier,
            )),
            opt(delimited(
                char('['),
                map_res(digit1, usize::from_str),
                char(']'),
            )),
        ),
        |(name, bit_index)| Bit { name, bit_index },
    )
    .parse(input)
}

fn port_connection(input: &str) -> IResult<&str, Connection<'_>> {
    map(
        separated_pair(
            preceded(char('.'), identifier),
            skip_white_spaces0,
            delimited(
                (char('('), skip_white_spaces0),
                identifier_with_possible_bit_choice,
                (skip_white_spaces0, char(')')),
            ),
        ),
        |(gate_pin, bit)| Connection { gate_pin, bit },
    )
    .parse(input)
}

fn port_connections(input: &str) -> IResult<&str, Vec<Connection<'_>>> {
    delimited(
        (char('('), skip_white_spaces0),
        separated_list0(
            char(','),
            delimited(skip_white_spaces0, port_connection, skip_white_spaces0),
        ),
        (skip_white_spaces0, char(')')),
    )
    .parse(input)
}

fn gate_declaration(input: &str) -> IResult<&str, Gate<'_>> {
    let (input, (gate_type, _, name, _, connections, _, _)) = (
        identifier,
        skip_white_spaces0,
        identifier,
        skip_white_spaces0,
        port_connections,
        skip_white_spaces0,
        tag(";"),
    )
        .parse(input)?;
    Ok((
        input,
        Gate {
            gate_type,
            name,
            connections,
        },
    ))
}

fn assign_declaration(input: &str) -> IResult<&str, Assignment<'_>> {
    map(
        (
            tag("assign"),
            skip_white_spaces1,
            identifier_with_possible_bit_choice,
            skip_white_spaces0,
            char('='),
            skip_white_spaces0,
            identifier_with_possible_bit_choice,
            skip_white_spaces0,
            char(';'),
        ),
        |(_, _, lhs, _, _, _, rhs, _, _)| Assignment { lhs, rhs },
    )
    .parse(input)
}

#[derive(Debug, PartialEq, Default)]
struct ModuleBody<'a> {
    inputs: Vec<Wire<'a>>,
    outputs: Vec<Wire<'a>>,
    wires: Vec<Wire<'a>>,
    gates: Vec<Gate<'a>>,
    assignments: Vec<Assignment<'a>>,
}

fn module_body(input: &str) -> IResult<&str, ModuleBody<'_>> {
    let mut body = ModuleBody::default();

    let mut remaining = input;
    loop {
        if let Ok((r, _)) = skip_white_spaces1(remaining) {
            remaining = r;
        } else if let Ok((r, input_port)) = input_declaration(remaining) {
            for name in input_port.names {
                body.inputs.push(Wire {
                    name,
                    bit_range: input_port.bit_range,
                });
            }
            remaining = r;
        } else if let Ok((r, output_port)) = output_declaration(remaining) {
            for name in output_port.names {
                body.outputs.push(Wire {
                    name,
                    bit_range: output_port.bit_range,
                });
            }
            remaining = r;
        } else if let Ok((r, wire)) = wire_declaration(remaining) {
            for name in wire.names {
                body.wires.push(Wire {
                    name,
                    bit_range: wire.bit_range,
                });
            }
            remaining = r;
        } else if let Ok((r, gate)) = gate_declaration(remaining) {
            body.gates.push(gate);
            remaining = r;
        } else if let Ok((r, assignment)) = assign_declaration(remaining) {
            body.assignments.push(assignment);
            remaining = r;
        } else {
            break;
        }
    }
    let mut wire_set = FxHashSet::default();
    let mut wire_vec: Vec<Wire> = Vec::new();
    for input in body.inputs.iter() {
        if wire_set.insert(input) {
            wire_vec.push(input.clone());
        }
    }
    for output in body.outputs.iter() {
        if wire_set.insert(output) {
            wire_vec.push(output.clone());
        }
    }
    for wire in body.wires.iter() {
        if wire_set.insert(wire) {
            wire_vec.push(wire.clone());
        }
    }
    body.wires = wire_vec;

    Ok((remaining, body))
}

pub fn module(input: &str) -> IResult<&str, Module<'_>> {
    map(
        delimited(
            (skip_white_spaces0, tag("module"), skip_white_spaces0),
            (module_declaration, module_body),
            (skip_white_spaces0, tag("endmodule"), skip_white_spaces0),
        ),
        |(
            (name, ports),
            ModuleBody {
                inputs,
                outputs,
                wires,
                gates,
                assignments,
            },
        )| {
            Module {
                name,
                ports,
                inputs,
                outputs,
                wires,
                gates,
                assignments,
            }
        },
    )
    .parse(input)
}

impl<'a> Module<'a> {
    pub fn verify(&self) -> Result<(), String> {
        // shortcut
        if self.ports.len() != self.inputs.len() + self.outputs.len() {
            return Err(format!(
                "ports.len {} != inputs.len {} + outputs.len {}",
                self.ports.len(),
                self.inputs.len(),
                self.outputs.len()
            ));
        }
        // 检查各Vec是否有重复元素 (条件1)
        let port_set: HashSet<_> = self.ports.iter().collect();
        let input_set: HashSet<_> = self.inputs.iter().map(|i| i.name).collect();
        let output_set: HashSet<_> = self.outputs.iter().map(|o| o.name).collect();

        // 如果有任何一个集合的大小与原始Vec不同，说明有重复
        if port_set.len() != self.ports.len()
            || input_set.len() != self.inputs.len()
            || output_set.len() != self.outputs.len()
        {
            return Err(format!(
                "Repeated items in ports ({}/{}), inputs ({}/{}) or outputs ({}/{})",
                port_set.len(),
                self.ports.len(),
                input_set.len(),
                self.inputs.len(),
                output_set.len(),
                self.outputs.len()
            ));
        }

        // 检查inputs和outputs无交集 (条件3)
        if input_set.intersection(&output_set).next().is_some() {
            return Err("Exists intersection between inputs and outputs".into());
        }

        // 计算并集大小，如果长度不等可快速失败
        let union_len = input_set.len() + output_set.len();
        if port_set.len() != union_len {
            return Err(format!(
                "port_set.len {} != input_set.len {} + output_set.len {}",
                port_set.len(),
                input_set.len(),
                output_set.len()
            ));
        }

        // 验证ports严格等于inputs和outputs的并集 (条件2)
        if port_set != input_set.union(&output_set).collect() {
            return Err("port_set != input_set union output_set".into());
        }

        // 验证inputs和outputs属于wires
        let wire_set: HashSet<_> = self.wires.iter().map(|wire| wire.name).collect();

        if !input_set.is_subset(&wire_set) || !output_set.is_subset(&wire_set) {
            return Err("input_set or output_set is not subset of wire_set".into());
        }

        // 验证connections中所有name的集合严格等于wire
        let mut connection_set = HashSet::new();

        for gate in &self.gates {
            for connection in &gate.connections {
                if connection.bit.name != "0" && connection.bit.name != "1" {
                    connection_set.insert(connection.bit.name);
                }
            }
        }

        if !connection_set.is_subset(&wire_set) {
            return Err("connection_set is not subset of wire_set".into());
        }

        // 验证wires中没有越界
        // let mut wire_map: HashMap<_, _> = self
        let mut wire_map: HashMap<_, _> = self
            .wires
            .iter()
            .map(|wire| {
                (
                    wire.name,
                    // wire.bit_range.map(|(low, high)| {
                    //     ((low, high), FixedBitSet::with_capacity(high - low + 1))
                    // }),
                    wire.bit_range,
                )
            })
            .collect();
        wire_map.insert("0", None);
        wire_map.insert("1", None);

        for input in &self.inputs {
            // if input.bit_range != wire_map[input.name].as_ref().map(|&(range, _)| range) {
            if input.bit_range != wire_map[&input.name] {
                return Err("input range != wire range".into());
            }
        }
        for output in &self.outputs {
            // if output.bit_range != wire_map[output.name].as_ref().map(|&(range, _)| range) {
            if output.bit_range != wire_map[&output.name] {
                return Err("output range != wire range".into());
            }
        }
        for gate in &self.gates {
            for connection in &gate.connections {
                match (
                    connection.bit.bit_index,
                    // wire_map.get_mut(connection.bit.name),
                    wire_map[connection.bit.name],
                ) {
                    // (None, Some(None)) => continue,
                    // (Some(bit_index), Some(Some(((low, high), used_bits)))) => {
                    //     if bit_index < *low || bit_index > *high {
                    //         return Err("bit index out of range".into());
                    //     } else {
                    //         used_bits.insert(bit_index - *low);
                    //     }
                    // }
                    // (x, y) => return Err(format!(
                    //     "Invalid index {:?} and connection {:?}",
                    //     x, y
                    // )),
                    (None, None) => continue,
                    (Some(bit_index), Some((low, high))) => {
                        if bit_index < low && bit_index > high {
                            return Err("bit index out of range".into());
                        }
                    }
                    (x, y) => return Err(format!("Invalid index {:?} and connection {:?}", x, y)),
                }
            }
        }
        // for value in wire_map.values() {
        //     if let Some(((_, _), used_bits)) = value {
        //         if !used_bits.contains_all_in_range(..) {
        //             return Err("Not all bits is used".into());
        //         }
        //     }
        // }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nom::Err;
    use nom::error::Error;
    use nom::error::ErrorKind;
    use std::{env, fs};

    #[test]
    fn test_skip_white_spaces() {
        let input = r#"
// Generated by Cadence Genus(TM) Synthesis Solution 19.12-s121_1
// Generated on: Apr 15 2025 17:39:31 CST (Apr 15 2025 09:39:31 UTC)

// Verification Directory fv/Multiplier

/* asdasdasd */

/*
asdasd

asd
 */

useful data"#;
        assert_eq!(skip_white_spaces0(input), Ok(("useful data", "")));
    }

    #[test]
    fn test_identifier() {
        let input = r#"XOR2x2_ASAP7_6t_L asd"#;
        assert_eq!(identifier(input), Ok((" asd", "XOR2x2_ASAP7_6t_L")));
        let input = r#"0ad"#;
        assert_eq!(
            identifier(input),
            Err(Err::Error(Error {
                input: "0ad",
                code: ErrorKind::Tag
            }))
        );
    }

    #[test]
    fn test_bit_range() {
        let result = Ok(("", Some((0, 31))));
        let input = r#"[0:31]"#;
        assert_eq!(bit_range(input), result);
        let input = r#"[ 0 : 31 ]"#;
        assert_eq!(bit_range(input), result);
        let input = r#"[ 0  :  31 ]"#;
        assert_eq!(bit_range(input), result);
        let input = r#"[31:0]"#;
        assert_eq!(bit_range(input), result);
    }

    #[test]
    fn test_module_declaration() {
        let result = Ok(("", ("Multiplier", vec!["clk", "a", "b", "mult"])));
        let input = r#"Multiplier(clk, a, b, mult);"#;
        assert_eq!(module_declaration(input), result);
        let input = r#"Multiplier  ( clk  , 
a , b, mult
)
;"#;
        assert_eq!(module_declaration(input), result);
    }

    #[test]
    fn test_io_wire_declaration() {
        let result = Ok((
            "",
            Wires {
                bit_range: Some((0, 31)),
                names: vec!["a", "b"],
            },
        ));
        let input = r#"input [31:0] a, b;"#;
        assert_eq!(input_declaration(input), result);
        let input = r#"input   [ 31 : 0] a , // a
        b /* b
        */
        
        ;"#;
        assert_eq!(input_declaration(input), result);
        let input = r#"output [31:0] a, b;"#;
        assert_eq!(output_declaration(input), result);
        let input = r#"wire [31:0] a, b;"#;
        assert_eq!(wire_declaration(input), result);
    }

    #[test]
    fn test_port_connection() {
        let result = Ok((
            "",
            Connection {
                gate_pin: "A",
                bit: Bit {
                    name: "a",
                    bit_index: Some(0),
                },
            },
        ));
        let input = r#".A (a[0])"#;
        assert_eq!(port_connection(input), result);
        let input = r#".A (  a[0]  )"#;
        assert_eq!(port_connection(input), result);

        let constant_zero = Ok((
            "",
            Connection {
                gate_pin: "A1",
                bit: Bit {
                    name: "0",
                    bit_index: None,
                },
            },
        ));
        assert_eq!(port_connection(".A1 (0)"), constant_zero);
    }

    #[test]
    fn test_gate_declaration() {
        let result = Ok((
            "",
            Gate {
                gate_type: "NAND2x1_ASAP7_6t_L",
                name: "mul_7_21_g26127__7098",
                connections: vec![
                    Connection {
                        gate_pin: "A",
                        bit: Bit {
                            name: "a",
                            bit_index: Some(0),
                        },
                    },
                    Connection {
                        gate_pin: "B",
                        bit: Bit {
                            name: "n_1746",
                            bit_index: None,
                        },
                    },
                    Connection {
                        gate_pin: "Y",
                        bit: Bit {
                            name: "mul_7_21_n_547",
                            bit_index: None,
                        },
                    },
                ],
            },
        ));
        let input = r#"NAND2x1_ASAP7_6t_L mul_7_21_g26127__7098(.A (a[0]), .B (n_1746), .Y
       (mul_7_21_n_547));"#;
        assert_eq!(gate_declaration(input), result);
        let input = r#"NAND2x1_ASAP7_6t_L   mul_7_21_g26127__7098  (
    .A ( a[0]           ), 
    .B ( n_1746         ), 
    .Y ( mul_7_21_n_547 )
);"#;
        assert_eq!(gate_declaration(input), result);
    }

    #[test]
    fn test_module_body() {
        let result = Ok((
            "",
            ModuleBody {
                inputs: vec![
                    Wire {
                        name: "clk",
                        bit_range: None,
                    },
                    Wire {
                        name: "a",
                        bit_range: Some((0, 31)),
                    },
                    Wire {
                        name: "b",
                        bit_range: Some((0, 31)),
                    },
                ],
                outputs: vec![Wire {
                    name: "mult",
                    bit_range: Some((0, 31)),
                }],
                wires: vec![
                    Wire {
                        name: "clk",
                        bit_range: None,
                    },
                    Wire {
                        name: "a",
                        bit_range: Some((0, 31)),
                    },
                    Wire {
                        name: "b",
                        bit_range: Some((0, 31)),
                    },
                    Wire {
                        name: "mult",
                        bit_range: Some((0, 31)),
                    },
                ],
                gates: vec![
                    Gate {
                        gate_type: "AO22x1_ASAP7_6t_L",
                        name: "mul_7_21_g26022__8246",
                        connections: vec![
                            Connection {
                                gate_pin: "A1",
                                bit: Bit {
                                    name: "mul_7_21_n_529",
                                    bit_index: None,
                                },
                            },
                            Connection {
                                gate_pin: "A2",
                                bit: Bit {
                                    name: "mul_7_21_n_91",
                                    bit_index: None,
                                },
                            },
                            Connection {
                                gate_pin: "B1",
                                bit: Bit {
                                    name: "b",
                                    bit_index: Some(0),
                                },
                            },
                            Connection {
                                gate_pin: "B2",
                                bit: Bit {
                                    name: "mul_7_21_n_527",
                                    bit_index: None,
                                },
                            },
                            Connection {
                                gate_pin: "Y",
                                bit: Bit {
                                    name: "mul_7_21_n_616",
                                    bit_index: None,
                                },
                            },
                        ],
                    },
                    Gate {
                        gate_type: "INVx1_ASAP7_6t_L",
                        name: "mul_7_21_g26023",
                        connections: vec![
                            Connection {
                                gate_pin: "A",
                                bit: Bit {
                                    name: "mul_7_21_n_614",
                                    bit_index: None,
                                },
                            },
                            Connection {
                                gate_pin: "Y",
                                bit: Bit {
                                    name: "mul_7_21_n_615",
                                    bit_index: None,
                                },
                            },
                        ],
                    },
                ],
                assignments: vec![],
            },
        ));
        let input = r#"
  input clk;
  input [31:0] a, b;
  output [31:0] mult;
  wire clk;
  wire [31:0] a, b;
  wire [31:0] mult;
  AO22x1_ASAP7_6t_L mul_7_21_g26022__8246(.A1 (mul_7_21_n_529), .A2
       (mul_7_21_n_91), .B1 (b[0]), .B2 (mul_7_21_n_527), .Y
       (mul_7_21_n_616));
  INVx1_ASAP7_6t_L mul_7_21_g26023(.A (mul_7_21_n_614), .Y
       (mul_7_21_n_615));"#;
        assert_eq!(module_body(input), result);
    }

    #[test]
    fn test_module() {
        let input = fs::read_to_string(
            env::current_dir()
                .unwrap()
                .join("../../test/mul32_map_genus.v"),
        )
        .unwrap();
        let result = module(&input);
        assert!(result.is_ok());
        let (remaining, parsed_module) = result.unwrap();
        assert!(remaining.is_empty());
        parsed_module.verify().unwrap()
    }

    #[test]
    fn test_module_body_parses_writer_output_assignments() {
        let input = "wire y; wire n; assign y = n;";
        let (_, body) = module_body(input).unwrap();
        assert_eq!(body.assignments.len(), 1);
        assert_eq!(body.assignments[0].lhs.name, "y");
        assert_eq!(body.assignments[0].rhs.name, "n");
    }
}
