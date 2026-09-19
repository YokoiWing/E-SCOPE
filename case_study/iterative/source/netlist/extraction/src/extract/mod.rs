// 使用indexmap库，它保持插入顺序的哈希映射
use indexmap::IndexMap;
use ordered_float::NotNan;
use crate::ExtendedEGraph;
use crate::ExtendedCost;
// use crate::ExtendedEGraph;
// use crate::ExtendedCost;
use egraph_serialize::{ClassId, NodeId};

// 使用rustc_hash库中的快速哈希实现（FxHashMap和FxHashSet）
use rustc_hash::{FxHashMap, FxHashSet};
// 使用标准库中的HashMap和VecDeque
use std::collections::{HashMap, VecDeque};



// 定义子模块
pub mod prio_queue;
pub mod timing_trace;
#[path = "timing.rs"]
pub mod timing_v2;
pub mod soft_criticality;
pub mod circuit_adjoint;
pub mod region_boundary;

pub use timing_trace::{NldmEvaluation, TimingPoint};
pub use timing_v2::{CriticalArcV2, EdgeTimingPointV2, NldmEvaluationV2, NldmV2Config, NldmV2PerturbationOverlay, SignalEdge, TimingArcIdV2, TimingArcPointV2, TimingPointV2};
pub use soft_criticality::{criticality, CriticalityMethod, CriticalityParams, CriticalityResult};
pub use circuit_adjoint::{
    CircuitAdjointConfig, CircuitAdjointV1, CircuitObjectiveWeights, TimingAdjointControl,
    TransitionBackward, circuit_adjoint, circuit_adjoint_weighted,
};
pub use region_boundary::{incumbent_region_boundary,replay_incumbent_region,RegionBoundaryState,RegionInputCap,RegionOutputState};

// 定义浮点数比较的容差常量
pub const EPSILON_ALLOWANCE: f64 = 0.00001;

// 定义提取器trait，要求实现Sync（线程安全）
pub trait Extractor: Sync {
    // 核心提取方法，从e-graph中提取表达式
    fn extract(&self, egraph: &ExtendedEGraph, roots: &[ClassId]) -> ExtractionResult;

    // 将提取器装箱的方法
    fn boxed(self) -> Box<dyn Extractor>
    where
        Self: Sized + 'static,
    {
        Box::new(self)
    }
}

// 定义泛型trait，用于统一不同映射类型的get方法
pub trait MapGet<K, V> {
    fn get(&self, key: &K) -> Option<&V>;
}

// 为标准HashMap实现MapGet trait
impl<K, V> MapGet<K, V> for HashMap<K, V>
where
    K: Eq + std::hash::Hash,
{
    fn get(&self, key: &K) -> Option<&V> {
        HashMap::get(self, key)
    }
}

// 为FxHashMap实现MapGet trait
impl<K, V> MapGet<K, V> for FxHashMap<K, V>
where
    K: Eq + std::hash::Hash,
{
    fn get(&self, key: &K) -> Option<&V> {
        FxHashMap::get(self, key)
    }
}

// 为IndexMap实现MapGet trait
impl<K, V> MapGet<K, V> for IndexMap<K, V>
where
    K: Eq + std::hash::Hash,
{
    fn get(&self, key: &K) -> Option<&V> {
        IndexMap::get(self, key)
    }
}

// 提取结果结构体，使用默认派生
#[derive(Default, Clone)]
pub struct ExtractionResult {
    // 从类ID到节点ID的映射，表示每个类选择的代表节点
    pub choices: IndexMap<ClassId, NodeId>,
}

// 用于DFS遍历的状态枚举
#[derive(Clone, Copy)]
enum Status {
    Doing,  // 正在处理中
    Done,   // 已处理完成
}

