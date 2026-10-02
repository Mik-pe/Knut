import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import {spawn, spawnSync} from 'node:child_process';
import {createInterface} from 'node:readline';
import {knutBinary} from './binary.mjs';

const binary=knutBinary();
const root=fs.mkdtempSync('/var/tmp/knut-self-update-');
const workspace=path.join(root,'source');
const target=path.join(root,'knut');
fs.mkdirSync(path.join(workspace,'src'),{recursive:true});
fs.copyFileSync(binary,target);fs.chmodSync(target,0o755);
fs.writeFileSync(path.join(workspace,'Cargo.toml'),'[package]\nname="knut"\nversion="0.1.0"\nedition="2024"\n');
fs.writeFileSync(path.join(workspace,'.gitignore'),'.knut-update/\n');
fs.writeFileSync(path.join(workspace,'src/main.rs'),'fn main() {\n    println!("updated fixture");\n}\n');
const lock=spawnSync('cargo',['generate-lockfile','--offline'],{cwd:workspace,encoding:'utf8'});
assert.equal(lock.status,0,lock.stderr);
let calls=0;
const model=http.createServer(async(req,res)=>{
  try {
    let body='';for await(const chunk of req)body+=chunk;
    const request=JSON.parse(body);calls++;
    const results=request.messages.filter(item=>item.role==='tool').map(item=>JSON.parse(item.content));
    const last=results.at(-1);
    let tool, args,content='Update installed. The original session is still running.';
    if(!last) {tool=request.tools.find(item=>item.function.description.includes('Inspect the installed Knut hash'));args={};}
    else if(last.source_revision) {
      tool=request.tools.find(item=>item.function.parameters.properties?.expect_source_revision);
      args={expect_installed_hash:last.installed_hash,expect_source_revision:last.source_revision};
    } else {assert(last.installed_hash && !last.error,JSON.stringify(last));}
    const delta=tool?{tool_calls:[{index:0,id:`call-${calls}`,type:'function',function:{name:tool.function.name,arguments:JSON.stringify(args)}}]}:{content};
    res.setHeader('Content-Type','text/event-stream');
    res.write(`data: ${JSON.stringify({choices:[{index:0,delta}]})}\n\n`);
    res.write(`data: ${JSON.stringify({choices:[{index:0,delta:{},finish_reason:tool?'tool_calls':'stop'}]})}\n\n`);
    res.end('data: [DONE]\n\n');
  }catch(error){res.writeHead(500);res.end(String(error));}
});
await new Promise(resolve=>model.listen(0,'127.0.0.1',resolve));
const config=path.join(root,'config');fs.mkdirSync(config,{mode:0o700});
const child=spawn(target,['jsonl'],{cwd:workspace,env:{...process.env,KNUT_CONFIG_DIR:config,KNUT_UPDATE_TARGET:target,KNUT_PROVIDER:'chat-completions',KNUT_PROVIDER_API_KEY:'fixture-key',KNUT_PROVIDER_MODEL:'fixture',KNUT_PROVIDER_BASE_URL:`http://127.0.0.1:${model.address().port}/v1`,KNUT_PROFILE:'general',KNUT_LOAD_ENV:'0'},stdio:['pipe','pipe','pipe']});
const send=value=>child.stdin.write(JSON.stringify(value)+'\n');
let approvals=0,completed=false,stderr='';
child.stderr.on('data',chunk=>{stderr+=chunk;});
const deadline=setTimeout(()=>child.kill('SIGKILL'),120_000);
const exit=new Promise(resolve=>child.on('exit',(code,signal)=>resolve({code,signal})));
try {
  for await(const line of createInterface({input:child.stdout})) {
    const message=JSON.parse(line),event=message.event;
    if(message.type==='ready')send({type:'submit',prompt:'Inspect and install the current source as an update.'});
    if(!event)continue;
    assert(!['runtime_error','task_failed'].includes(event.kind),JSON.stringify(event));
    if(event.kind==='waiting_for_user') {
      assert(event.wait.approval);approvals++;
      assert.equal(spawnSync(target,['--help'],{encoding:'utf8'}).status,0,'Original executable is active before approval');
      send({type:'approve',approval_key:event.wait.approval.approval_key});
    }
    if(event.kind==='tool_call_failed')throw new Error(JSON.stringify(event));
    if(event.kind==='task_completed') {completed=true;send({type:'close'});child.stdin.end();}
  }
  const result=await exit;assert.equal(result.code,0,JSON.stringify(result)+stderr);
  assert(completed);assert.equal(approvals,1);assert.equal(calls,3);
  const updated=spawnSync(target,[],{encoding:'utf8'});assert.equal(updated.status,0);assert.equal(updated.stdout.trim(),'updated fixture');
  assert.equal(fs.readFileSync(`${target}.previous`).compare(fs.readFileSync(binary)),0);
  console.log(JSON.stringify({root,approvals,checks:'sandboxed offline format/test/clippy/release build',atomicUpdate:'passed',residentSession:'passed',rollbackBinary:'passed'}));
}finally {
  clearTimeout(deadline);if(child.exitCode===null)child.kill('SIGTERM');model.close();model.closeAllConnections();
}
