use crate::*;
use ordered_float::NotNan;
use regex::Regex;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtendedEGraph {
    pub inner: BaseEGraph,
    pub node_costs: HashMap<NodeId, ExtendedCost>,
    pub node_ops: HashMap<NodeId, String>,
    pub cell_nldm: HashMap<String, NLDM>,
}

// 实现转换方法
impl ExtendedEGraph {
    pub fn default() -> Self {
        Self {
            inner: BaseEGraph::default(),
            node_costs: HashMap::new(),
            node_ops: HashMap::new(),
            cell_nldm: HashMap::new(),
        }
    }

    pub fn get_node_cost(&self, nid: &NodeId) -> ExtendedCost {
        self.node_costs
            .get(nid)
            .cloned()
            .unwrap_or(ExtendedCost::zero())
    }

    pub fn from_base_to_extention_fast(egraph: BaseEGraph) -> Self {
        Self {
            inner: egraph,
            node_costs: HashMap::new(),
            node_ops: HashMap::new(),
            cell_nldm: HashMap::new(),
        }
    }

    pub fn from_base_to_extention(
        egraph: BaseEGraph,
        nldm_lib: &Value,
        costs_json: &Value,
    ) -> Self {
        // 从 LIB 读取单元的 NLDM 数据
        let lib_path = nldm_lib.as_str().expect("nldm_lib must be a string");
        let lib_content = std::fs::read_to_string(lib_path).expect("Failed to read lib file");
        let mut cache = HashMap::new();
        Self::from_base_to_extention_with_shared_nldm(egraph, costs_json, &lib_content, &mut cache)
    }

    /// Build a per-netlist physical e-graph while memoizing immutable
    /// Liberty-derived NLDM tables by exact standard-cell name.
    pub fn from_base_to_extention_with_shared_nldm(
        egraph: BaseEGraph,
        costs_json: &Value,
        lib_content: &str,
        shared_cell_nldm: &mut HashMap<String, NLDM>,
    ) -> Self {
        let mut node_costs = HashMap::new();
        let mut node_ops = HashMap::new();
        let mut cell_nldm = HashMap::new();
        // 从 JSON 中读取节点成本
        if let Value::Object(nodes) = &costs_json["nodes"] {
            for (nid, node_data) in nodes {
                if let Value::Object(node) = node_data {
                    if let Some(Value::String(op)) = node.get("op") {
                        // 为每个op构建NLDM
                        if !shared_cell_nldm.contains_key(op) {
                            // 在lib文件中查找对应的cell
                            if let Some(cell_content) = extract_cell_content(lib_content, op) {
                                // 解析area
                                let area = parse_float_value(&cell_content, "area")
                                    .unwrap_or_else(|| NotNan::new(0.0).unwrap());
                                let leakage_power =
                                    parse_float_value(&cell_content, "cell_leakage_power")
                                        .or_else(|| parse_conditional_vdd_leakage(&cell_content))
                                        .unwrap_or_else(|| NotNan::new(0.0).unwrap());
                                let max_capacitance =
                                    parse_float_value(&cell_content, "max_capacitance")
                                        .unwrap_or_else(|| NotNan::new(0.0).unwrap());
                                let (pin_order, pin_info) = parse_pin_info(&cell_content);
                                let pin_y_content =
                                    extract_pin_y_content(&cell_content, &pin_order[0]);
                                let delay =
                                    parse_template_data(&pin_y_content, "cell_rise", "cell_fall");
                                let transition = parse_template_data(
                                    &pin_y_content,
                                    "rise_transition",
                                    "fall_transition",
                                );
                                let internal_power =
                                    parse_template_data(&pin_y_content, "rise_power", "fall_power");
                                let timing_arcs = parse_timing_arcs(&pin_y_content);
                                let logic_truth_table =
                                    parse_logic_truth_table(&pin_y_content, &pin_order[1..]);

                                // 构建NLDM
                                let nldm = NLDM::new(
                                    area,
                                    leakage_power,
                                    max_capacitance,
                                    pin_order,
                                    pin_info,
                                    delay,
                                    transition,
                                    internal_power,
                                    timing_arcs,
                                    logic_truth_table,
                                );

                                shared_cell_nldm.insert(op.clone(), nldm);
                            }
                        }
                        if let Some(nldm) = shared_cell_nldm.get(op) {
                            cell_nldm.insert(op.clone(), nldm.clone());
                        }
                        node_ops.insert(NodeId::from(nid), op.clone());
                    }
                    if let Some(Value::Array(cost_array)) = node.get("cost") {
                        if cost_array.len() == 3 {
                            // 将 JSON 数组转换为 ExtendedCost
                            let cost = ExtendedCost {
                                components: [
                                    NotNan::new(cost_array[0].as_f64().unwrap_or(0.0)).unwrap(),
                                    NotNan::new(cost_array[1].as_f64().unwrap_or(0.0)).unwrap(),
                                    NotNan::new(cost_array[2].as_f64().unwrap_or(0.0)).unwrap(),
                                    NotNan::new(0.0).unwrap(),
                                    NotNan::new(0.0).unwrap(),
                                ],
                            };
                            node_costs.insert(NodeId::from(nid), cost);
                        }
                    }
                }
            }
        }

        Self {
            inner: egraph,
            node_costs,
            node_ops,
            cell_nldm,
        }
    }