impl ExtractionResult {
    // 检查提取结果的完整性
    pub fn check(&self, egraph: &ExtendedEGraph) {
        // 应该有根节点
        assert!(!egraph.inner.root_eclasses.is_empty());
        // println!("Root eclasses: {:?}", egraph.inner.root_eclasses);
        // println!("Choices: {:?}", self.choices);
        // 所有根类都应该有选择
        for cid in egraph.inner.root_eclasses.iter() {
            assert!(self.choices.contains_key(cid));
        }
       
        // 不应该有循环
        assert!(self.find_cycles(&egraph, &egraph.inner.root_eclasses).is_empty());

        // 节点应该属于它们被选择的类
        for (cid, nid) in &self.choices {
            let node = &egraph.inner[nid];
            assert!(node.eclass == *cid);
        }

        // 所有根节点依赖的节点都应该被选择
        let mut todo: Vec<ClassId> = egraph.inner.root_eclasses.to_vec();
        let mut visited: FxHashSet<ClassId> = Default::default();
        while let Some(cid) = todo.pop() {
            if !visited.insert(cid.clone()) {
                continue;
            }
            assert!(self.choices.contains_key(&cid));

            // 将当前节点依赖的子节点加入待处理列表
            for child in &egraph.inner[&self.choices[&cid]].children {
                todo.push(egraph.inner.nid_to_cid(child).clone());
            }
        }
    }

    // 添加一个选择
    pub fn choose(&mut self, class_id: ClassId, node_id: NodeId) {
        self.choices.insert(class_id, node_id);
    }

    // 查找循环依赖
    pub fn find_cycles(&self, egraph: &ExtendedEGraph, roots: &[ClassId]) -> Vec<ClassId> {
        let mut status = IndexMap::<ClassId, Status>::default();
        let mut cycles = vec![];
        for root in roots {
            self.cycle_dfs(egraph, root, &mut status, &mut cycles)
        }
        cycles
    }

    // 深度优先搜索检测循环
    fn cycle_dfs(
        &self,
        egraph: &ExtendedEGraph,
        class_id: &ClassId,
        status: &mut IndexMap<ClassId, Status>,
        cycles: &mut Vec<ClassId>,
    ) {
        match status.get(class_id).cloned() {
            Some(Status::Done) => (),  // 已处理完成，跳过
            Some(Status::Doing) => cycles.push(class_id.clone()),  // 发现循环
            None => {
                status.insert(class_id.clone(), Status::Doing);  // 标记为处理中
                let node_id = &self.choices[class_id];
                let node = &egraph.inner[node_id];
                // 递归处理所有子节点
                for child in &node.children {
                    let child_cid = egraph.inner.nid_to_cid(child);
                    self.cycle_dfs(egraph, child_cid, status, cycles)
                }
                status.insert(class_id.clone(), Status::Done);  // 标记为处理完成
            }
        }
    }

    // 计算树结构的成本（考虑缓存）
    pub fn tree_cost(&self, egraph: &ExtendedEGraph, roots: &[ClassId]) -> ExtendedCost {
        let node_roots = roots
            .iter()
            .map(|cid| self.choices[cid].clone())
            .collect::<Vec<NodeId>>();
        self.tree_cost_rec(egraph, &node_roots, &mut HashMap::new())
    }

    //递归计算树成本
    fn tree_cost_rec(
        &self,
        egraph: &ExtendedEGraph,
        roots: &[NodeId],
        memo: &mut HashMap<NodeId, ExtendedCost>,
    ) -> ExtendedCost {
        let mut cost = ExtendedCost::zero();
        for root in roots {
            if let Some(c) = memo.get(root) {  // 检查缓存
                cost += *c;
                continue;
            }
            let class = egraph.inner.nid_to_cid(root);
            let node = &egraph.inner[&self.choices[class]];
            // 计算当前节点的成本（节点成本 + 所有子节点成本）
            let node_cost = egraph.get_node_cost(root);
            let inner = node_cost + self.tree_cost_rec(egraph, &node.children, memo);
            memo.insert(root.clone(), inner);  // 缓存结果
            cost += inner;
        }
        cost
    }

