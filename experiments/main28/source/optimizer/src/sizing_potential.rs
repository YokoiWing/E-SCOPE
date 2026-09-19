//! Low-cost sizing-potential preview for topology promotion.
//!
//! Iterative normally chooses its Top-K topology portfolio from pre-A2 P1
//! values.  V8-potential is opt-in: it gives a bounded superset a small Exact
//! sizing preview, then admits a capped number of candidates that were outside
//! the original portfolio.  The full Top-K size and downstream 25/50/500
//! schedule stay unchanged.

use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct V8PotentialConfig {
    pub preview_pool_limit: usize,
    pub preview_exact_budget: usize,
    pub rescued_candidate_cap: usize,
}

impl V8PotentialConfig {
    pub fn from_env() -> Result<Option<Self>, String> {
        let enabled = std::env::var("EGG_V8_POTENTIAL")
            .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
            .unwrap_or(false);
        if !enabled {
            return Ok(None);
        }
        let parse = |name: &str, default: usize| -> Result<usize, String> {
            std::env::var(name)
                .map(|value| {
                    value
                        .parse::<usize>()
                        .map_err(|error| format!("invalid {name}={value}: {error}"))
                })
                .unwrap_or(Ok(default))
        };
        let config = Self {
            preview_pool_limit: parse("EGG_V8_POTENTIAL_PREVIEW_POOL", 64)?,
            preview_exact_budget: parse("EGG_V8_POTENTIAL_PREVIEW_EXACT", 1)?,
            rescued_candidate_cap: parse("EGG_V8_POTENTIAL_RESCUE_CAP", 8)?,
        };
        if config.preview_pool_limit == 0
            || config.preview_exact_budget == 0
            || config.rescued_candidate_cap == 0
        {
            return Err("V8-potential limits must be positive".to_owned());
        }
        Ok(Some(config))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PotentialPromotionDecision {
    pub selected: Vec<String>,
    pub retained_baseline: Vec<String>,
    pub rescued: Vec<String>,
    pub displaced_baseline: Vec<String>,
}

/// Take the best post-preview candidates while limiting how many candidates
/// may displace members of the original P1-selected portfolio.  If no outside
/// candidate reaches the preview Top-K, the original set is retained.
pub fn choose_potential_portfolio(
    baseline: &[String],
    preview_ranking: &[String],
    rescued_candidate_cap: usize,
) -> Result<PotentialPromotionDecision, String> {
    if baseline.is_empty() {
        return Err("V8-potential baseline portfolio is empty".to_owned());
    }
    let baseline_set: BTreeSet<_> = baseline.iter().cloned().collect();
    if baseline_set.len() != baseline.len() {
        return Err("V8-potential baseline portfolio contains duplicates".to_owned());
    }
    let ranking_set: BTreeSet<_> = preview_ranking.iter().cloned().collect();
    if !baseline_set.is_subset(&ranking_set) {
        return Err("V8-potential preview omitted a baseline candidate".to_owned());
    }

    let mut selected = Vec::with_capacity(baseline.len());
    let mut rescued = Vec::new();
    for id in preview_ranking {
        if selected.len() == baseline.len() {
            break;
        }
        if baseline_set.contains(id) {
            selected.push(id.clone());
        } else if rescued.len() < rescued_candidate_cap {
            selected.push(id.clone());
            rescued.push(id.clone());
        }
    }
    if selected.len() < baseline.len() {
        for id in baseline {
            if selected.len() == baseline.len() {
                break;
            }
            if !selected.contains(id) {
                selected.push(id.clone());
            }
        }
    }
    let selected_set: BTreeSet<_> = selected.iter().cloned().collect();
    let retained_baseline = baseline
        .iter()
        .filter(|id| selected_set.contains(*id))
        .cloned()
        .collect();
    let displaced_baseline = baseline
        .iter()
        .filter(|id| !selected_set.contains(*id))
        .cloned()
        .collect();
    Ok(PotentialPromotionDecision {
        selected,
        retained_baseline,
        rescued,
        displaced_baseline,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn preview_rescues_only_outsiders_that_reach_the_topk() {
        let decision = choose_potential_portfolio(
            &ids(&["a", "b", "c", "d"]),
            &ids(&["x", "a", "b", "c", "d", "y"]),
            1,
        )
        .unwrap();
        assert_eq!(decision.selected, ids(&["x", "a", "b", "c"]));
        assert_eq!(decision.rescued, ids(&["x"]));
        assert_eq!(decision.displaced_baseline, ids(&["d"]));
    }

    #[test]
    fn rescue_cap_prevents_preview_pool_from_becoming_the_main_k() {
        let decision = choose_potential_portfolio(
            &ids(&["a", "b", "c", "d"]),
            &ids(&["x", "y", "z", "a", "b", "c", "d"]),
            1,
        )
        .unwrap();
        assert_eq!(decision.selected, ids(&["x", "a", "b", "c"]));
        assert_eq!(decision.rescued, ids(&["x"]));
    }

    #[test]
    fn no_outsider_in_preview_topk_preserves_the_baseline_set() {
        let decision = choose_potential_portfolio(
            &ids(&["a", "b", "c", "d"]),
            &ids(&["a", "b", "c", "d", "x"]),
            2,
        )
        .unwrap();
        assert_eq!(decision.selected, ids(&["a", "b", "c", "d"]));
        assert!(decision.rescued.is_empty());
        assert!(decision.displaced_baseline.is_empty());
    }
}
