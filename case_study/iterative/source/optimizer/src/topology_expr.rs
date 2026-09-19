//! Source-independent topology expression shared by rewrite and generator lanes.

use mac_egg::physical_topology_egraph::TopologyExpr as PhysicalTopologyExpr;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TopologyExpr {
    Anchor {
        anchor: usize,
    },
    Cell {
        op: String,
        children: Vec<TopologyExpr>,
    },
}

impl From<&PhysicalTopologyExpr> for TopologyExpr {
    fn from(expression: &PhysicalTopologyExpr) -> Self {
        match expression {
            PhysicalTopologyExpr::Anchor(anchor) => Self::Anchor {
                anchor: anchor.index(),
            },
            PhysicalTopologyExpr::Cell { op, children } => Self::Cell {
                op: op.clone(),
                children: children.iter().map(Self::from).collect(),
            },
        }
    }
}