    // this will loop if there are cycles
    pub fn dag_cost(&self, egraph: &ExtendedEGraph) -> ExtendedCost {
        // 构建邻接表和入度表
        let mut adj_list: FxHashMap<ClassId, Vec<ClassId>> = FxHashMap::default();
        let mut in_degree: FxHashMap<ClassId, usize> = FxHashMap::default();
        
        // 初始化图结构
        for (cid, _) in &self.choices {
            in_degree.insert(cid.clone(), 0);
            adj_list.insert(cid.clone(), Vec::new());
        }
        
        // 构建有向图：边从子类指向父类，以支持自底向上的拓扑排序
        for (cid, node_id) in &self.choices {
            let node = &egraph.inner[node_id];
            for child in &node.children {
                let child_cid = egraph.inner.nid_to_cid(child);
                adj_list.entry(child_cid.clone()).or_insert_with(Vec::new).push(cid.clone());
                *in_degree.entry(cid.clone()).or_insert(0) += 1;
            }
        }
        
        // 拓扑排序起点：入度为0的节点（叶节点，无子节点）
        let mut queue: Vec<ClassId> = in_degree
            .iter()
            .filter(|(_, &degree)| degree == 0)
            .map(|(cid, _)| cid.clone())
            .collect();
        
        // 存储每个节点的成本
        let mut costs: FxHashMap<ClassId, ExtendedCost> = FxHashMap::default();
        
        // 按拓扑序计算成本（自底向上）
        while let Some(cid) = queue.pop() {
            let node_id = &self.choices[&cid];
            let node = &egraph.inner[node_id];
            let node_cost = egraph.get_node_cost(node_id);
            
            if node.children.is_empty() {
                costs.insert(cid.clone(), node_cost);
            } else {
                let mut total_costs = ExtendedCost::zero();
                let mut max_critical = NotNan::new(0.0).unwrap();
                
                // 计算子节点的关键路径最大值
                for child in &node.children {
                    let child_cid = egraph.inner.nid_to_cid(child);
                    let child_costs = &costs[&child_cid];
                    
                    max_critical = max_critical.max(child_costs.components[0]);
                }
                
                total_costs.components[0] = max_critical + node_cost.components[0];
                // 对于组件1和2，只计入当前节点的成本（唯一计数，避免共享子结构重复计入）
                total_costs.components[1] = node_cost.components[1];
                total_costs.components[2] = node_cost.components[2];
                
                costs.insert(cid.clone(), total_costs);
            }
            
            // 更新后继节点（父节点）的入度
            for next_cid in adj_list.get(&cid).unwrap_or(&Vec::new()) {
                let degree = in_degree.get_mut(next_cid).unwrap();
                *degree -= 1;
                if *degree == 0 {
                    queue.push(next_cid.clone());
                }
            }
        }
        
        // 检测环：如果未处理所有节点，则存在环
        if costs.len() != self.choices.len() {
            // 存在环，返回无限成本（或根据需要处理）
            return ExtendedCost::infinity();
        }
        
        // 收集所有作为子节点的类ID，用于识别根节点
        let mut all_children: FxHashSet<ClassId> = FxHashSet::default();
        for (_, node_id) in &self.choices {
            let node = &egraph.inner[node_id];
            for child in &node.children {
                all_children.insert(egraph.inner.nid_to_cid(child).clone());
            }
        }
        
        // 计算最终成本
        let mut final_costs = ExtendedCost::zero();
        let mut sum1 = NotNan::new(0.0).unwrap();
        let mut sum2 = NotNan::new(0.0).unwrap();
        
        // 组件1和2：所有唯一节点的总和
        for (cid, node_id) in &self.choices {
            let node_cost = egraph.get_node_cost(node_id);
            sum1 += node_cost.components[1];
            sum2 += node_cost.components[2];
            
            // 组件0：根节点的关键路径最大值
            if !all_children.contains(cid) {
                let cost = &costs[cid];
                final_costs.components[0] = final_costs.components[0].max(cost.components[0]);
            }
        }
        
        final_costs.components[1] = sum1;
        final_costs.components[2] = sum2;
        
        final_costs
    }

