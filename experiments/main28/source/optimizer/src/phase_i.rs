//! Incremental Phase-I extraction from an extended rewritten equivalence representation.
//! Choice domains, never provenance, determine expansion. Fixed transactions remain
//! atomic even when their enodes share a backing space with other transactions.

use anyhow::{Context, Result, bail};
use extraction_gym::ExtendedEGraph;
use extraction_gym::extract::incumbent_region_boundary;
use mac_egg::adaptive_region::{AdaptiveRegionSpace, BoundaryClosure, RegionAssignment};
use mac_egg::language::StdCellType;
use mac_egg::netlist::Netlist;
use mac_egg::region_local_evaluator::{
    BoundaryDualComponent, boundary_dual_components_region, boundary_dual_score_region,
    evaluate_physicalized_region,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::cmp::Ordering;
use std::sync::{
    atomic::{AtomicUsize, Ordering as AtomicOrdering},
    mpsc,
};

/// Allowed combinations, independent of where the enodes were produced.
#[derive(Clone, Copy)]
pub enum ImplementationDomain<'a> {
    Unresolved { anchors: &'a [usize] },
    Fixed { assignment: &'a RegionAssignment },
}

/// Zero-copy execution view: expressions/eclass bindings plus allowed choices.
/// Proof and boundary legality are checked when physicalizing the entire assignment.
/// Provenance is retained by the producer and admission, never consulted here.
pub struct RewrittenEquivalence<'a> {
    pub space: &'a AdaptiveRegionSpace,
    pub domain: ImplementationDomain<'a>,
}

pub struct RegionalScoring<'a> {
    pub ext: &'a ExtendedEGraph,
    pub result: &'a extraction_gym::extract::ExtractionResult,
    pub trace: &'a extraction_gym::extract::NldmEvaluationV2,
    pub adjoint: &'a extraction_gym::extract::CircuitAdjointV1,
    pub default_physical: &'a mac_egg::physical_topology_egraph::PhysicalizedTopology,
    pub limit: usize,
    pub unary_scores: &'a FxHashMap<String, f64>,
    pub macro_beam: bool,
    pub macro_pair_beam: bool,
}

/// Common expansion entry. Adaptive planning consumes unresolved domains in
/// batches because their scores determine subsequent regions. Complete domains
/// pass through once; no generator-candidate input or cross-domain product exists.
pub fn extract_phase_i(
    parent: &Netlist<StdCellType, ()>,
    representation: RewrittenEquivalence<'_>,
    scoring: Option<RegionalScoring<'_>>,
) -> Result<(Vec<ScoredAssignment>, &'static str, usize)> {
    match representation.domain {
        ImplementationDomain::Unresolved { anchors } => {
            let s = scoring.context("unresolved choices require regional scoring")?;
            score_region(
                parent,
                representation.space,
                s.ext,
                s.result,
                s.trace,
                s.adjoint,
                s.default_physical,
                anchors,
                s.limit,
                s.unary_scores,
                s.macro_beam,
                s.macro_pair_beam,
            )
        }
        ImplementationDomain::Fixed { assignment } => Ok((
            vec![ScoredAssignment {
                assignment: assignment.clone(),
                score: 0.0,
                components: Vec::new(),
            }],
            "fixed_atomic_implementation",
            1,
        )),
    }
}

#[derive(Clone, Debug)]
pub struct ScoredAssignment {
    pub assignment: RegionAssignment,
    pub score: f64,
    pub components: Vec<BoundaryDualComponent>,
}

#[derive(Clone, Debug)]
struct MacroFragmentCandidate {
    component_index: usize,
    partial_choices: Vec<mac_egg::adaptive_region::RegionChoice>,
    score: f64,
    changed_eclasses: usize,
}

