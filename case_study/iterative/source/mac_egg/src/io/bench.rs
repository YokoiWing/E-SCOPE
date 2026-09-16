use nom::{
    IResult, Parser,
    bytes::complete::{is_not, tag},
    character::complete::{char, multispace0, multispace1, not_line_ending},
    combinator::map,
    multi::separated_list0,
    sequence::{delimited, preceded},
};

fn parse_input(input: &str) -> IResult<&str, &str> {
    delimited(tag("INPUT("), is_not(")"), char(')')).parse(input)
}

fn parse_output(input: &str) -> IResult<&str, &str> {
    delimited(tag("OUTPUT("), is_not(")"), char(')')).parse(input)
}

fn parse_comment(input: &str) -> IResult<&str, ()> {
    map(preceded(tag("#"), not_line_ending), |_| ()).parse(input)
}

#[derive(Debug, PartialEq)]
pub struct Gate<'a> {
    pub gate_type: &'a str,
    pub output: &'a str,
    pub inputs: Vec<&'a str>,
}

fn parse_gate(input: &str) -> IResult<&str, Gate<'_>> {
    let (input, (output, _, _, _, gate_type, _, inputs, _)) = (
        is_not(" \t\r\n="),
        multispace0,
        char('='),
        multispace0,
        is_not("("),
        char('('),
        separated_list0((multispace0, char(','), multispace0), is_not(",) \t\r\n")),
        char(')'),
    )
        .parse(input)?;
    Ok((
        input,
        Gate {
            gate_type,
            output,
            inputs,
        },
    ))
}

#[derive(Debug, PartialEq)]
pub struct BenchFile<'a> {
    pub inputs: Vec<&'a str>,
    pub outputs: Vec<&'a str>,
    pub gates: Vec<Gate<'a>>,
}