    /// Build an extended graph from a complete pre-parsed cell cache.
    ///
    /// Unlike `from_base_to_extention_with_shared_nldm`, this path never scans
    /// Liberty for a missing operation.  That distinction matters for mapped
    /// occurrence graphs: primary-input symbols are ordinary zero-cost nodes,
    /// not cell names, and repeatedly searching a large Liberty file for each
    /// such symbol is pure overhead.  Callers must provide every real mapped
    /// cell used by the graph; absent operations remain non-physical exactly
    /// as absent input/constant symbols did in the legacy constructor.
    pub fn from_base_to_extention_with_preparsed_nldm(
        egraph: BaseEGraph,
        costs_json: &Value,
        preparsed: &HashMap<String, NLDM>,
    ) -> Self {
        let mut node_costs = HashMap::new();
        let mut node_ops = HashMap::new();
        let mut cell_nldm = HashMap::new();
        if let Value::Object(nodes) = &costs_json["nodes"] {
            for (nid, node_data) in nodes {
                if let Value::Object(node) = node_data {
                    if let Some(Value::String(op)) = node.get("op") {
                        if let Some(nldm) = preparsed.get(op) {
                            cell_nldm
                                .entry(op.clone())
                                .or_insert_with(|| nldm.clone());
                        }
                        node_ops.insert(NodeId::from(nid), op.clone());
                    }
                    if let Some(Value::Array(cost_array)) = node.get("cost") {
                        if cost_array.len() == 3 {
                            node_costs.insert(
                                NodeId::from(nid),
                                ExtendedCost {
                                    components: [
                                        NotNan::new(cost_array[0].as_f64().unwrap_or(0.0))
                                            .unwrap(),
                                        NotNan::new(cost_array[1].as_f64().unwrap_or(0.0))
                                            .unwrap(),
                                        NotNan::new(cost_array[2].as_f64().unwrap_or(0.0))
                                            .unwrap(),
                                        NotNan::new(0.0).unwrap(),
                                        NotNan::new(0.0).unwrap(),
                                    ],
                                },
                            );
                        }
                    }
                }
            }
        }
        Self {
            inner: egraph,
            node_costs,
            node_ops,
            cell_nldm,
        }
    }

