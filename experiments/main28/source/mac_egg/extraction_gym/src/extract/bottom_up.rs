use super::*;
// use crate::ExtendedEGraph;

pub struct BottomUpExtractor;
impl Extractor for BottomUpExtractor {
    fn extract(&self, egraph: &ExtendedEGraph, _roots: &[ClassId]) -> ExtractionResult {
        let mut result = ExtractionResult::default();
        let mut costs = FxHashMap::<ClassId, ExtendedCost>::with_capacity_and_hasher(
            egraph.inner.classes().len(),
            Default::default(),
        );
        let mut did_something = false;

        loop {
            for class in egraph.inner.classes().values() {
                for node in &class.nodes {
                    let cost = result.node_sum_cost(egraph, node, &costs);
                    let infinity_cost = ExtendedCost::infinity();
                    if &cost < costs.get(&class.id).unwrap_or(&infinity_cost) {
                        result.choose(class.id.clone(), node.clone());
                        costs.insert(class.id.clone(), cost);
                        did_something = true;
                    }
                }
            }

            if did_something {
                did_something = false;
            } else {
                break;
            }
        }

        result
    }
}
