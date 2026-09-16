// FasterGreedyDagExtractor 实现了一个高效的 e-graph 提取算法，核心目标是从等价类图中提取具有最小共享成本的有向无环图(DAG)
// 算法特点：1) 共享子表达式只计算一次成本 2) 自底向上动态计算最优成本 3) 使用短路剪枝优化性能
// 主要流程：1) 初始化父节点索引和处理队列 2) 从叶子节点开始成本传播 3) 合并子节点成本集 4) 生成最终提取结果
use super::*;  // 导入父模块的所有内容（通常包含EGraph、Node、Class等定义）
use rustc_hash::{FxHashMap, FxHashSet};  // 使用高性能哈希实现
use ordered_float::NotNan;
use crate::ExtendedEGraph;

// 存储节点成本信息的结构体
struct CostSet {
    costs: HashMap<ClassId, (ExtendedCost, ExtendedCost)>,  // 映射：类ID -> 成本值，记录当前子树中所有唯一类的成本
    total: ExtendedCost,                    // 当前子树的总成本（共享节点只计一次）
    choice: NodeId,                 // 当前类的最优节点ID（即成本最低的节点）
}

// 主提取器结构体（无状态，仅作为算法入口）
pub struct FasterGreedyDagExtractor;

impl FasterGreedyDagExtractor {
    // 核心函数：计算给定节点的成本集
    // 参数说明：
    //   egraph: 等价类图引用
    //   node_id: 当前处理的节点ID
    //   costs: 已计算的类成本集（类ID -> CostSet）
    //   best_cost: 当前类已知的最佳成本（用于短路优化）
    pub fn new() -> Self {
        FasterGreedyDagExtractor
    }
    fn calculate_cost_set(
        egraph: &ExtendedEGraph,
        node_id: NodeId,
        costs: &FxHashMap<ClassId, CostSet>,
        best_cost: ExtendedCost,
    ) -> CostSet {
        let node = &egraph.inner[&node_id];  // 获取节点数据
        // println!("计算节点 {:?} 的成本集", node.op);
        let cid = egraph.inner.nid_to_cid(&node_id);  // 获取节点所属类ID
        let node_cost = egraph.get_node_cost(&node_id);
        
        // 处理叶子节点（无子节点）
        if node.children.is_empty(){
            return CostSet {
                costs: HashMap::from([(cid.clone(), (node_cost, node_cost))]),  // 仅包含自身成本
                total: node_cost,  // 总成本即自身成本
                choice: node_id.clone(),  // 最优选择即自身
            };
        }
        
        // 获取去重后的子节点类ID列表
        let mut childrens_classes = node
            .children  // 遍历所有子节点
            .iter()
            .map(|c| egraph.inner.nid_to_cid(&c).clone())  // 获取子节点类ID
            .collect::<Vec<ClassId>>();  // 收集为向量
        // println!("子节点类ID列表: {:?}", childrens_classes);
        childrens_classes.sort();  // 排序（为去重准备）
        childrens_classes.dedup();  // 去重（保留唯一类ID）
        
        // 短路优化：检测无效分支（提前返回INFINITY避免不必要计算）
        let first_cost = costs.get(&childrens_classes[0]).unwrap();  // 获取第一个子类成本
        if childrens_classes.contains(cid)  // 检测环：子类包含自身类ID
            || (childrens_classes.len() == 1  // 单子类且成本超限
                && ((node_cost+first_cost.total).abs()) > best_cost.abs())
        {
            return CostSet {
                costs: Default::default(),  // 空成本集
                total: ExtendedCost::infinity(),  // 标记为无穷大成本（无效分支）
                choice: node_id.clone(),  // 保留节点ID（但不会被最终选择）
            };
        }
        
        // 选择最大子成本集作为合并基础（优化性能）
        let id_of_biggest = childrens_classes
            .iter()
            .max_by_key(|s| costs.get(s).unwrap().costs.len())  // 找到最大成本集
            .unwrap();  // 安全：childrens_classes非空
        let mut result = costs.get(&id_of_biggest).unwrap().costs.clone();  // 克隆最大集
        // println!("选择最大子类: {:?}，成本集: {:?}", id_of_biggest, result);
        // 合并其他子节点成本集
        for child_cid in &childrens_classes {
            if child_cid == id_of_biggest { continue; }  // 跳过已处理的最大集
            let next_cost = &costs.get(child_cid).unwrap().costs;  // 获取子类成本集
            for (key, value) in next_cost.iter() {  // 遍历子类成本条目
                result.insert(key.clone(), value.clone());  // 插入/更新到结果集
            }
        }
        // println!("当前节点: {:?}, 成本: {:?}", node_id, node_cost);
        // println!("成本集大小: {:?}", result);
        // 添加当前节点成本并检测自包含
        let contains = result.contains_key(&cid);  // 检查是否已包含当前类
        
        // 计算最终成本：若已包含当前类则无效(INFINITY)，否则求所有值之和
        let result_cost = if contains { 
            ExtendedCost::infinity()
        } else {
            // 第一个分量：当前节点成本 + 子节点成本的最大值（关键路径）
            let first_components_sum = node_cost.components[0] + result.values()
                .map(|cost| cost.1.components[0])
                .max()
                .unwrap_or(NotNan::new(0.0).unwrap());
            
            // 第二、三个分量：当前节点成本 + 所有子节点成本的和
            let other_components_sum: [NotNan<f64>; 2] = result.values()
                .fold([node_cost.components[1], node_cost.components[2]], |mut acc, cost| {
                    acc[0] = acc[0] + cost.0.components[1];
                    acc[1] = acc[1] + cost.0.components[2];
                    acc
                });
        
            ExtendedCost {
                components: [
                    first_components_sum,
                    other_components_sum[0],
                    other_components_sum[1],
                    node_cost.components[3], // 第四个分量保持不变
                    node_cost.components[4], 
                ]
            }
        };
        // let result_cost = if contains { INFINITY } else { result.values().sum() };
        result.insert(cid.clone(), (node_cost, result_cost));  // 添加当前节点成本
        // 返回最终成本集
        CostSet {
            costs: result,  // 合并后的成本映射
            total: result_cost,  // 计算的总成本
            choice: node_id.clone(),  // 当前节点作为选择
        }
    }
}

