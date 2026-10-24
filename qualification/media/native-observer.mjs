// Test-only browser observations. The worklet copies PCM unchanged to output.
export async function installNativeObserver(identity=null) {
  const m=window.__mediaLab;
  m.nativePlayEvents=[];
  const originalPlay=HTMLMediaElement.prototype.play;
  HTMLMediaElement.prototype.play=function(...args){
    m.nativePlayEvents.push({event:'request',t:performance.now()});
    const returned=Reflect.apply(originalPlay,this,args);
    returned?.then(()=>m.nativePlayEvents.push({event:'resolved',t:performance.now()}),error=>m.nativePlayEvents.push({event:'rejected',name:error.name,t:performance.now()}));
    return returned;
  };
  m.timeOriginMs=performance.timeOrigin;
  m.nativeAudioEvents=[];m.identityVideoMarkers=[];
  const marked=new WeakSet();
  const marker=document.createElement('canvas');marker.width=208;marker.height=48;
  const pixels=marker.getContext('2d',{willReadFrequently:true});
  const context=new AudioContext();
  const contextId=window.__mediaLabAudioContexts.length;
  window.__mediaLabAudioContexts.push(context);
  m.nativeContext=contextId;
  function worklet() {
    class Tap extends AudioWorkletProcessor {
      constructor(options){super();this.identity=options.processorOptions?.identity;this.started=false;this.last=-1;this.pending=null;this.coefficients=new Map();}
      identify(values,count){
        let coefficients=this.coefficients.get(count);
        if(!coefficients){coefficients=Array.from({length:count},(_,i)=>2*Math.cos(2*Math.PI*(1000+128*i)/sampleRate));this.coefficients.set(count,coefficients);}
        let best=-1,maximum=0,total=0;
        for(let id=0;id<count;id++){
          const c=coefficients[id];let a=0,b=0;
          for(const value of values){const next=value+c*a-b;b=a;a=next;}
          const power=Math.max(0,a*a+b*b-c*a*b);total+=power;
          if(power>maximum){maximum=power;best=id;}
        }
        return {id:best,confidence:maximum/(total||1)};
      }
      process(inputs,outputs){
        const input=inputs[0],output=outputs[0];
        for(let channel=0;channel<output.length;channel++)if(input[channel])output[channel].set(input[channel]);
        const values=input[0];if(!values)return true;
        for(let index=0;index<values.length;index++){
          const time=currentTime+index/sampleRate;
          if(!this.started&&Math.abs(values[index])>.001){this.started=true;this.port.postMessage({kind:'start',time});}
          if(!this.identity)continue;
          if(!this.pending&&time-this.last>.5&&Math.abs(values[index])>.16){this.last=time;this.pending={time,samples:[]};}
          const pending=this.pending;
          if(pending){
            pending.samples.push(values[index]);
            const fsk=this.identity.version===2;
            if(pending.samples.length===(fsk?Math.ceil(sampleRate*.096):1024)){
              let decoded;
              if(fsk){
                const symbols=Array.from({length:4},(_,i)=>this.identify(pending.samples.slice(Math.floor(sampleRate*(i*.02+.004)),Math.floor(sampleRate*(i*.02+.016))),16));
                decoded={id:symbols.reduce((id,symbol,i)=>id|(symbol.id<<(i*4)),0),confidence:Math.min(...symbols.map(s=>s.confidence))};
              }else decoded=this.identify(pending.samples,64);
              this.port.postMessage({kind:'identity',time:pending.time,...decoded});this.pending=null;
            }
          }
        }
        return true;
      }
    }
    registerProcessor('media-lab-native-tap',Tap);
  }
  const module=URL.createObjectURL(new Blob(['('+worklet.toString()+')()'],{type:'text/javascript'}));
  try{await context.audioWorklet.addModule(module);}finally{URL.revokeObjectURL(module);}
  await context.resume();
  const attach=video=>{
    if(marked.has(video))return;marked.add(video);
    const tap=new AudioWorkletNode(context,'media-lab-native-tap',{processorOptions:{identity}});
    tap.port.onmessage=({data})=>m.nativeAudioEvents.push(data);
    if(identity?.observe_audio!==false)context.createMediaElementSource(video).connect(tap).connect(context.destination);
    let wasWhite=false;
    const tick=(time,metadata)=>{
      const t=metadata.expectedDisplayTime??time;
      m.draws.push({t,pts:metadata.mediaTime});
      if(m.draws.length>120000)m.draws.splice(0,60000);
      if(identity){
        try{
          pixels.drawImage(video,0,0,208,48,0,0,208,48);
          const pulse=pixels.getImageData(10,34,1,1).data;
          const white=pulse[0]>192&&pulse[1]>192&&pulse[2]>192;
          if(white&&!wasWhite){
            let id=0;
            for(let bit=0;bit<(identity.bits??6);bit++){
              const value=pixels.getImageData(20+(identity.version===2?12:16)*bit,12,1,1).data;
              if(value[0]+value[1]+value[2]>3*128)id|=1<<bit;
            }
            m.identityVideoMarkers.push({id,t});
          }
          wasWhite=white;
        }catch{}
      }
      video.requestVideoFrameCallback(tick);
    };
    video.requestVideoFrameCallback(tick);
  };
  const scan=()=>document.querySelectorAll('#viptv-video-layer video').forEach(attach);
  new MutationObserver(scan).observe(document.documentElement,{childList:true,subtree:true});scan();
}
