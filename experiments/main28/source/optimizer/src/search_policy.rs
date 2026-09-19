//! Configuration for the isolated V8-Ultra large-netlist experiment.
//!
//! V8-Ultra is opt-in.  When no configuration is supplied, or when the
//! candidate has fewer than `gate_threshold` instances, the frozen A2 path is
//! unchanged.  Large candidates may restrict mixed-sizing distributions to a
//! deterministic sparse set; written candidates still receive the ordinary
//! full-circuit Exact evaluation and cold reparse guard.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct V8UltraConfig {
    pub gate_threshold: usize,
    /// Optional structural-recall controls for the outer planner.  Zero
    /// preserves the historical 4 seeds / P2 keep 8 / R16 configuration.
    pub planner_top_seeds: usize,
    pub planner_p2_keep_per_region: usize,
    pub planner_max_region_size: usize,
    /// Optional D2AP-only compatible local-closure lane.  It packs several
    /// independently proved local transformations into a few topology
    /// candidates without widening the progressive Top-32 Exact schedule.
    pub d2ap_closure_candidate_cap: usize,
    pub d2ap_closure_max_transactions: usize,
    pub d2ap_closure_max_predicted_regression: f64,
    /// Evaluate D1 rewrite candidates with the same explicit TimingBoundary
    /// used by planner, Generator P1 and progressive Exact.  False preserves
    /// the historical reranker path.
    pub d1_p1_use_timing_boundary: bool,
    /// Opt-in D2AP access to the existing proof-carrying deletion, mapped
    /// k-feasible and 5/6-input bounded functional-window providers.
    pub d2ap_functional_window_candidate_cap: usize,
    pub d2ap_functional_window_root_cap: usize,
    /// Maximum distinct local divisor signatures considered per bounded
    /// functional root.  Zero disables the lane together with the two caps
    /// above; the historical programmable-objective lane is unaffected.
    pub d2ap_functional_window_divisor_cap: usize,
    /// Maximum replacement-area debt measured in minimum mapped-cell areas.
    /// The full-circuit Exact guard remains authoritative.
    pub d2ap_functional_window_max_area_debt_cells: usize,
    pub active_instance_cap: usize,
    pub timing_share_percent: usize,
    pub area_share_percent: usize,
    /// Optional stage-specific caps.  Zero inherits `active_instance_cap`.
    pub stage25_active_instance_cap: usize,
    pub stage50_active_instance_cap: usize,
    pub stage500_active_instance_cap: usize,
    /// Optional no-regression leader-only continuation after stage-500.
    /// The continuation resumes the stage-500 checkpoint, so its Tracker
    /// retains the baseline endpoint and can only improve it.
    pub leader_followup_active_instance_cap: usize,
    pub leader_followup_exact_budget: usize,
    /// Zero means no upper size bound.  A finite bound avoids paying the
    /// wider follow-up on very large candidates where it has not helped.
    pub leader_followup_max_instance_count: usize,
    /// Zero disables the large-only pre-materialization generator cap.
    pub pre_materialization_candidate_cap: usize,
    /// Optional tighter cap for very large parents.  Both values must be
    /// nonzero to enable the override.
    pub very_large_gate_threshold: usize,
    pub very_large_pre_materialization_candidate_cap: usize,
    /// Zero disables gain-saturation stopping.  Otherwise stopping is
    /// considered after this many completed rounds.
    pub early_stop_min_rounds: usize,
    pub early_stop_min_relative_gain: f64,
    pub early_stop_patience: usize,
}

impl Default for V8UltraConfig {
    fn default() -> Self {
        Self {
            gate_threshold: 1000,
            planner_top_seeds: 0,
            planner_p2_keep_per_region: 0,
            planner_max_region_size: 0,
            d2ap_closure_candidate_cap: 0,
            d2ap_closure_max_transactions: 0,
            d2ap_closure_max_predicted_regression: 0.0,
            d1_p1_use_timing_boundary: false,
            d2ap_functional_window_candidate_cap: 0,
            d2ap_functional_window_root_cap: 0,
            d2ap_functional_window_divisor_cap: 0,
            d2ap_functional_window_max_area_debt_cells: 0,
            active_instance_cap: 32,
            timing_share_percent: 50,
            area_share_percent: 25,
            stage25_active_instance_cap: 0,
            stage50_active_instance_cap: 0,
            stage500_active_instance_cap: 0,
            leader_followup_active_instance_cap: 0,
            leader_followup_exact_budget: 0,
            leader_followup_max_instance_count: 0,
            pre_materialization_candidate_cap: 0,
            very_large_gate_threshold: 0,
            very_large_pre_materialization_candidate_cap: 0,
            early_stop_min_rounds: 0,
            early_stop_min_relative_gain: 0.0,
            early_stop_patience: 1,
        }
    }
}

