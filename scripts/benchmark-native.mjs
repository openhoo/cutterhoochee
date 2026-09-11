#!/usr/bin/env node
import { readFile, writeFile } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import { parseArgs } from 'node:util';

const { values } = parseArgs({ options: {
  endpoint: { type: 'string', default: 'http://127.0.0.1:4459' },
  session: { type: 'string' }, output: { type: 'string' },
  samples: { type: 'string', default: '7' },
  frames: { type: 'string', default: '3,33,303,3033,15003' },
  'scroll-samples': { type: 'string', default: '30' },
  'playback-seconds': { type: 'string', default: '0' },
  'root-pid': { type: 'string' },
  help: { type: 'boolean', default: false },
} });
if (values.help) {
  console.log('benchmark-native --session WEBDRIVER_SESSION [--endpoint http://127.0.0.1:4459] [--output result.json] [--samples 7] [--frames 3,33,303] [--scroll-samples 30] [--playback-seconds 8] [--root-pid DRIVER_PID]\nAttach to a running tauri-driver session with a generated benchmark project already open. This does not edit the document, enter credentials, or invoke models. Rendering creates ordinary managed preview artifacts; scrolling changes only the viewport. Optional playback advances the playhead and then pauses; start from the same frame for before/after comparisons. Linux --root-pid records process-tree CPU and summed RSS (not unique or peak memory). Canvas draw counts are presentation proxies, not proof of unique decoded frames. Output also saves an immutable .plan.json for benchmark-render. Do not interact with the application during measurement.');
  process.exit(0);
}
const endpoint = new URL(values.endpoint);
if (!['http:', 'https:'].includes(endpoint.protocol) || !['127.0.0.1', 'localhost', '[::1]'].includes(endpoint.hostname)) throw new Error('Use a loopback WebDriver endpoint');
if (!values.session || !/^[a-zA-Z0-9-]+$/.test(values.session)) throw new Error('--session is required');
const samples = Number(values.samples), scrollSamples = Number(values['scroll-samples']);
const playbackSeconds = Number(values['playback-seconds']);
const rootPid = values['root-pid'] === undefined ? undefined : Number(values['root-pid']);
if (!Number.isFinite(playbackSeconds) || playbackSeconds < 0 || playbackSeconds > 30 || (rootPid !== undefined && (!Number.isSafeInteger(rootPid) || rootPid <= 1))) throw new Error('Invalid playback duration or process root');
if (!Number.isInteger(samples) || samples < 1 || samples > 100 || !Number.isInteger(scrollSamples) || scrollSamples < 1 || scrollSamples > 300) throw new Error('Invalid sample count');
const requestedFrames = values.frames.split(',').map(Number);
if (requestedFrames.some(n => !Number.isSafeInteger(n) || n < 0)) throw new Error('Invalid frame number');
const base = `/session/${values.session}`;
async function wd(path, data) {
  const response = await fetch(new URL(path, endpoint), { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(data), signal: AbortSignal.timeout(65000) });
  const result = await response.json();
  if (!response.ok) throw new Error(JSON.stringify(result.value));
  return result.value;
}
async function invoke(request) {
  const result = await wd(`${base}/execute/async`, { script: "var done=arguments[arguments.length-1];var start=performance.now();window.__TAURI_INTERNALS__.invoke('editor_call',{request:arguments[0]}).then(value=>done({ok:true,value,ms:performance.now()-start}),error=>done({ok:false,error,ms:performance.now()-start}));", args: [request] });
  if (!result.ok) throw new Error(JSON.stringify(result.error));
  return result;
}
function stats(times) {
  const sorted = [...times].sort((a, b) => a - b);
  return { samplesMs: times, medianMs: sorted[Math.floor(sorted.length / 2)], p95Ms: sorted[Math.ceil(sorted.length * .95) - 1] };
}
async function processTreeStats(pid) {
  if (pid === undefined) return null;
  const rows = execFileSync('ps', ['-eo', 'pid=,ppid='], { encoding: 'utf8', timeout: 5000 }).trim().split('\n').map(row => row.trim().split(/\s+/).map(Number));
  const included = new Set([pid]);
  for (let changed = true; changed;) {
    changed = false;
    for (const [child, parent] of rows) if (included.has(parent) && !included.has(child)) { included.add(child); changed = true; }
  }
  let ticks = 0, residentPages = 0, processes = 0;
  for (const child of included) {
    try {
      const stat = await readFile(`/proc/${child}/stat`, 'utf8');
      const fields = stat.slice(stat.lastIndexOf(')') + 2).split(' ');
      ticks += [11, 12, 13, 14].reduce((sum, index) => sum + Number(fields[index]), 0);
      residentPages += Number(fields[21]);
      processes++;
    } catch (error) {
      if (error.code !== 'ENOENT' && error.code !== 'ESRCH') throw error;
    }
  }
  if (!processes) throw new Error('Benchmark process root is no longer running');
  return { ticks, residentPages, processes };
}
await wd(`${base}/timeouts`, { script: 60000 });
const document = (await invoke({ method: 'project_snapshot', params: {} })).value.data.document;
const plan = (await invoke({ method: 'preview', params: { action: 'plan', revision: document.revision } })).value.data.data;
const frames = [...new Set(requestedFrames.filter(frame => frame < plan.durationFrames))];
if (!frames.length) throw new Error('No requested frames are inside the project');
const firstRequests = [];
for (const frame of frames) firstRequests.push({ frame, ms: (await invoke({ method: 'preview', params: { action: 'render_frame', revision: document.revision, frame } })).ms });
const status = [], snapshot = [], planning = [], repeatedFrames = [], audio = [];
for (let i = 0; i < samples; i++) {
  status.push((await invoke({ method: 'project_status', params: {} })).ms);
  snapshot.push((await invoke({ method: 'project_snapshot', params: {} })).ms);
  planning.push((await invoke({ method: 'preview', params: { action: 'plan', revision: document.revision } })).ms);
  for (const frame of frames) repeatedFrames.push((await invoke({ method: 'preview', params: { action: 'render_frame', revision: document.revision, frame } })).ms);
  audio.push((await invoke({ method: 'preview', params: { action: 'render_audio_window', planHash: plan.planHash, startSample: 0, sampleCount: Math.min(48000, plan.audio.totalSamples) } })).ms);
}
const scroll = await wd(`${base}/execute/async`, { script: `var count=arguments[0],done=arguments[arguments.length-1],scroll=document.querySelector('.timeline-scroll');if(!scroll){done({error:'Timeline is not visible'});return;}var original=scroll.scrollLeft,rows=[],step=0;function tick(){var start=performance.now();scroll.scrollLeft=step%2?Math.min(6000,scroll.scrollWidth-scroll.clientWidth):0;scroll.dispatchEvent(new Event('scroll'));requestAnimationFrame(()=>requestAnimationFrame(()=>{rows.push(performance.now()-start);if(++step<count)tick();else{var result={samplesMs:rows,clipNodes:document.querySelectorAll('.timeline-clip').length,domNodes:document.querySelectorAll('*').length};scroll.scrollLeft=original;scroll.dispatchEvent(new Event('scroll'));done(result);}}));}tick();`, args: [scrollSamples] });
if (scroll.error) throw new Error(scroll.error);
let playback = null;
if (playbackSeconds > 0) {
  const cpuBefore = await processTreeStats(rootPid);
  const started = performance.now();
  playback = await wd(`${base}/execute/async`, {
    script: `var seconds=arguments[0],done=arguments[arguments.length-1],original=CanvasRenderingContext2D.prototype.drawImage,times=[],start=performance.now();
      var play=document.querySelector('button[aria-label="Play"]');if(!play){done({error:'Pause playback before benchmarking'});return;}
      CanvasRenderingContext2D.prototype.drawImage=function(){if(this.canvas.getAttribute('aria-label')==='Video preview')times.push(performance.now());return original.apply(this,arguments);};
      play.click();setTimeout(function(){var pause=document.querySelector('button[aria-label="Pause"]');if(pause)pause.click();CanvasRenderingContext2D.prototype.drawImage=original;
        done({elapsedMs:performance.now()-start,firstCanvasMs:times.length?times[0]-start:null,draws:times.length,intervalsMs:times.slice(1).map((t,i)=>t-times[i]),playhead:Number(document.querySelector('input[aria-label="Preview playhead"]')?.value),quality:document.querySelector('.quality-select')?.textContent});},seconds*1000);`,
    args: [playbackSeconds],
  });
  if (playback.error) throw new Error(playback.error);
  const cpuAfter = await processTreeStats(rootPid);
  if (cpuBefore && cpuAfter) {
    const ticksPerSecond = Number(execFileSync('getconf', ['CLK_TCK'], { encoding: 'utf8' }).trim());
    const pageBytes = Number(execFileSync('getconf', ['PAGESIZE'], { encoding: 'utf8' }).trim());
    playback.processTreeCpuSeconds = (cpuAfter.ticks - cpuBefore.ticks) / ticksPerSecond;
    playback.averageCpuCores = playback.processTreeCpuSeconds / ((performance.now() - started) / 1000);
    playback.residentSumBytes = cpuAfter.residentPages * pageBytes;
    playback.residentSumNote = 'Sum of live descendant RSS, shared pages may be counted more than once; not peak or unique memory.';
  }
}
const after = (await invoke({ method: 'project_snapshot', params: {} })).value.data.document;
if (JSON.stringify(document) !== JSON.stringify(after)) throw new Error('Project changed during benchmark; discard measurements');
const result = { benchmark: 'native-desktop', clipCount: document.clips.length, captionCount: document.textItems.length, revision: document.revision, planHash: plan.planHash, width: plan.width, height: plan.height, status: stats(status), snapshot: stats(snapshot), planning: stats(planning), firstFrameRequests: firstRequests, repeatedFrames: stats(repeatedFrames), audioWindow: stats(audio), scroll: { ...stats(scroll.samplesMs), clipNodes: scroll.clipNodes, domNodes: scroll.domNodes }, unchanged: true };
result.playback = playback;
if (values.output) {
  await writeFile(values.output, JSON.stringify(result, null, 2));
  await writeFile(values.output + '.plan.json', JSON.stringify(plan));
}
console.log(JSON.stringify(result, null, 2));
