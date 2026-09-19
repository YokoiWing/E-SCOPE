//! Objective definitions for the experimental objective-programmable Union path.
//!
//! `FrozenD2ap` deliberately has a dedicated arithmetic path.  It is the
//! compatibility preset for Union V7 and must not be replaced by a generic
//! expression evaluator.  The other variants describe minimization objectives
//! over Internal-NLDM-V2 delay, area and power.  Hard constraints are compared
//! feasible-first and are never encoded with an arbitrary large scalar.

use std::{cmp::Ordering, fs, path::Path, sync::Arc};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PpaPoint {
    pub delay: f64,
    pub area: f64,
    pub power: f64,
}

impl PpaPoint {
    pub fn validate(self, label: &str) -> Result<()> {
        for (metric, value) in [
            (Metric::Delay, self.delay),
            (Metric::Area, self.area),
            (Metric::Power, self.power),
        ] {
            ensure!(
                value.is_finite() && value > 0.0,
                "{label} {} must be finite and positive, got {value}",
                metric.name()
            );
        }
        Ok(())
    }

    pub fn get(self, metric: Metric) -> f64 {
        match metric {
            Metric::Delay => self.delay,
            Metric::Area => self.area,
            Metric::Power => self.power,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    Delay,
    Area,
    Power,
}

impl Metric {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Delay => "delay",
            Self::Area => "area",
            Self::Power => "power",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricWeights {
    #[serde(default)]
    pub delay: f64,
    #[serde(default)]
    pub area: f64,
    #[serde(default)]
    pub power: f64,
}

impl MetricWeights {
    pub const fn d2ap() -> Self {
        Self {
            delay: 2.0,
            area: 1.0,
            power: 1.0,
        }
    }

    fn values(self) -> [(Metric, f64); 3] {
        [
            (Metric::Delay, self.delay),
            (Metric::Area, self.area),
            (Metric::Power, self.power),
        ]
    }

    fn validate_nonnegative_nonzero(self, label: &str) -> Result<()> {
        let mut any_positive = false;
        for (metric, value) in self.values() {
            ensure!(
                value.is_finite() && value >= 0.0,
                "{label} {} weight must be finite and nonnegative, got {value}",
                metric.name()
            );
            any_positive |= value > 0.0;
        }
        ensure!(
            any_positive,
            "{label} must contain at least one positive weight"
        );
        Ok(())
    }

    fn scale(self, factor: f64) -> Self {
        Self {
            delay: self.delay * factor,
            area: self.area * factor,
            power: self.power * factor,
        }
    }

    fn add(self, other: Self) -> Self {
        Self {
            delay: self.delay + other.delay,
            area: self.area + other.area,
            power: self.power + other.power,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObjectiveFunction {
    /// Compatibility fast path: exactly `delay * delay * area * power`.
    FrozenD2ap,
    MinimizeMetric {
        metric: Metric,
    },
    /// Product of reference-normalized metrics raised to nonnegative powers.
    Product {
        exponents: MetricWeights,
    },
    /// Sum of reference-normalized metrics with nonnegative weights.
    WeightedNormalizedSum {
        weights: MetricWeights,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintRelation {
    AtMost,
    AtLeast,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricConstraint {
    pub metric: Metric,
    pub relation: ConstraintRelation,
    pub bound: f64,
}

impl MetricConstraint {
    fn validate(self, index: usize) -> Result<()> {
        ensure!(
            self.bound.is_finite() && self.bound > 0.0,
            "constraint {index} bound must be finite and positive, got {}",
            self.bound
        );
        Ok(())
    }

    fn relative_violation(self, point: PpaPoint) -> f64 {
        let value = point.get(self.metric);
        match self.relation {
            ConstraintRelation::AtMost => (value / self.bound - 1.0).max(0.0),
            ConstraintRelation::AtLeast => (self.bound / value - 1.0).max(0.0),
        }
    }

    fn relative_slack(self, point: PpaPoint) -> f64 {
        let value = point.get(self.metric);
        match self.relation {
            ConstraintRelation::AtMost => (self.bound - value) / self.bound,
            ConstraintRelation::AtLeast => (value - self.bound) / self.bound,
        }
    }

    fn relative_gradient(self, point: PpaPoint) -> MetricWeights {
        let ratio = point.get(self.metric) / self.bound;
        let signed = match self.relation {
            ConstraintRelation::AtMost => ratio,
            ConstraintRelation::AtLeast => -1.0 / ratio,
        };
        match self.metric {
            Metric::Delay => MetricWeights {
                delay: signed,
                ..MetricWeights::default()
            },
            Metric::Area => MetricWeights {
                area: signed,
                ..MetricWeights::default()
            },
            Metric::Power => MetricWeights {
                power: signed,
                ..MetricWeights::default()
            },
        }
    }

    fn signed_pressure(self, point: PpaPoint, guard_ratio: f64) -> f64 {
        let value = point.get(self.metric);
        match self.relation {
            ConstraintRelation::AtMost => value / (guard_ratio * self.bound) - 1.0,
            ConstraintRelation::AtLeast => self.bound / (guard_ratio * value) - 1.0,
        }
    }
}

// Internal-NLDM replay can differ by a few final floating-point bits after a
// write/cold-parse cycle.  Treat only roundoff-scale relative violations as
// feasible; this is a numerical comparison tolerance, not constraint
// relaxation or an objective-specific guard band.
const CONSTRAINT_RELATIVE_FEASIBILITY_TOLERANCE: f64 = 1e-12;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrimalDualPolicy {
    pub initial_multiplier: f64,
    pub step_size: f64,
    pub guard_ratio: f64,
    /// Feasible constraints inside this relative slack band remain active in
    /// first-order search guidance.  This is an active-set trust band, not a
    /// tighter acceptance constraint: Exact acceptance still uses `bound`.
    pub active_slack_ratio: f64,
}

impl Default for PrimalDualPolicy {
    fn default() -> Self {
        Self {
            initial_multiplier: 1.0,
            step_size: 2.0,
            // Update against the real constraint boundary.  A hidden 0.98
            // target kept rewarding timing slack after a point was feasible
            // and could steer constrained-area search away from area.
            guard_ratio: 1.0,
            active_slack_ratio: 0.01,
        }
    }
}

impl PrimalDualPolicy {
    pub fn validate(self) -> Result<()> {
        ensure!(
            self.initial_multiplier.is_finite() && self.initial_multiplier >= 0.0,
            "initial primal-dual multiplier must be finite and nonnegative"
        );
        ensure!(
            self.step_size.is_finite() && self.step_size > 0.0,
            "primal-dual step size must be finite and positive"
        );
        ensure!(
            self.guard_ratio.is_finite() && self.guard_ratio > 0.0 && self.guard_ratio <= 1.0,
            "primal-dual guard ratio must be in (0, 1]"
        );
        ensure!(
            self.active_slack_ratio.is_finite()
                && self.active_slack_ratio >= 0.0
                && self.active_slack_ratio < 1.0,
            "active constraint slack ratio must be in [0, 1)"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PrimalDualUpdate {
    pub before: Vec<f64>,
    pub signed_pressure: Vec<f64>,
    pub after: Vec<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectiveSpec {
    pub schema_version: u32,
    pub name: String,
    pub objective: ObjectiveFunction,
    #[serde(default)]
    pub constraints: Vec<MetricConstraint>,
}

impl ObjectiveSpec {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn frozen_d2ap() -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            name: "frozen_d2ap".to_owned(),
            objective: ObjectiveFunction::FrozenD2ap,
            constraints: Vec::new(),
        }
    }

    pub fn from_json_str(input: &str) -> Result<Self> {
        let spec: Self = serde_json::from_str(input).context("invalid ObjectiveSpec JSON")?;
        spec.validate()?;
        Ok(spec)
    }

    pub fn from_json_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let input = fs::read_to_string(path)
            .with_context(|| format!("failed to read ObjectiveSpec {}", path.display()))?;
        Self::from_json_str(&input)
            .with_context(|| format!("failed to load ObjectiveSpec {}", path.display()))
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == Self::SCHEMA_VERSION,
            "unsupported ObjectiveSpec schema_version {}; expected {}",
            self.schema_version,
            Self::SCHEMA_VERSION
        );
        ensure!(
            !self.name.trim().is_empty(),
            "ObjectiveSpec name must not be empty"
        );
        match &self.objective {
            ObjectiveFunction::FrozenD2ap => ensure!(
                self.constraints.is_empty(),
                "frozen_d2ap is the constraint-free Union V7 compatibility preset"
            ),
            ObjectiveFunction::MinimizeMetric { .. } => {}
            ObjectiveFunction::Product { exponents } => {
                exponents.validate_nonnegative_nonzero("product exponents")?;
            }
            ObjectiveFunction::WeightedNormalizedSum { weights } => {
                weights.validate_nonnegative_nonzero("weighted sum")?;
            }
        }
        for (index, constraint) in self.constraints.iter().copied().enumerate() {
            constraint.validate(index)?;
        }
        Ok(())
    }

    pub fn bind(self, reference: PpaPoint) -> Result<ObjectiveContext> {
        self.validate()?;
        reference.validate("objective reference")?;
        let constraint_multipliers = vec![0.0; self.constraints.len()];
        Ok(ObjectiveContext {
            spec: self,
            reference,
            constraint_multipliers,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ObjectiveEvaluation {
    pub feasible: bool,
    pub objective_value: f64,
    pub total_relative_violation: f64,
    pub max_relative_violation: f64,
    pub relative_violations: Vec<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ObjectiveContext {
    spec: ObjectiveSpec,
    reference: PpaPoint,
    constraint_multipliers: Vec<f64>,
}

impl ObjectiveContext {
    pub fn spec(&self) -> &ObjectiveSpec {
        &self.spec
    }

    pub fn reference(&self) -> PpaPoint {
        self.reference
    }

    pub fn constraint_multipliers(&self) -> &[f64] {
        &self.constraint_multipliers
    }

    pub fn set_constraint_multipliers(&mut self, multipliers: Vec<f64>) -> Result<()> {
        ensure!(
            multipliers.len() == self.spec.constraints.len(),
            "expected {} constraint multipliers, got {}",
            self.spec.constraints.len(),
            multipliers.len()
        );
        for (index, multiplier) in multipliers.iter().copied().enumerate() {
            ensure!(
                multiplier.is_finite() && multiplier >= 0.0,
                "constraint multiplier {index} must be finite and nonnegative, got {multiplier}"
            );
        }
        self.constraint_multipliers = multipliers;
        Ok(())
    }

    pub fn initialize_primal_dual(&mut self, policy: PrimalDualPolicy) -> Result<()> {
        policy.validate()?;
        self.constraint_multipliers = vec![policy.initial_multiplier; self.spec.constraints.len()];
        Ok(())
    }

    pub fn update_primal_dual(
        &mut self,
        point: PpaPoint,
        policy: PrimalDualPolicy,
    ) -> Result<PrimalDualUpdate> {
        point.validate("primal-dual point")?;
        policy.validate()?;
        let before = self.constraint_multipliers.clone();
        let signed_pressure: Vec<_> = self
            .spec
            .constraints
            .iter()
            .map(|constraint| constraint.signed_pressure(point, policy.guard_ratio))
            .collect();
        for (multiplier, pressure) in self.constraint_multipliers.iter_mut().zip(&signed_pressure) {
            *multiplier = (*multiplier + policy.step_size * pressure).max(0.0);
        }
        Ok(PrimalDualUpdate {
            before,
            signed_pressure,
            after: self.constraint_multipliers.clone(),
        })
    }

    pub fn evaluate(&self, point: PpaPoint) -> Result<ObjectiveEvaluation> {
        point.validate("objective point")?;
        let objective_value = self.objective_value(point);
        ensure!(
            objective_value.is_finite() && objective_value >= 0.0,
            "objective value must be finite and nonnegative, got {objective_value}"
        );
        let relative_violations: Vec<f64> = self
            .spec
            .constraints
            .iter()
            .map(|constraint| constraint.relative_violation(point))
            .collect();
        let total_relative_violation = relative_violations.iter().sum();
        let max_relative_violation = relative_violations.iter().copied().fold(0.0, f64::max);
        Ok(ObjectiveEvaluation {
            feasible: max_relative_violation <= CONSTRAINT_RELATIVE_FEASIBILITY_TOLERANCE,
            objective_value,
            total_relative_violation,
            max_relative_violation,
            relative_violations,
        })
    }

    /// Compare two already-evaluated points.  `Ordering::Less` means `lhs` is
    /// preferred.  Feasible points always beat infeasible points.  Infeasible
    /// points first minimize total normalized violation, then worst violation.
    pub fn compare_evaluations(
        &self,
        lhs: &ObjectiveEvaluation,
        rhs: &ObjectiveEvaluation,
    ) -> Ordering {
        match (lhs.feasible, rhs.feasible) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (true, true) => lhs.objective_value.total_cmp(&rhs.objective_value),
            (false, false) => lhs
                .total_relative_violation
                .total_cmp(&rhs.total_relative_violation)
                .then_with(|| {
                    lhs.max_relative_violation
                        .total_cmp(&rhs.max_relative_violation)
                })
                .then_with(|| lhs.objective_value.total_cmp(&rhs.objective_value)),
        }
    }

    pub fn compare(&self, lhs: PpaPoint, rhs: PpaPoint) -> Result<Ordering> {
        let lhs = self.evaluate(lhs)?;
        let rhs = self.evaluate(rhs)?;
        Ok(self.compare_evaluations(&lhs, &rhs))
    }

    pub fn minimum_relative_constraint_slack(&self, point: PpaPoint) -> Result<f64> {
        point.validate("constraint slack point")?;
        Ok(self
            .spec
            .constraints
            .iter()
            .map(|constraint| constraint.relative_slack(point))
            .reduce(f64::min)
            .unwrap_or(0.0))
    }

    pub fn relative_constraint_slacks(&self, point: PpaPoint) -> Result<Vec<f64>> {
        point.validate("constraint slack vector point")?;
        Ok(self
            .spec
            .constraints
            .iter()
            .map(|constraint| constraint.relative_slack(point))
            .collect())
    }

    pub fn strictly_better(&self, candidate: PpaPoint, incumbent: PpaPoint) -> Result<bool> {
        let candidate = self.evaluate(candidate)?;
        let incumbent = self.evaluate(incumbent)?;
        let materially_less = |lhs: f64, rhs: f64| {
            let tolerance = 1e-12 * rhs.abs().max(1.0);
            lhs < rhs - tolerance
        };
        Ok(match (candidate.feasible, incumbent.feasible) {
            (true, false) => true,
            (false, true) => false,
            (true, true) => materially_less(candidate.objective_value, incumbent.objective_value),
            (false, false) => {
                materially_less(
                    candidate.total_relative_violation,
                    incumbent.total_relative_violation,
                ) || (!materially_less(
                    incumbent.total_relative_violation,
                    candidate.total_relative_violation,
                ) && (materially_less(
                    candidate.max_relative_violation,
                    incumbent.max_relative_violation,
                ) || (!materially_less(
                    incumbent.max_relative_violation,
                    candidate.max_relative_violation,
                ) && materially_less(
                    candidate.objective_value,
                    incumbent.objective_value,
                ))))
            }
        })
    }

    /// Relative first-order weights for downstream adjoint/search pricing.
    /// These are derivatives with respect to fractional D/A/P perturbations.
    /// Constraint multipliers are supplied by the later primal-dual search
    /// integration; this interface does not prescribe their update schedule.
    pub fn relative_gradient(&self, point: PpaPoint) -> Result<MetricWeights> {
        self.relative_gradient_with_active_slack(point, 0.0)
    }

    /// Relative first-order weights with a fixed active-set trust band.
    /// Constraints close to their boundary contribute directional pressure
    /// even while feasible, but the scalar ranking and Exact acceptance retain
    /// the original hard bound and requested objective.
    pub fn relative_gradient_with_active_slack(
        &self,
        point: PpaPoint,
        active_slack_ratio: f64,
    ) -> Result<MetricWeights> {
        point.validate("objective gradient point")?;
        ensure!(
            active_slack_ratio.is_finite() && active_slack_ratio >= 0.0 && active_slack_ratio < 1.0,
            "active constraint slack ratio must be in [0, 1)"
        );
        let mut gradient = match &self.spec.objective {
            ObjectiveFunction::FrozenD2ap => MetricWeights::d2ap(),
            ObjectiveFunction::MinimizeMetric { metric } => match metric {
                Metric::Delay => MetricWeights {
                    delay: 1.0,
                    ..MetricWeights::default()
                },
                Metric::Area => MetricWeights {
                    area: 1.0,
                    ..MetricWeights::default()
                },
                Metric::Power => MetricWeights {
                    power: 1.0,
                    ..MetricWeights::default()
                },
            },
            ObjectiveFunction::Product { exponents } => *exponents,
            ObjectiveFunction::WeightedNormalizedSum { weights } => {
                let terms = MetricWeights {
                    delay: weights.delay * point.delay / self.reference.delay,
                    area: weights.area * point.area / self.reference.area,
                    power: weights.power * point.power / self.reference.power,
                };
                terms.scale(1.0 / self.objective_value(point))
            }
        };
        for (constraint, multiplier) in self
            .spec
            .constraints
            .iter()
            .zip(&self.constraint_multipliers)
        {
            if constraint.relative_slack(point) <= active_slack_ratio {
                gradient = gradient.add(constraint.relative_gradient(point).scale(*multiplier));
            }
        }
        Ok(gradient)
    }

    /// One-sided scalar used inside planner/A2 search.  Exact acceptance does
    /// not consume this scalar: it uses `compare` and therefore remains
    /// feasible-first.  Feasible points are ranked solely by the requested
    /// objective; constraint pressure is paid only for actual violation.
    pub fn search_score(&self, point: PpaPoint) -> Result<f64> {
        point.validate("objective search point")?;
        let objective = self.objective_value(point);
        ensure!(objective.is_finite() && objective > 0.0);
        let constraint_term: f64 = self
            .spec
            .constraints
            .iter()
            .zip(&self.constraint_multipliers)
            .map(|(constraint, multiplier)| multiplier * constraint.relative_violation(point))
            .sum();
        Ok(objective.ln() + constraint_term)
    }

    fn objective_value(&self, point: PpaPoint) -> f64 {
        match &self.spec.objective {
            // Preserve the frozen expression and operation order.
            ObjectiveFunction::FrozenD2ap => point.delay * point.delay * point.area * point.power,
            ObjectiveFunction::MinimizeMetric { metric } => point.get(*metric),
            ObjectiveFunction::Product { exponents } => {
                (point.delay / self.reference.delay).powf(exponents.delay)
                    * (point.area / self.reference.area).powf(exponents.area)
                    * (point.power / self.reference.power).powf(exponents.power)
            }
            ObjectiveFunction::WeightedNormalizedSum { weights } => {
                weights.delay * point.delay / self.reference.delay
                    + weights.area * point.area / self.reference.area
                    + weights.power * point.power / self.reference.power
            }
        }
    }
}

/// Compatibility shim for the earlier constrained-area experiment.  Existing
/// binaries keep their old opt-in environment behavior until the new objective
/// context is explicitly wired through the full Union pipeline.
#[derive(Clone, Debug, PartialEq)]
pub enum SearchObjective {
    D2ap,
    AreaUnderDelay {
        delay_cap_ps: f64,
        guard_ratio: f64,
        timing_weight: f64,
    },
    Programmable {
        context: Arc<ObjectiveContext>,
        primal_dual_policy: PrimalDualPolicy,
    },
}

impl SearchObjective {
    pub fn from_env() -> Result<Self> {
        match std::env::var("EGG_EXPERIMENTAL_OBJECTIVE") {
            Err(std::env::VarError::NotPresent) => Ok(Self::D2ap),
            Ok(value) if value.is_empty() || value == "d2ap" => Ok(Self::D2ap),
            Ok(value) if value == "area_under_delay" => {
                let delay_cap_ps: f64 = std::env::var("EGG_DELAY_CAP_PS")
                    .context("area_under_delay requires EGG_DELAY_CAP_PS")?
                    .parse()
                    .context("invalid EGG_DELAY_CAP_PS")?;
                let guard_ratio: f64 = std::env::var("EGG_DELAY_GUARD_RATIO")
                    .unwrap_or_else(|_| "0.98".to_owned())
                    .parse()
                    .context("invalid EGG_DELAY_GUARD_RATIO")?;
                let timing_weight: f64 = std::env::var("EGG_DELAY_TIMING_WEIGHT")
                    .unwrap_or_else(|_| "8.0".to_owned())
                    .parse()
                    .context("invalid EGG_DELAY_TIMING_WEIGHT")?;
                ensure!(delay_cap_ps.is_finite() && delay_cap_ps > 0.0);
                ensure!(guard_ratio.is_finite() && guard_ratio > 0.0 && guard_ratio <= 1.0);
                ensure!(timing_weight.is_finite() && timing_weight > 0.0);
                Ok(Self::AreaUnderDelay {
                    delay_cap_ps,
                    guard_ratio,
                    timing_weight,
                })
            }
            Ok(value) => bail!("unknown EGG_EXPERIMENTAL_OBJECTIVE={value:?}"),
            Err(error) => Err(error.into()),
        }
    }

    pub fn programmable(context: ObjectiveContext, policy: PrimalDualPolicy) -> Result<Self> {
        policy.validate()?;
        Ok(Self::Programmable {
            context: Arc::new(context),
            primal_dual_policy: policy,
        })
    }

    pub fn name(&self) -> &str {
        match self {
            Self::D2ap => "d2ap",
            Self::AreaUnderDelay { .. } => "area_under_delay",
            Self::Programmable { context, .. } => &context.spec().name,
        }
    }

    pub fn delay_cap_ps(&self) -> Option<f64> {
        match self {
            Self::D2ap => None,
            Self::AreaUnderDelay { delay_cap_ps, .. } => Some(*delay_cap_ps),
            Self::Programmable { context, .. } => context
                .spec()
                .constraints
                .iter()
                .find(|constraint| {
                    constraint.metric == Metric::Delay
                        && constraint.relation == ConstraintRelation::AtMost
                })
                .map(|constraint| constraint.bound),
        }
    }

    pub fn score(&self, delay: f64, area: f64, power: f64) -> f64 {
        const LEGACY_INFEASIBLE_BASE: f64 = 1.0e12;
        match self {
            Self::D2ap => delay * delay * area * power,
            Self::AreaUnderDelay { delay_cap_ps, .. } => {
                if delay <= *delay_cap_ps {
                    area
                } else {
                    LEGACY_INFEASIBLE_BASE
                        + LEGACY_INFEASIBLE_BASE * ((delay - delay_cap_ps) / delay_cap_ps)
                        + area
                }
            }
            Self::Programmable { context, .. } => context
                .search_score(PpaPoint { delay, area, power })
                .expect("validated programmable objective point"),
        }
    }

    pub fn feasible(&self, delay: f64) -> bool {
        self.delay_cap_ps().is_none_or(|cap| delay <= cap)
    }

    pub fn feasible_ppa(&self, delay: f64, area: f64, power: f64) -> bool {
        match self {
            Self::Programmable { context, .. } => {
                context
                    .evaluate(PpaPoint { delay, area, power })
                    .expect("validated programmable objective point")
                    .feasible
            }
            _ => self.feasible(delay),
        }
    }

    pub fn minimum_relative_constraint_slack(&self, point: PpaPoint) -> Result<f64> {
        point.validate("search objective constraint slack point")?;
        match self {
            Self::Programmable { context, .. } => context.minimum_relative_constraint_slack(point),
            Self::AreaUnderDelay { delay_cap_ps, .. } => {
                Ok((delay_cap_ps - point.delay) / delay_cap_ps)
            }
            Self::D2ap => Ok(0.0),
        }
    }

    pub fn exact_objective_equivalent(&self, lhs: PpaPoint, rhs: PpaPoint) -> Result<bool> {
        let lhs = self.exact_value(lhs)?;
        let rhs = self.exact_value(rhs)?;
        Ok((lhs - rhs).abs() <= 1e-12 * rhs.abs().max(1.0))
    }

    pub fn delay_multiplier(&self, delay: f64, area: f64, power: f64) -> f64 {
        match self {
            Self::D2ap => 2.0 / delay,
            Self::AreaUnderDelay {
                delay_cap_ps,
                guard_ratio,
                timing_weight,
            } => {
                if delay >= guard_ratio * delay_cap_ps {
                    timing_weight / delay
                } else {
                    0.0
                }
            }
            Self::Programmable { context, .. } => {
                let active_slack_ratio = match self {
                    Self::Programmable {
                        primal_dual_policy, ..
                    } => primal_dual_policy.active_slack_ratio,
                    _ => unreachable!(),
                };
                context
                    .relative_gradient_with_active_slack(
                        PpaPoint { delay, area, power },
                        active_slack_ratio,
                    )
                    .expect("validated programmable objective point")
                    .delay
                    / delay
            }
        }
    }

    pub fn relative_delta(
        &self,
        delay: f64,
        area: f64,
        power: f64,
        delay_delta: f64,
        area_delta: f64,
        power_delta: f64,
    ) -> f64 {
        match self {
            Self::D2ap => area_delta / area + power_delta / power + (2.0 / delay) * delay_delta,
            Self::AreaUnderDelay { .. } => {
                area_delta / area + self.delay_multiplier(delay, area, power) * delay_delta
            }
            Self::Programmable {
                context,
                primal_dual_policy,
            } => {
                let gradient = context
                    .relative_gradient_with_active_slack(
                        PpaPoint { delay, area, power },
                        primal_dual_policy.active_slack_ratio,
                    )
                    .expect("validated programmable objective point");
                gradient.delay * delay_delta / delay
                    + gradient.area * area_delta / area
                    + gradient.power * power_delta / power
            }
        }
    }

    pub fn relative_weights(&self, point: PpaPoint) -> Result<MetricWeights> {
        point.validate("objective weight point")?;
        Ok(match self {
            Self::D2ap => MetricWeights::d2ap(),
            Self::AreaUnderDelay { .. } => MetricWeights {
                delay: self.delay_multiplier(point.delay, point.area, point.power) * point.delay,
                area: 1.0,
                power: 0.0,
            },
            Self::Programmable {
                context,
                primal_dual_policy,
            } => context.relative_gradient_with_active_slack(
                point,
                primal_dual_policy.active_slack_ratio,
            )?,
        })
    }

    pub fn relative_constraint_slacks(&self, point: PpaPoint) -> Result<Vec<f64>> {
        point.validate("search objective constraint slack vector point")?;
        match self {
            Self::Programmable { context, .. } => context.relative_constraint_slacks(point),
            Self::AreaUnderDelay { delay_cap_ps, .. } => {
                Ok(vec![(delay_cap_ps - point.delay) / delay_cap_ps])
            }
            Self::D2ap => Ok(Vec::new()),
        }
    }

    pub fn active_constraint_slack_ratio(&self) -> Option<f64> {
        match self {
            Self::Programmable {
                primal_dual_policy, ..
            } => Some(primal_dual_policy.active_slack_ratio),
            _ => None,
        }
    }

    pub fn strictly_better(&self, candidate: PpaPoint, incumbent: PpaPoint) -> Result<bool> {
        match self {
            // Preserve the frozen V7 strict guard and epsilon exactly.
            Self::D2ap => Ok(self.score(candidate.delay, candidate.area, candidate.power)
                < self.score(incumbent.delay, incumbent.area, incumbent.power) - 1e-9),
            Self::AreaUnderDelay { .. } => {
                Ok(self.score(candidate.delay, candidate.area, candidate.power)
                    < self.score(incumbent.delay, incumbent.area, incumbent.power) - 1e-9)
            }
            Self::Programmable { context, .. } => context.strictly_better(candidate, incumbent),
        }
    }

    pub fn exact_value(&self, point: PpaPoint) -> Result<f64> {
        point.validate("exact objective point")?;
        Ok(match self {
            Self::Programmable { context, .. } => context.evaluate(point)?.objective_value,
            _ => self.score(point.delay, point.area, point.power),
        })
    }

    pub fn with_primal_dual_update(
        &self,
        point: PpaPoint,
    ) -> Result<(Self, Option<PrimalDualUpdate>)> {
        match self {
            Self::Programmable {
                context,
                primal_dual_policy,
            } => {
                let mut next = context.as_ref().clone();
                let update = next.update_primal_dual(point, *primal_dual_policy)?;
                Ok((
                    Self::Programmable {
                        context: Arc::new(next),
                        primal_dual_policy: *primal_dual_policy,
                    },
                    Some(update),
                ))
            }
            _ => Ok((self.clone(), None)),
        }
    }

    pub fn programmable_context(&self) -> Option<&ObjectiveContext> {
        match self {
            Self::Programmable { context, .. } => Some(context),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::{
        ConstraintRelation, Metric, MetricConstraint, MetricWeights, ObjectiveFunction,
        ObjectiveSpec, PpaPoint, SearchObjective,
    };

    fn reference() -> PpaPoint {
        PpaPoint {
            delay: 100.0,
            area: 20.0,
            power: 4.0,
        }
    }

    #[test]
    fn frozen_d2ap_score_and_gradient_are_exact() {
        let point = PpaPoint {
            delay: 3.0,
            area: 5.0,
            power: 7.0,
        };
        let context = ObjectiveSpec::frozen_d2ap().bind(reference()).unwrap();
        assert_eq!(
            context.evaluate(point).unwrap().objective_value,
            3.0 * 3.0 * 5.0 * 7.0
        );
        assert_eq!(
            context.relative_gradient(point).unwrap(),
            MetricWeights::d2ap()
        );

        let legacy = SearchObjective::D2ap;
        assert_eq!(legacy.score(3.0, 5.0, 7.0), 3.0 * 3.0 * 5.0 * 7.0);
        assert_eq!(
            legacy.relative_delta(3.0, 5.0, 7.0, 0.2, 0.3, 0.4),
            0.3 / 5.0 + 0.4 / 7.0 + (2.0 / 3.0) * 0.2
        );
    }

    #[test]
    fn json_area_under_delay_is_feasible_first() {
        let spec = ObjectiveSpec::from_json_str(
            r#"{
                "schema_version": 1,
                "name": "area_under_delay",
                "objective": {"kind": "minimize_metric", "metric": "area"},
                "constraints": [
                    {"metric": "delay", "relation": "at_most", "bound": 100.0}
                ]
            }"#,
        )
        .unwrap();
        let context = spec.bind(reference()).unwrap();
        let feasible = PpaPoint {
            delay: 100.0,
            area: 20.0,
            power: 100.0,
        };
        let infeasible_but_tiny = PpaPoint {
            delay: 100.1,
            area: 1.0,
            power: 1.0,
        };
        assert_eq!(
            context.compare(feasible, infeasible_but_tiny).unwrap(),
            Ordering::Less
        );
    }

    #[test]
    fn infeasible_points_minimize_normalized_violation_before_objective() {
        let spec = ObjectiveSpec {
            schema_version: 1,
            name: "area_under_delay".to_owned(),
            objective: ObjectiveFunction::MinimizeMetric {
                metric: Metric::Area,
            },
            constraints: vec![MetricConstraint {
                metric: Metric::Delay,
                relation: ConstraintRelation::AtMost,
                bound: 100.0,
            }],
        };
        let context = spec.bind(reference()).unwrap();
        let closer = PpaPoint {
            delay: 101.0,
            area: 100.0,
            power: 1.0,
        };
        let farther = PpaPoint {
            delay: 102.0,
            area: 1.0,
            power: 1.0,
        };
        assert!(context.strictly_better(closer, farther).unwrap());
    }

    #[test]
    fn product_and_weighted_sum_have_expected_relative_gradients() {
        let product = ObjectiveSpec {
            schema_version: 1,
            name: "product".to_owned(),
            objective: ObjectiveFunction::Product {
                exponents: MetricWeights {
                    delay: 1.5,
                    area: 2.0,
                    power: 0.5,
                },
            },
            constraints: vec![],
        }
        .bind(reference())
        .unwrap();
        assert_eq!(
            product.relative_gradient(reference()).unwrap(),
            MetricWeights {
                delay: 1.5,
                area: 2.0,
                power: 0.5
            }
        );

        let weighted = ObjectiveSpec {
            schema_version: 1,
            name: "weighted".to_owned(),
            objective: ObjectiveFunction::WeightedNormalizedSum {
                weights: MetricWeights {
                    delay: 2.0,
                    area: 1.0,
                    power: 1.0,
                },
            },
            constraints: vec![],
        }
        .bind(reference())
        .unwrap();
        assert_eq!(
            weighted.relative_gradient(reference()).unwrap(),
            MetricWeights {
                delay: 0.5,
                area: 0.25,
                power: 0.25
            }
        );
    }

    #[test]
    fn constraint_multipliers_add_directional_guidance() {
        let spec = ObjectiveSpec {
            schema_version: 1,
            name: "area_under_delay".to_owned(),
            objective: ObjectiveFunction::MinimizeMetric {
                metric: Metric::Area,
            },
            constraints: vec![MetricConstraint {
                metric: Metric::Delay,
                relation: ConstraintRelation::AtMost,
                bound: 100.0,
            }],
        };
        let mut context = spec.bind(reference()).unwrap();
        context.set_constraint_multipliers(vec![3.0]).unwrap();
        assert_eq!(
            context.relative_gradient(reference()).unwrap(),
            MetricWeights {
                delay: 3.0,
                area: 1.0,
                power: 0.0
            }
        );
        let infeasible_gradient = context
            .relative_gradient(PpaPoint {
                delay: 110.0,
                ..reference()
            })
            .unwrap();
        assert!((infeasible_gradient.delay - 3.3).abs() < 1e-12);
        assert_eq!(infeasible_gradient.area, 1.0);
        assert_eq!(infeasible_gradient.power, 0.0);
    }

    #[test]
    fn active_set_band_guides_boundary_without_changing_feasibility() {
        let spec = ObjectiveSpec {
            schema_version: 1,
            name: "area_under_delay".to_owned(),
            objective: ObjectiveFunction::MinimizeMetric {
                metric: Metric::Area,
            },
            constraints: vec![MetricConstraint {
                metric: Metric::Delay,
                relation: ConstraintRelation::AtMost,
                bound: 100.0,
            }],
        };
        let mut context = spec.bind(reference()).unwrap();
        context.set_constraint_multipliers(vec![2.0]).unwrap();
        let near = PpaPoint {
            delay: 99.5,
            ..reference()
        };
        let far = PpaPoint {
            delay: 98.0,
            ..reference()
        };
        assert!(context.evaluate(near).unwrap().feasible);
        assert_eq!(
            context
                .relative_gradient_with_active_slack(near, 0.01)
                .unwrap()
                .delay,
            1.99
        );
        assert_eq!(
            context
                .relative_gradient_with_active_slack(far, 0.01)
                .unwrap()
                .delay,
            0.0
        );
    }

    #[test]
    fn feasible_search_does_not_reward_unrequested_timing_slack() {
        let spec = ObjectiveSpec {
            schema_version: 1,
            name: "area_under_delay".to_owned(),
            objective: ObjectiveFunction::MinimizeMetric {
                metric: Metric::Area,
            },
            constraints: vec![MetricConstraint {
                metric: Metric::Delay,
                relation: ConstraintRelation::AtMost,
                bound: 100.0,
            }],
        };
        let mut context = spec.bind(reference()).unwrap();
        context.set_constraint_multipliers(vec![10.0]).unwrap();
        let smaller_area = PpaPoint {
            delay: 99.0,
            area: 10.0,
            power: 4.0,
        };
        let more_timing_slack = PpaPoint {
            delay: 80.0,
            area: 11.0,
            power: 4.0,
        };
        assert!(
            context.search_score(smaller_area).unwrap()
                < context.search_score(more_timing_slack).unwrap()
        );
    }

    #[test]
    fn programmable_constraint_feasibility_tolerates_only_roundoff() {
        let context = ObjectiveSpec {
            schema_version: 1,
            name: "roundoff-feasibility".to_owned(),
            objective: ObjectiveFunction::MinimizeMetric {
                metric: Metric::Area,
            },
            constraints: vec![MetricConstraint {
                metric: Metric::Delay,
                relation: ConstraintRelation::AtMost,
                bound: 100.0,
            }],
        }
        .bind(PpaPoint {
            delay: 100.0,
            area: 10.0,
            power: 1.0,
        })
        .unwrap();
        assert!(
            context
                .evaluate(PpaPoint {
                    delay: 100.0 * (1.0 + 5e-13),
                    area: 9.0,
                    power: 1.0,
                })
                .unwrap()
                .feasible
        );
        assert!(
            !context
                .evaluate(PpaPoint {
                    delay: 100.0 * (1.0 + 2e-12),
                    area: 9.0,
                    power: 1.0,
                })
                .unwrap()
                .feasible
        );
    }

    #[test]
    fn validation_rejects_zero_objective_and_frozen_constraints() {
        let zero = ObjectiveSpec {
            schema_version: 1,
            name: "zero".to_owned(),
            objective: ObjectiveFunction::Product {
                exponents: MetricWeights::default(),
            },
            constraints: vec![],
        };
        assert!(zero.validate().is_err());

        let mut frozen = ObjectiveSpec::frozen_d2ap();
        frozen.constraints.push(MetricConstraint {
            metric: Metric::Delay,
            relation: ConstraintRelation::AtMost,
            bound: 100.0,
        });
        assert!(frozen.validate().is_err());
    }

    #[test]
    fn fixed_primal_dual_update_is_projected_and_deterministic() {
        let spec = ObjectiveSpec {
            schema_version: 1,
            name: "area_under_delay".to_owned(),
            objective: ObjectiveFunction::MinimizeMetric {
                metric: Metric::Area,
            },
            constraints: vec![MetricConstraint {
                metric: Metric::Delay,
                relation: ConstraintRelation::AtMost,
                bound: 100.0,
            }],
        };
        let policy = super::PrimalDualPolicy::default();
        let mut context = spec.bind(reference()).unwrap();
        context.initialize_primal_dual(policy).unwrap();
        let update = context
            .update_primal_dual(
                PpaPoint {
                    delay: 100.0,
                    ..reference()
                },
                policy,
            )
            .unwrap();
        assert_eq!(update.before, vec![1.0]);
        assert_eq!(update.signed_pressure, vec![0.0]);
        assert_eq!(update.after, vec![1.0]);

        let update = context
            .update_primal_dual(
                PpaPoint {
                    delay: 110.0,
                    ..reference()
                },
                policy,
            )
            .unwrap();
        assert!((update.after[0] - 1.2).abs() < 1e-12);
    }

    #[test]
    fn programmable_exact_acceptance_is_feasible_first() {
        let spec = ObjectiveSpec {
            schema_version: 1,
            name: "area_under_delay".to_owned(),
            objective: ObjectiveFunction::MinimizeMetric {
                metric: Metric::Area,
            },
            constraints: vec![MetricConstraint {
                metric: Metric::Delay,
                relation: ConstraintRelation::AtMost,
                bound: 100.0,
            }],
        };
        let objective = SearchObjective::programmable(
            spec.bind(reference()).unwrap(),
            super::PrimalDualPolicy::default(),
        )
        .unwrap();
        let feasible = PpaPoint {
            delay: 100.0,
            area: 20.0,
            power: 4.0,
        };
        let infeasible_smaller = PpaPoint {
            delay: 100.1,
            area: 1.0,
            power: 4.0,
        };
        assert!(
            !objective
                .strictly_better(infeasible_smaller, feasible)
                .unwrap()
        );
        assert!(
            objective
                .strictly_better(feasible, infeasible_smaller)
                .unwrap()
        );
        assert!(
            !objective
                .strictly_better(
                    PpaPoint {
                        area: feasible.area - 1e-14,
                        ..feasible
                    },
                    feasible,
                )
                .unwrap()
        );
        assert!(
            objective
                .strictly_better(
                    PpaPoint {
                        area: feasible.area - 1e-8,
                        ..feasible
                    },
                    feasible,
                )
                .unwrap()
        );
    }

    #[test]
    fn constraint_slack_has_consistent_feasible_sign() {
        let spec = ObjectiveSpec {
            schema_version: 1,
            name: "area_under_delay".to_owned(),
            objective: ObjectiveFunction::MinimizeMetric {
                metric: Metric::Area,
            },
            constraints: vec![MetricConstraint {
                metric: Metric::Delay,
                relation: ConstraintRelation::AtMost,
                bound: 100.0,
            }],
        };
        let context = spec.bind(reference()).unwrap();
        assert!(
            (context
                .minimum_relative_constraint_slack(PpaPoint {
                    delay: 90.0,
                    ..reference()
                })
                .unwrap()
                - 0.1)
                .abs()
                < 1e-12
        );
        assert!(
            (context
                .minimum_relative_constraint_slack(PpaPoint {
                    delay: 110.0,
                    ..reference()
                })
                .unwrap()
                + 0.1)
                .abs()
                < 1e-12
        );
    }

    #[test]
    fn exact_objective_equivalence_uses_objective_not_other_metrics() {
        let spec = ObjectiveSpec {
            schema_version: 1,
            name: "area_under_delay".to_owned(),
            objective: ObjectiveFunction::MinimizeMetric {
                metric: Metric::Area,
            },
            constraints: vec![MetricConstraint {
                metric: Metric::Delay,
                relation: ConstraintRelation::AtMost,
                bound: 100.0,
            }],
        };
        let objective = SearchObjective::programmable(
            spec.bind(reference()).unwrap(),
            super::PrimalDualPolicy::default(),
        )
        .unwrap();
        let incumbent = PpaPoint {
            delay: 99.0,
            area: 10.0,
            power: 4.0,
        };
        assert!(
            objective
                .exact_objective_equivalent(
                    PpaPoint {
                        delay: 80.0,
                        power: 9.0,
                        ..incumbent
                    },
                    incumbent,
                )
                .unwrap()
        );
        assert!(
            !objective
                .exact_objective_equivalent(
                    PpaPoint {
                        area: 10.001,
                        ..incumbent
                    },
                    incumbent,
                )
                .unwrap()
        );
    }

    #[test]
    fn checked_in_examples_parse_and_validate() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("config/objective_programmable_v1");
        for name in [
            "frozen_d2ap.json",
            "area_under_delay.example.json",
            "weighted_ppa.example.json",
        ] {
            ObjectiveSpec::from_json_path(root.join(name)).unwrap();
        }
    }
}
