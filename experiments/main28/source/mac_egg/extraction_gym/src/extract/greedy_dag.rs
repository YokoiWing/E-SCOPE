use super::*;
use rustc_hash::FxHashMap;
use egraph_serialize::{ClassId, NodeId};
use ordered_float::NotNan;


struct CostSet {
    costs: FxHashMap<ClassId, ExtendedCost>,
    total: ExtendedCost,
    choice: NodeId,
}

pub struct GreedyDagExtractor;
impl Extractor for GreedyDagExtractor {
    fn extract(&self, egraph: &ExtendedEGraph, _roots: &[ClassId]) -> ExtractionResult {
        let mut costs = FxHashMap::<ClassId, CostSet>::with_capacity_and_hasher(
            egraph.inner.classes().len(),
            Default::default(),
        );

        let mut keep_going = true;

        let mut i = 0;
        while keep_going {
            i += 1;
            println!("iteration {}", i);
            keep_going = false;

            'node_loop: for (node_id, node) in &egraph.inner.nodes {
                let cid = egraph.inner.nid_to_cid(node_id);
                let mut cost_set = CostSet {
                    costs: Default::default(),
                    total: ExtendedCost::zero(),
                    choice: node_id.clone(),
                };

                // compute the cost set from the children
                for child in &node.children {
                    let child_cid = egraph.inner.nid_to_cid(child);
                    if let Some(child_cost_set) = costs.get(child_cid) {
                        // prevent a cycle
                        if child_cost_set.costs.contains_key(cid) {
                            continue 'node_loop;
                        }
                        cost_set.costs.extend(child_cost_set.costs.clone());
                    } else {
                        continue 'node_loop;
                    }
                }
                
                // let node_cost = egraph.get_node_cost(node_id);
                // add this node
                // cost_set.costs.insert(cid.clone(), node_cost);

                // cost_set.total = cost_set.costs.values().sum();

                for child in &node.children {
                    let child_cid = egraph.inner.nid_to_cid(child);
                    if let Some(child_cost_set) = costs.get(child_cid) {
                        if child_cost_set.costs.contains_key(cid) {
                            continue 'node_loop;
                        }
                        cost_set.costs.extend(child_cost_set.costs.clone());
                    } else {
                        continue 'node_loop;
                    }
                }
                
                let node_cost = egraph.get_node_cost(node_id);
                
                // 计算子节点的成本
                let children_costs = cost_set.costs.values();
                
                // 第一个分量：找到关键路径（最大路径和）
                let critical_path = node_cost.components[0] + children_costs
                    .clone()
                    .map(|cost| cost.components[0])
                    .max()
                    .unwrap_or(NotNan::new(0.0).unwrap());
                
                // 第二、三个分量：直接求和
                let other_components_sum = children_costs.fold(
                    [node_cost.components[1], node_cost.components[2]],
                    |mut acc, cost| {
                        acc[0] = acc[0] + cost.components[1];
                        acc[1] = acc[1] + cost.components[2];
                        acc
                    }
                );
                
                // 设置总成本
                cost_set.total = ExtendedCost {
                    components: [
                        critical_path,
                        other_components_sum[0],
                        other_components_sum[1],
                        node_cost.components[3], // 第四个分量保持不变
                        node_cost.components[4],
                    ]
                };
                
                // 最后添加当前节点的成本
                cost_set.costs.insert(cid.clone(), node_cost);

                // if the cost set is better than the current one, update it
                if let Some(old_cost_set) = costs.get(cid) {
                    if cost_set.total < old_cost_set.total {
                        costs.insert(cid.clone(), cost_set);
                        keep_going = true;
                    }
                } else {
                    costs.insert(cid.clone(), cost_set);
                    keep_going = true;
                }
            }
        }

        let mut result = ExtractionResult::default();
        for (cid, cost_set) in costs {
            result.choose(cid, cost_set.choice);
        }
        result
    }
}
