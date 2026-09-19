//! Data-driven policy primitives for the isolated V8-Pro experiment.
//!
//! This module deliberately has no benchmark names, filesystem paths, or
//! evaluator calls.  The V8-Pro controller supplies observations from one
//! cold-parent attempt and consumes the resulting rule-tier and progressive
//! budget decisions.  Frozen V8 does not call this module.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleTier {
    SmallInvNoXor23,
    DemorganCore9,
    FrozenFull181,
}

impl RuleTier {
    pub fn next_fallback(self) -> Option<Self> {
        match self {
            Self::SmallInvNoXor23 => Some(Self::DemorganCore9),
            Self::DemorganCore9 => Some(Self::FrozenFull181),
            Self::FrozenFull181 => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct SafetyThresholds {
    pub min_legal_candidates: usize,
    pub min_unique_topologies: usize,
    pub min_unique_regions: usize,
    pub min_unique_provenance_classes: usize,
    pub require_d1_when_available: bool,
    pub require_generator_when_available: bool,
}

impl Default for SafetyThresholds {
    fn default() -> Self {
        Self {
            min_legal_candidates: 32,
            min_unique_topologies: 8,
            min_unique_regions: 2,
            min_unique_provenance_classes: 2,
            require_d1_when_available: true,
            require_generator_when_available: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ProgressivePolicy {
    pub frozen_portfolio_limit: usize,
    pub fast_portfolio_limit: usize,
    pub fast_min_legal_candidates: usize,
    pub fast_min_unique_topologies: usize,
    pub fast_min_unique_regions: usize,
    pub generator_region_representatives: usize,
    pub stage25_budget: usize,
    pub stage50_budget: usize,
    pub stage500_budget: usize,
    pub min_stage50_survivors: usize,
    pub max_stage50_survivors: usize,
    pub min_stage50_unique_topologies: usize,
    pub weak_leader_relative_margin: f64,
}

impl Default for ProgressivePolicy {
    fn default() -> Self {
        Self {
            frozen_portfolio_limit: 32,
            fast_portfolio_limit: 24,
            fast_min_legal_candidates: 48,
            fast_min_unique_topologies: 16,
            fast_min_unique_regions: 4,
            generator_region_representatives: 8,
            stage25_budget: 25,
            stage50_budget: 50,
            stage500_budget: 500,
            min_stage50_survivors: 4,
            max_stage50_survivors: 8,
            min_stage50_unique_topologies: 4,
            weak_leader_relative_margin: 0.005,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CandidateObservation<'a> {
    pub candidate_id: &'a str,
    pub topology_signature: &'a str,
    pub region: &'a str,
    pub provenance_class: &'a str,
    pub source_class: &'a str,
    pub p1_score: f64,
    pub legal: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct V8ProPolicyConfig {
    pub safety: SafetyThresholds,
    pub progressive: ProgressivePolicy,
}

impl CandidateObservation<'_> {
    fn is_d1(&self) -> bool {
        self.source_class == "d1-rewrite"
    }

    fn is_generator(&self) -> bool {
        self.source_class.starts_with("generator-v2")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PortfolioStats {
    pub legal_candidates: usize,
    pub unique_topologies: usize,
    pub unique_regions: usize,
    pub unique_provenance_classes: usize,
    pub d1_candidates: usize,
    pub generator_candidates: usize,
    pub best_p1_score: Option<f64>,
    pub topk_relative_score_spread: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SafetyDecision {
    pub safe: bool,
    pub reasons: Vec<String>,
    pub pool: PortfolioStats,
    pub selected: PortfolioStats,
    pub incumbent_fallback_available: bool,
}

pub fn summarize_candidates(candidates: &[CandidateObservation<'_>]) -> PortfolioStats {
    let legal: Vec<_> = candidates.iter().filter(|row| row.legal).collect();
    let unique_topologies = legal
        .iter()
        .filter(|row| !row.topology_signature.is_empty())
        .map(|row| row.topology_signature)
        .collect::<BTreeSet<_>>()
        .len();
    let unique_regions = legal
        .iter()
        .filter(|row| !row.region.is_empty())
        .map(|row| row.region)
        .collect::<BTreeSet<_>>()
        .len();
    let unique_provenance_classes = legal
        .iter()
        .filter(|row| !row.provenance_class.is_empty())
        .map(|row| row.provenance_class)
        .collect::<BTreeSet<_>>()
        .len();
    let mut scores: Vec<_> = legal
        .iter()
        .filter_map(|row| row.p1_score.is_finite().then_some(row.p1_score))
        .collect();
    scores.sort_by(f64::total_cmp);
    let best_p1_score = scores.first().copied();
    let topk_relative_score_spread = scores
        .first()
        .zip(scores.last())
        .map(|(best, worst)| (worst - best) / best.abs().max(1.0));
    PortfolioStats {
        legal_candidates: legal.len(),
        unique_topologies,
        unique_regions,
        unique_provenance_classes,
        d1_candidates: legal.iter().filter(|row| row.is_d1()).count(),
        generator_candidates: legal.iter().filter(|row| row.is_generator()).count(),
        best_p1_score,
        topk_relative_score_spread,
    }
}

pub fn evaluate_portfolio_safety(
    pool: &[CandidateObservation<'_>],
    selected: &[CandidateObservation<'_>],
    incumbent_fallback_available: bool,
    thresholds: &SafetyThresholds,
) -> SafetyDecision {
    let pool_stats = summarize_candidates(pool);
    let selected_stats = summarize_candidates(selected);
    let mut reasons = Vec::new();
    if pool_stats.legal_candidates < thresholds.min_legal_candidates {
        reasons.push(format!(
            "legal_candidates:{}<{}",
            pool_stats.legal_candidates, thresholds.min_legal_candidates
        ));
    }
    if selected_stats.unique_topologies < thresholds.min_unique_topologies {
        reasons.push(format!(
            "unique_topologies:{}<{}",
            selected_stats.unique_topologies, thresholds.min_unique_topologies
        ));
    }
    let required_regions = thresholds.min_unique_regions.min(pool_stats.unique_regions);
    if selected_stats.unique_regions < required_regions {
        reasons.push(format!(
            "unique_regions:{}<{}",
            selected_stats.unique_regions, required_regions
        ));
    }
    let required_provenance = thresholds
        .min_unique_provenance_classes
        .min(pool_stats.unique_provenance_classes);
    if selected_stats.unique_provenance_classes < required_provenance {
        reasons.push(format!(
            "unique_provenance_classes:{}<{}",
            selected_stats.unique_provenance_classes, required_provenance
        ));
    }
    if thresholds.require_d1_when_available
        && pool_stats.d1_candidates > 0
        && selected_stats.d1_candidates == 0
    {
        reasons.push("missing_d1_anchor".to_owned());
    }
    if thresholds.require_generator_when_available
        && pool_stats.generator_candidates > 0
        && selected_stats.generator_candidates == 0
    {
        reasons.push("missing_generator_anchor".to_owned());
    }
    if !incumbent_fallback_available {
        reasons.push("missing_incumbent_fallback".to_owned());
    }
    SafetyDecision {
        safe: reasons.is_empty(),
        reasons,
        pool: pool_stats,
        selected: selected_stats,
        incumbent_fallback_available,
    }
}

pub fn choose_portfolio_limit(stats: &PortfolioStats, policy: &ProgressivePolicy) -> usize {
    if stats.legal_candidates >= policy.fast_min_legal_candidates
        && stats.unique_topologies >= policy.fast_min_unique_topologies
        && stats.unique_regions >= policy.fast_min_unique_regions
        && stats.d1_candidates > 0
        && stats.generator_candidates > 0
    {
        policy.fast_portfolio_limit
    } else {
        policy.frozen_portfolio_limit
    }
}

/// Reserve the best P1 candidate from each Generator region before the final
/// portfolio truncation. This protects a window whose best cheap proxy ranks
/// behind many mutually redundant D1 candidates, without naming a benchmark
/// or increasing the portfolio limit.
pub fn choose_generator_region_representatives(
    candidates: &[CandidateObservation<'_>],
    limit: usize,
) -> Vec<String> {
    let mut best_by_region: BTreeMap<&str, &CandidateObservation<'_>> = BTreeMap::new();
    for row in candidates.iter().filter(|row| {
        row.legal && row.is_generator() && !row.region.is_empty() && row.p1_score.is_finite()
    }) {
        let replace = best_by_region.get(row.region).is_none_or(|current| {
            row.p1_score
                .total_cmp(&current.p1_score)
                .then_with(|| row.candidate_id.cmp(current.candidate_id))
                .is_lt()
        });
        if replace {
            best_by_region.insert(row.region, row);
        }
    }
    let mut representatives: Vec<_> = best_by_region.into_values().collect();
    representatives.sort_by(|left, right| {
        left.p1_score
            .total_cmp(&right.p1_score)
            .then_with(|| left.candidate_id.cmp(right.candidate_id))
    });
    representatives
        .into_iter()
        .take(limit)
        .map(|row| row.candidate_id.to_owned())
        .collect()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PromotionDecision {
    pub survivors: Vec<String>,
    pub weak_leader_margin: bool,
    pub relative_margin_at_min_survivors: Option<f64>,
}

/// Select the Exact-50 survivors after Exact-25.
///
/// A weak leader keeps the frozen eight survivors.  With a clearly separated
/// leader, the policy starts from the best four and adds representatives until
/// every topology/provenance class already present in the frozen Top-8 remains
/// represented, capped at eight.  Thus pruning cannot silently collapse the
/// safety portfolio to one topology or provenance class.
pub fn choose_stage50_survivors(
    ranked: &[CandidateObservation<'_>],
    policy: &ProgressivePolicy,
) -> PromotionDecision {
    let max_survivors = policy.max_stage50_survivors.min(ranked.len());
    let min_survivors = policy.min_stage50_survivors.min(max_survivors);
    if max_survivors == 0 {
        return PromotionDecision {
            survivors: Vec::new(),
            weak_leader_margin: true,
            relative_margin_at_min_survivors: None,
        };
    }
    let leader_score = ranked[0].p1_score;
    let margin = (min_survivors > 1)
        .then(|| (ranked[min_survivors - 1].p1_score - leader_score) / leader_score.abs().max(1.0));
    let weak_leader_margin = margin.is_none_or(|value| value <= policy.weak_leader_relative_margin);
    if weak_leader_margin {
        return PromotionDecision {
            survivors: ranked[..max_survivors]
                .iter()
                .map(|row| row.candidate_id.to_owned())
                .collect(),
            weak_leader_margin,
            relative_margin_at_min_survivors: margin,
        };
    }

    let reference = &ranked[..max_survivors];
    let required_provenance: BTreeSet<_> = reference
        .iter()
        .filter(|row| !row.provenance_class.is_empty())
        .map(|row| row.provenance_class)
        .collect();
    let mut chosen = BTreeMap::new();
    for (rank, row) in reference.iter().take(min_survivors).enumerate() {
        chosen.insert(row.candidate_id, rank);
    }
    let required_topology_count = policy.min_stage50_unique_topologies.min(
        reference
            .iter()
            .map(|row| row.topology_signature)
            .filter(|signature| !signature.is_empty())
            .collect::<BTreeSet<_>>()
            .len(),
    );
    let mut represented_topologies: BTreeSet<_> = chosen
        .keys()
        .filter_map(|id| {
            reference
                .iter()
                .find(|row| row.candidate_id == *id)
                .map(|row| row.topology_signature)
        })
        .filter(|signature| !signature.is_empty())
        .collect();
    for (rank, row) in reference.iter().enumerate() {
        if represented_topologies.len() >= required_topology_count {
            break;
        }
        if represented_topologies.insert(row.topology_signature) {
            chosen.insert(row.candidate_id, rank);
        }
    }
    for wanted in required_provenance {
        if chosen.keys().any(|id| {
            reference
                .iter()
                .find(|row| row.candidate_id == *id)
                .is_some_and(|row| row.provenance_class == wanted)
        }) {
            continue;
        }
        if let Some((rank, row)) = reference
            .iter()
            .enumerate()
            .find(|(_, row)| row.provenance_class == wanted)
        {
            chosen.insert(row.candidate_id, rank);
        }
    }
    let mut chosen: Vec<_> = chosen.into_iter().collect();
    chosen.sort_by_key(|(_, rank)| *rank);
    chosen.truncate(max_survivors);
    PromotionDecision {
        survivors: chosen.into_iter().map(|(id, _)| id.to_owned()).collect(),
        weak_leader_margin,
        relative_margin_at_min_survivors: margin,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(index: usize, source: &'static str, score: f64) -> CandidateObservation<'static> {
        CandidateObservation {
            candidate_id: Box::leak(format!("c{index}").into_boxed_str()),
            topology_signature: Box::leak(format!("t{}", index % 32).into_boxed_str()),
            region: Box::leak(format!("r{}", index % 4).into_boxed_str()),
            provenance_class: if source == "d1-rewrite" {
                "d1-rewrite"
            } else if index % 2 == 0 {
                "multi-output-shared-dag"
            } else {
                "reconvergent-shared-host"
            },
            source_class: source,
            p1_score: score,
            legal: true,
        }
    }

    #[test]
    fn rule_tiers_fallback_in_the_required_order() {
        assert_eq!(
            RuleTier::SmallInvNoXor23.next_fallback(),
            Some(RuleTier::DemorganCore9)
        );
        assert_eq!(
            RuleTier::DemorganCore9.next_fallback(),
            Some(RuleTier::FrozenFull181)
        );
        assert_eq!(RuleTier::FrozenFull181.next_fallback(), None);
    }

    #[test]
    fn safety_rejects_small_or_collapsed_portfolios() {
        let pool: Vec<_> = (0..40)
            .map(|index| {
                candidate(
                    index,
                    if index < 4 {
                        "d1-rewrite"
                    } else {
                        "generator-v2"
                    },
                    index as f64 + 1.0,
                )
            })
            .collect();
        let mut selected: Vec<_> = (0..8)
            .map(|index| candidate(index, "generator-v2", index as f64 + 1.0))
            .collect();
        for row in &mut selected {
            row.topology_signature = "one-topology";
            row.region = "one-region";
        }
        let decision =
            evaluate_portfolio_safety(&pool, &selected, true, &SafetyThresholds::default());
        assert!(!decision.safe);
        assert!(
            decision
                .reasons
                .iter()
                .any(|reason| reason.starts_with("unique_topologies"))
        );
        assert!(decision.reasons.contains(&"missing_d1_anchor".to_owned()));
    }

    #[test]
    fn safety_requires_an_incumbent_fallback() {
        let pool: Vec<_> = (0..40)
            .map(|index| {
                candidate(
                    index,
                    if index < 4 {
                        "d1-rewrite"
                    } else {
                        "generator-v2"
                    },
                    index as f64 + 1.0,
                )
            })
            .collect();
        let decision =
            evaluate_portfolio_safety(&pool, &pool[..32], false, &SafetyThresholds::default());
        assert!(!decision.safe);
        assert!(
            decision
                .reasons
                .contains(&"missing_incumbent_fallback".to_owned())
        );
    }

    #[test]
    fn rich_pool_uses_the_fast_portfolio_limit() {
        let pool: Vec<_> = (0..64)
            .map(|index| {
                candidate(
                    index,
                    if index < 4 {
                        "d1-rewrite"
                    } else {
                        "generator-v2"
                    },
                    index as f64 + 1.0,
                )
            })
            .collect();
        assert_eq!(
            choose_portfolio_limit(&summarize_candidates(&pool), &ProgressivePolicy::default()),
            24
        );
    }

    #[test]
    fn weak_leader_keeps_all_eight_survivors() {
        let ranked: Vec<_> = (0..8)
            .map(|index| {
                candidate(
                    index,
                    if index == 0 {
                        "d1-rewrite"
                    } else {
                        "generator-v2"
                    },
                    100.0 + index as f64 * 0.1,
                )
            })
            .collect();
        let decision = choose_stage50_survivors(&ranked, &ProgressivePolicy::default());
        assert!(decision.weak_leader_margin);
        assert_eq!(decision.survivors.len(), 8);
    }

    #[test]
    fn separated_leader_prunes_but_preserves_diversity() {
        let mut ranked: Vec<_> = (0..8)
            .map(|index| {
                candidate(
                    index,
                    if index < 2 {
                        "d1-rewrite"
                    } else {
                        "generator-v2"
                    },
                    100.0 + index as f64 * 10.0,
                )
            })
            .collect();
        ranked[7].provenance_class = "late-required-class";
        let decision = choose_stage50_survivors(&ranked, &ProgressivePolicy::default());
        assert!(!decision.weak_leader_margin);
        assert!(decision.survivors.len() >= 4);
        assert!(decision.survivors.len() <= 8);
        assert!(decision.survivors.contains(&"c7".to_owned()));
    }

    #[test]
    fn generator_region_reservation_keeps_late_global_candidate() {
        let mut candidates = vec![
            candidate(0, "d1-rewrite", 1.0),
            candidate(1, "generator-v2", 20.0),
            candidate(2, "generator-v2", 10.0),
            candidate(3, "generator-v2", 30.0),
        ];
        candidates[1].region = "window-a";
        candidates[2].region = "window-a";
        candidates[3].region = "window-b";
        assert_eq!(
            choose_generator_region_representatives(&candidates, 8),
            vec!["c2".to_owned(), "c3".to_owned()]
        );
    }
}
