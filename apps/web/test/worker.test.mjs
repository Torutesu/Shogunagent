import test from 'node:test';
import assert from 'node:assert/strict';
import {readFile,stat} from 'node:fs/promises';
import {resolve,dirname,join,extname} from 'node:path';
import {fileURLToPath} from 'node:url';
import worker, {SECURITY_HEADERS} from '../worker.mjs';

const root=resolve(dirname(fileURLToPath(import.meta.url)),'..');
const dist=join(root,'dist');
const types={'.html':'text/html; charset=utf-8','.md':'text/markdown; charset=utf-8','.txt':'text/plain; charset=utf-8','.css':'text/css','.js':'text/javascript','.mjs':'text/javascript','.svg':'image/svg+xml','.woff2':'font/woff2','.png':'image/png','.webp':'image/webp','.xml':'application/xml'};

// A stand-in for the Workers Static Assets binding with the settings in wrangler.jsonc:
// html_handling "auto-trailing-slash" (a directory path serves its index.html) and
// not_found_handling "404-page" (an unknown path serves 404.html with status 404).
const ASSETS={async fetch(request){
  const url=new URL(request.url);
  let path=join(dist,decodeURIComponent(url.pathname));
  if(!path.startsWith(dist))return new Response('Forbidden',{status:403});
  try{if((await stat(path)).isDirectory())path=join(path,'index.html');const body=await readFile(path);return new Response(request.method==='HEAD'?null:body,{status:200,headers:{'content-type':types[extname(path)]||'application/octet-stream'}})}
  catch{return new Response(await readFile(join(dist,'404.html')),{status:404,headers:{'content-type':types['.html']}})}
}};
const call=(path,init={})=>worker.fetch(new Request('https://shogunagent.com'+path,init),{ASSETS});

test('the worker serves every page the site links to',async()=>{
  for(const path of ['/','/product/','/docs/','/docs/quickstart/','/docs/mcp/','/pricing/','/llms.txt','/llms-full.txt','/index.md','/setup-prompt.txt','/setup-prompt.mjs','/assets/logo-black.svg','/robots.txt','/sitemap.xml']){
    const res=await call(path);
    assert.equal(res.status,200,path);
    for(const [name,value] of Object.entries(SECURITY_HEADERS))assert.equal(res.headers.get(name),value,`${path} ${name}`);
  }
});

test('the quickstart the site and setup prompt point to is served',async()=>{
  const res=await call('/docs/quickstart/');
  assert.equal(res.status,200);
  assert.match(await res.text(),/127\.0\.0\.1:7464/);
});

test('/ negotiates the agent-readable page',async()=>{
  const html=await call('/',{headers:{accept:'text/html'}});
  assert.match(html.headers.get('content-type'),/text\/html/);
  assert.match(html.headers.get('vary'),/Accept/);
  const md=await call('/',{headers:{accept:'text/markdown'}});
  assert.match(md.headers.get('content-type'),/text\/markdown/);
  assert.equal(await md.text(),await readFile(join(dist,'index.md'),'utf8'));
  const agent=await call('/?agent=1');
  assert.match(agent.headers.get('content-type'),/text\/markdown/);
  const other=await call('/product/',{headers:{accept:'text/markdown'}});
  assert.match(other.headers.get('content-type'),/text\/html/,'only / negotiates');
});

test('unknown paths are real 404s and only GET and HEAD are served',async()=>{
  const missing=await call('/no-such-page/');
  assert.equal(missing.status,404);
  assert.match(await missing.text(),/<html/i);
  for(const method of ['POST','PUT','DELETE','PATCH']){
    const res=await call('/',{method});
    assert.equal(res.status,405,method);
    assert.equal(res.headers.get('allow'),'GET, HEAD');
  }
  const head=await call('/docs/quickstart/',{method:'HEAD'});
  assert.equal(head.status,200);
});

test('wrangler.jsonc publishes dist/ with the assets settings the worker relies on, and only the production website domains',async()=>{
  const text=(await readFile(join(root,'wrangler.jsonc'),'utf8')).replace(/^\s*\/\/.*$/gm,'');
  const config=JSON.parse(text);
  assert.equal(config.main,'worker.mjs');
  assert.equal(config.assets.directory,'./dist');
  assert.equal(config.assets.binding,'ASSETS');
  assert.equal(config.assets.not_found_handling,'404-page');
  assert.equal(config.assets.html_handling,'auto-trailing-slash');
  assert.equal(config.assets.run_worker_first,true);
  assert.deepEqual(config.routes,[{pattern:'shogunagent.com',custom_domain:true},{pattern:'www.shogunagent.com',custom_domain:true}]);
  assert.equal(config.workers_dev,false);
  assert.equal(config.preview_urls,false);
  assert.equal(config.route,undefined);
});
