use egraph_serialize::{EGraph as BaseEGraph, Node as BaseNode, NodeId};
use std::collections::HashMap;

pub mod egraph;
pub mod cost;
pub mod nldm;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtendedNode {
    pub inner: BaseNode,
    pub extended_cost: ExtendedCost,
}

pub mod extract;
pub use cost::ExtendedCost;
pub use egraph::ExtendedEGraph;
pub use nldm::{
    DEFAULT_PRIMARY_INPUT_DRIVER_CELL, Lut2D, LutLookupWithPartials, NLDM,
    PrimaryInputDriverTiming, TimingArc, TimingSense,
};


// Re-export types from egraph_serialize that we still want to use directly
// pub use egraph_serialize::{NodeId, ClassId};
