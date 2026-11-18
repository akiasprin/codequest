#!/usr/bin/env node
/** Inventory is not execution coverage: retain every upstream assertion and
 * source decision point, even when CodeQuest has no equivalent test yet. */
import fs from 'node:fs';
import path from 'node:path';
import {execFileSync} from 'node:child_process';
import {createRequire} from 'node:module';
import {createHash} from 'node:crypto';
const args=Object.fromEntries(process.argv.slice(2).reduce((a,x,i,xs)=>i%2?a:[...a,[x.replace(/^--/,''),xs[i+1]]],[]));
if(!args.ckg||!args.typescript||!args.output) throw Error('--ckg /checkout --typescript /module-directory --output /inventory.json required');
const root=path.resolve(args.ckg), ts=createRequire(import.meta.url)(path.resolve(args.typescript));
const git=(...xs)=>execFileSync('git',['-C',root,...xs],{encoding:'utf8',maxBuffer:64*1024*1024});
const commit=git('rev-parse','HEAD').trim();
if(commit!=='1bf9486b6afda13b6a78d309fef910cffaab1f9d') throw Error('unexpected upstream revision');
const paths=git('ls-tree','-r','--name-only','HEAD').trim().split('\n');
const cases=[],sources=[];
const branchKind=node=>ts.isIfStatement(node)?'if':ts.isConditionalExpression(node)?'ternary':ts.isCaseClause(node)?'case':ts.isDefaultClause(node)?'default':ts.isCatchClause(node)?'catch':ts.isForStatement(node)||ts.isForOfStatement(node)||ts.isForInStatement(node)||ts.isWhileStatement(node)||ts.isDoStatement(node)?'loop':ts.isBinaryExpression(node)&&[ts.SyntaxKind.AmpersandAmpersandToken,ts.SyntaxKind.BarBarToken,ts.SyntaxKind.QuestionQuestionToken].includes(node.operatorToken.kind)?'short-circuit':null;
function parse(file,text,test){
 const ast=ts.createSourceFile(file,text,ts.ScriptTarget.Latest,true), assertions=[],helper_invocations=[],decisions=[],helpers=new Set();
 function containsAssertion(node){let found=false;function walk(n){if(ts.isCallExpression(n)&&n.expression.getText(ast).startsWith('TestValidator.'))found=true;ts.forEachChild(n,walk)}walk(node);return found}
 function findHelpers(node){
  if(ts.isFunctionDeclaration(node)&&node.name&&containsAssertion(node))helpers.add(node.name.text);
  if(ts.isVariableDeclaration(node)&&ts.isIdentifier(node.name)&&node.initializer&&(ts.isArrowFunction(node.initializer)||ts.isFunctionExpression(node.initializer))&&containsAssertion(node.initializer))helpers.add(node.name.text);
  ts.forEachChild(node,findHelpers);
 }
 if(test)findHelpers(ast);
 function visit(node){
  const line=ast.getLineAndCharacterOfPosition(node.getStart(ast)).line+1;
  const kind=branchKind(node);
  if(kind) decisions.push({line,kind,code:node.getText(ast).slice(0,400)});
  if(test&&ts.isCallExpression(node)&&node.expression.getText(ast).startsWith('TestValidator.')) {
   const a=node.arguments[0];assertions.push({line,api:node.expression.getText(ast),label:a?(ts.isStringLiteral(a)?a.text:a.getText(ast)):'',code:node.getText(ast).slice(0,1000)});
  }
  if(test&&ts.isCallExpression(node)&&ts.isIdentifier(node.expression)&&helpers.has(node.expression.text)&&node.arguments[0]&&ts.isStringLiteral(node.arguments[0])) {
   helper_invocations.push({line,api:node.expression.text,label:node.arguments[0].text,code:node.getText(ast).slice(0,1500),notice:'helper invocation, not an additional direct TestValidator callsite or proof of execution'});
  }
  ts.forEachChild(node,visit);
 }
 visit(ast);
 const sha256=createHash('sha256').update(text).digest('hex');
 if(test){
  const other=/_(typescript|ttsc|python|ruby|php|dart|lua|java|javac|csharp|kotlin|swift|scala|zig)(_|\.)/i.test(path.basename(file));
  const language=/_rust_/.test(file)?'rust':/_go_/.test(file)?'go':/_(cpp|c)_/.test(file)?'c/c++':other?'other-language':'shared';
  const lane=/fake-[a-z-]*(lsp|graph|scip|toolchain)|fixtureScipIndexer/.test(text)?'contains simulated producer/protocol fixtures':/indexer:\s*["']static|mode:\s*["']static/.test(text)?'contains static/model fixtures':'inspect source; no native-execution proof inferred';
  cases.push({file,sha256,language,lane,scope:other?'outside phase-1 language scope; shared rules may still apply':'requires semantic correspondence review',assertions,helper_invocations,decisions,execution:'not executed by this inventory tool',parity:'unassigned assertions remain an explicit audit backlog'});
 }else sources.push({file,sha256,decisions,execution:'not measured by static inventory'});
}
for(const file of paths){
 const test=file.startsWith('tests/test-graph/src/features/')&&file.endsWith('.ts');
 const source=file.startsWith('packages/graph/src/')&&file.endsWith('.ts')&&!file.includes('/viewer/');
 if(test||source) parse(file,git('show','HEAD:'+file),test);
}
const result={schema_version:1,parser:{name:'typescript',version:ts.version,purpose:'syntax inventory only; not upstream compilation'},repository:'https://github.com/samchon/compiler-knowledge-graph',commit,
 notice:'Complete enumerated inventory, NOT branch coverage or completed one-to-one parity. Simulated, static and native paths must be distinguished. Cases outside the three language families are retained.',
 totals:{test_files:cases.length,assertions:cases.reduce((n,c)=>n+c.assertions.length,0),helper_invocations:cases.reduce((n,c)=>n+c.helper_invocations.length,0),source_files:sources.length,decision_points:sources.reduce((n,c)=>n+c.decisions.length,0)},cases,sources};
fs.mkdirSync(path.dirname(path.resolve(args.output)),{recursive:true});fs.writeFileSync(args.output,JSON.stringify(result,null,2));console.log(result.totals);