// 实现提取器接口
impl Extractor for FasterGreedyDagExtractor {
    // 主提取函数
    fn extract(&self, egraph: &ExtendedEGraph, _roots: &[ClassId]) -> ExtractionResult {
        // === 初始化阶段 ===
        // 构建父节点索引（类ID -> 父节点列表）
        let mut parents = IndexMap::<ClassId, Vec<NodeId>>::with_capacity(egraph.inner.classes().len());
        let n2c = |nid: &NodeId| egraph.inner.nid_to_cid(nid);  // 辅助函数：节点ID->类ID
        let mut analysis_pending = UniqueQueue::default();  // 初始化处理队列
        
        // println!("=== 开始提取过程 ===");
        // println!("根节点列表: {:?}", _roots);
        // println!("初始化父节点映射...");
    
        // 初始化父节点映射（为每个类创建空列表）
        for class in egraph.inner.classes().values() {
            parents.insert(class.id.clone(), Vec::new());
        }
        // println!("类总数: {}", parents.len());
        
        // // 构建父节点索引并填充初始队列
        // println!("\n构建父节点索引...");
        for class in egraph.inner.classes().values() {
            // println!("处理类 {:?}:", class.id);
            for node in &class.nodes {
                // println!("  节点 {:?}:", node);
                for c in &egraph.inner[node].children {
                    // println!("    添加父节点关系: 子节点{:?} -> 父节点{:?}", c, node);
                    parents[n2c(c)].push(node.clone());
                }
                if egraph.inner[node].is_leaf() {
                    // println!("    叶子节点，加入处理队列: {:?}", node);
                    analysis_pending.insert(node.clone());
                }
            }
        }
        
        // === 成本传播主循环 ===
        // println!("\n=== 开始成本传播 ===");
        let mut result = ExtractionResult::default();  // 初始化结果容器
        // 初始化成本映射（类ID -> CostSet），使用高性能哈希
        let mut costs = FxHashMap::<ClassId, CostSet>::with_capacity_and_hasher(
            egraph.inner.classes().len(),
            Default::default(),
        );
        
        // 处理队列直到为空
        let mut iteration = 0;
        while let Some(node_id) = analysis_pending.pop() {  // 取出下一个待处理节点
            iteration += 1;
            // println!("\n迭代 {}: 处理节点 {:?}", iteration, node_id);
            // println!("当前队列状态: {:?}", analysis_pending.queue);
            let class_id = n2c(&node_id);  // 获取节点类ID
            let node = &egraph.inner[&node_id];  // 获取节点数据
            // println!("  所属类: {:?}", class_id);
            // println!("  子节点: {:?}", node.children);
        
            // 检查所有子节点是否已计算成本
            if node.children.iter().all(|c| costs.contains_key(n2c(c))) {
                // println!("  所有子节点成本已计算");
                let prev_cost = costs.get(class_id).map_or(ExtendedCost::infinity(), |cs| cs.total);
                // println!("  当前最佳成本: {:?},{:?}", prev_cost,prev_cost.abs());
                
                let cost_set = Self::calculate_cost_set(egraph, node_id.clone(), &costs, prev_cost);
                // println!("  计算得到新成本: {:?},{:?}", cost_set.total, cost_set.total.abs());
                
                if cost_set.total.abs() < prev_cost.abs() {
                    // println!("  发现更优成本，更新类 {:?}", class_id);
                    costs.insert(class_id.clone(), cost_set);
                    // println!("  将父节点加入队列: {:?}", parents[class_id]);
                    analysis_pending.extend(parents[class_id].iter().cloned());
                }
            } else {
                // println!("  跳过：部分子节点成本未计算");
            }
        }
        
        // === 生成最终结果 ===
        // println!("\n\n\n=== 生成最终结果 ===");
        // println!("已计算成本的类: {:?}", costs.keys().collect::<Vec<_>>());
        // println!("\n\n\n根节点类: {:?}", _roots);
        for (cid, cost_set) in costs {
            // println!("类 {:?} 选择节点 {:?}", cid, cost_set.choice);
            result.choose(cid, cost_set.choice);
        }
        
        println!("\n\n\n最终选择结果:");
        // println!("Choices: {:?}", result.choices);
        println!("=== 提取完成 ===\n");
        result  // 返回提取结果
    }
}

