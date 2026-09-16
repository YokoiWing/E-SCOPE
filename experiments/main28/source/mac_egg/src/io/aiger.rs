// 导入所需的模块和类型
use crate::egraph_roots::EGraphRoots; // 自定义模块：等式图的根节点管理
use crate::language::{AigLanguage, AigType}; // 自定义模块：AIG逻辑语言定义
use crate::netlist::Netlist; // 自定义模块：电路网表数据结构
use aiger::{Aiger, AigerError, Literal, Reader, RecordsIter}; // AIGER格式解析库
use egg::Id; // 等式图中的节点ID
use indexmap::IndexMap; // 保持插入顺序的哈希映射
use petgraph::Incoming; // 图遍历方向（入边）
use petgraph::graph::NodeIndex; // 图节点索引
use rustc_hash::FxHashMap; // 高性能哈希映射
use std::fs::File; // 文件操作
use std::io; // I/O基础模块

/// 自定义错误类型：处理AIGER文件读取过程中的错误
#[derive(Debug)]
pub enum ReadError {
    NotImplementedForLatch,   // 不支持锁存器（Latch）类型
    InvalidInverted,          // 无效的反相输入
    InvalidLiteral,           // 无效的字面量
    InvalidInputSymbol,       // 无效的输入符号
    InvalidOutputSymbol,      // 无效的输出符号
    SymbolPositionOutOfBound, // 符号位置越界
    AigerError(AigerError),   // 底层AIGER解析错误
    IoError(io::Error),       // I/O错误
}

// 实现从AigerError到ReadError的转换
impl From<AigerError> for ReadError {
    fn from(_error: AigerError) -> Self {
        ReadError::AigerError(_error)
    }
}

// 实现从io::Error到ReadError的转换
impl From<io::Error> for ReadError {
    fn from(_error: io::Error) -> Self {
        ReadError::IoError(_error)
    }
}

/// 将AAG文件转换为网表结构（Netlist）
pub fn read_aag_to_netlist(path: &str) -> Result<Netlist<AigType, ()>, ReadError> {
    // 处理反相输入的辅助函数
    fn handle_inverted(
        input: Literal,                                    // AIGER字面量（含变量和反相标志）
        netlist: &mut Netlist<AigType, ()>,                // 待构建的网表
        variable_to_nid: &mut FxHashMap<usize, NodeIndex>, // 变量到节点索引的映射
    ) -> NodeIndex {
        if input.is_inverted() {
            // 如果是反相输入，添加NOT节点
            let input = variable_to_nid[&input.variable()];
            let output = netlist.graph.add_node(AigType::Not); // 创建NOT节点
            netlist.graph.add_edge(output, input, ()); // 连接NOT节点到输入
            output
        } else {
            // 非反相则直接返回节点索引
            variable_to_nid[&input.variable()]
        }
    }

    // 打开AAG文件
    let file = File::open(path)?;
    // 创建AIGER读取器
    let reader = Reader::from_reader(file)?;
    // 检查文件头（不支持锁存器）
    if reader.header().l > 0 {
        return Err(ReadError::NotImplementedForLatch);
    }

    // 计算最大节点数（2 * 输入数）
    let max_num_nodes = 2 * reader.header().m;
    // 解析AIGER记录
    let aiger_data = parse_aag(reader.records())?;
    // 初始化空网表
    let mut netlist: Netlist<AigType, ()> = Default::default();
    // 初始化变量到节点索引的映射
    let mut variable_to_nid =
        FxHashMap::with_capacity_and_hasher(max_num_nodes, Default::default());

    // 添加常量"false"节点（AIGER约定变量0为false）
    let nid = netlist.graph.add_node(AigType::Bool(false));
    variable_to_nid.insert(0, nid);

    // 处理输入节点
    for input in aiger_data.inputs {
        // 输入不能是反相
        if input.is_inverted() {
            return Err(ReadError::InvalidInverted);
        }
        // 获取或生成符号名
        let symbol = if let Some(symbol) = aiger_data.symbols.get(&input) {
            if symbol.type_spec != aiger::Symbol::Input {
                return Err(ReadError::InvalidInputSymbol);
            }
            AigType::Symbol((&symbol.symbol).into()) // 使用文件中的符号
        } else {
            AigType::Symbol(format!("{:?}", input.variable()).into()) // 生成默认符号
        };
        // 添加输入节点
        let nid = netlist.graph.add_node(symbol);
        variable_to_nid.insert(input.variable(), nid);
        netlist.leaves.push(nid); // 标记为叶子节点（无输入依赖）
    }

    // 处理逻辑门（AND节点）
    for gate in aiger_data.gates {
        // 检查输入变量是否已定义
        if !gate
            .inputs
            .iter()
            .all(|i| variable_to_nid.contains_key(&i.variable()))
        {
            return Err(ReadError::InvalidLiteral);
        }
        // 处理输入的反相
        let iid_1 = handle_inverted(gate.inputs[0], &mut netlist, &mut variable_to_nid);
        let iid_2 = handle_inverted(gate.inputs[1], &mut netlist, &mut variable_to_nid);
        // 创建AND节点
        let oid = netlist.graph.add_node(AigType::And);
        // 连接输入
        netlist.graph.add_edge(oid, iid_1, ());
        netlist.graph.add_edge(oid, iid_2, ());
        // 记录输出变量
        variable_to_nid.insert(gate.output.variable(), oid);
    }

    // 处理输出节点
    for output in aiger_data.outputs {
        // 检查输出变量是否已定义
        if !variable_to_nid.contains_key(&output.variable()) {
            return Err(ReadError::InvalidLiteral);
        }
        // 处理输出的反相
        let iid = handle_inverted(output, &mut netlist, &mut variable_to_nid);
        // 获取或生成符号名
        let symbol = if let Some(symbol) = aiger_data.symbols.get(&output) {
            if symbol.type_spec != aiger::Symbol::Output {
                return Err(ReadError::InvalidOutputSymbol);
            }
            AigType::Symbol((&symbol.symbol).into())
        } else {
            AigType::Symbol(format!("{:?}", output.variable()).into())
        };
        // 创建输出节点
        let oid = netlist.graph.add_node(symbol);
        netlist.graph.add_edge(oid, iid, ());
        netlist.roots.push(oid); // 标记为根节点（最终输出）
    }

    // 特殊处理常量false节点（变量0）
    let false_key = 0usize;
    let nid = variable_to_nid[&false_key];
    // 如果没有节点使用false节点，则移除它
    if netlist.graph.edges_directed(nid, Incoming).count() == 0 {
        netlist.graph.remove_node(nid);
    } else {
        // 否则标记为叶子节点
        netlist.leaves.push(nid);
    }

    Ok(netlist)
}

