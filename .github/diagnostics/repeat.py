import json, os, pathlib, signal, subprocess, time
out=pathlib.Path('diagnostics');out.mkdir(exist_ok=True)
results=[]
for i in range(30):
 start=time.monotonic()
 with (out/f'concurrency-{i}.log').open('w') as log:
  p=subprocess.Popen(['node','scripts/concurrency-gate.mjs'],stdout=log,stderr=subprocess.STDOUT,start_new_session=True,env={**os.environ,'NODE_OPTIONS':'--require='+str(pathlib.Path('.github/diagnostics/native-timing.cjs').resolve()),'HTTP3_CONCURRENCY_MAX_MS':'12000'})
  try: code=p.wait(timeout=45)
  except subprocess.TimeoutExpired:
   os.killpg(p.pid,signal.SIGKILL);p.wait();code=124
 text=(out/f'concurrency-{i}.log').read_text()
 print(f'RUN {i}: exit={code}, seconds={time.monotonic()-start:.3f}',flush=True)
 for line in text.splitlines():
  if any(s in line for s in ['Event loop latency:','gate passed','gate failed','AssertionError','DIAGNOSTICS','YIELD_PROBE']):print(line,flush=True)
 results.append({'run':i,'exit':code,'seconds':time.monotonic()-start})
(out/'summary.json').write_text(json.dumps(results,indent=2))
