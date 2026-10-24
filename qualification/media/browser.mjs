// Observe the unmodified browser application's real playback and lifecycle.
import {readFileSync,writeFileSync,mkdirSync} from 'node:fs';
import {createHash} from 'node:crypto';
import {performance} from 'node:perf_hooks';
import {installIdentityObserver,compareIdentities} from './timing-observer.mjs';
import {installNativeObserver} from './native-observer.mjs';
const config=JSON.parse(readFileSync(process.argv[2]));
const {chromium}=await import(config.tv_root+'/node_modules/playwright/index.mjs');
const result={fixture:config.fixture.id,status:'failed',events:[],metrics:{},limitations:[]};
result.observer_sha256=Object.fromEntries(['browser.mjs','native-observer.mjs','timing-observer.mjs'].map(name=>[name,createHash('sha256').update(readFileSync(new URL(name,import.meta.url))).digest('hex')]));
result.has_audio=!!config.fixture.audio;
result.timing_markers=!!config.fixture.timing_identity&&!config.fixture.multi_audio;
result.phase_timing_markers=config.fixture.timing_markers!==false&&!config.fixture.multi_audio;
result.driver=config.driver??'product';
result.player_profile=config.player_profile??'browser';
result.marker_recipe=config.fixture.timing_identity??null;
const browser=await chromium.launch({headless:config.headless!==false,...(config.browser_executable?{executablePath:config.browser_executable}:{}),args:['--autoplay-policy=no-user-gesture-required']});
result.browser_build=browser.version();
const context=await browser.newContext({viewport:{width:1280,height:720},...(config.driver==='kmp'?{screen:{width:1920,height:1080}}:{}),ignoreHTTPSErrors:false});
const page=await context.newPage();
const diagnostics=await browser.newBrowserCDPSession();
result.resources=[];
const sampleResources=async()=>{
  const {processInfo}=await diagnostics.send('SystemInfo.getProcessInfo');
  result.resources.push({wall_ms:Date.now(),processes:processInfo.map(p=>{let rss=null;try{const value=readFileSync('/proc/'+p.id+'/status','utf8').match(/^VmRSS:\s+(\d+)\s+kB/m);if(value)rss=Number(value[1])*1024;}catch{}return {pid:p.id,role:p.type,cpu_seconds:p.cpuTime,rss_bytes:rss};})});
};
let clicked, sessionId, heartbeat=0, released=0, firstMedia;
const pending=[];
async function canvasClick(locator){
  await locator.waitFor({state:'visible',timeout:15000});
  const bounds=await locator.boundingBox();
  if(!bounds)throw Error('No rendered bounds for the Compose action');
  // Compose exposes semantics behind its canvas. Click the observed bounds so
  // the actual canvas handles the pointer, without dispatching a synthetic DOM action.
  await page.mouse.click(bounds.x+bounds.width/2,bounds.y+bounds.height/2);
}
async function catalogCard(id){
  for(let step=0;step<12;step++){
    const cards=page.getByRole('button',{name:id,exact:true});
    for(let index=0;index<await cards.count();index++){
      const candidate=cards.nth(index),box=await candidate.boundingBox();
      if(box&&box.y>=70&&box.y+box.height<=720&&box.x>=0&&box.x+box.width<=1280)return candidate;
    }
    await page.mouse.move(1180,600);await page.mouse.wheel(0,320);await page.waitForTimeout(100);
  }
  throw Error('The current app did not show the requested catalog card');
}
const mediaSamples=new Map(),sampleRetries=new Set();let sampleBudget=32*1024*1024;
result.media_observer_enabled=!!config.capture_media;
page.on('request',request=>{
  const path=new URL(request.url()).pathname;
  if(/^\/api\/(v2\/)?playback$/.test(path)&&request.method()==='POST')result.events.push({stage:'backend_request',ms:performance.now()-clicked});
  if(/^\/api\/(v2\/)?playback\//.test(path)&&path.endsWith('/heartbeat'))heartbeat++;
  if(/^\/api\/(v2\/)?playback\//.test(path)&&request.method()==='DELETE')released++;
});
page.on('response',response=>{
  const path=new URL(response.url()).pathname;
  if(path.startsWith('/api/'))result.events.push({stage:'api_response',path,status:response.status()});
  if(path.startsWith('/gateway/media/')&&firstMedia===undefined)firstMedia=performance.now();
  if(path.startsWith('/gateway/media/'))result.events.push({stage:'media_response',file:path.split('/').at(-1),status:response.status(),ms:performance.now()-clicked});
  const filename=path.split('/').at(-1),length=Number(response.headers()['content-length']);
  if(config.capture_media&&path.startsWith('/gateway/media/')&&/^(?:init\.mp4|segment-000000000\.(?:ts|m4s))$/.test(filename)&&!sampleRetries.has(filename)){
    sampleRetries.add(filename);
    // Demuxers can cancel an initial range response after reading its header.
    // Retain the same immutable public resource through a bounded follow-up
    // request while the real lease is still active. Quality runs alone do this.
    pending.push((async()=>{
      if(mediaSamples.get(filename))return;
      const head=await context.request.head(response.url(),{timeout:5000});
      const size=Number(head.headers()['content-length']);
      if(!head.ok()||!(size>0&&size<=8*1024*1024&&size<=sampleBudget))return;
      sampleBudget-=size;
      const copy=await context.request.get(response.url(),{headers:{Range:`bytes=0-${size-1}`},timeout:5000});
      const bytes=await copy.body();
      if(copy.ok()&&bytes.length===size)mediaSamples.set(filename,bytes);
    })().catch(error=>result.limitations.push('A required public output sample could not be retained: '+String(error.message).replace(/https?:\/\/\S+/g,'<url>').slice(0,300))));
  }
  if(config.capture_media&&path.startsWith('/gateway/media/')&&/^(?:index\.m3u8|init\.mp4|segment-\d{9}\.(?:ts|m4s))$/.test(filename)&&!mediaSamples.has(filename)&&mediaSamples.size<6&&length>0&&length<=8*1024*1024&&length<=sampleBudget){
    sampleBudget-=length;mediaSamples.set(filename,null);
    pending.push(response.body().then(bytes=>{if(bytes.length<=length)mediaSamples.set(filename,bytes);}).catch(()=>{}));
  }
  if(/^\/api\/(v2\/)?playback(?:\/[^/]+)?$/.test(path)&&response.request().method()!=='DELETE'){
    const elapsed=performance.now()-clicked;
    pending.push(response.json().then(body=>{
      result.events.push({stage:body.status==='ready'?'backend_ready':'backend_response',ms:elapsed,status:response.status()});
      sessionId=body.id??sessionId;
      if(config.driver==='kmp'&&(response.status()>=400||body.status==='failed')){
        const code=body.error_code??body.error?.code??'backend_playback_failed';
        pending.push(page.evaluate(code=>{window.__labFailure='Backend playback failed: '+code;},String(code)).catch(()=>{}));
      }
      const d=body.delivery;
      if(d)result.delivery={kind:d.kind,live:d.live,mode:d.mode,format:d.format,video_mode:d.video_mode,audio_mode:d.audio_mode,
        audio_tracks:d.audio_tracks,subtitle_tracks:d.subtitle_tracks};
    }).catch(()=>{}));
  }
});
page.on('pageerror',()=>result.events.push({stage:'page_error'}));
await page.addInitScript(({origin,fixture,playerProfile})=>{
  window.__labFixture=fixture;
  window.__labPlayerProfile=playerProfile;
  localStorage.setItem('viptv-device:'+origin,JSON.stringify({sessionId:'s1',accountId:'1',profileId:'1',accessToken:'member-token-1',refreshToken:'fixture-refresh',expiresIn:172800}));
  // Observers only: call the original draw/audio methods unchanged.
  const m=window.__mediaLab={draws:[],audio:[],videos:[],videoMarkers:[],audioMarkers:[],audioSchedules:[]};
  const contexts=window.__mediaLabAudioContexts=[];
  const lastMarker=new WeakMap(),audioStarted=new WeakSet(),audioBuffersStarted=new WeakSet(),quietPeaks=new WeakMap();
  let lastWhite=false;
  const draw=CanvasRenderingContext2D.prototype.drawImage;
  CanvasRenderingContext2D.prototype.drawImage=function(...args){
    const returned=Reflect.apply(draw,this,args);
    if(this.canvas.classList.contains('player-canvas')){
      const source=args[0];
      const frame={t:performance.now(),pts:Number.isFinite(source?.timestamp)?source.timestamp/1e6:null};m.draws.push(frame);
      queueMicrotask(()=>{const state=window.__labPlayer?.snapshot;frame.pts??=state?.diagnostics?.presentedPositionSeconds??state?.time?.positionSeconds??0;});
      try{const p=this.getImageData(10,10,1,1).data;const white=p[0]>220&&p[1]>220&&p[2]>220;if(white&&!lastWhite)m.videoMarkers.push({t:frame.t,pts:frame.pts});lastWhite=white;}catch{}
      if(m.draws.length>120000)m.draws.splice(0,60000);
    }
    return returned;
  };
  const start=AudioBufferSourceNode.prototype.start;
  AudioBufferSourceNode.prototype.start=function(...args){
    const ctx=this.context,contextTime=ctx.currentTime;
    const returned=Reflect.apply(start,this,args);
    m.audio.push({t:performance.now(),when:args[0]??0,contextTime,state:ctx.state});
    if(this.buffer){
      let index=contexts.indexOf(ctx);if(index<0){index=contexts.length;contexts.push(ctx);}
      const samples=this.buffer.getChannelData(0),rate=this.buffer.sampleRate,offset=args[1]??0;
      const when=Math.max(contextTime,args[0]??0);
      if(!audioBuffersStarted.has(ctx)){audioBuffersStarted.add(ctx);m.audioSchedules.push({context:index,time:when,kind:'buffer'});}
      let peak=0;for(let i=Math.floor(offset*rate);i<samples.length;i++)peak=Math.max(peak,Math.abs(samples[i]));
      if(peak>.0001)quietPeaks.set(ctx,Math.min(quietPeaks.get(ctx)??Infinity,peak));
      const threshold=Math.max(.0002,2*(quietPeaks.get(ctx)??Infinity));
      for(let i=Math.floor(offset*rate);i<samples.length;i++){
        const time=when+i/rate-offset;
        if(!audioStarted.has(ctx)&&Math.abs(samples[i])>.001){audioStarted.add(ctx);m.audioSchedules.push({context:index,time,kind:'signal'});}
        if(Math.abs(samples[i])>threshold&&time-(lastMarker.get(ctx)??-1)>.5){lastMarker.set(ctx,time);m.audioSchedules.push({context:index,time,kind:'marker'});}
      }
    }
    if(m.audio.length>120000)m.audio.splice(0,60000);
    return returned;
  };
},{origin:config.origin,fixture:config.fixture,playerProfile:config.player_profile??'browser'});
try {
  await sampleResources();
  if(config.driver==='kmp'){
    const login=await context.request.post(config.origin+'/api/auth/login',{data:config.login,headers:{Origin:config.origin}});
    if(!login.ok())throw Error('Disposable browser login returned '+login.status());
    const identity=await login.json();
    const profile=await context.request.post(config.origin+'/api/auth/profile',{data:{profile_id:'1'},headers:{Origin:config.origin,'x-csrf-token':identity.csrf_token}});
    if(!profile.ok())throw Error('Disposable browser profile selection returned '+profile.status());
    if(config.fixture.subtitles||config.fixture.multi_audio){
      const status=await context.request.get(config.origin+'/api/auth/status');
      const auth=await status.json();
      const current=await context.request.get(config.origin+'/api/profiles/1/preferences');
      const preferences=await current.json();
      const changes={...(config.fixture.subtitles?{subtitles_enabled:true,subtitle_mode:'always',preferred_subtitle_language:'eng'}:{}),...(config.fixture.multi_audio?{preferred_audio_language:'spa'}:{})};
      const update=await context.request.put(config.origin+'/api/profiles/1/preferences',{headers:{Origin:config.origin,'x-csrf-token':auth.csrf_token},data:{revision:preferences.revision,preferences:{...preferences.preferences,...changes}}});
      if(!update.ok())throw Error('Disposable caption preferences returned '+update.status());
      result.fixture_preferences=changes;
    }
  }
  await page.goto(config.origin+(config.driver==='current-api'?'/lab/flow.html':'/tv/'),{waitUntil:'domcontentloaded'});
  if(config.driver==='kmp'){
    await page.evaluate(installNativeObserver,config.fixture.timing_identity??null);
    result.limitations.push('Native audio uses a test-only PCM pass-through observer initialized before Play; the browser output clock is observed, not physical speakers.');
  }else if(config.fixture.timing_identity)await page.evaluate(installIdentityObserver,config.fixture.timing_identity);
  if(config.driver==='current-api'){
    await page.waitForFunction(()=>document.querySelector('.media-card')||window.__labFailure,undefined,{timeout:45000});
    const error=await page.evaluate(()=>window.__labFailure??null);if(error)throw Error(error);
  }
  if(config.driver==='kmp'){
    const until=Date.now()+45000;
    while(!result.events.some(e=>e.path==='/api/bootstrap'&&e.status===200)&&Date.now()<until)await page.waitForTimeout(100);
    await page.waitForTimeout(500);
    await page.keyboard.press('Tab');
    const accessibility=await context.newCDPSession(page);
    const tree=await accessibility.send('Accessibility.getFullAXTree');
    result.initial_accessibility=tree.nodes.map(n=>({role:n.role?.value,name:n.name?.value})).filter(n=>n.name).slice(0,150);
    if(config.fixture.live){
      await canvasClick(page.getByRole('button',{name:'Live',exact:true}));
      await page.getByRole('button',{name:'Watch',exact:true}).waitFor({timeout:45000});
      const name=config.fixture.id.replace(/[.*+?^${}()|[\]\\]/g,'\\$&');
      await page.getByRole('button',{name:new RegExp('^'+name+'(?:\\s|$)')}).last().waitFor({timeout:45000});
    }else{
      await canvasClick(await catalogCard(config.fixture.id));
      await page.getByRole('button',{name:'Play',exact:true}).waitFor({timeout:10000});
    }
  }else await page.locator('.media-card').filter({hasText:config.fixture.id}).first().click({timeout:45000});
  if(config.driver!=='current-api'&&config.driver!=='kmp')await page.locator('[data-focus-id="detail-source"]').click();
  else if(config.driver==='current-api')await page.locator('#play').waitFor({state:'visible',timeout:45000});
  clicked=performance.now();
  const pageClick=await page.evaluate(()=>performance.now());
  if(config.driver==='kmp'){
    if(config.fixture.live){
      const name=config.fixture.id.replace(/[.*+?^${}()|[\]\\]/g,'\\$&');
      await canvasClick(page.getByRole('button',{name:new RegExp('^'+name+'(?:\\s|$)')}).last());
    }
    else await canvasClick(page.getByRole('button',{name:'Play',exact:true}));
    if(!config.fixture.live)await canvasClick(page.getByText(/Owned HTTPS fixture/).last());
  }
  else await page.locator(config.driver==='current-api'?'#play':'[data-focus-id="source-0"]').click();
  await page.waitForFunction(driver=>{
    const m=window.__mediaLab;
    if(window.__labFailure)return true;
    if(driver==='current-api')return m.draws.length>0||window.__labFailure;
    const canvas=document.querySelector('.player-canvas');
    if(m.draws.length&&canvas?.width>0)return true;
    const v=document.querySelector('video');
    return v&&v.videoWidth>0&&v.currentTime>0&&!v.paused;
  },config.driver,{timeout:45000});
  const openingError=await page.evaluate(()=>window.__labFailure??null);if(openingError)throw Error(openingError);
  const first=await page.evaluate(()=>({draw:window.__mediaLab.draws[0],now:performance.now(),video:document.querySelector('video')?.currentTime,playRequested:window.__labPlayRequested}));
  result.metrics.first_frame_ms=first.draw?first.draw.t-(first.playRequested??pageClick):performance.now()-clicked;
  result.metrics.first_frame_pts=first.draw?.pts??null;
  if(!first.draw)result.limitations.push('Native video first-frame timing is polling-observed.');
  if(config.driver==='current-api'){
    await page.waitForFunction(()=>window.__labPlayer?.snapshot?.state==='playing'||window.__labFailure,undefined,{timeout:45000});
    const error=await page.evaluate(()=>window.__labFailure??null);if(error)throw Error(error);
  }
  const steadyPageTime=await page.evaluate(()=>performance.now());
  result.metrics.playing_ms=steadyPageTime-(first.playRequested??pageClick);
  const observeStart=performance.now();
  const samples=[];
  let nextResource=0;
  const seconds=config.seconds??30;
  while(performance.now()-observeStart<seconds*1000){
    const playbackError=await page.evaluate(()=>window.__labFailure??null);if(playbackError){result.playback_error=playbackError;if(samples.length)break;}
    if(performance.now()>=nextResource){await sampleResources();nextResource=performance.now()+1000;}
    samples.push(await page.evaluate(()=>{
      const v=document.querySelector('video');const m=window.__mediaLab;const player=window.__labPlayer?.snapshot;
      const canvas=player?.diagnostics?.engine==='mediabunny';
      return {t:performance.now(),lastDraw:m.draws.at(-1)?.t??null,draws:m.draws.length,
        pts:m.draws.at(-1)?.pts??null,position:player?.time?.positionSeconds??v?.currentTime??Number(document.querySelector('[data-focus-id="timeline"]')?.getAttribute('aria-valuenow')),
        buffering:player?.state==='buffering'||!!document.querySelector('.vx-player__buffering'),ended:player?.state==='ended'||v?.ended||false,
        captionCues:v?Array.from(v.textTracks).flatMap(t=>Array.from(t.activeCues??[]).map(c=>c.text)):[],
        paused:v?.paused,readyState:v?.readyState,playbackRate:v?.playbackRate,
        playerState:window.__labPlayer?.snapshot?.state,
        liveWindow:player?.time?.liveWindow??(window.__labFixture?.live&&v?.seekable.length?{start:v.seekable.start(0),end:v.seekable.end(v.seekable.length-1)}:null),
        buffered:v?Array.from({length:v.buffered.length},(_,i)=>[v.buffered.start(i),v.buffered.end(i)]):[],
        engine:player?.diagnostics?.engine,
        audioCodec:player?.diagnostics?.audioCodec??null,
        dimensions:player?.diagnostics?{width:player.diagnostics.width,height:player.diagnostics.height}:v?{width:v.videoWidth,height:v.videoHeight}:null,
        quality:canvas?{total:player.diagnostics.presentedFrames,dropped:player.diagnostics.droppedFrames}:((()=>{const q=v?.getVideoPlaybackQuality?.();return q?{total:q.totalVideoFrames,dropped:q.droppedVideoFrames,corrupted:q.corruptedVideoFrames}:null;})())};
    }));
    if(samples.at(-1).ended)break;
    await page.waitForTimeout(100);
  }
  const observed=await page.evaluate(()=>{
    const m=window.__mediaLab;
    const scheduled=m.audioSchedules.map(a=>{const c=window.__mediaLabAudioContexts[a.context];const stamp=c.getOutputTimestamp();return {...a,t:stamp.performanceTime+(a.time-stamp.contextTime)*1000};});
    if(scheduled.length){m.audioMarkers=scheduled.filter(a=>a.kind==='marker');const first=scheduled.find(a=>a.kind==='buffer');if(first){m.hasScheduledBuffers=true;m.audio.unshift({t:first.t,when:0,contextTime:0,state:'running'});}m.firstAudioSignal=scheduled.find(a=>a.kind==='signal')?.t??null;}
    if(m.nativeAudioEvents){
      const stamp=window.__mediaLabAudioContexts[m.nativeContext].getOutputTimestamp();
      const events=m.nativeAudioEvents.map(e=>({...e,t:stamp.performanceTime+(e.time-stamp.contextTime)*1000}));
      m.audio=events.filter(e=>e.kind==='start').map(e=>({t:e.t,when:0,contextTime:0,state:'running'}));
      m.firstAudioSignal=events.find(e=>e.kind==='start')?.t??null;
      m.identityAudioMarkers=events.filter(e=>e.kind==='identity');
    }else if(m.identityAudioMarkers)m.identityAudioMarkers=m.identityAudioMarkers.filter(a=>m.identityAudioNodes[a.node].stop===null||a.time<m.identityAudioNodes[a.node].stop).map(a=>{
      const stamp=window.__mediaLabAudioContexts[a.context].getOutputTimestamp();
      return {...a,t:stamp.performanceTime+(a.time-stamp.contextTime)*1000};
    });
    return m;
  });
  const draws=observed.draws.filter(x=>x.t>=steadyPageTime);
  const gaps=draws.slice(1).map((d,i)=>d.t-draws[i].t);
  const terminalAge=samples.at(-1).lastDraw===null?0:samples.at(-1).t-samples.at(-1).lastDraw;
  if(terminalAge>500&&!samples.at(-1).ended)gaps.push(terminalAge);
  result.metrics.observed_seconds=(performance.now()-observeStart)/1000;
  result.metrics.rendered_frames=draws.length;
  result.metrics.max_frame_gap_ms=gaps.length?Math.max(...gaps):null;
  result.metrics.frame_gaps_over_500ms=gaps.filter(g=>g>500).length;
  result.metrics.first_media_ms=firstMedia===undefined?null:firstMedia-clicked;
  const audio=observed.audio.find(a=>a.state==='running');
  result.metrics.first_audio_scheduled_ms=audio&&observed.hasScheduledBuffers?audio.t-(first.playRequested??pageClick)+Math.max(0,audio.when-audio.contextTime)*1000:null;
  result.metrics.first_audio_signal_ms=observed.firstAudioSignal===undefined||observed.firstAudioSignal===null?null:observed.firstAudioSignal-(first.playRequested??pageClick);
  result.metrics.audio_buffers_observed=observed.audio.length;
  if(config.driver==='kmp'){
    result.metrics.audio_buffers_observed=null;
    result.metrics.audio_signal_observations=observed.audio.length;
  }
  const videoMarkers=observed.videoMarkers??[],audioMarkers=observed.audioMarkers??[];
  result.timing_marker_samples={video:videoMarkers,audio:audioMarkers};
  const drifts=videoMarkers.slice(1).map(v=>audioMarkers.reduce((best,a)=>Math.abs(a.t-v.t)<Math.abs(best)?a.t-v.t:best,Infinity)).filter(d=>Math.abs(d)<500);
  result.metrics.av_marker_pairs=drifts.length;
  result.metrics.av_phase_warning_ms=drifts.length>=5?Math.max(...drifts.map(Math.abs)):null;
  result.metrics.av_drift_ms=null;
  result.metrics.av_marker_identity_verified=false;
  if(config.fixture.timing_identity){
    const identity=compareIdentities(observed.identityVideoMarkers??[],observed.identityAudioMarkers??[]);
    result.timing_identity=identity;
    result.browser_time_origin_ms=observed.timeOriginMs;
    result.timing_marker_samples={video:observed.identityVideoMarkers,audio:observed.identityAudioMarkers};
    result.metrics.av_drift_ms=identity.maximum_drift_ms;
    result.metrics.av_marker_pairs=identity.pairs.length;
    result.metrics.av_marker_identity_verified=identity.verified;
  }else if(result.phase_timing_markers)result.limitations.push('Periodic pulses measure phase only and are excluded from absolute A/V acceptance.');
  result.limitations.push('Audio is observed at the browser output clock, not physical speakers.');
  result.samples=samples;
  if(config.driver==='kmp'){
    const release=page.waitForResponse(r=>r.request().method()==='DELETE'&&/^\/api\/playback\/[^/]+$/.test(new URL(r.url()).pathname),{timeout:15000}).then(response=>({response}),error=>({error}));
    // Hidden canvas controls can leave a stale accessibility node for a frame.
    // A first tap reveals the toolbar; the next activates the actual Back button.
    for(let attempt=0;attempt<3&&await page.locator('#viptv-video-layer video').count();attempt++){
      const close=page.getByRole('button',{name:'Back',exact:true});
      if(await close.count()){
        const bounds=await close.boundingBox();
        if(bounds)await page.mouse.click(bounds.x+bounds.width/2,bounds.y+bounds.height/2);
        else await page.mouse.click(640,360);
      }else await page.mouse.click(640,360);
      await page.waitForTimeout(200);
    }
    await page.locator('#viptv-video-layer video').waitFor({state:'detached',timeout:10000});
    const finished=await release;
    if(finished.error||!finished.response.ok())throw Error('The current app did not successfully release playback');
  }else{
    await page.locator(config.driver==='current-api'?'#stop':'[data-focus-id="player-back"]').click();
    await page.locator(config.driver==='current-api'?'#stop':'.player-overlay').waitFor({state:'hidden',timeout:10000});
  }
  await Promise.all(pending);
  result.metrics.renewals=heartbeat;result.metrics.releases=released;
  const progress=samples.at(-1).position-samples[0].position;
  result.metrics.timeline_advance_seconds=progress;
  result.metrics.initial_position_seconds=samples[0].position;
  result.metrics.caption_cues_seen=samples.reduce((n,s)=>n+s.captionCues.length,0);
  const liveLags=samples.filter(s=>s.liveWindow).map(s=>s.liveWindow.end-s.position);
  result.metrics.live_edge_lag_seconds=liveLags.length?{minimum:Math.min(...liveLags),maximum:Math.max(...liveLags),mean:liveLags.reduce((a,b)=>a+b,0)/liveLags.length}:null;
  if(config.fixture.live)result.limitations.push('Live lag is measured against the available playlist edge, not source capture time.');
  const fails=[];
  if(result.playback_error)fails.push(result.playback_error);
  if(result.delivery?.kind!=='gateway')fails.push('gateway_delivery_missing');
  if(!!result.delivery?.live!==!!config.fixture.live)fails.push('live_classification_mismatch');
  if(released===0)fails.push('release_missing');
  if(!config.fixture.live&&samples[0].position>1)fails.push('vod_started_after_zero');
  if(!config.fixture.live&&result.metrics.first_frame_pts>1)fails.push('first_frame_not_at_start');
  const requestedSegments=result.events.filter(e=>e.stage==='media_response'&&/^segment-\d{9}\.(?:ts|m4s)$/.test(e.file??'')).map(e=>Number(e.file.slice(8,17)));
  if(!config.fixture.live&&requestedSegments.length&&Math.min(...requestedSegments)>0)fails.push('initial_vod_segments_missing');
  if(result.metrics.observed_seconds>=25&&heartbeat===0)fails.push('renewal_missing');
  if(progress<Math.min(seconds,config.fixture.duration-2)*.8)fails.push('insufficient_progress');
  if(result.metrics.frame_gaps_over_500ms)fails.push('render_stall');
  if(config.fixture.audio&&!audio)fails.push('audio_observation_missing');
  if(config.fixture.subtitles&&['current-api','kmp'].includes(config.driver)&&result.player_profile!=='browser'&&!result.metrics.caption_cues_seen)fails.push('caption_cues_missing');
  if(config.fixture.multi_audio&&!result.delivery?.audio_tracks?.some(t=>t.selected&&t.input_index===2))fails.push('selected_audio_mismatch');
  if(config.fixture.interlaced&&result.delivery?.video_mode!=='encode')fails.push('interlaced_video_was_not_converted');
  if(samples.some(s=>s.dimensions?.width>1920||s.dimensions?.height>1080))fails.push('output_exceeds_declared_dimensions');
  if(config.driver==='current-api'&&config.fixture.audio&&result.timing_markers&&seconds>=10&&result.metrics.av_drift_ms===null)fails.push('av_measurement_missing');
  if(result.timing_markers&&!result.metrics.av_marker_identity_verified)fails.push('av_identity_unverified');
  if(result.timing_markers&&result.metrics.av_drift_ms>100)fails.push('av_drift');
  const appFailure=await page.evaluate(()=>window.__labFailure??null);
  if(appFailure)fails.push(appFailure);
  result.failures=fails;result.status=fails.length?'failed':'passed';
} catch(error){
  result.failure=String(error.message).replace(/https?:\/\/\S+/g,'<url>').slice(0,1200);
  result.page_text=(await page.locator('body').innerText().catch(()=>'')).slice(0,1500);
  if(config.driver==='kmp'){
    const a11y=await context.newCDPSession(page);
    result.failure_accessibility=(await a11y.send('Accessibility.getFullAXTree')).nodes.map(n=>({role:n.role?.value,name:n.name?.value})).filter(n=>n.name).slice(0,200);
    result.native_media_diagnostics=await page.evaluate(()=>({videos:Array.from(document.querySelectorAll('video')).map(v=>({position:v.currentTime,paused:v.paused,readyState:v.readyState,error:v.error?{code:v.error.code,message:v.error.message}:null,buffered:Array.from({length:v.buffered.length},(_,i)=>[v.buffered.start(i),v.buffered.end(i)])})),play:window.__mediaLab?.nativePlayEvents,frames:window.__mediaLab?.draws?.length,audio:window.__mediaLab?.nativeAudioEvents?.length}));
  }
  await page.screenshot({path:config.result+'.png'}).catch(()=>{});
} finally {
  if(sessionId){
    if(config.driver==='kmp'){
      const status=await context.request.get(config.origin+'/api/auth/status').catch(()=>null);
      const auth=await status?.json().catch(()=>null);
      if(auth?.csrf_token)await context.request.delete(config.origin+'/api/playback/'+sessionId,{headers:{Origin:config.origin,'x-csrf-token':auth.csrf_token}}).catch(()=>{});
    }else await context.request.delete(config.origin+(config.driver==='current-api'?'/api/playback/':'/api/v2/playback/')+sessionId,{headers:{Authorization:'Bearer member-token-1'}}).catch(()=>{});
  }
  await browser.close();
  await Promise.all(pending);
  if(config.capture_media&&(result.status==='failed'||config.fixture.transfer&&config.fixture.transfer!=='bt709')){
    const directory=config.result+'.media';mkdirSync(directory,{recursive:true,mode:0o700});
    result.media_samples=[];
    for(const [file,bytes] of mediaSamples)if(bytes){writeFileSync(directory+'/'+file,bytes);result.media_samples.push({file,bytes:bytes.length,sha256:createHash('sha256').update(bytes).digest('hex')});}
  }
  writeFileSync(config.result,JSON.stringify(result,null,2));
}
console.log(JSON.stringify({fixture:result.fixture,status:result.status,metrics:result.metrics,failures:result.failures??[result.failure]}));
process.exitCode=result.status==='passed'?0:1;