impl V8UltraConfig {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.gate_threshold > 0, "gate_threshold must be positive");
        anyhow::ensure!(
            self.active_instance_cap > 0,
            "active_instance_cap must be positive"
        );
        anyhow::ensure!(
            self.timing_share_percent <= 100,
            "timing_share_percent must not exceed 100"
        );
        anyhow::ensure!(
            self.area_share_percent <= 100,
            "area_share_percent must not exceed 100"
        );
        anyhow::ensure!(
            self.timing_share_percent + self.area_share_percent <= 100,
            "timing and area shares must leave a non-negative power share"
        );
        anyhow::ensure!(
            self.early_stop_min_relative_gain.is_finite()
                && self.early_stop_min_relative_gain >= 0.0
                && self.early_stop_min_relative_gain < 1.0,
            "early_stop_min_relative_gain must be finite and in [0, 1)"
        );
        anyhow::ensure!(
            self.early_stop_patience > 0,
            "early_stop_patience must be positive"
        );
        anyhow::ensure!(
            self.leader_followup_active_instance_cap == 0
                || self.leader_followup_exact_budget > 500,
            "leader follow-up requires an exact budget greater than 500"
        );
        anyhow::ensure!(
            (self.very_large_gate_threshold == 0)
                == (self.very_large_pre_materialization_candidate_cap == 0),
            "very-large candidate cap threshold and cap must be enabled together"
        );
        anyhow::ensure!(
            (self.d2ap_closure_candidate_cap == 0) == (self.d2ap_closure_max_transactions == 0),
            "D2AP closure candidate and transaction caps must be enabled together"
        );
        anyhow::ensure!(
            self.d2ap_closure_max_transactions == 0 || self.d2ap_closure_max_transactions >= 2,
            "D2AP closure requires at least two transactions"
        );
        anyhow::ensure!(
            self.d2ap_closure_max_predicted_regression.is_finite()
                && self.d2ap_closure_max_predicted_regression >= 0.0
                && self.d2ap_closure_max_predicted_regression < 0.1,
            "D2AP closure screening band must be finite and in [0, 0.1)"
        );
        anyhow::ensure!(
            (self.d2ap_functional_window_candidate_cap == 0)
                == (self.d2ap_functional_window_root_cap == 0)
                && (self.d2ap_functional_window_candidate_cap == 0)
                    == (self.d2ap_functional_window_divisor_cap == 0)
                && (self.d2ap_functional_window_candidate_cap == 0)
                    == (self.d2ap_functional_window_max_area_debt_cells == 0),
            "D2AP functional-window candidate, root, divisor and area-debt caps must be enabled together"
        );
        Ok(())
    }

    pub fn from_path(path: &Path) -> Result<Self> {
        let config: Self = serde_json::from_slice(&fs::read(path)?)
            .with_context(|| format!("invalid V8-Ultra config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn from_env() -> Result<Option<(PathBuf, Self)>> {
        let Some(path) = std::env::var_os("EGG_V8_ULTRA_CONFIG").map(PathBuf::from) else {
            return Ok(None);
        };
        let config = Self::from_path(&path)?;
        Ok(Some((path, config)))
    }

    pub fn enabled_for_instances(&self, instance_count: usize) -> bool {
        instance_count >= self.gate_threshold
    }

    pub fn planner_top_seeds(&self) -> usize {
        if self.planner_top_seeds == 0 {
            4
        } else {
            self.planner_top_seeds
        }
    }

    pub fn planner_p2_keep_per_region(&self) -> usize {
        if self.planner_p2_keep_per_region == 0 {
            8
        } else {
            self.planner_p2_keep_per_region
        }
    }

    pub fn planner_max_region_size(&self) -> usize {
        if self.planner_max_region_size == 0 {
            16
        } else {
            self.planner_max_region_size
        }
    }

    pub fn d2ap_closure_enabled(&self) -> bool {
        self.d2ap_closure_candidate_cap > 0 && self.d2ap_closure_max_transactions >= 2
    }

    pub fn timing_slots(&self) -> usize {
        self.timing_slots_for_cap(self.active_instance_cap)
    }

    pub fn area_slots(&self) -> usize {
        self.area_slots_for_cap(self.active_instance_cap)
    }

    pub fn power_slots(&self) -> usize {
        self.power_slots_for_cap(self.active_instance_cap)
    }

    pub fn active_instance_cap_for_budget(&self, budget: usize) -> usize {
        let configured = if budget > 500 && self.leader_followup_active_instance_cap > 0 {
            self.leader_followup_active_instance_cap
        } else if budget <= 25 {
            self.stage25_active_instance_cap
        } else if budget <= 50 {
            self.stage50_active_instance_cap
        } else {
            self.stage500_active_instance_cap
        };
        if configured == 0 {
            self.active_instance_cap
        } else {
            configured
        }
    }

    pub fn timing_slots_for_cap(&self, cap: usize) -> usize {
        cap * self.timing_share_percent / 100
    }

    pub fn area_slots_for_cap(&self, cap: usize) -> usize {
        cap * self.area_share_percent / 100
    }

    pub fn power_slots_for_cap(&self, cap: usize) -> usize {
        cap.saturating_sub(self.timing_slots_for_cap(cap) + self.area_slots_for_cap(cap))
    }

    pub fn early_stop_enabled(&self) -> bool {
        self.early_stop_min_rounds > 0 && self.early_stop_min_relative_gain > 0.0
    }

    pub fn candidate_cap_for_instances(&self, instance_count: usize) -> usize {
        if self.very_large_gate_threshold > 0 && instance_count >= self.very_large_gate_threshold {
            self.very_large_pre_materialization_candidate_cap
        } else {
            self.pre_materialization_candidate_cap
        }
    }

    pub fn leader_followup_enabled_for_instances(&self, instance_count: usize) -> bool {
        self.enabled_for_instances(instance_count)
            && self.leader_followup_active_instance_cap > 0
            && self.leader_followup_exact_budget > 500
            && (self.leader_followup_max_instance_count == 0
                || instance_count <= self.leader_followup_max_instance_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_large_only_and_partitions_cap() {
        let config = V8UltraConfig::default();
        config.validate().unwrap();
        assert!(!config.enabled_for_instances(999));
        assert!(config.enabled_for_instances(1000));
        assert_eq!(config.timing_slots(), 16);
        assert_eq!(config.area_slots(), 8);
        assert_eq!(config.power_slots(), 8);
        assert_eq!(config.active_instance_cap_for_budget(25), 32);
        assert_eq!(config.active_instance_cap_for_budget(50), 32);
        assert_eq!(config.active_instance_cap_for_budget(500), 32);
        assert_eq!(config.planner_top_seeds(), 4);
        assert_eq!(config.planner_p2_keep_per_region(), 8);
        assert_eq!(config.planner_max_region_size(), 16);
        assert!(!config.d2ap_closure_enabled());
        assert!(!config.early_stop_enabled());
    }

    #[test]
    fn stage_caps_and_early_stop_are_opt_in() {
        let config = V8UltraConfig {
            planner_top_seeds: 8,
            planner_p2_keep_per_region: 12,
            planner_max_region_size: 32,
            d2ap_closure_candidate_cap: 3,
            d2ap_closure_max_transactions: 8,
            d2ap_closure_max_predicted_regression: 0.002,
            d1_p1_use_timing_boundary: true,
            d2ap_functional_window_candidate_cap: 64,
            d2ap_functional_window_root_cap: 8,
            d2ap_functional_window_divisor_cap: 24,
            d2ap_functional_window_max_area_debt_cells: 8,
            stage25_active_instance_cap: 16,
            stage50_active_instance_cap: 32,
            stage500_active_instance_cap: 64,
            leader_followup_active_instance_cap: 128,
            leader_followup_exact_budget: 650,
            leader_followup_max_instance_count: 4096,
            pre_materialization_candidate_cap: 512,
            very_large_gate_threshold: 4096,
            very_large_pre_materialization_candidate_cap: 256,
            early_stop_min_rounds: 2,
            early_stop_min_relative_gain: 0.004,
            ..V8UltraConfig::default()
        };
        config.validate().unwrap();
        assert_eq!(config.active_instance_cap_for_budget(25), 16);
        assert_eq!(config.active_instance_cap_for_budget(50), 32);
        assert_eq!(config.active_instance_cap_for_budget(500), 64);
        assert_eq!(config.active_instance_cap_for_budget(650), 128);
        assert_eq!(config.planner_top_seeds(), 8);
        assert_eq!(config.planner_p2_keep_per_region(), 12);
        assert_eq!(config.planner_max_region_size(), 32);
        assert!(config.d2ap_closure_enabled());
        assert_eq!(config.timing_slots_for_cap(64), 32);
        assert_eq!(config.area_slots_for_cap(64), 16);
        assert_eq!(config.power_slots_for_cap(64), 16);
        assert!(!config.leader_followup_enabled_for_instances(999));
        assert!(config.leader_followup_enabled_for_instances(1550));
        assert!(!config.leader_followup_enabled_for_instances(5124));
        assert_eq!(config.candidate_cap_for_instances(1550), 512);
        assert_eq!(config.candidate_cap_for_instances(5124), 256);
        assert!(config.early_stop_enabled());
    }
}
