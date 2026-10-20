#!/usr/bin/env python3
"""旧ACL入口的兼容汇总；所有验收委托严格的隔离runner，不再解析PASS文本。"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

root=Path(__file__).resolve().parents[1]
kind=sys.argv[1]
provider=os.environ.get('ACT_KERNEL_PROVIDER','portable')
cases=['000','001','002','003','004'] if kind=='correctness' else [os.environ.get('ACT_CASE_ID','000')]
replicates=1 if kind=='correctness' else int(os.environ.get('ACT_BENCHMARK_REPLICATES','3'))
if not 1 <= replicates <= 10: raise SystemExit('replicates必须为1..10')
out=Path(tempfile.mkdtemp(prefix=f'acl-{kind}-{provider}-',dir=root/'target'))
rows=[]
for case in cases:
    for boot in range(replicates):
        phase='correctness' if kind=='correctness' else ('stability' if os.environ.get('ACT_CONTINUOUS_CHECK')=='1' else 'benchmark')
        command=['python3',str(root/'scripts/run_act_release.py'),'--provider',provider,'--case',case,'--phase',phase,'--timeout',os.environ.get('ACT_QEMU_TIMEOUT_SECONDS','1200')]
        result=subprocess.run(command,cwd=root,text=True,stdout=subprocess.PIPE,stderr=subprocess.STDOUT)
        (out/f'case-{case}-boot-{boot+1}.log').write_text(result.stdout)
        if result.returncode: print(result.stdout); raise SystemExit(result.returncode)
        directory=Path(next(line.split('=',1)[1] for line in result.stdout.splitlines() if line.startswith('ACT_RELEASE_OUT=')))
        value=json.loads((directory/'verification.json').read_text())
        rows.append(dict(case_id=case,boot=boot+1,values=value['values'],failed_values=0,max_abs_error=value['max_abs_error'],output_sha256=value['output_sha256'],el0_exit_zero=True,model_only_median_ms=value['statistics']['median_ms'],model_only_p95_ms=value['statistics']['p95_ms'],model_only_samples_ms=value['model_samples_ms'],source=str(directory)))
summary=dict(provider=provider,case_id=cases[0] if len(cases)==1 else None,replicates=rows,correctness=dict(all_pass=True,values=sum(row['values'] for row in rows),failed_values=0),cases=rows)
(out/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
print(('ACL_CORRECTNESS_OUT=' if kind=='correctness' else 'ACL_BENCHMARK_OUT=')+str(out))
print('ACL_'+kind.upper()+'_PASS=true')