/// 将AAG文件转换为等式图（EGraph）的根节点集合
pub fn read_aag_to_egraph_roots(path: &str) -> Result<EGraphRoots<AigLanguage, ()>, ReadError> {
    // 处理反相输入的辅助函数
    fn handle_inverted(
        input: Literal,
        egraph_roots: &mut EGraphRoots<AigLanguage, ()>, // 待构建的等式图
        variable_to_nid: &mut FxHashMap<usize, Id>,      // 变量到EGraph节点ID的映射
    ) -> Id {
        if input.is_inverted() {
            // 添加NOT节点到EGraph
            egraph_roots
                .egraph
                .add(AigLanguage::Not(variable_to_nid[&input.variable()]))
        } else {
            variable_to_nid[&input.variable()]
        }
    }

    // 打开AAG文件
    let file = File::open(path)?;
    // 创建AIGER读取器
    let reader = Reader::from_reader(file)?;
    // 检查文件头（不支持锁存器）
    if reader.header().l > 0 {
        return Err(ReadError::NotImplementedForLatch);
    }

    // 计算最大节点数
    let max_num_nodes = 2 * reader.header().m;
    // 解析AIGER记录
    let aiger_data = parse_aag(reader.records())?;
    // 初始化等式图根节点集合
    let mut egraph_roots: EGraphRoots<AigLanguage, ()> = Default::default();
    // 初始化变量到节点ID的映射
    let mut variable_to_nid =
        FxHashMap::with_capacity_and_hasher(max_num_nodes, Default::default());

    // 添加常量"false"节点
    let nid = egraph_roots.egraph.add(AigLanguage::Bool(false));
    variable_to_nid.insert(0, nid);

    // 处理输入节点
    for input in aiger_data.inputs {
        // 输入不能是反相
        if input.is_inverted() {
            return Err(ReadError::InvalidInverted);
        }
        // 获取或生成符号名
        let symbol = if let Some(symbol) = aiger_data.symbols.get(&input) {
            if symbol.type_spec != aiger::Symbol::Input {
                return Err(ReadError::InvalidInputSymbol);
            }
            AigLanguage::Input((&symbol.symbol).into()) // 使用Input类型
        } else {
            AigLanguage::Input(format!("{:?}", input.variable()).into()) // 生成默认名
        };
        // 添加输入节点到EGraph
        let nid = egraph_roots.egraph.add(symbol);
        variable_to_nid.insert(input.variable(), nid);
    }

    // 处理逻辑门（AND节点）
    for gate in aiger_data.gates {
        // 检查输入变量是否已定义
        if !gate
            .inputs
            .iter()
            .all(|i| variable_to_nid.contains_key(&i.variable()))
        {
            return Err(ReadError::InvalidLiteral);
        }
        // 处理输入的反相
        let input_1 = handle_inverted(gate.inputs[0], &mut egraph_roots, &mut variable_to_nid);
        let input_2 = handle_inverted(gate.inputs[1], &mut egraph_roots, &mut variable_to_nid);
        // 创建AND节点
        let nid = egraph_roots
            .egraph
            .add(AigLanguage::And([input_1, input_2]));
        // 记录输出变量
        variable_to_nid.insert(gate.output.variable(), nid);
    }

    // 处理输出节点
    for output in aiger_data.outputs {
        // 检查输出变量是否已定义
        if !variable_to_nid.contains_key(&output.variable()) {
            return Err(ReadError::InvalidLiteral);
        }
        // 处理输出的反相
        let nid = handle_inverted(output, &mut egraph_roots, &mut variable_to_nid);
        // 获取或生成符号名
        let symbol = if let Some(symbol) = aiger_data.symbols.get(&output) {
            if symbol.type_spec != aiger::Symbol::Output {
                return Err(ReadError::InvalidOutputSymbol);
            }
            AigLanguage::Output((&symbol.symbol).into(), nid) // 输出节点包含内部节点引用
        } else {
            AigLanguage::Output(format!("{:?}", output.variable()).into(), nid)
        };
        // 添加输出节点
        let nid = egraph_roots.egraph.add(symbol);
        egraph_roots.roots.push(nid); // 记录根节点
    }

    // 重建EGraph以确保一致性
    egraph_roots.egraph.rebuild();
    Ok(egraph_roots)
}

