// 导入标准库中的格式化 trait
use std::fmt::Display;
// 导入 egg e-graph 库的核心类型
use egg::{Analysis, EGraph, Id, Language};
// 导入当前 crate 中的转换函数和序列化类型
use crate::{SerializedEGraph, egg_to_serialized_egraph};

/// 封装 e-graph 及其根节点的结构体
///
/// 泛型参数:
/// - L: 语言类型，需实现 Language trait
/// - N: 分析类型，需实现 Analysis<L> trait，且其关联类型 Data 需可克隆
#[derive(Clone, Debug)] // 自动实现 Clone 和 Debug trait
pub struct EGraphRoots<L: Language, N: Analysis<L, Data: Clone>> {
    pub egraph: EGraph<L, N>, // e-graph 实例
    pub roots: Vec<Id>,       // 根节点 ID 列表
}

/// 为 EGraphRoots 实现 Default trait
///
/// 当类型参数满足以下条件时可用:
/// - L: 实现 Language
/// - N: 实现 Analysis<L> + Default
/// - N::Data: 可克隆
impl<L, N> Default for EGraphRoots<L, N>
where
    L: Language,
    N: Analysis<L> + Default,
    N::Data: Clone,
{
    /// 创建默认的 EGraphRoots 实例
    fn default() -> Self {
        Self {
            egraph: Default::default(), // 默认的 e-graph
            roots: Default::default(),  // 空的根节点列表
        }
    }
}

/// 实现从 EGraphRoots 引用到 SerializedEGraph 的转换
///
/// 当类型参数满足以下条件时可用:
/// - L: 实现 Language + Display
/// - N: 实现 Analysis<L>
/// - N::Data: 可克隆
impl<L, N> From<&EGraphRoots<L, N>> for SerializedEGraph
where
    L: Language + Display, // L 必须可显示(用于序列化)
    N: Analysis<L>,        // N 必须实现分析
    N::Data: Clone,        // 分析数据必须可克隆
{
    /// 将 EGraphRoots 转换为 SerializedEGraph
    fn from(er: &EGraphRoots<L, N>) -> Self {
        // 使用工具函数进行转换
        egg_to_serialized_egraph(&er.egraph, &er.roots)
    }
}