    pub fn dag_cost_nldm(&self, egraph: &ExtendedEGraph) -> ExtendedCost {
        // 构建邻接表和入度表
        let mut adj_list: FxHashMap<ClassId, Vec<ClassId>> = FxHashMap::default();
        let mut in_degree: FxHashMap<ClassId, usize> = FxHashMap::default();
        
        // 初始化图结构
        for (cid, _) in &self.choices {
            in_degree.insert(cid.clone(), 0);
            adj_list.insert(cid.clone(), Vec::new());
        }
        
        // 构建有向图：边从子类指向父类，以支持自底向上的拓扑排序
        for (cid, node_id) in &self.choices {
            let node = &egraph.inner[node_id];
            for child in &node.children {
                let child_cid = egraph.inner.nid_to_cid(child);
                adj_list.entry(child_cid.clone()).or_insert_with(Vec::new).push(cid.clone());
                *in_degree.entry(cid.clone()).or_insert(0) += 1;
            }
        }
        
        // 拓扑排序起点：入度为0的节点（叶节点，无子节点）
        let mut queue: Vec<ClassId> = in_degree
            .iter()
            .filter(|(_, &degree)| degree == 0)
            .map(|(cid, _)| cid.clone())
            .collect();
        
        // 存储每个节点的累积成本
        let mut costs: FxHashMap<ClassId, ExtendedCost> = FxHashMap::default();
        // 存储每个节点的本地成本
        let mut local_costs: FxHashMap<ClassId, ExtendedCost> = FxHashMap::default();
        
        // 按拓扑序计算成本（自底向上）
        while let Some(cid) = queue.pop() {
            let node_id = &self.choices[&cid];
            let op = &egraph.node_ops[node_id];
            // println!("Processing node {:?} of op {}", node_id, op);
            let node = &egraph.inner[node_id];
            
            // 计算本地成本
            let mut local_cost = ExtendedCost::zero();
            
            if !node.children.is_empty() {
                let empty_vec = Vec::new();
                let parents = adj_list.get(&cid).unwrap_or(&empty_vec);
                // println!(" Node {} it operator: {} Parents: {:?}\n", node_id, op, parents);
                if !parents.is_empty() {
                    // 计算 slews
                    let nldm = &egraph.cell_nldm[op];
                    let mut slews: HashMap<String, NotNan<f64>> = HashMap::new();
                    let pin_order = &nldm.pin_order;
                    for (i, child) in node.children.iter().enumerate() {
                        let pin = pin_order[i + 1].clone();
                        let child_cid = egraph.inner.nid_to_cid(child);
                        let child_transition = costs[&child_cid].components[3];
                        slews.insert(pin, child_transition);
                    }
                    // println!(" Node {} it operator: {} Slews: {:?}", node_id, op, slews);
                    // 计算 load
                    let mut load = if parents.is_empty() {
                        NotNan::new(1.0).unwrap()
                    } else {
                        NotNan::new(0.0).unwrap()
                    };
                    for parent_cid in parents {
                        let parent_node_id = &self.choices[parent_cid];
                        let parent_node = &egraph.inner[parent_node_id];
                        let parent_op = &egraph.node_ops[parent_node_id];
                        // println!("  Parent node {} of op {} for children {:?}",parent_node_id, egraph.node_ops[parent_node_id],parent_node.children);
                        if !egraph.cell_nldm.contains_key(parent_op) {
                            load += NotNan::new(1.0).unwrap();
                            // println!(
                            //     "  Parent node {}'s op not found in egraph.node_ops, adding 0.9 to load",
                            //     parent_node_id
                            // );
                            continue;
                        }
                        let index = match parent_node.children.iter().position(|c| {
                            let child_class = egraph.inner.nid_to_cid(c);
                            let current_class = egraph.inner.nid_to_cid(node_id);
                            child_class == current_class
                        }) {
                            Some(idx) => idx,
                            None => {
                                // println!("Child class {} not found in parent node {}'s children", egraph.inner.nid_to_cid(node_id),parent_node_id);
                                continue;
                            }
                        };
                        let parent_nldm = &egraph.cell_nldm[parent_op];
                        let pin = parent_nldm.pin_order[index + 1].clone();
                        load += parent_nldm.pin_info[&pin].0;
                    }
                    // println!("  Load for node {} of slew {:?}: {:?}",node_id, slews, load);
                    // 设置本地成本
                    local_cost.components[0] = nldm.lookup_table("delay", slews.clone(), load).unwrap();
                    local_cost.components[1] = nldm.area;
                    local_cost.components[2] = nldm.lookup_table("internal_power", slews.clone(), load).unwrap() + nldm.leakage_power/1000.0; // 转换为纳瓦
                    local_cost.components[3] = nldm.lookup_table("transition", slews.clone(), load).unwrap();
                    // println!("  Local cost for node {} of op {}: {:?}",node_id, op, local_cost);
                }
            }
            
            local_costs.insert(cid.clone(), local_cost);
            
            // 计算累积成本
            let mut total_costs = ExtendedCost::zero();
            let mut max_critical = NotNan::new(0.0).unwrap();
            
            for child in &node.children {
                let child_cid = egraph.inner.nid_to_cid(child);
                let child_costs = &costs[&child_cid];
                
                max_critical = max_critical.max(child_costs.components[0]);
            }
            
            total_costs.components[0] = max_critical + local_costs[&cid].components[0];
            total_costs.components[1] = local_costs[&cid].components[1];
            total_costs.components[2] = local_costs[&cid].components[2];
            total_costs.components[3] = local_costs[&cid].components[3];
            
            costs.insert(cid.clone(), total_costs);
            
            // 更新后继节点（父节点）的入度
            for next_cid in adj_list.get(&cid).unwrap_or(&Vec::new()) {
                let degree = in_degree.get_mut(next_cid).unwrap();
                *degree -= 1;
                if *degree == 0 {
                    queue.push(next_cid.clone());
                }
            }
        }
        
        // 检测环：如果未处理所有节点，则存在环
        if costs.len() != self.choices.len() {
            // 存在环，返回无限成本（或根据需要处理）
            return ExtendedCost::infinity();
        }
        
        // 收集所有作为子节点的类ID，用于识别根节点
        let mut all_children: FxHashSet<ClassId> = FxHashSet::default();
        for (_, node_id) in &self.choices {
            let node = &egraph.inner[node_id];
            for child in &node.children {
                all_children.insert(egraph.inner.nid_to_cid(child).clone());
            }
        }
        
        // 计算最终成本
        let mut final_costs = ExtendedCost::zero();
        let mut sum1 = NotNan::new(0.0).unwrap();
        let mut sum2 = NotNan::new(0.0).unwrap();
        
        // 组件1和2：所有唯一节点的总和
        for (cid, _) in &self.choices {
            sum1 += local_costs[cid].components[1];
            sum2 += local_costs[cid].components[2];
            
            // 组件0：根节点的关键路径最大值
            if !all_children.contains(cid) {
                let cost = &costs[cid];
                final_costs.components[0] = final_costs.components[0].max(cost.components[0]);
            }
        }
        
        final_costs.components[1] = sum1;
        final_costs.components[2] = sum2;
        
        final_costs
    }