/// AND门数据结构（输出+两个输入）
#[derive(Debug)]
struct AndGate {
    output: Literal,      // 输出字面量
    inputs: [Literal; 2], // 输入字面量
}

/// 锁存器数据结构（当前不支持）
#[derive(Debug)]
struct Latch {
    output: Literal, // 输出字面量
    input: Literal,  // 输入字面量
}

/// 符号数据结构（输入/输出名称）
#[derive(Debug)]
struct Symbol {
    type_spec: aiger::Symbol, // 符号类型（输入/输出）
    position: usize,          // 在输入/输出列表中的位置
    symbol: String,           // 符号名称
}

/// 存储解析后的AIGER数据结构
#[derive(Debug, Default)]
struct AigerData {
    inputs: Vec<Literal>,               // 输入列表
    outputs: Vec<Literal>,              // 输出列表
    gates: Vec<AndGate>,                // 逻辑门列表
    latches: Vec<Latch>,                // 锁存器列表（当前未使用）
    symbols: IndexMap<Literal, Symbol>, // 符号映射（字面量->符号）
}

/// 解析AIGER记录到结构化数据
fn parse_aag<T: io::Read>(records: RecordsIter<T>) -> Result<AigerData, ReadError> {
    let mut aiger_data = AigerData::default();
    // 遍历所有AIGER记录
    for record in records {
        match record {
            // 处理输入记录
            Ok(Aiger::Input(input)) => {
                aiger_data.inputs.push(input);
            }
            // 处理输出记录
            Ok(Aiger::Output(output)) => {
                aiger_data.outputs.push(output);
            }
            // 处理符号记录
            Ok(Aiger::Symbol {
                type_spec,
                position,
                symbol,
            }) => {
                // 根据符号类型找到对应的字面量
                let l = match type_spec {
                    aiger::Symbol::Input => aiger_data.inputs.get(position),
                    aiger::Symbol::Output => aiger_data.outputs.get(position),
                    aiger::Symbol::Latch => return Err(ReadError::NotImplementedForLatch),
                }
                .ok_or(ReadError::SymbolPositionOutOfBound)?;
                // 存储符号信息
                aiger_data.symbols.insert(
                    *l,
                    Symbol {
                        type_spec,
                        position,
                        symbol,
                    },
                );
            }
            // 处理AND门记录
            Ok(Aiger::AndGate { output, inputs }) => {
                aiger_data.gates.push(AndGate { output, inputs });
            }
            // 处理锁存器记录（不支持）
            Ok(Aiger::Latch { output, input }) => {
                return Err(ReadError::NotImplementedForLatch);
            }
            // 处理错误
            Err(error) => return Err(error.into()),
        }
    }
    Ok(aiger_data)
}

