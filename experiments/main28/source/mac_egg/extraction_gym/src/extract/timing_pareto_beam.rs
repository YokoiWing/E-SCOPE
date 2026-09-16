//! Complete-extraction timing-guided Pareto beam extractor.
//! The `Extractor` adapter returns one representative point; the experiment
//! binary keeps the full archive and trace for analysis.

use super::*;

#[derive(Clone, Debug)]
pub struct TimingParetoBeamConfig {
    pub seed: u64,
    pub exact_eval_budget: usize,
    pub beam_size: usize,
    pub moves_per_solution: usize,
}

impl Default for TimingParetoBeamConfig {
    fn default() -> Self { Self { seed: 0, exact_eval_budget: 1000, beam_size: 16, moves_per_solution: 8 } }
}

#[derive(Default)]
pub struct TimingParetoBeamExtractor { pub config: TimingParetoBeamConfig }

#[derive(Clone)] struct Candidate { result: ExtractionResult, cost: ExtendedCost }
struct Rng(u64);
impl Rng { fn new(seed:u64)->Self{Self(seed)} fn pick(&mut self,n:usize)->usize{self.0=self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);((self.0>>32)as usize)%n.max(1)} }

fn finite(c:ExtendedCost)->bool { c.components[..3].iter().all(|x| x.into_inner().is_finite()) }
fn triplet(c:ExtendedCost)->(f64,f64,f64){(c.components[0].into_inner(),c.components[1].into_inner(),c.components[2].into_inner())}
fn dominates(a:ExtendedCost,b:ExtendedCost)->bool{let a=triplet(a);let b=triplet(b);a.0<=b.0&&a.1<=b.1&&a.2<=b.2&&(a.0<b.0||a.1<b.1||a.2<b.2)}
fn hash(r:&ExtractionResult)->String{let mut v:Vec<_>=r.choices.iter().map(|(c,n)|format!("{c}={n}")).collect();v.sort();v.join(";")}
fn reachable(ext:&ExtendedEGraph,r:&ExtractionResult)->bool{let mut q:VecDeque<ClassId>=ext.inner.root_eclasses.clone().into();let mut seen=FxHashSet::default();while let Some(c)=q.pop_front(){let Some(n)=r.choices.get(&c)else{return false};if !seen.insert(c){continue}for ch in &ext.inner[n].children{q.push_back(ext.inner.nid_to_cid(ch).clone())}}true}
fn initial(ext:&ExtendedEGraph,_rng:&mut Rng)->Option<ExtractionResult>{let mut r=ExtractionResult::default();for c in ext.inner.classes().keys(){let n=ext.inner[c].nodes.iter().min_by_key(|n|n.to_string())?.clone();r.choose(c.clone(),n);}if r.find_cycles(ext,&ext.inner.root_eclasses).is_empty()&&reachable(ext,&r){Some(r)}else{None}}
fn proxy(ext:&ExtendedEGraph,n:&NodeId)->(f64,f64,f64){let op=ext.node_ops.get(n).map(String::as_str).unwrap_or(&ext.inner[n].op);if let Some(x)=ext.cell_nldm.get(op){let drive=op.split('x').nth(1).and_then(|s|s.chars().take_while(|c|c.is_ascii_digit()).collect::<String>().parse::<f64>().ok()).unwrap_or(1.0);(1.0/drive,x.area.into_inner(),x.leakage_power.into_inner())}else{(0.0,0.0,0.0)}}
fn proposal(ext:&ExtendedEGraph,parent:&Candidate,rng:&mut Rng)->Option<ExtractionResult>{let trace=parent.result.evaluate_nldm_v2_with_trace(ext,None,NldmV2Config::default());let mut options:Vec<_>=trace.timing.iter().filter(|(c,_)|ext.inner.classes()[*c].nodes.len()>1).map(|(c,p)|(c.clone(),p.rise.slack.min(p.fall.slack))).collect();options.sort_by_key(|x|x.0.to_string());let requested_timing=rng.pick(2)==0;for timing in [requested_timing,!requested_timing]{let mut eligible=Vec::new();for(cid,slack)in &options{if timing&&*slack>25.0||!timing&&*slack<=25.0{continue}let old=parent.result.choices.get(cid)?.clone();let before=proxy(ext,&old);let mut alts:Vec<_>=ext.inner[cid].nodes.iter().filter(|n|**n!=old).filter(|n|{let after=proxy(ext,n);if timing{after.0<before.0}else{after.1<=before.1&&after.2<=before.2&&(after.1<before.1||after.2<before.2)}}).cloned().collect();alts.sort_by(|a,b|{let pa=proxy(ext,a);let pb=proxy(ext,b);if timing{pa.0.total_cmp(&pb.0)}else{pa.1.total_cmp(&pb.1).then_with(||pa.2.total_cmp(&pb.2))}});if let Some(new)=alts.first(){eligible.push((cid.clone(),new.clone()));}}if !eligible.is_empty(){let(cid,new)=eligible.swap_remove(rng.pick(eligible.len()));let mut r=parent.result.clone();r.choose(cid,new);if r.find_cycles(ext,&ext.inner.root_eclasses).is_empty()&&reachable(ext,&r){return Some(r)}}}None}
fn select(mut pool:Vec<Candidate>,target:f64,size:usize)->Vec<Candidate>{pool.sort_by(|a,b|{let x=triplet(a.cost);let y=triplet(b.cost);let fx=x.0<=target;let fy=y.0<=target;fy.cmp(&fx).then_with(||if fx{x.1.total_cmp(&y.1).then_with(||x.2.total_cmp(&y.2))}else{(x.0-target).total_cmp(&(y.0-target))})});let mut seen=FxHashSet::default();pool.into_iter().filter(|x|seen.insert(hash(&x.result))).take(size).collect()}

impl Extractor for TimingParetoBeamExtractor {
    fn extract(&self, ext:&ExtendedEGraph, _roots:&[ClassId])->ExtractionResult {
        let mut initial_rng=Rng::new(0);
        let Some(start)=initial(ext,&mut initial_rng) else{return ExtractionResult::default()};
        let mut rng=Rng::new(self.config.seed);
        let start_cost=start.dag_cost_nldm_v2_on_pruned(ext); if !finite(start_cost){return start}
        let target=start_cost.components[0].into_inner(); let mut evals=1usize;
        let mut beam=vec![Candidate{result:start.clone(),cost:start_cost}]; let mut archive=beam.clone();
        while evals<self.config.exact_eval_budget { let before=evals;let mut pool=beam.clone(); for parent in &beam { for _ in 0..self.config.moves_per_solution { if evals>=self.config.exact_eval_budget{break} let Some(r)=proposal(ext,parent,&mut rng)else{continue}; let c=r.dag_cost_nldm_v2_on_pruned(ext);evals+=1;if !finite(c){continue}let cand=Candidate{result:r,cost:c};if !archive.iter().any(|x|dominates(x.cost,cand.cost)){archive.retain(|x|!dominates(cand.cost,x.cost));archive.push(cand.clone())}pool.push(cand); } } if evals==before{break}beam=select(pool,target,self.config.beam_size);if beam.is_empty(){break} }
        archive.into_iter().min_by(|a,b|a.cost.abs().cmp(&b.cost.abs())).map(|x|x.result).unwrap_or(start)
    }
}