// ===== 唯一队列实现 =====
// 高效数据结构：保证元素唯一性的队列（插入/删除O(1)均摊）
#[derive(Clone)]
#[cfg_attr(feature = "serde-1", derive(Serialize, Deserialize))]  // 条件序列化支持
pub(crate) struct UniqueQueue<T> 
where
    T: Eq + std::hash::Hash + Clone,  // 要求：可哈希、可克隆
{
    set: FxHashSet<T>,  // 哈希集（快速查重）
    queue: std::collections::VecDeque<T>,  // 双端队列（维护顺序）
}

// 默认实现
impl<T> Default for UniqueQueue<T> 
where
    T: Eq + std::hash::Hash + Clone,
{
    fn default() -> Self {
        UniqueQueue {
            set: Default::default(),  // 空哈希集
            queue: std::collections::VecDeque::new(),  // 空队列
        }
    }
}

impl<T> UniqueQueue<T> 
where
    T: Eq + std::hash::Hash + Clone,
{
    // 插入元素（若不存在）
    pub fn insert(&mut self, t: T) {
        if self.set.insert(t.clone()) {  // 如果成功插入集合
            self.queue.push_back(t);  // 加入队列尾部
        }
    }
    
    // 批量插入
    pub fn extend<I>(&mut self, iter: I)
    where
        I: IntoIterator<Item = T>,
    {
        for t in iter.into_iter() {  // 遍历迭代器
            self.insert(t);  // 调用单个插入
        }
    }
    
    // 弹出队首元素
    pub fn pop(&mut self) -> Option<T> {
        let res = self.queue.pop_front();  // 从队列取出
        res.as_ref().map(|t| self.set.remove(t));  // 同步从集合移除
        res  // 返回元素
    }
    
    // 检查队列是否为空
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        let r = self.queue.is_empty();
        debug_assert_eq!(r, self.set.is_empty());  // 调试断言：集合和队列状态一致
        r
    }

}