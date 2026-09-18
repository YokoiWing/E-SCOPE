#!/usr/bin/env python3
"""AreaPMO: prepared AIG -> native mapping G0 -> optimization -> feasible best."""
import argparse, hashlib, json, math, os, shutil, signal, subprocess, time
from pathlib import Path
ROOT = Path(__file__).resolve().parent
GENLIB = ROOT / 'assets/asap7.genlib'

def sha(p): return hashlib.sha256(p.read_bytes()).hexdigest()
def save(p, d):
    tmp = p.with_suffix('.tmp'); tmp.write_text(json.dumps(d, indent=2) + '\n'); tmp.replace(p)

def write_bound(source, target, case):
    graph = json.loads(source.read_text())
    outputs = {line.split()[1]: line.split()[3].split('=')[0]
               for line in GENLIB.read_text().splitlines() if line.startswith('GATE ')}
    pis = {n: f'pi{i}' for i, n in enumerate(graph['pis'])}
    def signal(node):
        if node['node'] == 0:
            return "1'b1" if node.get('complement', False) else "1'b0"
        assert not node.get('complement', False)
        return pis.get(node['node'], f'n{node["node"]}')
    pos = [f'po{i}' for i in range(len(graph['pos']))]
    lines = [f'module {case} (' + ', '.join([*pis.values(), *pos]) + ');',
             'input ' + ', '.join(pis.values()) + ';', 'output ' + ', '.join(pos) + ';',
             'wire ' + ', '.join(f'n{c["node"]}' for c in graph['cells']) + ';']
    for c in graph['cells']:
        conns = [f'.{outputs[c["cell"]]}(n{c["node"]})']
        conns += [f'.{p["pin"]}({signal(p)})' for p in c['inputs']]
        lines.append(f'{c["cell"]} u{c["node"]} (' + ', '.join(conns) + ');')
    lines += [f'assign {name} = {signal(node)};' for name, node in zip(pos, graph['pos'])]
    target.write_text('\n'.join(lines + ['endmodule']) + '\n')


def check_bound(path):
    graph = json.loads(path.read_text())
    cells = {r['node']: r for r in graph['cells']}
    arrival = {0: 0., **{n: 0. for n in graph['pis']}}
    def calc(n):
        if n not in arrival:
            values = [calc(p['node']) + math.ceil(max(p['rise'], p['fall']) * 100) / 100
                      for p in cells[n]['inputs']]
            arrival[n] = math.ceil(max(values, default=0.) * 100) / 100
        return arrival[n]
    # Match the author's exclusion of constant-driven single-input cells.
    # Explicit ordered additions reproduce C++ double summation; Python 3.12+
    # sum() uses compensated summation and can change ceil-to-cent at a boundary.
    area = 0.
    for c in graph['cells']:
        if not (len(c['inputs']) == 1 and c['inputs'][0]['node'] == 0):
            area += c['area']
    unique_bindings = {(c['cell'], tuple((p['node'], p['complement']) for p in c['inputs']))
                       for c in graph['cells']}
    return {'area_exact_sum': area,
            'area_author_rounding': math.ceil(area * 100) / 100,
            'delay': max(calc(p['node']) for p in graph['pos']),
            'gates': len(cells), 'author_hash_entries': len(unique_bindings)}


def run(cmd, out, log, timeout):
    start = time.monotonic()
    with log.open('w') as f:
        child = subprocess.Popen([str(x) for x in cmd], cwd=out, stdout=f, stderr=subprocess.STDOUT, start_new_session=True)
        save(out / 'status.json', {'stage':'running', 'pid':child.pid, 'command':[str(x) for x in cmd], 'timeout_seconds':timeout})
        try:
            code = child.wait(timeout=timeout)
            if code: raise RuntimeError(f'command failed ({code}): {log}')
        finally:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
                try: child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL); child.wait()
    return time.monotonic() - start

