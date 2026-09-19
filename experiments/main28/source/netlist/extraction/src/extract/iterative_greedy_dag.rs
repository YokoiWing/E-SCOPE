use std::iter;
use ordered_float::NotNan;
use rpds::HashTrieSet;
use std::collections::HashSet;

use super::*;

use egraph_serialize::{Node, ClassId, NodeId}; 

//use crate::ExtendedEGraph; 

type TermId = usize;

#[derive(Clone, PartialEq, Eq, Hash)]
struct Term {
    op: String,
    children: Vec<TermId>,
}

type Reachable = HashTrieSet<ClassId>;

struct TermInfo {
    node: NodeId,
    eclass: ClassId,
    node_cost: ExtendedCost,
    total_cost: ExtendedCost,
    // store the set of reachable terms from this term
    reachable: Reachable,
    size: usize,
}

/// A TermDag needs to store terms that share common
/// subterms using a hashmap.
/// However, it also critically needs to be able to answer
/// reachability queries in this dag `reachable`.
/// This prevents double-counting costs when
/// computing the cost of a term.
#[derive(Default)]
pub struct TermDag {
    nodes: Vec<Term>,
    info: Vec<TermInfo>,
    hash_cons: HashMap<Term, TermId>,
}

impl TermDag {
    /// Makes a new term using a node and children terms
    /// Correctly computes total_cost with sharing
    /// If this term contains itself, returns None
    /// If this term costs more than target, returns None
    pub fn make(
        &mut self,
        egraph: &ExtendedEGraph,
        eclass_parents: &HashMap<ClassId, Vec<(NodeId, String)>>,
        class_load: &HashMap<ClassId, NotNan<f64>>,
        node_id: NodeId,
        node: &Node,
        children: Vec<TermId>,
        target: ExtendedCost,
        mut memo: HashMap<TermId, (NotNan<f64>, NotNan<f64>)> 
    ) -> Option<TermId> {
        let term = Term {
            op: node.op.clone(),
            children: children.clone(),
        };
        // println!("  尝试创建新Term:");
        // println!("    节点: {:?}", node_id);
        // println!("    操作: {}", node.op);
        // println!("    子项: {:?}", children);
        // println!("    目标成本: {:?}", target);
        if let Some(id) = self.hash_cons.get(&term) {
            return Some(*id);
        }
           
        let mut node_cost = ExtendedCost::zero();
        if let Some(nldm) = egraph.cell_nldm.get(node.op.as_str()) {
            let load = class_load[&node.eclass];
            let mut slews = HashMap::new(); 
            for child_id in &term.children {
                let info = &self.info[*child_id];
                for (pid, pin) in &eclass_parents[&info.eclass] {
                    if *pid == node_id {
                        slews.insert(
                            pin.clone(),
                            info.node_cost.components[3],
                        );
                    }
                }
            }
            node_cost.components[0] = nldm.lookup_table("delay", slews.clone(), load).unwrap_or(NotNan::new(0.0).unwrap());
            node_cost.components[1] = nldm.area;
            node_cost.components[2] = nldm.lookup_table("internal_power", slews.clone(), load).unwrap_or(NotNan::new(0.0).unwrap()) + nldm.leakage_power / 1000.0; 
            node_cost.components[3] = nldm.lookup_table("transition", slews.clone(), load).unwrap_or(NotNan::new(0.0).unwrap());
            node_cost.components[4] = nldm.pin_info.values().map(|info| info.0).sum();
        } 

        if children.is_empty() {
            // println!("    叶子节点，直接使用节点成本: {:?}", node_cost);
            let next_id = self.nodes.len();
            self.nodes.push(term.clone());
            // println!("    新Term节点: {:?}, 节点termid标号:{:?}", node_id, self.info.len());
            self.info.push(TermInfo {
                node: node_id,
                eclass: node.eclass.clone(),
                node_cost,
                total_cost: node_cost,
                reachable: iter::once(node.eclass.clone()).collect(),
                size: 1,
            });
            self.hash_cons.insert(term, next_id);
            Some(next_id)
        } else {
            // check if children contains this node, preventing cycles
            // This is sound because `reachable` is the set of reachable eclasses
            // from this term.
            for child in &children {
                if self.info[*child].reachable.contains(&node.eclass) {
                    // println!("    检测到循环依赖，终止创建");
                    return None;
                }
            }   

            // let biggest_child = (0..children.len())
            //     .max_by_key(|i| self.info[children[*i]].size)
            //     .unwrap();
            // let mut cost = node_cost + self.total_cost(children[biggest_child]);
            let mut cost = node_cost;
            let mut children_costs: Vec<ExtendedCost> = Vec::new();
            let mut reachable = self.info[children[0]].reachable.clone();
            let next_id = self.nodes.len();
            // let mut visited = HashSet::new();
            // let (total_comp1, total_comp2) = children.iter()
            //         .map(|child| self.collect_descendant_costs(*child, &mut visited, &mut memo))
            //         .fold(
            //             (NotNan::new(0.0).unwrap(), NotNan::new(0.0).unwrap()),
            //             |acc, x| (acc.0 + x.0, acc.1 + x.1)
            //         );
            

            for child in children.iter() {
                let child_total_cost = self.info[*child].total_cost;
                children_costs.push(child_total_cost);
            }
            // println!("    所有子节点的总成本 {:?}",  children_costs);
            let max_first_component = children_costs.iter()
                .map(|cost| cost.components[0])
                .max()
                .unwrap_or(NotNan::new(0.0).unwrap());
            cost.components[0] += max_first_component;
            // cost.components[1] += total_comp1;
            // cost.components[2] += total_comp2;
            
            if cost.abs() > target.abs() {
                // println!("    成本 {:?} 超过目标 {:?}，终止创建", cost, target);
                return None;
            }
            // println!("    创建成功，总成本: {:?}", cost);
            reachable = reachable.insert(node.eclass.clone());
            // println!("    新Term节点: {:?}, 节点termid标号:{:?}", node_id, self.info.len());
            self.info.push(TermInfo {
                node: node_id,
                node_cost,
                eclass: node.eclass.clone(),
                total_cost: cost,
                reachable,
                size: 1 + children.iter().map(|c| self.info[*c].size).sum::<usize>(),
            });
            
            self.nodes.push(term.clone());
            self.hash_cons.insert(term, next_id);
            Some(next_id)
        }
    }
    