fn parallel_map_ordered<T, R, F>(items: &[T], requested_jobs: usize, work: F) -> Result<Vec<R>>
where
    T: Sync,
    R: Send,
    F: Fn(usize, &T) -> Result<R> + Sync,
{
    if items.len() <= 1 || requested_jobs <= 1 {
        return items
            .iter()
            .enumerate()
            .map(|(index, item)| work(index, item))
            .collect();
    }
    let jobs = requested_jobs.min(items.len());
    let next = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            let sender = sender.clone();
            let next = &next;
            let work = &work;
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, AtomicOrdering::Relaxed);
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    if sender.send((index, work(index, item))).is_err() {
                        break;
                    }
                }
            });
        }
    });
    drop(sender);
    let mut ordered: Vec<Option<Result<R>>> = (0..items.len()).map(|_| None).collect();
    for (index, value) in receiver {
        ordered[index] = Some(value);
    }
    ordered
        .into_iter()
        .enumerate()
        .map(|(index, value)| value.with_context(|| format!("missing parallel result {index}"))?)
        .collect()
}

fn env_jobs(name: &str, default: usize) -> Result<usize> {
    Ok(std::env::var(name)
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()
        .with_context(|| format!("invalid {name}"))?
        .unwrap_or(default)
        .max(1))
}

fn assignment_signature(assignment: &RegionAssignment) -> String {
    assignment
        .choices
        .iter()
        .map(|choice| choice.enode_id.as_str())
        .collect::<Vec<_>>()
        .join("|")
}

fn full_assignment_from_fragments(
    space: &AdaptiveRegionSpace,
    region: &[usize],
    fragments: &[&MacroFragmentCandidate],
) -> Result<RegionAssignment> {
    let mut selected: FxHashMap<String, String> = FxHashMap::default();
    for fragment in fragments {
        for choice in &fragment.partial_choices {
            match selected.get(&choice.eclass_id) {
                Some(existing) if existing != &choice.enode_id => {
                    bail!(
                        "conflicting macro fragments for {}: {} vs {}",
                        choice.eclass_id,
                        existing,
                        choice.enode_id
                    );
                }
                Some(_) => {}
                None => {
                    selected.insert(choice.eclass_id.clone(), choice.enode_id.clone());
                }
            }
        }
    }
    let mut choices = Vec::with_capacity(region.len());
    for anchor in region {
        let class = space
            .class_by_anchor(*anchor)
            .context("missing region class while composing macro fragments")?;
        let enode_id = selected
            .get(&class.eclass_id)
            .cloned()
            .unwrap_or_else(|| class.incumbent_enode.clone());
        choices.push(mac_egg::adaptive_region::RegionChoice {
            eclass_id: class.eclass_id.clone(),
            enode_id,
        });
    }
    let changed_eclasses = choices
        .iter()
        .filter(|choice| {
            space
                .enode(&choice.enode_id)
                .is_some_and(|enode| !enode.incumbent)
        })
        .count();
    Ok(RegionAssignment {
        choices,
        changed_eclasses,
    })
}

fn physical_anchor(class: &egraph_serialize::ClassId) -> Result<usize> {
    Ok(class
        .to_string()
        .strip_prefix("physical.")
        .context("bad physical class")?
        .parse()?)
}

/// Partition a region into connected rewrite cones.  An edge is present when
/// an alternative enode at one occurrence references another active
/// occurrence as a boundary.  The old large-region path treated every
/// e-class as an independent unary decision; these components are the small
/// structural units used by the macro beam instead.
pub fn macro_components(space: &AdaptiveRegionSpace, region: &[usize]) -> Vec<Vec<usize>> {
    let active: FxHashSet<_> = region.iter().copied().collect();
    let mut parent: FxHashMap<usize, usize> =
        region.iter().map(|anchor| (*anchor, *anchor)).collect();

    fn find(parent: &mut FxHashMap<usize, usize>, item: usize) -> usize {
        let root = parent.get(&item).copied().unwrap_or(item);
        if root == item {
            item
        } else {
            let root = find(parent, root);
            parent.insert(item, root);
            root
        }
    }
    fn union(parent: &mut FxHashMap<usize, usize>, first: usize, second: usize) {
        let first_root = find(parent, first);
        let second_root = find(parent, second);
        if first_root != second_root {
            parent.insert(second_root, first_root);
        }
    }

    for anchor in region {
        if let Some(class) = space.class_by_anchor(*anchor) {
            for enode in &class.enodes {
                for boundary in &enode.boundary_anchors {
                    if active.contains(boundary) {
                        union(&mut parent, *anchor, *boundary);
                    }
                }
            }
        }
    }
    let mut groups: FxHashMap<usize, Vec<usize>> = FxHashMap::default();
    for anchor in region {
        let root = find(&mut parent, *anchor);
        groups.entry(root).or_default().push(*anchor);
    }
    let mut result: Vec<_> = groups.into_values().collect();
    for group in &mut result {
        group.sort_unstable();
    }
    result.sort_by_key(|group| group[0]);
    result
}

