#![recursion_limit = "256"]

pub mod equivalence_candidate;
#[path = "topology_generator.rs"]
pub mod generator_v2;
pub mod objective;
pub mod phase_i;
#[path = "shared_timing.rs"]
pub mod shared_load_v2;
pub mod topology_expr;
pub mod topology_signature;
#[path = "sizing_potential.rs"]
pub mod v8_potential;
#[path = "promotion_policy.rs"]
pub mod v8_pro;
#[path = "search_policy.rs"]
pub mod v8_ultra;