    pub fn node_cost(&self, id: TermId) -> ExtendedCost {
        self.info[id].node_cost
    }

    pub fn total_cost(&self, id: TermId) -> ExtendedCost {
        self.info[id].total_cost
    }

    pub fn collect_descendant_costs(&self, term_id: TermId, visited: &mut HashSet<ClassId>, memo: &mut HashMap<TermId, (NotNan<f64>, NotNan<f64>)>,) -> (NotNan<f64>, NotNan<f64>) {
        let info = &self.info[term_id];
        let term = &self.nodes[term_id];
        
        // 如果这个等价类已经访问过，跳过以避免重复计算
        if !visited.insert(info.eclass.clone()) {
            return (NotNan::new(0.0).unwrap(), NotNan::new(0.0).unwrap());
        }
        let has_overlap = info
            .reachable
            .iter()
            .any(|c| *c != info.eclass && visited.contains(c));
        if !has_overlap {
            if let Some(&(c1, c2)) = memo.get(&term_id) {
                // 标记其所有可达类为已访问，避免后续重复计数
                for c in info.reachable.iter() {
                    visited.insert(c.clone());
                }
                println!("    使用备忘录中的成本: ({:?}, {:?})", c1, c2);
                return (c1, c2);
            }
        }
        
        // 初始化为当前节点的成本
        let mut comp1 = info.node_cost.components[1];
        let mut comp2 = info.node_cost.components[2];
        
        // 递归收集所有子节点的成本
        for child_id in &term.children {
            let (c1, c2) = self.collect_descendant_costs(*child_id, visited, memo);
            comp1 += c1;
            comp2 += c2;
        }
        if !has_overlap {
            memo.insert(term_id, (comp1, comp2));
        }
        
        (comp1, comp2)
    }
}

