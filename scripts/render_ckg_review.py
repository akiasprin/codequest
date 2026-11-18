#!/usr/bin/env python3
"""Build a self-contained review from recorded evidence, never fabricated scores."""
import argparse
import json
from pathlib import Path
import re
import os

ROOT=Path(__file__).resolve().parent.parent
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--evidence',type=Path,required=True)
p.add_argument('--output',type=Path,required=True)
a=p.parse_args()
def read(path,default=None):
    file=a.evidence/path
    return json.loads(file.read_text()) if file.exists() else default
manifest=json.loads((ROOT/'fixtures/ckg-parity.json').read_text())
inventory=read('upstream-inventory.json')
# Classification is an index into reviewed scenario groups, not a claim that
# every assertion/decision point in that category has an equivalent test.
for c in inventory['cases']:
    name=Path(c['file']).name
    groups=[g['id'] for g in manifest['groups'] if any(x in name for x in g['patterns'])]
    if c['language']=='other-language' or any(x in name for x in ['ttsc','ensure_pub','viewer_','view_command','release_preflight','workflows_','readme_']):groups=['outside']
    c['review_groups']=groups or ['unassigned']
    c['assertion_equivalence']='not established individually; consult mapped scenarios and explicit gaps'
coverage=read('coverage-final/summary.json',read('coverage-combined/summary.json'))
gate=read('full-gate/gate.json',{})
unitlog=a.evidence/'full-gate/00.log'
unit_tests=re.findall(r'^test (\S+) \.\.\. ok$',unitlog.read_text() if unitlog.exists() else '',re.M)
unit_ignored=len(re.findall(r'^test (\S+) \.\.\. ignored',unitlog.read_text() if unitlog.exists() else '',re.M))
structures=read('full-gate/structures/results.json',read('complete/results.json',{}))
real=read('full-gate/real/results.json',{})
acceptance=read('full-gate/acceptance/acceptance.json',{})
foundations=read('full-gate/foundations/results.json',{})
cases=foundations.get('cases',[])+structures.get('cases',[])+real.get('cases',[])+[{'test':n,'state':'passed'} for n in unit_tests]
for g in manifest['groups']:
    g['matched_tests']=[c for c in cases if any(c['test'].endswith(t) or (t.endswith(('.', '::')) and t in c['test']) for t in g['tests'])]
assertion_map=json.loads((ROOT/'fixtures/ckg-foundation-assertions.json').read_text())
assertion_labels={r['label']:r for r in assertion_map['rows']}
assertion_rows=[]
for case in inventory['cases']:
    if Path(case['file']).name not in assertion_map['scope']: continue
    for entry in case['assertions']+case.get('helper_invocations',[]):
        mapped=assertion_labels.get(entry['label'],{})
        matches=[t['test'] for t in cases if t.get('state')=='passed' and any(t['test'].endswith(test) for test in mapped.get('tests',[]))]
        state=mapped.get('state','pending')
        if state.endswith('_verified') and not all(any(t['test'].endswith(test) and t.get('state')=='passed' for t in cases) for test in mapped.get('tests',[])): state='pending_evidence'
        if entry['label']=='label': state='helper_template'
        assertion_rows.append(dict(entry,file=case['file'],state=state,tests=mapped.get('tests',[]),passed_tests=matches,boundary=mapped.get('boundary','尚未建立独立等价断言；不能由相邻场景推算。')))