def main():
    routes=json.loads((ROOT/'data/routes.json').read_text())
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--case',required=True,choices=routes)
    ap.add_argument('--mode',choices=['plan','execute'],default='plan')
    ap.add_argument('--out',type=Path,required=True)
    ap.add_argument('--timeout-sec',type=float,default=3600)
    ap.add_argument('--areapmo-bin',type=Path,default=Path(os.environ.get('AREAPMO_BIN',ROOT/'source/build/areapmo_native')))
    ap.add_argument('--abc-bin',type=Path,default=Path(os.environ.get('ABC_BIN','abc')))
    args=ap.parse_args()
    checks=json.loads((ROOT/'CHECKSUMS.json').read_text())
    for rel,h in checks.items():
        if sha(ROOT/rel)!=h: raise RuntimeError('asset changed: '+rel)
    out=args.out.resolve(); out.mkdir(parents=True,exist_ok=False)
    route=routes[args.case]
    areapmo_bin=args.areapmo_bin.expanduser().resolve()
    abc_bin=args.abc_bin.expanduser()
    if args.mode=='execute' and not areapmo_bin.is_file():
        raise RuntimeError(f'AreaPMO executable not found: {areapmo_bin}; build source/ or pass --areapmo-bin')
    if args.mode=='execute' and not (abc_bin.is_file() or shutil.which(str(abc_bin))):
        raise RuntimeError(f'ABC executable not found: {abc_bin}; pass --abc-bin or set ABC_BIN')
    cmd=[areapmo_bin,'optimize',GENLIB,ROOT/route['input'],out/'native',ROOT/'assets/database/asap7']
    save(out/'PLAN.json',{'case':args.case,'command':list(map(str,cmd)),'input_scope':'prepared AIG; native mapping G0 is rerun','timeout_sec':args.timeout_sec,'selection':'feasible delay <= G0 + 1e-9; min(area, round)','round_cap':100,'minimum_rounds':10,'stagnation_patience':3,'seed':5})
    if args.mode=='plan':return
    try:
        wall=run(cmd,out,out/'native.log',args.timeout_sec)
        summary=json.loads((out/'native/summary.json').read_text())
        assert summary['stage']=='complete'
        best=min([summary['g0']|{'round':0}]+[r for r in summary['rounds'] if r['delay']<=summary['g0']['delay']+1e-9],key=lambda r:(r['area'],r['round']))
        finaltag='g0' if best['round']==0 else f'round_{best["round"]}'
        for name,tag in [('G0','g0'),('final',finaltag)]:
            write_bound(out/f'native/{tag}.binding.json',out/f'{name}.v',args.case)
        actual=check_bound(out/f'native/{finaltag}.binding.json')
        assert abs(actual['area_author_rounding']-best['area'])<1e-8
        assert abs(actual['delay']-best['delay'])<1e-8
        # Independent equivalence of prepared input -> G0 and G0 -> selected final.
        for name,lhs,rhs in [('g0',ROOT/route['input'],out/'native/g0.blif'),('final',out/'native/g0.blif',out/f'native/{finaltag}.blif')]:
            def quote(x): return '"'+str(x).replace('"','\"')+'"'
            abc=f'read_library {quote(GENLIB)}; cec -n {quote(lhs)} {quote(rhs)}'
            run([abc_bin,'-c',abc],out,out/f'{name}.cec.log',args.timeout_sec)
            assert 'Networks are equivalent' in (out/f'{name}.cec.log').read_text()
        g0sha=sha(out/'G0.v'); finalsha=sha(out/'final.v')
        result={'case':args.case,'stage':'complete','native_wall_sec':wall,'rounds':len(summary['rounds']),'best':best,'genlib_check':actual,'cec_pass':True,'g0_sha256':g0sha,'final_sha256':finalsha,'external_validation':'PENDING'}
        save(out/'RESULT.json',result);save(out/'status.json',result);print(json.dumps(result),flush=True)
    except BaseException as e:
        save(out/'status.json',{'stage':'failed','error':str(e)});raise

if __name__=='__main__':
    signal.signal(signal.SIGTERM,lambda *_: (_ for _ in ()).throw(KeyboardInterrupt()))
    main()