/// Generate a bounded set of complete macro assignments for one connected
/// rewrite cone.  This is intentionally lazy: it never materializes the full
/// Cartesian support.  The first members cover the incumbent and one-change
/// cones; the remaining slots are filled by a deterministic mixed-radix walk
/// over the same cone, so coupled alternatives can enter the beam together.
fn macro_variants(
    space: &AdaptiveRegionSpace,
    component: &[usize],
    cap: usize,
) -> Result<Vec<Vec<mac_egg::adaptive_region::RegionChoice>>> {
    let classes: Vec<_> = component
        .iter()
        .map(|anchor| {
            space
                .class_by_anchor(*anchor)
                .context("missing macro component eclass")
        })
        .collect::<Result<_>>()?;
    let mut variants = Vec::new();
    let mut seen = FxHashSet::default();
    let push_variant = |choices: Vec<mac_egg::adaptive_region::RegionChoice>,
                        variants: &mut Vec<Vec<mac_egg::adaptive_region::RegionChoice>>,
                        seen: &mut FxHashSet<String>| {
        let key = choices
            .iter()
            .map(|choice| choice.enode_id.as_str())
            .collect::<Vec<_>>()
            .join("|");
        if seen.insert(key) && variants.len() < cap {
            variants.push(choices);
        }
    };

    // Anchor is always retained as a legal fallback for every macro.
    let incumbent = classes
        .iter()
        .map(|class| mac_egg::adaptive_region::RegionChoice {
            eclass_id: class.eclass_id.clone(),
            enode_id: class.incumbent_enode.clone(),
        })
        .collect();
    push_variant(incumbent, &mut variants, &mut seen);

    // Reserve only half the budget for unary seeds.  The old beam could spend
    // every slot here and never expose an A+B rewrite interaction.
    let unary_cap = (cap / 2).max(1);
    'unary: for (index, class) in classes.iter().enumerate() {
        for enode in class.enodes.iter().filter(|enode| !enode.incumbent) {
            let choices = classes
                .iter()
                .enumerate()
                .map(
                    |(other_index, other)| mac_egg::adaptive_region::RegionChoice {
                        eclass_id: other.eclass_id.clone(),
                        enode_id: if other_index == index {
                            enode.enode_id.clone()
                        } else {
                            other.incumbent_enode.clone()
                        },
                    },
                )
                .collect();
            push_variant(choices, &mut variants, &mut seen);
            if variants.len() >= unary_cap {
                break 'unary;
            }
        }
    }

    // Explicitly reserve coupled slots for two e-classes in the same rewrite
    // cone.  These are the cases killed by independent unary pruning.
    'pairs: for first in 0..classes.len() {
        for second in first + 1..classes.len() {
            for first_enode in classes[first]
                .enodes
                .iter()
                .filter(|enode| !enode.incumbent)
            {
                for second_enode in classes[second]
                    .enodes
                    .iter()
                    .filter(|enode| !enode.incumbent)
                {
                    let choices = classes
                        .iter()
                        .enumerate()
                        .map(|(index, class)| mac_egg::adaptive_region::RegionChoice {
                            eclass_id: class.eclass_id.clone(),
                            enode_id: if index == first {
                                first_enode.enode_id.clone()
                            } else if index == second {
                                second_enode.enode_id.clone()
                            } else {
                                class.incumbent_enode.clone()
                            },
                        })
                        .collect();
                    push_variant(choices, &mut variants, &mut seen);
                    if variants.len() >= cap {
                        break 'pairs;
                    }
                }
            }
        }
    }

    // If a tiny cone still has spare slots, add bounded three-way macros using
    // the first structural alternative at each root.  Never turn an R8/R16
    // region into a simultaneous all-root rewrite: that was both expensive
    // and semantically unrelated to a local rewrite macro.
    'triples: for first in 0..classes.len() {
        for second in first + 1..classes.len() {
            for third in second + 1..classes.len() {
                let selected = [first, second, third]
                    .into_iter()
                    .map(|index| {
                        classes[index]
                            .enodes
                            .iter()
                            .find(|enode| !enode.incumbent)
                            .map(|enode| (index, enode.enode_id.clone()))
                    })
                    .collect::<Option<FxHashMap<_, _>>>();
                let Some(selected) = selected else { continue };
                let choices = classes
                    .iter()
                    .enumerate()
                    .map(|(index, class)| mac_egg::adaptive_region::RegionChoice {
                        eclass_id: class.eclass_id.clone(),
                        enode_id: selected
                            .get(&index)
                            .cloned()
                            .unwrap_or_else(|| class.incumbent_enode.clone()),
                    })
                    .collect();
                push_variant(choices, &mut variants, &mut seen);
                if variants.len() >= cap {
                    break 'triples;
                }
            }
        }
    }
    Ok(variants)
}