/// 测试模块
#[cfg(test)]
mod tests {
    use super::*;
    use petgraph::dot::{Config, Dot}; // 图可视化工具
    use std::env; // 环境变量操作

    // 测试读取add2.aag到EGraph
    #[test]
    fn test_read_add2_aag_egraph_roots() {
        let egraph_roots = read_aag_to_egraph_roots("test/add2.aag").unwrap();
        use crate::egg_to_serialized_egraph;
        // 序列化并保存为JSON
        let s = egg_to_serialized_egraph(&egraph_roots.egraph, &egraph_roots.roots);
        s.to_json_file(env::current_dir().unwrap().join("json/test_add2.json"))
            .unwrap();
        // Linux环境下生成SVG图
        #[cfg(target_os = "linux")]
        s.to_svg_file(env::current_dir().unwrap().join("svg/test_add2.svg"))
            .unwrap();
    }

    // 测试读取add1.aag到EGraph（类似add2测试）
    #[test]
    fn test_read_add1_aag_egraph_roots() {
        let egraph_roots = read_aag_to_egraph_roots("test/add1.aag").unwrap();
        use crate::egg_to_serialized_egraph;
        let s = egg_to_serialized_egraph(&egraph_roots.egraph, &egraph_roots.roots);
        s.to_json_file(env::current_dir().unwrap().join("json/test_add1.json"))
            .unwrap();
        #[cfg(target_os = "linux")]
        s.to_svg_file(env::current_dir().unwrap().join("svg/test_add1.svg"))
            .unwrap();
    }

    // 测试读取true.aag到EGraph
    #[test]
    fn test_read_true_aag_egraph_roots() {
        let egraph_roots = read_aag_to_egraph_roots("test/true.aag").unwrap();
        use crate::egg_to_serialized_egraph;
        let s = egg_to_serialized_egraph(&egraph_roots.egraph, &egraph_roots.roots);
        s.to_json_file(env::current_dir().unwrap().join("json/test_true.json"))
            .unwrap();
        #[cfg(target_os = "linux")]
        s.to_svg_file(env::current_dir().unwrap().join("svg/test_true.svg"))
            .unwrap();
    }

    // 测试读取false.aag到EGraph
    #[test]
    fn test_read_false_aag_egraph_roots() {
        let egraph_roots = read_aag_to_egraph_roots("test/false.aag").unwrap();
        use crate::egg_to_serialized_egraph;
        let s = egg_to_serialized_egraph(&egraph_roots.egraph, &egraph_roots.roots);
        s.to_json_file(env::current_dir().unwrap().join("json/test_false.json"))
            .unwrap();
        s.to_svg_file(env::current_dir().unwrap().join("svg/test_false.svg"))
            .unwrap();
    }

    // 测试读取add2.aag到网表
    #[test]
    fn test_read_add2_aag_netlist() {
        let netlist = read_aag_to_netlist("test/add2.aag").unwrap();
        // 生成DOT格式的图描述
        std::fs::write(
            env::current_dir().unwrap().join("dot/test_add2.dot"),
            format!(
                "{:?}",
                Dot::with_config(&netlist.graph, &[Config::EdgeNoLabel])
            ),
        )
        .unwrap();
    }

    // 测试读取add1.aag到网表（类似add2测试）
    #[test]
    fn test_read_add1_aag_netlist() {
        let netlist = read_aag_to_netlist("test/add1.aag").unwrap();
        std::fs::write(
            env::current_dir().unwrap().join("dot/test_add1.dot"),
            format!(
                "{:?}",
                Dot::with_config(&netlist.graph, &[Config::EdgeNoLabel])
            ),
        )
        .unwrap();
    }

    // 测试读取true.aag到网表
    #[test]
    fn test_read_true_aag_netlist() {
        let netlist = read_aag_to_netlist("test/true.aag").unwrap();
        std::fs::write(
            env::current_dir().unwrap().join("dot/test_true.dot"),
            format!(
                "{:?}",
                Dot::with_config(&netlist.graph, &[Config::EdgeNoLabel])
            ),
        )
        .unwrap();
    }

    // 测试读取false.aag到网表
    #[test]
    fn test_read_false_aag_netlist() {
        let netlist = read_aag_to_netlist("test/false.aag").unwrap();
        std::fs::write(
            env::current_dir().unwrap().join("dot/test_false.dot"),
            format!(
                "{:?}",
                Dot::with_config(&netlist.graph, &[Config::EdgeNoLabel])
            ),
        )
        .unwrap();
    }
}