performance=[]
for name in ['ripgrep','gin','leveldb','cjson']:
    index=read(f'full-gate/real/{name}/index.json',read(f'coverage-final/real/{name}/index.json'))
    if index:
        summary=json.loads(index['stdout']) if index['exit']==0 else {}
        times=[]
        queries=a.evidence/f'full-gate/real/{name}/queries.jsonl'
        if queries.exists():
            times=sorted(json.loads(line)['seconds']*1000 for line in queries.read_text().splitlines() if json.loads(line)['request']['method']=='tools/call')
        performance.append({'name':name,'seconds':index['seconds'],'exit':index['exit'],'summary':summary,
            'query_smoke_ms':{'count':len(times),'p50':times[len(times)//2] if times else None,'p95':times[min(len(times)-1,int(.95*len(times)))] if times else None}})
data={'manifest':manifest,'inventory':inventory,'go_inventory':read('go-inventory.json',[]),
      'coverage':coverage['data'][0] if coverage else {},'gate':gate,'structures':structures,'real':real,
      'resources':read('resources.json',{}),'foundations':foundations,'foundation_assertions':assertion_rows,'acceptance_checks':len(acceptance.get('checks',[])),'workspace_passed':len(unit_tests),'workspace_ignored':unit_ignored,
      'reference':read('ckg-reference-tests.json',{}),'native':read('native-comparison.json',{}),'performance':performance}
# Save a machine-readable matrix with explicit unassigned assertions.
(a.evidence/'comparison-matrix.json').write_text(json.dumps(data,ensure_ascii=False,indent=2))
body=r'''<!doctype html><html lang="zh-Hans"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>CodeQuest · CKG 测试与编译语义对齐</title><style>
:root{font-family:system-ui,-apple-system,"Noto Sans CJK SC",sans-serif;color:#17352f;background:#f2f6f3;line-height:1.65}*{box-sizing:border-box}body{margin:0}main{max-width:1320px;margin:auto;padding:42px 26px 100px}h1{font-size:clamp(25px,4vw,42px);line-height:1.22;margin:14px 0}h2{font-size:23px;margin-top:44px}h3{font-size:17px}p{max-width:960px}.eyebrow{font-size:13px;letter-spacing:.12em;color:#51766a}.lead{font-size:19px;color:#37594d}.notice{border-left:4px solid #b67420;background:#fff7e8;padding:15px 20px}.metrics{display:grid;grid-template-columns:repeat(auto-fit,minmax(170px,1fr));gap:12px;margin:24px 0}.metric,.card{padding:19px 21px;background:white;border:1px solid #d9e5dd;border-radius:9px}.number{font-size:29px;font-weight:700;display:block}.muted,small{color:#587367;font-size:13px}.pill{padding:3px 9px;border-radius:12px;background:#e4eee8;white-space:nowrap;font-size:12px}.partial,.different{background:#fff1cf}.not_implemented{background:#fbe3dd;color:#8c3223}.verified_cases{background:#d9eee0}.outside_scope{background:#ebedef}.controls{display:flex;gap:10px;flex-wrap:wrap;margin:18px 0;align-items:center}input,select,button{font:inherit;padding:8px 12px;border:1px solid #b4cbbd;border-radius:5px;background:#fff;color:#193e30}input{min-width:230px;flex:1}button{cursor:pointer}button:hover{background:#e7f0e9}table{width:100%;border-collapse:collapse;background:#fff;font-size:14px}th{text-align:left;background:#e6eee8;color:#264c3c}td,th{border-bottom:1px solid #dae6dd;padding:14px;vertical-align:top}td:first-child{min-width:165px}td li{margin-bottom:5px}ul{padding-left:19px;margin:7px 0}.tablewrap{overflow-x:auto;border:1px solid #d9e5dd;border-radius:8px}code,pre{font-family:ui-monospace,monospace;font-size:12px}pre{white-space:pre-wrap;overflow-wrap:anywhere;background:#f4f7f4;padding:12px;border-radius:5px}details{padding:12px 15px;background:white;border:1px solid #dce6df;border-radius:7px;margin:10px 0}summary{cursor:pointer;font-weight:600;overflow-wrap:anywhere}.twocol{display:grid;grid-template-columns:1fr 1fr;gap:16px}.branch{border-left:2px solid #87ad97;padding:8px 12px;margin:8px 0}a{color:#126c50}footer{margin-top:50px;border-top:1px solid #cdded1;padding-top:20px}.state{font-size:12px}.bad{color:#a23c2b}.good{color:#166940}.bar{height:7px;background:#e2ebe4;border-radius:8px;overflow:hidden}.bar span{height:100%;display:block;background:#399b70}@media(max-width:760px){main{padding:25px 15px}.twocol{grid-template-columns:1fr}td,th{padding:10px}.number{font-size:24px}}
</style><main><div class="eyebrow">CODEQUEST / COMPILE-DRIVEN UNDERSTANDING / REVIEW</div>
<h1>测试通过了哪些理解能力，<br>哪些仍没有对齐 CKG？</h1>
<p class="lead">沿真实构建 → 原生事实 → 通用模型 → 公开查询逐段检查。正确目标必须存在，同名错误目标必须缺席；执行、声明、候选和延迟 body 分开保留。</p>
<p class="notice"><b>结论：编译驱动的方向成立，转换层和查询边界确实有缺陷，已有复现与修复。但还没有达到逐断言、逐分支的一比一完整复刻。</b>下面将已验场景、设计差异、缺失能力和未完成核对分开列明。源码清单的完整枚举不是测试完成率。</p>
<div class="metrics" id="metrics"></div>
<div id="gate"></div>
<h2>本次修复与证据</h2><div class="twocol">
<div class="card"><h3>语言结构不能在归一化时丢失</h3><p>修复 Rust associated function 分类、跨 impl 类型成员、nested fn 归属；Go receiver/field owner、Alias 与泛型 Origin；C/C++ union/enum variant、qualified name 和重载身份。</p><p class="muted">声明容器保持在 declaring_scope；HasMember 与 LexicallyContains 独立存在，各有证据强度。</p></div>
<div class="card"><h3>结果筛选不能破坏起点身份</h3><p>成员 kind/file 只筛选返回结果；引用 file 按 occurrence 发生位置判断。同字节的不同编译实例按 context 隔离。</p></div>
<div class="card"><h3>宏与接口不能被压成一条确定调用</h3><p>cJSON 暴露宏前缀跨文件范围错误；LevelDB 暴露同一展开位置的多次调用参数冲突。修复 expansion 范围、原生 call_index 关联和含参数的调用身份；保留接口/虚调用的未知 executable。</p></div>
<div class="card"><h3>追踪必须在高扇出节点内部暂停</h3><p>扫描边也计入预算，保存 edge_offset；1,500 个邻居续查不丢失、不重复。候选实现默认收起并留提示；types focus 不混入执行边。</p></div></div>
<h2>本轮新增的边界修复</h2><div class="twocol"><div class="card"><h3>测试变体对应哪次真实编译？</h3><p>Go 的生产包和内部测试包可以具有相同的编译器包名。现在同时比较包名及精确源码集合，绑定内部、外部测试实例。go test 没有单独编译的生产实例明确保留 UnattributedArtifact / Partial。</p></div><div class="card"><h3>原生键不等于永远稳定</h3><p>函数内部声明、匿名属性及冲突的原生声明键使用代内句柄。有效声明仍保留稳定句柄；同一宏展开位置中不同 spelling 的读取各有 occurrence ID。</p></div><div class="card"><h3>输入预算和图输出分开计算</h3><p>此前的 ContentLimit 来自共享累计预算，不能归因于某个分片超限。每次分析拥有独立输入预算，派生 checkpoint 单独计量：每分片 1 GiB、每次发布 16 GiB。无源码位置元数据按编译单元拆分，发布避免两次额外全图复制。</p></div><div class="card"><h3>存储角色</h3><p>Ladybug 保存可重建的图投影；通用模型快照、冻结源码、编译证据和 checkpoint 由 store 模块写入本地文件及内容寻址 blob。下方的 LevelDB 是被分析的 C++ 源码语料，未被用作 CodeQuest 的第二套数据库。</p></div></div>
<h2>实现与测试对比矩阵</h2><p class="muted">“已验场景”仅表示该行列出的具体断言通过，不表示对应的所有上游断言均已移植。展开可看路径及用例。</p>
<div class="controls"><input id="matrix-search" placeholder="查 Rust / 宏 / 身份 / 预算 / …"><select id="matrix-state"><option value="">全部状态</option><option value="gaps">差异、部分及缺失</option><option value="verified_cases">已验场景</option><option value="not_implemented">尚未实现</option></select></div>
<div class="tablewrap"><table><thead><tr><th>核对环节</th><th>逐项检查的处理情况</th><th>实际差异 / 剩余缺口</th><th>验证与源码</th></tr></thead><tbody id="matrix"></tbody></table></div>
<h2>本轮基础能力与逐断言记录</h2><div class="metrics" id="foundation-metrics"></div><p>Go 的生产、内部、外部测试编译实例；C++ 的声明、定义、拼写、展开位置；三种语言的跨代声明句柄；full/no-op/delta/checkpoint，以及 Rust 常驻 HIR 和 C++ TU 缓存都分别保留验证。Go 另有 22 个 go/types 签名分支子例。</p><p class="notice">图分片 delta 说明哪些公共事实改变，不证明只重算了这些分片。Rust 的 database_reused 来自常驻 Salsa 引擎；C++ native_reused 来自已重验输入的 TU 缓存；Go 仍整包重建。原生完整导出和 Ladybug 投影仍有全量成本。</p><p id="assertion-count"></p><div class="controls"><input id="assertion-search" placeholder="查上游断言、测试名或缺口"><select id="assertion-state"><option value="">全部核对状态</option><option value="native_verified">真实编译已验</option><option value="contract_verified">共同契约已验</option><option value="pending">待核对</option><option value="different">明确差异</option></select><button id="assertion-more">再显示 15 项</button></div><div id="assertion-list"></div>
<h2>同一源码的原生差分</h2><p>Rust 使用固定 fork 的 HIR producer；Go 使用真实 sidecar + scip-go 0.2.7。来源检查拒绝 static/LSP fallback。CKG 原生 dump 沿用上轮相同固定夹具的记录，CodeQuest 数据来自本轮重新编译。C/C++ 没有运行 CKG 的定制 clangd，只有源码及模拟协议测试可比，不能声称完成原生 C++ 差分。</p>
<div class="tablewrap"><table><thead><tr><th>独立不变量</th><th>CKG 实际结果</th><th>CodeQuest 实际结果</th></tr></thead><tbody id="native"></tbody></table></div>
<p class="muted">closure/async 一项为关系粒度差异：CKG 将其内部引用归于外层命名函数；CodeQuest 使用独立 Deferred Body。这个比较不把任一静态 Calls 边宣称成运行轨迹，也不计算全图查全率/查准率。</p>
<h2>真实仓库与性能范围</h2><div class="tablewrap"><table><thead><tr><th>语料</th><th>真实构建范围</th><th>本次索引耗时</th><th>结果规模 / 查询样本</th></tr></thead><tbody id="perf"></tbody></table></div>
<p class="muted">精确 commit、命令与依赖范围见 <a href="../codequest/fixtures/real-corpus.lock.json">corpus lock</a>。单次本机测量，缓存已存在，机器有并发任务；它说明本次验收成本，不是冷启动/并发吞吐/百万行精确基准。查询样本混合首次与后续查询、不同操作及分页，分位数只供回归定位，不是稳定 SLA。Go/C++ 仍需额外索引外部头文件或包声明；按 TU 保留实例会放大图，不能直接与 CKG 跨 TU 合并后的节点数比较。</p>
<h3>本轮内存采样</h3><div id="resources"></div><p class="notice">大型 C++ 图仍有明显的全量成本。编译单元实例、外部头文件和长原生键会放大数据；增量查询协议与原生缓存不能消除完整导出、快照校验和 Ladybug 投影的成本。这里不承诺低内存或全仓库性能。</p>
<h2>运行覆盖率：仍未执行的分支不能算通过</h2><div class="metrics" id="coverage"></div><p><a href="validation/structure-tests/coverage-final/html/index.html">打开逐文件行与分支覆盖</a> · <a href="validation/structure-tests/coverage-final/summary.json">原始 LLVM 汇总</a></p><p class="muted">插桩范围为 workspace Rust（含真实 CLI/MCP/recorder/clang）；实际执行的单元、受控编译夹具及是否纳入真实仓库，以 coverage-final/scope.json 为准。真实仓库的 release 验收另行列出。排除外部 rust-analyzer、Go exporter、LLVM/libclang 内部。分支插桩使用所选 rustc 的实验性选项；RUSTC_BOOTSTRAP 只作用于测试子进程。覆盖率描述执行范围，不等于语义正确性。</p>
<h2>上游完整清单：每个未核对项继续可见</h2><p id="inventory-count"></p><p class="notice">这里保留所有测试和 AST 决策点。分类关联到上面的场景矩阵；<b>逐断言等价性仍未建立，不能显示为“全部通过”。</b>TypeScript 清单是语法枚举，不能代替 native Go/Rust/C++ 的运行分支覆盖。</p>
<div class="controls"><input id="inventory-search" placeholder="按测试名、断言文字或文件查找"><select id="inventory-language"><option value="">全部语言与共同路径</option><option>rust</option><option>go</option><option>c/c++</option><option>shared</option><option>other-language</option></select><button id="inventory-more">再显示 12 项</button></div><div id="inventory"></div>
<details id="sources"><summary>展开 TypeScript 实现的决策点目录（尚未逐分支建立等价映射）</summary><div class="controls"><input id="source-search" placeholder="按文件或条件查找"><button id="source-more">再显示 8 个文件</button></div><div id="source-list"></div></details>
<details id="go-sources"><summary>展开 native Go 实现与上游测试的 AST 清单</summary><div id="go-list"></div></details>
<h2>上游原测试的单独执行</h2><div id="reference"></div><p class="muted">原测试断言经 TypeScript 5.9.3 转译，在固定版本构建的上游库运行；仅将模块路径调整为可解析形式，没有更改断言。适配器测试包含模拟 producer，Rust LSP 测试包含 fake LSP；它们证明上游契约行为，不算 CodeQuest 的通过数，也不冒充真实编译器结果。Go sidecar 的原测试集另已执行通过。</p>
<footer><b>复跑与原始记录</b><p><a href="../codequest/README.md">运行入口</a> · <a href="../codequest/fixtures/ckg-parity.json">场景差异登记</a> · <a href="validation/structure-tests/comparison-matrix.json">完整矩阵 JSON</a> · <a href="validation/structure-tests/upstream-inventory.json">上游测试与决策点清单</a> · <a href="validation/structure-tests/full-gate/gate.json">最终 gate 与源码指纹</a></p><p class="muted">没有改动 Claude 的提案或分层 demo。固定 CKG commit：<span id="commit"></span>。作者完整设计意图仍需要把这些事实组织成可质疑的功能解释；图和测试本身不构成作者动机的直接证明。</p></footer>
</main><script id="data" type="application/json">__DATA__</script><script>
const D=JSON.parse(document.getElementById('data').textContent),$=id=>document.getElementById(id),esc=s=>String(s??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const labels={verified_cases:'已验列明场景',partial:'部分 / 未全覆盖',different:'明确差异',not_implemented:'未实现',outside_scope:'阶段范围外'};
const metric=(n,t)=>`<div class="metric"><span class="number">${esc(n)}</span>${esc(t)}</div>`;
$('foundation-metrics').innerHTML=metric(D.foundations.tests??'待完成','基础能力真实编译用例')+metric(D.foundation_assertions.filter(r=>r.state==='native_verified'||r.state==='contract_verified').length,'选中断言的明确对应项')+metric(D.foundation_assertions.filter(r=>r.state.startsWith('pending')).length,'选中断言仍待核对项');
$('metrics').innerHTML=metric(D.structures.tests??'待完成','受控编译语义用例')+metric(D.real.tests??'待完成','真实仓库语义用例')+metric(D.workspace_passed,'workspace 测试通过')+metric(D.inventory.totals.test_files,'上游测试文件（清单）');
$('gate').innerHTML=`<p class="${D.gate.passed?'good':'bad'}"><b>最终 native gate：${D.gate.passed?'全部命令通过':'尚未完成或失败，请看 gate.json'}</b></p><p class="muted">workspace 测试列表另有 ${D.workspace_ignored??0} 个子进程辅助入口标记 ignored，由恢复测试单独调用。CLI/MCP 另执行 ${D.acceptance_checks} 项检查，其中含协议响应和作业轮询，不算独立语义能力数。</p>`;
$('commit').textContent=D.manifest.upstream.commit;
function sourceLink(p,ours){if(ours)return `<a href="../codequest/${esc(p)}">${esc(p)}</a>`;let repo=D.manifest.upstream.repository,commit=D.manifest.upstream.commit,clean=p;if(p.includes('(Rust producer)')){repo='https://github.com/samchon/rust-analyzer';commit=D.manifest.upstream.rust_producer_commit;clean=p.replace(' (Rust producer)','')}if(p.includes('(LLVM producer)')){repo='https://github.com/samchon/llvm-project';commit=D.manifest.upstream.cpp_producer_commit;clean=p.replace(' (LLVM producer)','')}return `<a href="${repo}/blob/${commit}/${esc(clean)}">${esc(p)}</a>`}
function matrix(){let query=$('matrix-search').value.toLowerCase(),state=$('matrix-state').value;let groups=D.manifest.groups.filter(g=>(!query||JSON.stringify(g).toLowerCase().includes(query))&&(!state||(state==='gaps'?['partial','different','not_implemented'].includes(g.state):g.state===state)));$('matrix').innerHTML=groups.map(g=>`<tr><td><b>${esc(g.title)}</b><p><span class="pill ${g.state}">${labels[g.state]}</span></p></td><td><ul>${g.branches.map(b=>`<li>${esc(b)}</li>`).join('')}</ul></td><td>${esc(g.difference)}</td><td><details><summary>${g.matched_tests.length} 个匹配通过用例 / 来源</summary>${g.matched_tests.map(t=>`<pre>${esc(t.test)}</pre>`).join('')}<p class="muted">${g.tests.length?'只对应列明的用例，未匹配或未逐断言验证的内容不能据此算通过。':'没有为本组登记独立语义测试。'}</p><b>CKG</b><p>${g.upstream.map(p=>sourceLink(p,false)).join('<br>')}</p><b>CodeQuest</b><p>${g.ours.map(p=>sourceLink(p,true)).join('<br>')}</p></details></td></tr>`).join('')}
$('matrix-search').oninput=matrix;$('matrix-state').onchange=matrix;matrix();
$('native').innerHTML=(D.native.results||[]).map(r=>`<tr><td>${esc(r.language)} · ${esc(r.invariant)}</td><td class="${r.ckg?'good':'bad'}">${r.ckg?'满足':'关系粒度不同 / 不满足此不变量'}</td><td class="${r.codequest?'good':'bad'}">${r.codequest?'满足':'不满足'}</td></tr>`).join('');
const scopes={ripgrep:'grep-printer --lib（JSON 输出相关库）',gin:'go build -a ./...（生产包）',leveldb:'CMake 实际 compdb 中 6 个 TU',cjson:'CMake 实际 compdb 中 2 个 TU'};
$('perf').innerHTML=D.performance.map(p=>`<tr><td>${esc(p.name)}</td><td>${scopes[p.name]}</td><td>${p.seconds.toFixed(1)} 秒</td><td>${p.summary.entities??'?'} 实体 / ${p.summary.relations??'?'} 关系<br><small>${p.summary.unresolved_calls??'?'} 未决调用 · ${p.summary.coverage_gaps??'?'} 缺口条目</small><p>查询 ${p.query_smoke_ms.count} 次，p50 ${p.query_smoke_ms.p50?.toFixed(1)} ms / p95 ${p.query_smoke_ms.p95?.toFixed(1)} ms</p><details><summary>原始规模</summary><pre>${esc(JSON.stringify(p.summary,null,2))}</pre></details></td></tr>`).join('');
$('resources').innerHTML=`<div class="tablewrap"><table><thead><tr><th>源码语料</th><th>采样最高单进程 RSS</th><th>采样最高进程树 RSS</th></tr></thead><tbody>${Object.entries(D.resources.corpora||{}).map(([name,r])=>`<tr><td>${esc(name)}${name==='leveldb'?'（被分析的源码）':''}</td><td>${(r.max_process_rss_bytes/1073741824).toFixed(2)} GiB</td><td>${(r.max_tree_rss_bytes/1073741824).toFixed(2)} GiB</td></tr>`).join('')}</tbody></table></div><p class="muted">每 ${D.resources.interval_ms??'?'} ms 采样，可能漏掉短暂峰值；进程树为索引/查询进程及其子进程，不是全机用量。机器有其他并发任务，不能把本次耗时作为冷启动 SLA。</p>`;
const T=D.coverage.totals||{};$('coverage').innerHTML=['lines','branches','functions'].map(k=>{let v=T[k]||{};return `<div class="metric"><span class="number">${(v.percent||0).toFixed(1)}%</span>${({lines:'行',branches:'分支',functions:'函数'})[k]}覆盖 · ${v.covered||0}/${v.count||0}<div class="bar"><span style="width:${v.percent||0}%"></span></div></div>`}).join('');
$('inventory-count').textContent=`固定版本：${D.inventory.totals.test_files} 个测试文件，${D.inventory.totals.assertions} 个 TestValidator 调用，${D.inventory.totals.source_files} 个 TypeScript 实现文件的 ${D.inventory.totals.decision_points} 个 AST 决策点。`;
const assertionStates={native_verified:'真实编译已验',contract_verified:'共同契约已验',pending:'待逐项核对',pending_evidence:'对应测试尚无本轮通过证据',different:'明确设计差异',helper_template:'断言 helper 模板'};
$('assertion-count').textContent=`本轮选中 4 个上游文件，列出 ${D.foundation_assertions.length} 条直接调用及带标签 helper 调用；模板与循环调用点不能当成独立的运行断言数。`;
let assertionShown=15;function assertions(){const q=$('assertion-search').value.toLowerCase(),state=$('assertion-state').value,rows=D.foundation_assertions.filter(r=>(!q||JSON.stringify(r).toLowerCase().includes(q))&&(!state||r.state===state));$('assertion-list').innerHTML=rows.slice(0,assertionShown).map(r=>`<details><summary><span class="pill ${r.state.endsWith('_verified')?'verified_cases':'partial'}">${esc(assertionStates[r.state])}</span> ${esc(r.label)} · L${r.line}</summary><p>${sourceLink(r.file,false)}</p><p>${esc(r.boundary)}</p><p>本轮通过证据：${r.passed_tests.map(t=>`<code>${esc(t)}</code>`).join('<br>')||'尚无'}</p><pre>${esc(r.code)}</pre></details>`).join('')+`<p class="muted">显示 ${Math.min(assertionShown,rows.length)} / ${rows.length} 条</p>`;}
$('assertion-search').oninput=$('assertion-state').onchange=()=>{assertionShown=15;assertions()};$('assertion-more').onclick=()=>{assertionShown+=15;assertions()};assertions();
let shown=12;function inventory(){let q=$('inventory-search').value.toLowerCase(),language=$('inventory-language').value;let all=D.inventory.cases.filter(c=>(!language||c.language===language)&&(!q||JSON.stringify(c).toLowerCase().includes(q)));$('inventory').innerHTML=all.slice(0,shown).map((c,i)=>`<details data-case="${D.inventory.cases.indexOf(c)}"><summary>${esc(c.file.split('/').at(-1))} · ${c.assertions.length} 断言</summary><div class="case-body"></div></details>`).join('')+`<p class="muted">显示 ${Math.min(shown,all.length)} / ${all.length}；收起不会改变核对状态。</p>`;$('inventory').querySelectorAll('details').forEach(el=>el.ontoggle=()=>{if(el.open&&!el.dataset.loaded){el.dataset.loaded='1';let c=D.inventory.cases[+el.dataset.case];el.querySelector('.case-body').innerHTML=`<p>${esc(c.lane)}</p><p>关联场景：${esc(c.review_groups.join(', '))}。逐断言等价性：<b>尚未建立</b>。</p><p>${sourceLink(c.file,false)}</p>${[...c.assertions,...(c.helper_invocations||[])].map(a=>`<div class="branch"><b>L${a.line} · ${esc(a.label)}</b><details><summary>原断言</summary><pre>${esc(a.code)}</pre></details></div>`).join('')}`}})}
$('inventory-more').onclick=()=>{shown+=12;inventory()};$('inventory-search').oninput=$('inventory-language').onchange=()=>{shown=12;inventory()};inventory();
let sourceShown=8;function sources(){let q=$('source-search').value.toLowerCase(),files=D.inventory.sources.filter(s=>!q||JSON.stringify(s).toLowerCase().includes(q));$('source-list').innerHTML=files.slice(0,sourceShown).map(s=>`<details><summary>${esc(s.file)} · ${s.decisions.length} 决策点</summary><p>${sourceLink(s.file,false)}</p><p class="muted">静态枚举，逐项映射尚未建立，不能计为运行覆盖。</p>${s.decisions.map(b=>`<div class="branch">L${b.line} · ${esc(b.kind)}<pre>${esc(b.code)}</pre></div>`).join('')}</details>`).join('')+`<p class="muted">显示 ${Math.min(sourceShown,files.length)} / ${files.length} 文件</p>`}
$('sources').ontoggle=()=>{if($('sources').open)sources()};$('source-search').oninput=()=>{sourceShown=8;sources()};$('source-more').onclick=()=>{sourceShown+=8;sources()};
$('go-sources').ontoggle=()=>{if($('go-sources').open&&!$('go-list').childElementCount)$('go-list').innerHTML=D.go_inventory.map(s=>`<details><summary>${esc(s.file.split('/').slice(-3).join('/'))} · ${s.points.length} 条 AST 记录</summary><p class="muted">原测试断言与处理条件，未执行项不算通过。</p>${s.points.map(b=>`<div class="branch">${esc(b.function)} L${b.line} · ${esc(b.kind)}<pre>${esc(b.code)}</pre></div>`).join('')}</details>`).join('')};
$('reference').innerHTML=(D.reference.results||[]).map(r=>`<p><span class="pill ${r.state==='passed'?'verified_cases':'not_implemented'}">${esc(r.state)}</span> <code>${esc(r.name)}</code>${r.error?`<details><summary>执行问题</summary><pre>${esc(r.error)}</pre></details>`:''}</p>`).join('');
</script></html>'''
body=body.replace('validation/structure-tests/',os.path.relpath(a.evidence,a.output.parent).replace(os.sep,'/')+'/')
body=body.replace('__DATA__',json.dumps(data,ensure_ascii=False).replace('<','\\u003c'))
a.output.parent.mkdir(parents=True,exist_ok=True);a.output.write_text(body)
print(json.dumps({'output':str(a.output),'bytes':a.output.stat().st_size,'unassigned_files':sum(c['review_groups']==['unassigned'] for c in inventory['cases'])}))