    pub fn ensure_cell_nldm_from_lib(&mut self, lib_path: &str, op: &str) -> Result<(), String> {
        if self.cell_nldm.contains_key(op) {
            return Ok(());
        }
        let lib_content = std::fs::read_to_string(lib_path).map_err(|error| error.to_string())?;
        let cell_content = extract_cell_content(&lib_content, op)
            .ok_or_else(|| format!("Liberty has no cell {op}"))?;
        let area =
            parse_float_value(&cell_content, "area").unwrap_or_else(|| NotNan::new(0.0).unwrap());
        let leakage = parse_float_value(&cell_content, "cell_leakage_power")
            .or_else(|| parse_conditional_vdd_leakage(&cell_content))
            .unwrap_or_else(|| NotNan::new(0.0).unwrap());
        let max_cap = parse_float_value(&cell_content, "max_capacitance")
            .unwrap_or_else(|| NotNan::new(0.0).unwrap());
        let (pin_order, pin_info) = parse_pin_info(&cell_content);
        let output_pin = pin_order
            .first()
            .ok_or_else(|| format!("cell {op} has no pins"))?;
        let output = extract_pin_y_content(&cell_content, output_pin);
        let logic_truth_table = parse_logic_truth_table(&output, &pin_order[1..]);
        self.cell_nldm.insert(
            op.to_owned(),
            NLDM::new(
                area,
                leakage,
                max_cap,
                pin_order,
                pin_info,
                parse_template_data(&output, "cell_rise", "cell_fall"),
                parse_template_data(&output, "rise_transition", "fall_transition"),
                parse_template_data(&output, "rise_power", "fall_power"),
                parse_timing_arcs(&output),
                logic_truth_table,
            ),
        );
        Ok(())
    }
}

struct LogicFunctionParser<'a> {
    bytes: &'a [u8],
    cursor: usize,
    inputs: &'a HashMap<&'a str, bool>,
}

impl<'a> LogicFunctionParser<'a> {
    fn skip_space(&mut self) {
        while self
            .bytes
            .get(self.cursor)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            self.cursor += 1;
        }
    }

    fn consume(&mut self, token: u8) -> bool {
        self.skip_space();
        if self.bytes.get(self.cursor) == Some(&token) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn expression(&mut self) -> Result<bool, String> {
        let mut value = self.term()?;
        while self.consume(b'+') {
            value |= self.term()?;
        }
        Ok(value)
    }

    fn term(&mut self) -> Result<bool, String> {
        let mut value = self.unary()?;
        while self.consume(b'*') {
            value &= self.unary()?;
        }
        Ok(value)
    }

    fn unary(&mut self) -> Result<bool, String> {
        self.skip_space();
        if self.consume(b'!') {
            return Ok(!self.unary()?);
        }
        if self.consume(b'(') {
            let value = self.expression()?;
            if !self.consume(b')') {
                return Err("missing ')' in Liberty Boolean function".into());
            }
            return Ok(value);
        }
        let start = self.cursor;
        while self.bytes.get(self.cursor).is_some_and(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
        }) {
            self.cursor += 1;
        }
        if start == self.cursor {
            return Err("expected identifier in Liberty Boolean function".into());
        }
        let name = std::str::from_utf8(&self.bytes[start..self.cursor])
            .map_err(|error| error.to_string())?;
        match name {
            "0" => Ok(false),
            "1" => Ok(true),
            _ => self
                .inputs
                .get(name)
                .copied()
                .ok_or_else(|| format!("unknown Liberty Boolean input {name}")),
        }
    }
}

