#!/usr/bin/env python3
"""逐函数核对入口签名、依赖调用序列及 if 条件/嵌套；语义仍由行为测试补充。"""
import json,re
from pathlib import Path
from source_contract import tokens,verify_source
ROOT=Path(__file__).resolve().parents[2]
C=ROOT.parent/'linux/drivers/net/ethernet/litex/litex_liteeth.c'
R=ROOT/'libos/src/drivers/liteeth.rs'
verify_source(C);verify_source(ROOT/'libos/vendor/liteeth-linux/litex_liteeth.c')
cs=C.read_text();rs=R.read_text()
names=['liteeth_rx','liteeth_interrupt','liteeth_open','liteeth_stop','liteeth_start_xmit','liteeth_get_stats64','liteeth_setup_slots','liteeth_probe','liteeth_remove']
def balanced(ts,start,opening='(',closing=')'):
 depth=0
 for i in range(start,len(ts)):
  if ts[i]==opening:depth+=1
  elif ts[i]==closing:
   depth-=1
   if depth==0:return i+1
 raise AssertionError('缺少配对符号')
def bodies(text,lang):
 ts=tokens(text.replace('r#priv','priv'));result={}
 for name in names:
  for i,t in enumerate(ts):
   if t!=name or i+1>=len(ts) or ts[i+1]!='(':continue
   end=balanced(ts,i+1)
   k=end
   if lang=='rust':
    while ts[k]!='{':k+=1
   if ts[k]!='{':continue
   finish=balanced(ts,k,'{','}')
   # 定义只能位于文件级；调用位置不应盖过已经取得的定义。
   if name not in result:result[name]=(ts[i+2:end-1],ts[k+1:finish-1],ts[i-1]if lang=='c'else ''.join(ts[end:k]).removeprefix('->')or'()')
   break
 return result
cb=bodies(cs,'c');rb=bodies(rs,'rust')
def c_type(t):
 raw=''.join(t).replace('struct','')
 for a,b in [('void*','*mutc_void'),('int','i32')]:
  if raw==a:return b
 if raw.endswith('*'):return '*mut'+raw[:-1]
 return raw
for n in names:
 cp,_,ret=cb[n];rp,_,rret=rb[n]
 def split(ts):
  out=[[]]
  for t in ts:
   if t==',':out.append([])
   else:out[-1].append(t)
  return [v for v in out if v]
 expected=[(p[-1],c_type(p[:-1]))for p in split(cp)]
 actual=[(p[0],''.join(p[2:]))for p in split(rp)]
 assert actual==expected,(n,'参数',expected,actual)
 assert rret==('()'if ret=='void'else'i32'if ret=='int'else ret),(n,ret,rret)
 # 逐项声明的 C/Rust 映射；不把 i32 返回改成 Result/对象。
def normalize(ts):
 s=''.join(ts).replace('r#priv','priv')
 s=re.sub(r'((?:\(\*\w+\)|\w+)(?:\.\w+)*)\.is_null\(\)',r'!\1',s)
 s=re.sub(r'as(?:usize|u32|i32|u8|u16)','',s)
 s=s.replace('.byte_add','+').replace('->','.').replace('(*','(')
 s=s.replace('(','').replace(')','')
 if s.endswith('!=0'):s=s[:-3]
 elif s.endswith('==0'):s='!'+s[:-3]
 return s
# C 的单语句 if 和 Rust 的带括号块归一；保留条件出现顺序与嵌套层数。
def branches(ts,lang):
 def stmt_end(i):
  if ts[i]=='{':return balanced(ts,i,'{','}')
  if ts[i]=='if':
   e=balanced(ts,i+1);return stmt_end(e)
  while ts[i]!=';':
   if ts[i]=='(':i=balanced(ts,i)
   else:i+=1
  return i+1
 result=[];spans=[]
 for i,t in enumerate(ts):
  if t!='if':continue
  if lang=='c':
   k=balanced(ts,i+1);condition=ts[i+2:k-1];end=stmt_end(k)
  else:
   k=i+1
   while ts[k]!='{':k+=1
   condition=ts[i+1:k];end=balanced(ts,k,'{','}')
  result.append({'condition':normalize(condition),'depth':sum(a<i<b for a,b in spans)})
  spans.append((i,end))
 return result
external={i['name']for i in json.loads((ROOT/'docs/LiteEth/同名接口映射.json').read_text())['interfaces']}
def calls(ts):
 return [t for i,t in enumerate(ts[:-1])if t in external|set(names) and (ts[i+1]=='('or ts[i+1]=='!'and ts[i+2]=='(')]
result=[]
for n in names:
 bc=branches(cb[n][1],'c');br=branches(rb[n][1],'rust')
 assert bc==br,(n,'条件/嵌套',bc,br)
 cc=calls(cb[n][1]);rc=calls(rb[n][1]);assert cc==rc,(n,'依赖调用顺序',cc,rc)
 result.append(dict(name=n,branch_count=len(bc),branches=bc,dependency_calls=cc))
# 经对照 Linux 定义审核后冻结的普通函数签名；宏保留原参数角色，以 Rust 宏表示。
expected=json.loads((ROOT/'tools/liteeth/linux_signatures.json').read_text())
for n,definition in expected.items():
 s=(ROOT/definition['path']).read_text();m=re.search(r'pub\s+(?:unsafe\s+)?fn\s+'+re.escape(n)+r'\s*\(([^()]*)\)\s*(?:->\s*([^\{]+))?\{',s,re.S)
 assert m,(n,'缺少定义')
 params=re.sub(r'\s+','',m[1]).rstrip(',');ret=re.sub(r'\s+','',m[2]or'()')
 assert(params,ret)==(definition['parameters'],definition['return']),(n,'冻结签名不符',params,ret)
macros=external-set(expected)
all_api='\n'.join(p.read_text()for p in (ROOT/'libos/src/linux_api').glob('*.rs'))
for n in macros:assert re.search(r'macro_rules!\s*'+re.escape(n)+r'\b',all_api),(n,'缺少同名宏')
out=ROOT/'target/liteeth-translation-check.json';out.parent.mkdir(exist_ok=True)
out.write_text(json.dumps(dict(passed=True,driver_entries=len(result),ordinary_dependency_definitions=len(expected),macros=len(macros),functions=result,limit='检查签名、if 条件/嵌套与词法调用序列；不是形式化语义证明，生命周期和错误路径另由测试核对。'),ensure_ascii=False,indent=2)+'\n')
print(f'PASS 9 个入口定义、{sum(x["branch_count"]for x in result)} 个 if 条件/嵌套、依赖调用序列、39 个普通接口签名及 10 个宏')
