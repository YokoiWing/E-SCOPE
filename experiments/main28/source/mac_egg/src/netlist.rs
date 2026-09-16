use itertools::Itertools;
use petgraph::graph::NodeIndex;
use petgraph::{Direction, Graph};
use std::iter::Rev;
use std::vec::IntoIter;

#[derive(Debug, Clone)]
pub struct Netlist<N, E> {
    pub graph: Graph<N, E>,
    pub roots: Vec<NodeIndex>,
    pub leaves: Vec<NodeIndex>,
}

impl<N, E> Default for Netlist<N, E> {
    fn default() -> Self {
        Self {
            graph: Default::default(),
            roots: Default::default(),
            leaves: Default::default(),
        }
    }
}

impl<N, E> Netlist<N, E> {
    pub fn inputs(&self, idx: NodeIndex) -> Rev<IntoIter<NodeIndex>> {
        self.graph.neighbors(idx).collect_vec().into_iter().rev() // petaGraph use linked list to push edges and visit from tail, so we must reverse
        // .collect_vec()
    }
    pub fn outputs(&self, idx: NodeIndex) -> Rev<IntoIter<NodeIndex>> {
        self.graph
            .neighbors_directed(idx, Direction::Incoming)
            .collect_vec()
            .into_iter()
            .rev() // petaGraph use linked list to push edges and visit from tail, so we must reverse
    }
}
