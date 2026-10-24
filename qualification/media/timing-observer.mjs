// Read-only observations of uniquely labelled pixels and scheduled PCM.
export function installIdentityObserver(identity={}) {
  const m=window.__mediaLab;
  m.identityVideoMarkers=[];m.identityAudioMarkers=[];m.identityAudioNodes=[];
  m.timeOriginMs=performance.timeOrigin;
  const bits=identity.bits??6,fsk=identity.version===2;
  const audioStates=new WeakMap(),nodeIds=new WeakMap(),coefficientCache=new Map();
  let whiteBefore=false;
  const draw=CanvasRenderingContext2D.prototype.drawImage;
  CanvasRenderingContext2D.prototype.drawImage=function(...args){
    const result=Reflect.apply(draw,this,args);
    if(this.canvas.classList.contains('player-canvas')){
      try{
        const pulse=this.getImageData(10,34,1,1).data;
        const white=pulse[0]>192&&pulse[1]>192&&pulse[2]>192;
        if(white&&!whiteBefore){
          let id=0;
          for(let bit=0;bit<bits;bit++){
            const pixel=this.getImageData(20+(fsk?12:16)*bit,12,1,1).data;
            if(pixel[0]+pixel[1]+pixel[2]>3*128)id|=1<<bit;
          }
          m.identityVideoMarkers.push({id,t:performance.now()});
        }
        whiteBefore=white;
      }catch{}
    }
    return result;
  };
  const identify=(samples,rate,count=64)=>{
    const key=rate+':'+count;
    let coefficients=coefficientCache.get(key);
    if(!coefficients){coefficients=Array.from({length:count},(_,i)=>2*Math.cos(2*Math.PI*(1000+128*i)/rate));coefficientCache.set(key,coefficients);}
    let best=-1,maximum=0,total=0;
    for(let id=0;id<count;id++){
      const c=coefficients[id];let a=0,b=0;
      for(let i=0;i<samples.length;i++){const v=samples[i]+c*a-b;b=a;a=v;}
      const power=Math.max(0,a*a+b*b-c*a*b);total+=power;
      if(power>maximum){maximum=power;best=id;}
    }
    return {id:best,confidence:maximum/(total||1)};
  };
  const start=AudioBufferSourceNode.prototype.start;
  AudioBufferSourceNode.prototype.start=function(...args){
    const context=this.context,now=context.currentTime;
    const result=Reflect.apply(start,this,args);
    if(!this.buffer)return result;
    let contextId=window.__mediaLabAudioContexts.indexOf(context);
    if(contextId<0){contextId=window.__mediaLabAudioContexts.length;window.__mediaLabAudioContexts.push(context);}
    const nodeId=m.identityAudioNodes.length;
    nodeIds.set(this,nodeId);m.identityAudioNodes.push({stop:null});
    let state=audioStates.get(context);
    if(!state){state={last:-1,pending:null};audioStates.set(context,state);}
    const rate=this.buffer.sampleRate,values=this.buffer.getChannelData(0),offset=args[1]??0;
    const when=Math.max(now,args[0]??0),end=args[2]===undefined?values.length:Math.min(values.length,Math.ceil((offset+args[2])*rate));
    for(let i=Math.floor(offset*rate);i<end;i++){
      const time=when+i/rate-offset;
      if(!state.pending&&time-state.last>.5&&Math.abs(values[i])>.16){
        state.last=time;state.pending={samples:[],rate,time,context:contextId,node:nodeId};
      }
      const pending=state.pending;
      if(pending){
        // Ignore a pending window that crosses a real scheduling discontinuity.
        if(pending.rate!==rate||Math.abs(time-(pending.time+pending.samples.length/rate))>2/rate){state.pending=null;continue;}
        pending.samples.push(values[i]);
        if(pending.samples.length===(fsk?Math.ceil(rate*.096):1024)){
          let decoded;
          if(fsk){
            const symbols=Array.from({length:4},(_,i)=>identify(pending.samples.slice(Math.floor(rate*(i*.02+.004)),Math.floor(rate*(i*.02+.016))),rate,16));
            decoded={id:symbols.reduce((value,symbol,i)=>value|(symbol.id<<(i*4)),0),confidence:Math.min(...symbols.map(s=>s.confidence))};
          }else decoded=identify(pending.samples,rate);
          m.identityAudioMarkers.push({id:decoded.id,confidence:decoded.confidence,time:pending.time,context:pending.context,node:pending.node});
          state.pending=null;
        }
      }
    }
    return result;
  };
  const stop=AudioBufferSourceNode.prototype.stop;
  AudioBufferSourceNode.prototype.stop=function(...args){
    const result=Reflect.apply(stop,this,args);
    const id=nodeIds.get(this);
    if(id!==undefined)m.identityAudioNodes[id].stop=Math.max(this.context.currentTime,args[0]??0);
    return result;
  };
}

export function compareIdentities(video,audio,minimumPairs=20) {
  // Zero can be drawn while opening, before play. All other labels identify a
  // unique source second in these <64-second fixtures; no nearest-pulse match.
  const v=video.slice(1).filter(x=>x.id>0),a=audio.filter(x=>x.id>0&&x.confidence>=.7);
  const videoDuplicates=v.length-new Set(v.map(x=>x.id)).size;
  const audioDuplicates=a.length-new Set(a.map(x=>x.id)).size;
  const pairs=v.flatMap(x=>{
    const matches=a.filter(y=>y.id===x.id);
    return matches.length===1?[{id:x.id,video_ms:x.t,audio_ms:matches[0].t,drift_ms:matches[0].t-x.t}]:[];
  });
  return {verified:pairs.length>=minimumPairs&&!videoDuplicates&&!audioDuplicates,pairs,
    video_duplicates:videoDuplicates,audio_duplicates:audioDuplicates,
    maximum_drift_ms:pairs.length?Math.max(...pairs.map(x=>Math.abs(x.drift_ms))):null};
}