pub fn parse_bench(input: &str) -> IResult<&str, BenchFile<'_>> {
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut gates = Vec::new();

    let mut remaining = input;
    loop {
        // 跳过空格和注释
        if let Ok((r, _)) = multispace1::<&str, ()>(remaining) {
            remaining = r;
        } else if let Ok((r, _)) = parse_comment(remaining) {
            remaining = r;
        } else if let Ok((r, input_symbol)) = parse_input(remaining) {
            inputs.push(input_symbol);
            remaining = r;
        } else if let Ok((r, output_symbol)) = parse_output(remaining) {
            outputs.push(output_symbol);
            remaining = r;
        } else if let Ok((r, gate)) = parse_gate(remaining) {
            gates.push(gate);
            remaining = r;
        } else {
            break;
        }
    }
    Ok((
        remaining,
        BenchFile {
            inputs,
            outputs,
            gates,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_input() {
        let input = r#"INPUT(a0)"#;
        assert_eq!(parse_input(input), Ok(("", "a0")));
    }

    #[test]
    fn test_parse_output() {
        let input = r#"OUTPUT(s0)"#;
        assert_eq!(parse_output(input), Ok(("", "s0")));
    }

    #[test]
    fn test_parse_comment() {
        let input = r#"# Benchmark "add2" written by ABC on Sun Aug 10 21:47:16 2025"#;
        assert_eq!(parse_comment(input), Ok(("", ())));
        let input = r#"# Benchmark "add2" written by ABC on Sun Aug 10 21:47:16 2025
abc"#;
        assert_eq!(parse_comment(input), Ok(("\nabc", ())));
    }

    #[test]
    fn test_parse_gate() {
        let input = r#"s0          = AND(new_n13, new_n15)"#;
        assert_eq!(
            parse_gate(input),
            Ok((
                "",
                Gate {
                    gate_type: "AND",
                    output: "s0",
                    inputs: vec!["new_n13", "new_n15"]
                }
            ))
        );
    }

    #[test]
    fn test_parse_bench() {
        let input = r#"# Benchmark "add2" written by ABC on Sun Aug 10 21:47:16 2025
INPUT(a0)
INPUT(a1)
INPUT(b0)
OUTPUT(s0)
INPUT(b1)
OUTPUT(s1)
new_n8      = NOT(a0)
OUTPUT(s2)
new_n9      = NOT(a1)
new_n10     = NOT(b0)
new_n11     = NOT(b1)
new_n12     = AND(a0, b0)
new_n13     = NOT(new_n12)
new_n14     = AND(new_n8, new_n10)
new_n15     = NOT(new_n14)
s0          = AND(new_n13, new_n15)
new_n17     = AND(a1, b1)
new_n18     = NOT(new_n17)
new_n19     = AND(new_n9, new_n11)
new_n20     = NOT(new_n19)
new_n21     = AND(new_n18, new_n20)
new_n22     = NOT(new_n21)
new_n23     = AND(new_n12, new_n21)
new_n24     = NOT(new_n23)
new_n25     = AND(new_n13, new_n22)
new_n26     = NOT(new_n25)
s1          = AND(new_n24, new_n26)
new_n28     = AND(new_n18, new_n24)
s2          = NOT(new_n28)
# some comment
other things
"#;
        // println!("{:?}", parse_bench(input));
        assert_eq!(
            parse_bench(input),
            Ok((
                "other things\n",
                BenchFile {
                    inputs: vec!["a0", "a1", "b0", "b1"],
                    outputs: vec!["s0", "s1", "s2"],
                    gates: vec![
                        Gate {
                            gate_type: "NOT",
                            output: "new_n8",
                            inputs: vec!["a0"]
                        },
                        Gate {
                            gate_type: "NOT",
                            output: "new_n9",
                            inputs: vec!["a1"]
                        },
                        Gate {
                            gate_type: "NOT",
                            output: "new_n10",
                            inputs: vec!["b0"]
                        },
                        Gate {
                            gate_type: "NOT",
                            output: "new_n11",
                            inputs: vec!["b1"]
                        },
                        Gate {
                            gate_type: "AND",
                            output: "new_n12",
                            inputs: vec!["a0", "b0"]
                        },
                        Gate {
                            gate_type: "NOT",
                            output: "new_n13",
                            inputs: vec!["new_n12"]
                        },
                        Gate {
                            gate_type: "AND",
                            output: "new_n14",
                            inputs: vec!["new_n8", "new_n10"]
                        },
                        Gate {
                            gate_type: "NOT",
                            output: "new_n15",
                            inputs: vec!["new_n14"]
                        },
                        Gate {
                            gate_type: "AND",
                            output: "s0",
                            inputs: vec!["new_n13", "new_n15"]
                        },
                        Gate {
                            gate_type: "AND",
                            output: "new_n17",
                            inputs: vec!["a1", "b1"]
                        },
                        Gate {
                            gate_type: "NOT",
                            output: "new_n18",
                            inputs: vec!["new_n17"]
                        },
                        Gate {
                            gate_type: "AND",
                            output: "new_n19",
                            inputs: vec!["new_n9", "new_n11"]
                        },
                        Gate {
                            gate_type: "NOT",
                            output: "new_n20",
                            inputs: vec!["new_n19"]
                        },
                        Gate {
                            gate_type: "AND",
                            output: "new_n21",
                            inputs: vec!["new_n18", "new_n20"]
                        },
                        Gate {
                            gate_type: "NOT",
                            output: "new_n22",
                            inputs: vec!["new_n21"]
                        },
                        Gate {
                            gate_type: "AND",
                            output: "new_n23",
                            inputs: vec!["new_n12", "new_n21"]
                        },
                        Gate {
                            gate_type: "NOT",
                            output: "new_n24",
                            inputs: vec!["new_n23"]
                        },
                        Gate {
                            gate_type: "AND",
                            output: "new_n25",
                            inputs: vec!["new_n13", "new_n22"]
                        },
                        Gate {
                            gate_type: "NOT",
                            output: "new_n26",
                            inputs: vec!["new_n25"]
                        },
                        Gate {
                            gate_type: "AND",
                            output: "s1",
                            inputs: vec!["new_n24", "new_n26"]
                        },
                        Gate {
                            gate_type: "AND",
                            output: "new_n28",
                            inputs: vec!["new_n18", "new_n24"]
                        },
                        Gate {
                            gate_type: "NOT",
                            output: "s2",
                            inputs: vec!["new_n28"]
                        }
                    ]
                }
            ))
        );
    }
}