fn score_region(
    source: &Netlist<StdCellType, ()>,
    space: &AdaptiveRegionSpace,
    ext: &ExtendedEGraph,
    result: &extraction_gym::extract::ExtractionResult,
    trace: &extraction_gym::extract::NldmEvaluationV2,
    adjoint: &extraction_gym::extract::CircuitAdjointV1,
    default_physical: &mac_egg::physical_topology_egraph::PhysicalizedTopology,
    region: &[usize],
    limit: usize,
    unary_scores: &FxHashMap<String, f64>,
    macro_beam: bool,
    macro_pair_beam: bool,
) -> Result<(Vec<ScoredAssignment>, &'static str, usize)> {
    let region_score_jobs = env_jobs("EGG_REGION_SCORE_JOBS", 8)?;
    let region_classes: FxHashSet<_> = region
        .iter()
        .map(|anchor| egraph_serialize::ClassId::from(format!("physical.{anchor}")))
        .collect();
    let boundary = incumbent_region_boundary(result, ext, trace, &region_classes)
        .map_err(anyhow::Error::msg)?;
    let output_anchors: FxHashSet<_> = boundary
        .outputs
        .iter()
        .map(|item| physical_anchor(&item.class))
        .collect::<Result<_>>()?;
    let region_anchors: FxHashSet<_> = region.iter().copied().collect();
    let incumbent = evaluate_physicalized_region(
        default_physical,
        &region_anchors,
        &output_anchors,
        trace,
        ext,
    )
    .map_err(anyhow::Error::msg)?;
    let theoretical = region
        .iter()
        .try_fold(1usize, |product, anchor| {
            product.checked_mul(
                space
                    .class_by_anchor(*anchor)
                    .map_or(1, |class| class.enodes.len()),
            )
        })
        .unwrap_or(usize::MAX);
    let (assignments, solver) = if theoretical <= 4096 {
        (
            space
                .enumerate_region_assignments_unchecked(
                    region,
                    BoundaryClosure::FixedBoundary,
                    region.len(),
                    4096,
                )
                .map_err(anyhow::Error::msg)?,
            "l1_exact_enumeration",
        )
    } else if macro_pair_beam {
        let components = macro_components(space, region);
        let mut fragments = Vec::new();
        let mut seen = FxHashSet::default();
        let mut candidate_count = 0usize;
        for (component_index, component) in components.iter().enumerate() {
            let variants = macro_variants(space, component, 32)?;
            for partial_choices in variants {
                let fragment = MacroFragmentCandidate {
                    component_index,
                    partial_choices,
                    score: 0.0,
                    changed_eclasses: 0,
                };
                let assignment = full_assignment_from_fragments(space, region, &[&fragment])?;
                let signature = assignment_signature(&assignment);
                if !seen.insert(signature) {
                    continue;
                }
                let physical = match space.physicalize_assignment(source, &assignment) {
                    Ok(physical) => physical,
                    Err(_) => continue,
                };
                let response = evaluate_physicalized_region(
                    &physical,
                    &region_anchors,
                    &output_anchors,
                    trace,
                    ext,
                )
                .map_err(anyhow::Error::msg)?;
                let score = boundary_dual_score_region(&response, &incumbent, adjoint);
                let changed_eclasses = assignment.changed_eclasses;
                if changed_eclasses == 0 {
                    continue;
                }
                fragments.push(MacroFragmentCandidate {
                    score,
                    changed_eclasses,
                    ..fragment
                });
                candidate_count += 1;
            }
        }
        fragments.sort_by(|first, second| {
            first
                .score
                .partial_cmp(&second.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| first.changed_eclasses.cmp(&second.changed_eclasses))
                .then_with(|| first.component_index.cmp(&second.component_index))
        });
        let single_limit = 32usize.min(fragments.len());
        let pair_limit = 20usize.min(single_limit);
        let mut rows = Vec::new();
        let mut seen = FxHashSet::default();
        let mut consider_assignment = |assignment: RegionAssignment| -> Result<()> {
            if assignment.changed_eclasses == 0 {
                return Ok(());
            }
            let signature = assignment_signature(&assignment);
            if !seen.insert(signature) {
                return Ok(());
            }
            let physical = match space.physicalize_assignment(source, &assignment) {
                Ok(physical) => physical,
                Err(_) => return Ok(()),
            };
            let response = evaluate_physicalized_region(
                &physical,
                &region_anchors,
                &output_anchors,
                trace,
                ext,
            )
            .map_err(anyhow::Error::msg)?;
            let score = boundary_dual_score_region(&response, &incumbent, adjoint);
            let components = boundary_dual_components_region(&response, &incumbent, adjoint);
            rows.push(ScoredAssignment {
                assignment,
                score,
                components,
            });
            Ok(())
        };

        for fragment in fragments.iter().take(single_limit) {
            let assignment = full_assignment_from_fragments(space, region, &[fragment])?;
            candidate_count += 1;
            consider_assignment(assignment)?;
        }
        for first in 0..pair_limit {
            for second in first + 1..pair_limit {
                let left = &fragments[first];
                let right = &fragments[second];
                if left.component_index == right.component_index {
                    continue;
                }
                let assignment = full_assignment_from_fragments(space, region, &[left, right])?;
                candidate_count += 1;
                consider_assignment(assignment)?;
            }
        }

        rows.sort_by(|first, second| {
            first
                .score
                .partial_cmp(&second.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| {
                    first
                        .assignment
                        .changed_eclasses
                        .cmp(&second.assignment.changed_eclasses)
                })
        });
        rows.truncate(limit);
        return Ok((rows, "l3_p2_rewrite_macro_pair_beam64", candidate_count));
    } else if !macro_beam {
        // Reproducibility switch for a same-budget ablation.  The production
        // path uses complete rewrite macros below; this branch is retained so
        // a planner run can be compared under exactly the same library/rules.
        let mut partial: Vec<(f64, Vec<mac_egg::adaptive_region::RegionChoice>)> =
            vec![(0.0, Vec::new())];
        for anchor in region {
            let class = space
                .class_by_anchor(*anchor)
                .context("missing active region class")?;
            let mut expanded = Vec::new();
            for (score, choices) in &partial {
                for enode in &class.enodes {
                    let mut next = choices.clone();
                    next.push(mac_egg::adaptive_region::RegionChoice {
                        eclass_id: class.eclass_id.clone(),
                        enode_id: enode.enode_id.clone(),
                    });
                    expanded.push((
                        score + unary_scores.get(&enode.enode_id).copied().unwrap_or(0.0),
                        next,
                    ));
                }
            }
            expanded
                .sort_by(|first, second| first.0.partial_cmp(&second.0).unwrap_or(Ordering::Equal));
            expanded.truncate(64);
            partial = expanded;
        }
        let assignments = partial
            .into_iter()
            .filter_map(|(_, choices)| {
                let changed = choices
                    .iter()
                    .filter(|choice| {
                        space
                            .enode(&choice.enode_id)
                            .is_some_and(|enode| !enode.incumbent)
                    })
                    .count();
                (changed > 0).then_some(RegionAssignment {
                    choices,
                    changed_eclasses: changed,
                })
            })
            .filter(|assignment| space.physicalize_assignment(source, assignment).is_ok())
            .collect();
        (assignments, "l3_p2_unary_beam64_ablation")
    } else {
        let components = macro_components(space, region);
        let mut partial: Vec<(f64, Vec<mac_egg::adaptive_region::RegionChoice>)> =
            vec![(0.0, Vec::new())];
        for component in &components {
            let variants = macro_variants(space, component, 64)?;
            let mut candidate_choices = Vec::new();
            for (_, prefix) in &partial {
                for macro_choices in &variants {
                    let mut choices = prefix.clone();
                    choices.extend(macro_choices.iter().cloned());
                    candidate_choices.push(choices);
                }
            }
            let evaluated =
                parallel_map_ordered(&candidate_choices, region_score_jobs, |_, choices| {
                    let changed = choices
                        .iter()
                        .filter(|choice| {
                            space
                                .enode(&choice.enode_id)
                                .is_some_and(|enode| !enode.incumbent)
                        })
                        .count();
                    let assignment = RegionAssignment {
                        choices: choices.clone(),
                        changed_eclasses: changed,
                    };
                    let Ok(physical) = space.physicalize_assignment(source, &assignment) else {
                        return Ok(None);
                    };
                    let response = evaluate_physicalized_region(
                        &physical,
                        &region_anchors,
                        &output_anchors,
                        trace,
                        ext,
                    )
                    .map_err(anyhow::Error::msg)?;
                    // Crucially this is the P2 score of the complete macro
                    // prefix, not a sum of independent enode prices.
                    let score = boundary_dual_score_region(&response, &incumbent, adjoint);
                    Ok(Some((score, choices.clone())))
                })?;
            let mut expanded: Vec<_> = evaluated.into_iter().flatten().collect();
            expanded.sort_by(|first, second| {
                first
                    .0
                    .partial_cmp(&second.0)
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| first.1.len().cmp(&second.1.len()))
            });
            expanded.truncate(64);
            partial = expanded;
        }
        let assignments = partial
            .into_iter()
            .filter_map(|(score, choices)| {
                let changed = choices
                    .iter()
                    .filter(|choice| {
                        space
                            .enode(&choice.enode_id)
                            .is_some_and(|enode| !enode.incumbent)
                    })
                    .count();
                (changed > 0).then_some((
                    score,
                    RegionAssignment {
                        choices,
                        changed_eclasses: changed,
                    },
                ))
            })
            .filter(|(_, assignment)| space.physicalize_assignment(source, assignment).is_ok())
            .map(|(_, assignment)| assignment)
            .collect();
        (assignments, "l3_p2_rewrite_macro_beam64")
    };
    let evaluated = parallel_map_ordered(&assignments, region_score_jobs, |_, assignment| {
        let Ok(physical) = space.physicalize_assignment(source, assignment) else {
            return Ok(None);
        };
        let response =
            evaluate_physicalized_region(&physical, &region_anchors, &output_anchors, trace, ext)
                .map_err(anyhow::Error::msg)?;
        let score = boundary_dual_score_region(&response, &incumbent, adjoint);
        let components = boundary_dual_components_region(&response, &incumbent, adjoint);
        Ok(Some(ScoredAssignment {
            assignment: assignment.clone(),
            score,
            components,
        }))
    })?;
    let mut rows: Vec<_> = evaluated.into_iter().flatten().collect();
    rows.sort_by(|first, second| {
        first
            .score
            .partial_cmp(&second.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                first
                    .assignment
                    .changed_eclasses
                    .cmp(&second.assignment.changed_eclasses)
            })
    });
    rows.truncate(limit);
    Ok((rows, solver, theoretical))
}