    /// Legacy V1 timing model, retained only for regression.
    pub fn dag_cost_nldm_v1_on_pruned(&self, ext: &ExtendedEGraph,) -> ExtendedCost {
        // 1) 从根开始，仅保留 choices 中可达的类，构建精简子图
        let mut reachable: FxHashSet<ClassId> = FxHashSet::default();
        let in_egraph = &ext.inner;
        let mut q: VecDeque<ClassId> = in_egraph.root_eclasses.clone().into();
        while let Some(cid) = q.pop_front() {
            if !self.choices.contains_key(&cid) {
                // 根或其后继在 choices 中不存在，则跳过该分支
                continue;
            }
            if !reachable.insert(cid.clone()) {
                continue;
            }
            let nid = &self.choices[&cid];
            for ch in &in_egraph[nid].children {
                q.push_back(in_egraph.nid_to_cid(ch).clone());
            }
        }
        if reachable.is_empty() {
            return ExtendedCost::infinity();
        }
    
        // 2) 构建 child->parents 的邻接表与入度，仅在可达集合内
        let mut adj_list: FxHashMap<ClassId, Vec<ClassId>> = FxHashMap::default();
        let mut in_degree: FxHashMap<ClassId, usize> = FxHashMap::default();
        for cid in reachable.iter() {
            adj_list.insert(cid.clone(), Vec::new());
            in_degree.insert(cid.clone(), 0);
        }
        for cid in reachable.iter() {
            let nid = &self.choices[cid];
            let node = &in_egraph[nid];
            for ch in &node.children {
                let child_cid = in_egraph.nid_to_cid(ch);
                if reachable.contains(&child_cid) {
                    adj_list.entry(child_cid.clone()).or_default().push(cid.clone());
                    *in_degree.entry(cid.clone()).or_insert(0) += 1;
                }
            }
        }
    
        // 3) 入度为 0 的类作为拓扑起点（叶类）
        let mut stack: Vec<ClassId> = in_degree
            .iter()
            .filter(|(_, &deg)| deg == 0)
            .map(|(c, _)| c.clone())
            .collect();
    
        // 存储每个类的本地成本与累积成本
        let mut local_costs: FxHashMap<ClassId, ExtendedCost> = FxHashMap::default();
        let mut accum_costs: FxHashMap<ClassId, ExtendedCost> = FxHashMap::default();
    
        while let Some(cid) = stack.pop() {
            let nid = &self.choices[&cid];
            let node = &in_egraph[nid];
    
            // 操作名：优先用 ext.node_ops 映射，没有则回退用序列化节点上的 op
            let op = ext
                .node_ops
                .get(nid)
                .cloned()
                .unwrap_or_else(|| node.op.clone());
    
            // 计算本地成本（NLDM 查表）
            let mut local = ExtendedCost::zero();
    
            if !node.children.is_empty() {
                let parents = adj_list.get(&cid).cloned().unwrap_or_default();
    
                if let Some(nldm) = ext.cell_nldm.get(&op) {
                    // 子 pin 的输入 slew，来自子类累积成本的 transition 分量
                    let mut slews: HashMap<String, NotNan<f64>> = HashMap::new();
                    for (i, ch) in node.children.iter().enumerate() {
                        let ch_cid = in_egraph.nid_to_cid(ch);
                        let tr = accum_costs
                            .get(&ch_cid)
                            .map(|c| c.components[3])
                            .unwrap_or_else(|| NotNan::new(0.0).unwrap());
                        if let Some(pin) = nldm.pin_order.get(i + 1) {
                            slews.insert(pin.clone(), tr);
                        }
                    }
    
                    // 负载：若无父则设为 1.0，否则累加父输入引脚电容
                    let mut load = if parents.is_empty() {
                        NotNan::new(1.0).unwrap()
                    } else {
                        NotNan::new(0.0).unwrap()
                    };
    
                    for p in parents.iter() {
                        let pnid = &self.choices[p];
                        let pnode = &in_egraph[pnid];
                        let pop = ext
                            .node_ops
                            .get(pnid)
                            .cloned()
                            .unwrap_or_else(|| in_egraph[pnid].op.clone());
    
                        // 找当前 cid 在父节点 children 里的索引
                        if let Some(idx) = pnode
                            .children
                            .iter()
                            .position(|c| in_egraph.nid_to_cid(c) == &cid)
                        {
                            if let Some(pnldm) = ext.cell_nldm.get(&pop) {
                                if let Some(pin) = pnldm.pin_order.get(idx + 1) {
                                    if let Some((cap, _dir)) = pnldm.pin_info.get(pin) {
                                        load += *cap;
                                    }
                                }
                            } else {
                                // 父无 NLDM，给一个保守增量
                                load += NotNan::new(1.0).unwrap();
                            }
                        }
                    }
    
                    // 查表组成本地四元组
                    local.components[0] = nldm.lookup_table("delay", slews.clone(), load).unwrap_or(NotNan::new(0.0).unwrap());
                    local.components[1] = nldm.area;
                    local.components[2] = nldm.lookup_table("internal_power", slews.clone(), load).unwrap_or(NotNan::new(0.0).unwrap())
                        + nldm.leakage_power / NotNan::new(1000.0).unwrap();
                    local.components[3] = nldm.lookup_table("transition", slews.clone(), load).unwrap_or(NotNan::new(0.0).unwrap());
                } else {
                    // 缺 NLDM：本地成本置零
                    local = ExtendedCost::zero();
                }
            }
    
            local_costs.insert(cid.clone(), local);
    
            // 累积成本：delay = max(child.delay) + local.delay，其它分量从 local 传递
            let mut total = ExtendedCost::zero();
            let mut max_delay = NotNan::new(0.0).unwrap();
            for ch in node.children.iter() {
                let ch_cid = in_egraph.nid_to_cid(ch);
                if let Some(cc) = accum_costs.get(&ch_cid) {
                    max_delay = max_delay.max(cc.components[0]);
                }
            }
            total.components[0] = max_delay + local_costs[&cid].components[0];
            total.components[1] = local_costs[&cid].components[1];
            total.components[2] = local_costs[&cid].components[2];
            total.components[3] = local_costs[&cid].components[3];
    
            accum_costs.insert(cid.clone(), total);
    
            // 后继（父类）入度-1
            if let Some(nexts) = adj_list.get(&cid) {
                for nxt in nexts {
                    if let Some(d) = in_degree.get_mut(nxt) {
                        *d -= 1;
                        if *d == 0 {
                            stack.push(nxt.clone());
                        }
                    }
                }
            }
        }
    
        // 若未覆盖所有可达类，表示存在环或缺数据
        if accum_costs.len() != reachable.len() {
            return ExtendedCost::infinity();
        }
    
        // 4) 统计最终 PPA：delay 取根的最大关键路径；area/power 为所有唯一节点本地和；transition 保持与 dag_cost_nldm 一致（不额外聚合）
        let mut all_children: FxHashSet<ClassId> = FxHashSet::default();
        for cid in reachable.iter() {
            let nid = &self.choices[cid];
            for ch in &in_egraph[nid].children {
                all_children.insert(in_egraph.nid_to_cid(ch).clone());
            }
        }
    
        let mut final_cost = ExtendedCost::zero();
        let mut sum_area = NotNan::new(0.0).unwrap();
        let mut sum_power = NotNan::new(0.0).unwrap();
    
        for cid in reachable.iter() {
            sum_area += local_costs[cid].components[1];
            sum_power += local_costs[cid].components[2];
    
            if !all_children.contains(cid) {
                let acc = &accum_costs[cid];
                final_cost.components[0] = final_cost.components[0].max(acc.components[0]);
            }
        }
    
        final_cost.components[1] = sum_area;
        final_cost.components[2] = sum_power;
        // components[3]（transition）保持与 dag_cost_nldm 一致，不额外聚合
        final_cost
    }

