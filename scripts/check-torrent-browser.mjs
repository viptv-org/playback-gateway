// SPDX-License-Identifier: GPL-2.0-only
// Account/catalog fixture only. Torrent transport, gateway leases/media and decode are real.
const tvRoot=process.env.TV_WEB_ROOT;
if(!tvRoot) throw Error('TV_WEB_ROOT must point to a checked and built tv-web checkout');
const {chromium}=await import(tvRoot+'/node_modules/playwright/index.mjs');
const {installBackend}=await import(tvRoot+'/tests/e2e/helpers/responsiveBackend.ts');
import {readFileSync,writeFileSync} from 'node:fs';
import http from 'node:http';
import assert from 'node:assert/strict';
const q=process.env.QUALIFICATION_DIR+'/';
if(!process.env.QUALIFICATION_DIR) throw Error('QUALIFICATION_DIR required');
const config=JSON.parse(readFileSync(q+'fixture.json'));
const origin=process.env.VIPTV_TEST_BROWSER_ORIGIN;
const gateway=(method,path,body,identity)=>new Promise((resolve,reject)=>{
 const req=http.request({socketPath:process.env.GATEWAY_SOCKET,path,method,headers:{Authorization:'Bearer '+config.key,'Content-Type':'application/json',...(identity?{'Idempotency-Key':identity}:{})}},res=>{
  let out=''; res.on('data',c=>out+=c); res.on('end',()=> { if(res.statusCode>=400) return reject(new Error('Gateway rejected fixture: '+res.statusCode+' '+(['processing_failed','source_preparation_failed','startup_timeout'].includes(JSON.parse(out)?.error?.code)?JSON.parse(out).error.code:'other'))); resolve(out?JSON.parse(out):{}); });
 }); req.on('error',reject); req.end(body?JSON.stringify(body):undefined);
});
const browser=await chromium.launch({args:['--autoplay-policy=no-user-gesture-required']});
const context=await browser.newContext({viewport:{width:1440,height:900},ignoreHTTPSErrors:false});
const page=await context.newPage();
page.on('response',r=>{if(new URL(r.url()).pathname.startsWith('/media/')) console.log('media response',r.status(),new URL(r.url()).pathname.split('/').at(-1));});
const sessions=new Map(),events=[];
let serial=0; const runIdentity='browser-'+Date.now()+'-';
try {
 const backend=await installBackend(page);
 await page.route(new RegExp('/api/(?:v2/)?playback'), async route=>{
  const req=route.request(),path=new URL(req.url()).pathname,body=JSON.parse(req.postData()||'{}');
  const json=value=>route.fulfill({status:200,contentType:'application/json',body:JSON.stringify(value)});
  if(req.method()==='OPTIONS') return route.fulfill({status:204});
  let value;
  if(path==='/api/v2/playback'&&req.method()==='POST') {
   assert.equal(body.stream_id,'responsive-stream');
   value=await gateway('POST','/v1/sessions',{namespace:'fixture',input:{url:config.source,live:false,file_index:0},output:{protocol:'hls',video_codecs:['h264'],audio_codecs:['aac'],max_width:320,max_height:180},position_seconds:body.position||0},runIdentity+(++serial));
   assert.equal(value.status,'ready'); sessions.set(value.id,value);events.push({op:'create',position:body.position||0});
  } else {
   const id=path.split('/')[4];
   if(req.method()==='DELETE') {await gateway('DELETE','/v1/sessions/'+id);sessions.delete(id);events.push({op:'release'});return json({});}
   value=await gateway(req.method(),'/v1/sessions/'+id+(path.endsWith('/heartbeat')?'/renew':''));
   if(path.endsWith('/heartbeat')) events.push({op:'renew'});
  }
  const playback=value.playback;
  if(req.method()==='POST'&&path==='/api/v2/playback') console.log('gateway media metadata',{mode:playback.mode,format:playback.format,position:playback.position,videoMode:playback.video_mode,audioMode:playback.audio_mode});
  return json({id:value.id,status:value.status,expires_at:value.expires_at,renew_after_seconds:value.renew_after_seconds,delivery:{...playback,kind:'gateway',url:origin+playback.url,position:playback.position_seconds??playback.position??0,duration:120,live:false,audio_tracks:playback.audio_tracks||[],subtitle_tracks:playback.subtitle_tracks||[],subtitles_supported:false}});
 });
 await page.goto(origin+'/tv/');
 await page.locator('[data-focus-id="profile-0"]').click();
 await page.locator('.media-card').filter({hasText:'A Different Horizon'}).click();
 await page.locator('[data-focus-id="detail-source"]').click();
 await page.locator('[data-focus-id="source-0"]').click();
 await page.waitForFunction(()=>{const c=document.querySelector('.player-canvas'); if(!c||c.width!==320)return false;const p=c.getContext('2d').getImageData(0,0,320,180).data; return p.some((v,i)=>i%4!==3&&v>20);},{},{timeout:45000});
 await page.locator('.vx-player__buffering').waitFor({state:'hidden',timeout:45000});
 if(await page.locator('[data-focus-id="pause"]').getAttribute('aria-label')==='Pause') await page.locator('[data-focus-id="pause"]').click();
 await page.waitForFunction(()=>document.querySelector('[data-focus-id="pause"]')?.getAttribute('aria-label')==='Play');
 // Exercise actual UI rewind to establish a paused zero-origin baseline.
 await page.locator('[data-focus-id="rewind"]').click();
 await page.waitForTimeout(1000);
 await page.waitForFunction(()=>Number(document.querySelector('[data-focus-id="timeline"]')?.getAttribute('aria-valuenow'))===0);
 console.log('paused zero-origin baseline verified');
 const capture=()=>page.evaluate(()=>{const c=document.querySelector('.player-canvas'),p=c.getContext('2d').getImageData(0,0,320,180).data;return Array.from(p).filter((_,i)=>i%4!==3);});
 const firstPixels=await capture();
 const first={width:320,height:180,decodedCanvas:true};
 console.log('actual WebCodecs canvas decoded',first);
 const timeline=page.locator('[data-focus-id="timeline"]');
 const box=await timeline.boundingBox();
 await page.mouse.click(box.x+box.width*3/120,box.y+box.height/2);
 for(let i=0;i<900&&!events.some(e=>e.op==='create'&&Math.abs(e.position-3)<0.01);i++)await page.waitForTimeout(50);
 console.log('seek events',events);
 assert(events.some(e=>e.op==='create'&&Math.abs(e.position-3)<0.01),'seek must admit replacement gateway output at three seconds');
 const mse=(a,b)=>a.reduce((sum,v,i)=>sum+(v-b[i])**2,0)/a.length;
 const reference=Array.from(readFileSync(q+'expected3.rgb'));
 let soughtPixels, soughtMse;
 for(let i=0;i<200;i++) {
  soughtPixels=await capture(); soughtMse=mse(soughtPixels,reference);
  if(soughtMse<150)break;
  await page.waitForTimeout(50);
 }
 const firstMse=mse(firstPixels,reference);
 console.log('seek measurements',soughtMse,firstMse,'delta',mse(soughtPixels,firstPixels));
 
 assert(soughtMse<150,'browser seek frame matches source at 3 seconds');
 assert(soughtMse<firstMse*0.1,'browser seek must not restart at zero');
 console.log('three-second decoded browser seek MSE',soughtMse,'zero-origin comparison',firstMse);
 await page.waitForTimeout(20500);
 assert(events.some(e=>e.op==='renew'),'real gateway lease renewal');
 await page.locator('[data-focus-id="player-back"]').click();
 await page.locator('.player-overlay').waitFor({state:'hidden'});
 for(let i=0;i<400;i++) {const c=await gateway('GET','/v1/capabilities');if(c.available.inputs===2&&c.available.outputs===2&&c.available.viewers===4)break;await new Promise(r=>setTimeout(r,100)); if(i===399)throw Error('admission not reclaimed');}
 assert(events.some(e=>e.op==='release'));
 assert.equal(backend.errors.length,0);
 writeFileSync(q+'browser-result.json',JSON.stringify({first,soughtMse,firstMse,events,errors:backend.errors},null,2));
 writeFileSync(q+'stop','browser passed');
 console.log('real decoding, gateway renew/release and admission reclamation passed');
} catch(error) {
 writeFileSync(q+'failure.html',await page.content().catch(()=>''));
 await page.screenshot({path:q+'failure.png'}).catch(()=>{});
 console.log('browser decode state',await page.evaluate(()=>({canvas:Array.from(document.querySelectorAll('canvas')).map(c=>({width:c.width,height:c.height,visible:getComputedStyle(c).visibility})),video:Array.from(document.querySelectorAll('video')).map(v=>({time:v.currentTime,width:v.videoWidth,error:v.error?.code??null})),text:document.body.innerText,app:(()=>{const el=document.querySelector('.player-overlay');let f=el?.[Object.keys(el).find(k=>k.startsWith('__reactFiber$'))];while(f){const a=f.memoizedProps?.app;if(a)return{mode:a.session?.mode,deliveryKind:a.session?.deliveryKind,controller:a.controller?.current?.snapshot?.state,hasActive:!!a.controller?.current?.snapshot?.active,position:a.snapshot?.time?.positionSeconds};f=f.return;}return null;})()})).catch(()=>({})));
 throw error;
} finally {
 for(const id of sessions.keys()) await gateway('DELETE','/v1/sessions/'+id).catch(()=>{});
 await browser.close();
}