fn parse_logic_truth_table(pin_content: &str, pins: &[String]) -> Vec<bool> {
    let function = Regex::new(r#"\bfunction\s*:\s*\"([^\"]+)\""#)
        .expect("static function regex")
        .captures(pin_content)
        .and_then(|capture| capture.get(1))
        .map(|value| value.as_str());
    let Some(function) = function else {
        return Vec::new();
    };
    let Some(rows) = 1usize.checked_shl(pins.len() as u32) else {
        return Vec::new();
    };
    (0..rows)
        .map(|assignment| {
            let inputs: HashMap<_, _> = pins
                .iter()
                .enumerate()
                .map(|(index, pin)| (pin.as_str(), assignment & (1 << index) != 0))
                .collect();
            let mut parser = LogicFunctionParser {
                bytes: function.as_bytes(),
                cursor: 0,
                inputs: &inputs,
            };
            let value = parser.expression()?;
            parser.skip_space();
            if parser.cursor != parser.bytes.len() {
                return Err("trailing tokens in Liberty Boolean function".to_owned());
            }
            Ok(value)
        })
        .collect::<Result<Vec<_>, String>>()
        .unwrap_or_default()
}

fn extract_cell_content(lib_content: &str, cell_name: &str) -> Option<String> {
    // 构造要查找的cell起始标记

    let cell_start = format!("cell ({}) {{", cell_name);

    // 查找cell的开始位置
    if let Some(start_pos) = lib_content.find(&cell_start) {
        let content = &lib_content[start_pos..];
        let mut brace_count = 0;
        let mut pos = 0;

        // 遍历字符,统计括号匹配
        for (i, c) in content.chars().enumerate() {
            match c {
                '{' => brace_count += 1,
                '}' => {
                    brace_count -= 1;
                    if brace_count == 0 {
                        pos = i + 1;
                        break;
                    }
                }
                _ => continue,
            }
        }

        if pos > 0 {
            // 提取去掉外层大括号的内容
            let inner_content = &content[cell_start.len()..pos - 1];
            return Some(inner_content.to_string());
        }
    }
    None
}

// fn extract_pinY_content(cell_content: &str, output_pin: &str) -> String {
//     let pin_pattern = r"pin\s*\(Y\)\s*\{([^}]*(?:\{[^}]*\}[^}]*)*)\}";
//     let pin_re = regex::Regex::new(pin_pattern).ok().unwrap();
//     pin_re.captures(cell_content)
//         .map(|cap| cap[1].to_string())
//         .unwrap_or_else(|| String::new())
// }
fn extract_pin_y_content(cell_content: &str, output_pin: &str) -> String {
    // 构造要查找的pin起始标记
    let pin_start = format!("pin ({}) {{", output_pin);

    // 查找pin的开始位置
    if let Some(start_pos) = cell_content.find(&pin_start) {
        let content = &cell_content[start_pos..];
        let mut brace_count = 0;
        let mut pos = 0;

        // 遍历字符,统计括号匹配
        for (i, c) in content.chars().enumerate() {
            match c {
                '{' => brace_count += 1,
                '}' => {
                    brace_count -= 1;
                    if brace_count == 0 {
                        pos = i + 1;
                        break;
                    }
                }
                _ => continue,
            }
        }

        if pos > 0 {
            // 提取去掉外层大括号的内容
            let inner_content = &content[pin_start.len()..pos - 1];
            return inner_content.to_string();
        }
    }

    String::new()
}

fn parse_float_value(cell_content: &str, key: &str) -> Option<NotNan<f64>> {
    let search_pattern = format!("{} :", key);
    if let Some(start_pos) = cell_content.find(&search_pattern) {
        let value_start = start_pos + search_pattern.len();
        let remaining = &cell_content[value_start..];
        if let Some(end_pos) = remaining.find(';') {
            let value_str = &remaining[..end_pos].trim();
            if let Ok(parsed_value) = value_str.parse::<f64>() {
                return NotNan::new(parsed_value).ok();
            }
        }
    }
    None
}

/// Recover the scalar leakage convention used by the characterized SELECT
/// cells: arithmetic mean of conditional VDD leakage values.  VSS entries
/// are rail-accounting zeros and must not halve the result.
fn parse_conditional_vdd_leakage(cell_content: &str) -> Option<NotNan<f64>> {
    let marker = "leakage_power ()";
    let mut offset = 0usize;
    let mut values = Vec::new();
    while let Some(relative) = cell_content[offset..].find(marker) {
        let start = offset + relative;
        let open = start + cell_content[start..].find('{')?;
        let mut depth = 0isize;
        let mut quoted = false;
        let mut escaped = false;
        let mut end = None;
        for (relative_index, byte) in cell_content.as_bytes()[open..].iter().enumerate() {
            let character = *byte as char;
            if quoted {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    quoted = false;
                }
                continue;
            }
            match character {
                '"' => quoted = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(open + relative_index + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end?;
        let block = &cell_content[open..end];
        let is_vss = block
            .split_once("related_pg_pin :")
            .and_then(|(_, suffix)| suffix.split(';').next())
            .is_some_and(|rail| rail.trim() == "VSS");
        if !is_vss {
            if let Some(value) = parse_float_value(block, "value") {
                values.push(value);
            }
        }
        offset = end;
    }
    if values.is_empty() {
        return None;
    }
    let sum = values
        .iter()
        .copied()
        .fold(NotNan::new(0.0).unwrap(), |left, right| left + right);
    Some(sum / NotNan::new(values.len() as f64).ok()?)
}

#[cfg(test)]
mod leakage_tests {
    use super::parse_conditional_vdd_leakage;

    #[test]
    fn conditional_leakage_averages_vdd_and_excludes_vss() {
        let cell = r#"
            leakage_power () {
              value : 100;
              related_pg_pin : VDD;
            }
            leakage_power () {
              value : 0;
              related_pg_pin : VSS;
            }
            leakage_power () {
              value : 300;
              related_pg_pin : VDD;
            }
        "#;
        assert_eq!(
            parse_conditional_vdd_leakage(cell)
                .expect("conditional leakage")
                .into_inner(),
            200.0
        );
    }
}

fn parse_pin_info(
    cell_content: &str,
) -> (Vec<String>, HashMap<String, (NotNan<f64>, NotNan<f64>)>) {
    let mut pin_order = Vec::new();
    let mut pin_info = HashMap::new();

    // 正则表达式模式
    let pin_pattern = Regex::new(r"\bpin\s*\(\s*(\w+)\s*\)\s*\{([^}]+)\}").unwrap();
    let capacitance_pattern = Regex::new(r"capacitance\s*:\s*([\d.]+)").unwrap();
    let max_trans_pattern = Regex::new(r"max_transition\s*:\s*([\d.]+)").unwrap();

    for pin_cap in pin_pattern.captures_iter(cell_content) {
        let pin_name = pin_cap.get(1).unwrap().as_str();
        let pin_content = pin_cap.get(2).unwrap().as_str();
        pin_order.push(pin_name.to_string());

        // Mark: 其实改成 direction:output 的判定更合适
        if pin_name != pin_order[0] {
            // 输入引脚：解析capacitance和max_transition
            let mut capacitance = NotNan::new(0.0).unwrap();
            let mut max_transition = NotNan::new(0.0).unwrap();

            if let Some(cap_cap) = capacitance_pattern.captures(pin_content) {
                if let Ok(value) = cap_cap.get(1).unwrap().as_str().parse::<f64>() {
                    if let Ok(notnan_value) = NotNan::new(value) {
                        capacitance = notnan_value;
                    }
                }
            }
            if let Some(trans_cap) = max_trans_pattern.captures(pin_content) {
                if let Ok(value) = trans_cap.get(1).unwrap().as_str().parse::<f64>() {
                    if let Ok(notnan_value) = NotNan::new(value) {
                        max_transition = notnan_value;
                    }
                }
            }

            pin_info.insert(pin_name.to_string(), (capacitance, max_transition));
        }
    }

    (pin_order, pin_info)
}

fn parse_template_data(
    cell_content: &str,
    rise_str: &str,
    fall_str: &str,
) -> HashMap<String, Vec<Vec<NotNan<f64>>>> {
    let mut template_data = HashMap::new();
    let mut temp_matrices: HashMap<String, Vec<Vec<Vec<NotNan<f64>>>>> = HashMap::new();
    let blocks: Vec<_>;
    if rise_str == "rise_power" {
        blocks = cell_content.split("internal_power ()").skip(1).collect();
    } else {
        blocks = cell_content.split("timing ()").skip(1).collect();
    }

    // 第一步：收集所有同一pin_name的矩阵
    for block in blocks {
        if let Some(pin_start) = block.find("related_pin") {
            if let Some(quote_start) = block[pin_start..].find('"') {
                if let Some(quote_end) = block[pin_start + quote_start + 1..].find('"') {
                    let pin_name = &block
                        [pin_start + quote_start + 1..pin_start + quote_start + 1 + quote_end];

                    let rise_values = parse_value_matrix(block, rise_str);
                    let fall_values = parse_value_matrix(block, fall_str);

                    if !rise_values.is_empty() && !fall_values.is_empty() {
                        let n = rise_values[0].len();
                        let mut matrix = vec![vec![NotNan::new(0.0).unwrap(); n]; n + 2];

                        if let Some(idx1_values) = parse_index_values(block, "index_1") {
                            matrix[0] = idx1_values;
                        }
                        if let Some(idx2_values) = parse_index_values(block, "index_2") {
                            matrix[1] = idx2_values;
                        }

                        // 计算rise和fall的最大值
                        for i in 0..rise_values.len() {
                            for j in 0..n {
                                if rise_str == "rise_power" {
                                    matrix[i + 2][j] = (rise_values[i][j] + fall_values[i][j])
                                        / NotNan::new(2.0).unwrap();
                                } else {
                                    matrix[i + 2][j] = rise_values[i][j].max(fall_values[i][j]);
                                }
                            }
                        }

                        // 将矩阵添加到临时存储中
                        temp_matrices
                            .entry(pin_name.to_string())
                            .or_insert_with(Vec::new)
                            .push(matrix);
                    }
                }
            }
        }
    }

    // 第二步：对每个pin_name的所有矩阵取最大值
    for (pin_name, matrices) in temp_matrices {
        if !matrices.is_empty() {
            let n = matrices[0][0].len();
            let mut final_matrix = matrices[0].clone(); // 使用第一个矩阵作为基准

            if rise_str != "rise_power" {
                for matrix in matrices.iter().skip(1) {
                    for i in 2..matrix.len() {
                        // 从第三行开始，保留index_1和index_2
                        for j in 0..n {
                            final_matrix[i][j] = final_matrix[i][j].max(matrix[i][j]);
                        }
                    }
                }
            } else {
                for matrix in matrices.iter().skip(1) {
                    for i in 2..(n + 2) {
                        // 从第三行开始，保留index_1和index_2
                        for j in 0..n {
                            final_matrix[i][j] += matrix[i][j];
                        }
                    }
                }
                for i in 2..(n + 2) {
                    for j in 0..n {
                        final_matrix[i][j] =
                            final_matrix[i][j] / NotNan::new((matrices.len()) as f64).unwrap();
                    }
                }
            }

            template_data.insert(pin_name, final_matrix);
        }
    }

    template_data
}

fn extract_blocks(content: &str, marker: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut offset = 0;
    while let Some(rel) = content[offset..].find(marker) {
        let start = offset + rel;
        let Some(open_rel) = content[start..].find('{') else {
            break;
        };
        let open = start + open_rel;
        let mut depth = 0i32;
        let mut end = None;
        for (i, ch) in content[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(open + i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(stop) = end else { break };
        out.push(content[open + 1..stop - 1].to_string());
        offset = stop;
    }
    out
}

fn parse_lut(block: &str, keyword: &str) -> Option<Lut2D> {
    let marker = format!("{} (", keyword);
    let table = extract_blocks(block, &marker).into_iter().next()?;
    Some(Lut2D {
        index_1: parse_index_values(&table, "index_1")?,
        index_2: parse_index_values(&table, "index_2")?,
        values: parse_value_matrix(&table, "values"),
    })
}

fn parse_timing_arcs(output_pin: &str) -> Vec<TimingArc> {
    let related = Regex::new(r#"related_pin\s*:\s*\"([^\"]+)\""#).unwrap();
    let sense = Regex::new(r"timing_sense\s*:\s*(\w+)").unwrap();
    let when = Regex::new(r#"\bwhen\s*:\s*\"([^\"]+)\""#).unwrap();
    extract_blocks(output_pin, "timing ()")
        .into_iter()
        .filter_map(|block| {
            let pin = related.captures(&block)?.get(1)?.as_str().to_string();
            let timing_sense = match sense
                .captures(&block)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str())
            {
                Some("positive_unate") => TimingSense::PositiveUnate,
                Some("negative_unate") => TimingSense::NegativeUnate,
                _ => TimingSense::NonUnate,
            };
            Some(TimingArc {
                related_pin: pin,
                timing_sense,
                when: when
                    .captures(&block)
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str().to_string()),
                cell_rise: parse_lut(&block, "cell_rise")?,
                cell_fall: parse_lut(&block, "cell_fall")?,
                rise_transition: parse_lut(&block, "rise_transition")?,
                fall_transition: parse_lut(&block, "fall_transition")?,
            })
        })
        .collect()
}

// 辅助函数:解析值矩阵
fn parse_value_matrix(content: &str, keyword: &str) -> Vec<Vec<NotNan<f64>>> {
    let mut result = Vec::new();

    if let Some(start) = content.find(keyword) {
        if let Some(values_start) = content[start..].find("values") {
            if let Some(values_end) = content[start + values_start..].find(';') {
                let values_str = &content[start + values_start..start + values_start + values_end];
                if let Some(matrix_start) = values_str.find('(') {
                    if let Some(matrix_end) = values_str[matrix_start..].find(')') {
                        let matrix_str = &values_str[matrix_start + 1..matrix_start + matrix_end];
                        let re = Regex::new(r#""([^"]+)""#).unwrap();
                        for cap in re.captures_iter(matrix_str) {
                            if let Some(row_match) = cap.get(1) {
                                let row_str = row_match.as_str();
                                let row_values: Vec<NotNan<f64>> = row_str
                                    .split(',')
                                    .map(|s| s.trim())
                                    .filter(|s| !s.is_empty())
                                    .map(|s| s.parse::<f64>().unwrap_or(0.0))
                                    .map(|f| NotNan::new(f).unwrap())
                                    .collect();

                                if !row_values.is_empty() {
                                    result.push(row_values);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    result
}

fn parse_index_values(block: &str, index_name: &str) -> Option<Vec<NotNan<f64>>> {
    if let Some(idx_start) = block.find(index_name) {
        let remaining = &block[idx_start..];
        // 首先找到包含数值的行
        if let Some(line_end) = remaining.find(';') {
            let line = &remaining[..line_end];
            // 找到括号内的内容
            if let Some(start_bracket) = line.find('(') {
                if let Some(end_bracket) = line.rfind(')') {
                    // 提取并清理括号内的内容
                    let values_str = &line[start_bracket + 1..end_bracket];
                    // 删除所有引号
                    let values_str = values_str.replace('"', "");

                    // println!("解析 {} 的原始字符串: {}", index_name, values_str);

                    // 分割并解析数值
                    return Some(
                        values_str
                            .split(',')
                            .map(|s| s.trim())
                            .filter(|s| !s.is_empty())
                            .map(|s| match s.parse::<f64>() {
                                Ok(v) => NotNan::new(v).unwrap(),
                                Err(e) => {
                                    println!("解析错误 '{}': {}", s, e);
                                    NotNan::new(0.0).unwrap()
                                }
                            })
                            .collect(),
                    );
                }
            }
        }
    }
    None
}