    /// Formal exact extraction cost model. Since Timing V2 validation this
    /// name intentionally routes all existing search code to V2.
    pub fn dag_cost_nldm_on_pruned(&self, ext: &ExtendedEGraph) -> ExtendedCost {
        self.dag_cost_nldm_v2_on_pruned(ext)
    }
    
    // 计算单个节点的总成本（节点成本 + 所有子节点成本）
    pub fn node_sum_cost<M>(&self, egraph: &ExtendedEGraph, node_id: &NodeId, costs: &M) -> ExtendedCost
    where
        M: MapGet<ClassId, ExtendedCost>,
    {
        let node = &egraph.inner[node_id];
        
        // 获取所有子节点的成本
        let children_costs: Vec<ExtendedCost> = node.children.iter()
            .map(|n| {
                let cid = egraph.inner.nid_to_cid(n);
                costs.get(cid).cloned().unwrap_or(ExtendedCost::infinity())
            })
            .collect();
    
        // 获取当前节点的成本
        let node_cost = egraph.get_node_cost(node_id);
    
        // 计算总成本：
        // 1. 第一个分量使用关键路径
        // 2. 其他分量正常累加
        let total_cost = ExtendedCost::critical_path_cost(&children_costs) + node_cost;
    
        // 返回计算结果
        total_cost
    }
}
