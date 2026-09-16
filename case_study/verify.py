#!/usr/bin/env python3
"""Verify packaged files, netlist identities, G0 normalization and Genus reports; no EDA."""
import gzip, hashlib, json, math, re
from pathlib import Path
ROOT=Path(__file__).resolve().parent

def sha(p):return hashlib.sha256(p.read_bytes()).hexdigest()
def main():
    n=0
    for part in [ROOT,ROOT/'iterative',ROOT/'areapmo']:
        for name,h in json.loads((part/'CHECKSUMS.json').read_text()).items():
            assert sha(part/name)==h, f'checksum mismatch: {part/name}'
            n+=1
    routes=json.loads((ROOT/'iterative/data/routes.json').read_text())
    ar=json.loads((ROOT/'areapmo/data/routes.json').read_text())
    targets=json.loads((ROOT/'iterative/data/results.json').read_text())
    rows=json.loads((ROOT/'results/TABLE.json').read_text())['rows']
    assert len(rows)==24 and len(targets)==12 and len(ar)==6
    for key,route in routes.items():
        case=key.split('/')[0];plan=route['plan']
        assert sha(ROOT/'iterative'/plan['g0'])==ar[case]['g0_sha256']
    for r in rows:
        case=r['case'];assert sha(ROOT/r['netlist'])==r['sha256']
        for k in ['area','delay_ps','power_W']:
            if r[k] is not None:
                delta=100*(r[k]/r['g0_'+k]-1)
                assert math.isclose(delta,r['delta_'+k+'_pct'],abs_tol=1e-10)
                assert f'{delta:.2f}'==r['display_delta_'+k+'_pct']
        if r['evaluator']=='Genus':
            d=ROOT/'results'/case/'genus'/('AreaPMO' if r['method']=='AreaPMO' else 'Iterative')
            timing=float(re.search(r'^\.arrival\s+(\S+)',(d/'metrics.rpt').read_text(),re.M)[1])
            power=float(re.search(r'^\s*Subtotal\s+\S+\s+\S+\s+\S+\s+(\S+)',(d/'power.rpt').read_text(),re.M)[1])
            assert abs(timing-r['delay_ps'])<1e-8
            assert abs(power-r['power_W'])<1e-12
            assert 'No unresolved references' in (d/'unresolved.rpt').read_text()
        if r['method'].startswith('Iterative'):
            t=next(t for t in targets if t['case']==case and r['method']=='Iterative-'+t['guide'])
            assert r['sha256']==t['final_sha256']
    for d in (ROOT/'results').glob('*/genus/*'):
        if not d.is_dir():continue
        evidence=json.loads((d/'INPUT.json').read_text())
        assert sha(ROOT/evidence['netlist'])==evidence['sha256']
        def contents(name):
            p=d/name
            return p.read_text() if p.exists() else gzip.decompress(p.with_name(name+'.gz').read_bytes()).decode()
        assert sorted(contents('instances_before.tsv').splitlines())==sorted(contents('instances_after.tsv').splitlines())
    print(json.dumps({'checksums_checked':n,'table_rows':24,'iterative_paths':12,'same_G0':True,'genus_delay_power_reports_match':True,'unresolved_zero':True,'search_executed':False}))
if __name__=='__main__':main()
