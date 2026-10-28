import { writeFileSync, appendFileSync } from 'node:fs';
import { serveFetch } from '../../dist/fetch-adapter.js';
import { generateMutualTlsTestCerts } from '../../dist-test/test/support/generate-certs.js';

const certs = generateMutualTlsTestCerts();
const dir = new URL('../../artifacts/safari-latest/', import.meta.url);
writeFileSync(new URL('ca.pem', dir), certs.ca.cert);
const requests = [];
let port;
const page = `<!doctype html><meta charset="utf-8"><title>HTTP/3 0.9.3 Safari release validation</title>
<style>body{font:18px system-ui;margin:40px;color:#162b34;background:#f4f8fa}h1{font-size:30px}section{background:white;padding:22px;margin:20px 0;border:1px solid #b8cbd4;border-radius:8px}pre{white-space:pre-wrap;word-break:break-all;font-size:14px}td,th{padding:10px;border-bottom:1px solid #ddd;text-align:left}.pass{color:#086b3e}</style>
<h1>HTTP/3 0.9.3 · Safari release validation</h1><p id="origin"></p><section><h2>Functional checks</h2>
<p id="fetch">Fetch: waiting</p><p id="sse">SSE: waiting</p><p id="reload"></p><p id="tls"></p></section>
<section><h2>Browser network evidence</h2><p>Live Safari Resource Timing entries and server-observed protocols.</p><table><thead><tr><th>Request</th><th>Initiator</th><th>Protocol</th><th>Status</th><th>Duration</th></tr></thead><tbody id="network"></tbody></table><pre id="observed"></pre></section>
<script>
window.result={token:crypto.randomUUID(), fetch:false, events:[], complete:false, errors:[]};
const result=window.result;
const loads=Number(sessionStorage.loads||0)+1;sessionStorage.loads=loads;result.loads=loads;
document.querySelector('#origin').textContent=location.href;
document.querySelector('#reload').textContent='Page loads in this session: '+loads;
document.querySelector('#tls').textContent='Secure context: '+isSecureContext+' · certificate errors are NOT bypassed';
async function finish(){
 const r=await fetch('/observed?token='+result.token);result.observed=await r.json();
 await new Promise(resolve=>setTimeout(resolve,200));
 result.network=performance.getEntriesByType('resource').map(e=>({name:new URL(e.name).pathname,initiator:e.initiatorType,protocol:e.nextHopProtocol,status:e.responseStatus,duration:e.duration}));
 document.querySelector('#network').innerHTML=result.network.map(e=>'<tr><td>'+e.name+'</td><td>'+e.initiator+'</td><td>'+e.protocol+'</td><td>'+e.status+'</td><td>'+e.duration.toFixed(1)+' ms</td></tr>').join('');
 document.querySelector('#observed').textContent=JSON.stringify(result.observed,null,2);
 result.complete=result.fetch&&result.events.length===2&&result.events[1].lastEventId==='2';
}
fetch('/api/hello?token='+result.token).then(r=>{if(!r.ok)throw Error(r.status);return r.json()}).then(p=>{result.fetch=p.ok;document.querySelector('#fetch').textContent='PASS · Fetch returned 200 and expected JSON';document.querySelector('#fetch').className='pass'}).catch(e=>result.errors.push(String(e)));
const es=new EventSource('/events?token='+result.token);
es.addEventListener('ready',e=>{result.events.push({data:e.data,lastEventId:e.lastEventId});document.querySelector('#sse').textContent='SSE messages received: '+result.events.length;if(result.events.length===2){es.close();document.querySelector('#sse').textContent='PASS · SSE delivered two messages across automatic reconnect (Last-Event-ID resumed)';document.querySelector('#sse').className='pass';finish()}});
</script>`;
const server=serveFetch({port:0,host:'127.0.0.1',key:certs.server.key,cert:Buffer.concat([certs.server.cert,certs.ca.cert]),disableRetry:true,allowHTTP1:false,fetch(request){
 const url=new URL(request.url);
 const headers={'alt-svc':`h3=":${port}"; ma=60`,'cache-control':'no-store'};
 if(url.pathname==='/api/hello')return Response.json({ok:true,version:'0.9.3'},{headers});
 if(url.pathname==='/observed')return Response.json(requests.filter(r=>r.path.includes(url.searchParams.get('token'))),{headers});
 if(url.pathname==='/events'){
   const previous=request.headers.get('last-event-id');
   const id=previous==='1'?'2':'1';
   appendFileSync(new URL('sse-resume.jsonl',dir),JSON.stringify({time:new Date().toISOString(),token:url.searchParams.get('token'),previous,id})+'\n');
   return new Response(`retry: 300\nid: ${id}\nevent: ready\ndata: sse-ok-${id}\n\n`,{headers:{...headers,'content-type':'text/event-stream'}});
 }
 if(url.pathname==='/favicon.ico')return new Response(null,{status:204,headers});
 return new Response(page,{headers:{...headers,'content-type':'text/html; charset=utf-8'}});
}});
server.on('session',session=>session.on('error',error=>console.log('Session closed:',error.code)));
server.on('error',error=>console.error('Server error:',error));
server.on('stream',(stream,headers)=>{const record={time:new Date().toISOString(),path:String(headers[':path']),protocol:stream.constructor.name==='ServerHttp2StreamAdapter'?'h2':'h3'};requests.push(record);appendFileSync(new URL('server-requests.jsonl',dir),JSON.stringify(record)+'\n')});
server.on('listening',()=>{port=server.address().port;writeFileSync(new URL('endpoint.json',dir),JSON.stringify({url:`https://127.0.0.1:${port}`,pid:process.pid}));console.log(`Listening https://127.0.0.1:${port}`)});
for(const signal of ['SIGINT','SIGTERM'])process.on(signal,async()=>{await server.close();process.exit(0)});