pub struct IterativeGreedyDagExtractor;
impl Extractor for IterativeGreedyDagExtractor {
    fn extract(&self, egraph: &ExtendedEGraph, _roots: &[ClassId]) -> ExtractionResult {
        
        let mut eclass_parents: HashMap<ClassId, Vec<(NodeId, String)>> = HashMap::new();
        for (node_id, node) in &egraph.inner.nodes {
            for (i, child) in node.children.iter().enumerate() {
                let child_class = egraph.inner.nid_to_cid(child);
                let pin = if let Some(nldm) = egraph.cell_nldm.get(&node.op) {
                    nldm.pin_order.get(i + 1).cloned().unwrap_or_else(|| "O".to_string())
                } else {
                    "O".to_string()
                };

                eclass_parents
                    .entry(child_class.clone())
                    .or_insert_with(Vec::new)
                    .push((node_id.clone(), pin));
            }
        }
        let mut parent_to_children: HashMap<NodeId, Vec<(ClassId, String)>> = HashMap::new();
        for (cid, parents) in &eclass_parents {
            for (pid, pin) in parents {
                parent_to_children.entry(pid.clone()).or_default().push((cid.clone(), pin.clone()));
            }
        }
        let mut selected_nodes: HashSet<NodeId> = HashSet::new();
        // println!("初始化load\n");
        let mut class_load = HashMap::new();
        for (cid, parents) in &eclass_parents {
            let mut load = NotNan::new(0.0).unwrap();
            for (parent_nid, pin) in parents {
                let parent_node = &egraph.inner[parent_nid];
                let parent_op = &egraph.node_ops[parent_nid];
                if !egraph.cell_nldm.contains_key(parent_op){
                    load += NotNan::new(1.0).unwrap();
                    continue;
                }else{
                    let parent_nldm = &egraph.cell_nldm[parent_op];
                    let parent_eclass_size = egraph.inner.classes()[&parent_node.eclass].nodes.len();
                    load += NotNan::new(0.0).unwrap() * parent_nldm.pin_info[pin].0 / NotNan::new(parent_eclass_size as f64).unwrap();
                }
            }
            class_load.insert(cid.clone(), load);
        }
        
        // println!("\n=== 开始全局贪心 DAG 提取 ===");
        let mut keep_going = 1;
        let nodes = egraph.inner.nodes.clone();
        let mut termdag = TermDag::default();
        let mut best_in_class: HashMap<ClassId, TermId> = HashMap::default();
        let mut record_choices: HashMap<ClassId, NodeId> = HashMap::default();
        let mut i = 0;
        let debug_eclasses: HashSet<ClassId> = vec![
            ClassId::from("89"),
            ClassId::from("120"),
            ClassId::from("382"),
            ClassId::from("913"),
            ClassId::from("1436"),
            ClassId::from("1661"),
            ClassId::from("2245"),
            ClassId::from("2484"),
            ClassId::from("2867"),
            ClassId::from("3301"),
        ]
        .into_iter()
        .collect();

        while keep_going > 0 {
            i += 1;
            println!("\n迭代 {} 开始了, keep_going={}", i, keep_going);
            println!("      当前最优选择数量: {}", best_in_class.len());
            keep_going -= 1;
            let mut loop_update = true;
            let mut memo: HashMap<TermId, (NotNan<f64>, NotNan<f64>)> = HashMap::new();
            let mut cnt = 0;
            let mut new_cnt = 0;
            let mut changed_parents: HashSet<NodeId> = HashSet::new();
            'node_loop: for (node_id, node) in &nodes {
                // println!("\n处理节点 {:?}", node_id);
                // println!("所属类: {:?}", node.eclass);
                // println!("操作: {}", node.op);
                // println!("子节点: {:?}", node.children);
                let mut children: Vec<TermId> = vec![];
                // 收集子节点的最优选择
                for child in &node.children {
                    let child_cid = egraph.inner.nid_to_cid(child);
                    if let Some(best) = best_in_class.get(child_cid) {
                        // println!("  子节点 {:?} 的最优选择: Term{}", child, best);
                        children.push(*best);
                    } else {
                        // println!("  子节点 {:?} 尚未有最优选择，跳过当前节点", child);
                        continue 'node_loop;
                    }
                }
                
                let old_cost = best_in_class
                    .get(&node.eclass)
                    .map(|id| termdag.total_cost(*id))
                    .unwrap_or(ExtendedCost::infinity());
                // println!("当前类的最优成本: {:?}", old_cost);

                if let Some(candidate) = termdag.make(egraph, &eclass_parents, &class_load, node_id.clone(), node, children, old_cost, memo.clone()) {
                    cnt += 1;
                    let candidate_cost = termdag.total_cost(candidate);
                    // println!("新候选项成本: {:?}", candidate_cost);

                    if candidate_cost.abs() < old_cost.abs() {
                        // println!("发现更优解！更新类 {:?}", node.eclass);
                        // println!("旧成本: {:?}", old_cost);
                        // println!("新成本: {:?}", candidate_cost);
                        new_cnt += 1;
                        if let Some(prev) = record_choices.insert(node.eclass.clone(), node_id.clone()) {
                            if prev != *node_id {
                                selected_nodes.remove(&prev);
                                changed_parents.insert(prev);
                            }
                        }
                        if selected_nodes.insert(node_id.clone()) {
                            changed_parents.insert(node_id.clone());
                        }

                        best_in_class.insert(node.eclass.clone(), candidate);

                        if loop_update {
                            keep_going += 1;
                            loop_update = false;
                        }
                    } else {
                        // println!("未找到更优解，保持原有选择");
                    }
                } else {
                    // println!("无法创建候选项（可能存在循环或超出目标成本）");
                }
            }
            println!("本轮处理节点数: {}, 更新数: {}, 有效更新数: {}", nodes.len(), cnt, new_cnt);
            if !changed_parents.is_empty() {
                let mut affected_classes: HashSet<ClassId> = HashSet::new();
                for pnid in &changed_parents {
                    if let Some(children) = parent_to_children.get(pnid) {
                        for (cid, _pin) in children {
                            affected_classes.insert(cid.clone());
                        }
                    }
                }

                for cid in affected_classes {
                    let mut load = NotNan::new(0.0).unwrap();
                    if let Some(parents) = eclass_parents.get(&cid) {
                        for (parent_nid, pin) in parents {
                            let parent_op = &egraph.node_ops[parent_nid];
                            if !egraph.cell_nldm.contains_key(parent_op) {
                                // 未建模的父：固定+1.0
                                if selected_nodes.contains(parent_nid) {
                                    load += NotNan::new(1.0).unwrap();
                                }
                            } else if selected_nodes.contains(parent_nid) {
                                let parent_nldm = &egraph.cell_nldm[parent_op];
                                load += parent_nldm.pin_info[pin].0;
                            }
                        }
                    }
                    if let Some(previous_load) = class_load.get(&cid) {
                        let average_load = (*previous_load + load) / NotNan::new(2.0).unwrap();
                        class_load.insert(cid.clone(), average_load);
                    } else {
                        class_load.insert(cid.clone(), load);
                    }
                    if debug_eclasses.contains(&cid) {
                        println!("       类 {:?} 的负载更新为: {:?}", cid, class_load.get(&cid).unwrap());
                    }
                }
            }
            // println!("\n迭代 {} 完成", i);
            // println!("当前已处理的类: {:?}", best_in_class.keys().collect::<Vec<_>>());
        }

        println!("\n=== 提取完成 ===");
        println!("总迭代次数: {}", i);
        // println!("最终选择: {:?}", record_choices);

        let mut result = ExtractionResult::default();
        for (class, term) in best_in_class {
            result.choose(class, termdag.info[term].node.clone());
        }
        result
    }
}
