import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {installNativeObserver} from './native-observer.mjs';

async function processor(identity){
  const saved=new Map(['window','document','AudioContext','MutationObserver','URL','HTMLMediaElement'].map(k=>[k,globalThis[k]]));
  const messages=[];let blob,Processor;
  const scope={sampleRate:48000,currentTime:0,AudioWorkletProcessor:class{constructor(){this.port={postMessage:value=>messages.push(value)};}},registerProcessor:(_name,type)=>{Processor=type;}};
  try{
    globalThis.window={__mediaLab:{draws:[]},__mediaLabAudioContexts:[]};
    globalThis.document={createElement:()=>({getContext:()=>({})}),documentElement:{},querySelectorAll:()=>[]};
    globalThis.MutationObserver=class{observe(){}};
    globalThis.HTMLMediaElement=class{play(){return Promise.resolve();}};
    globalThis.URL={createObjectURL:value=>{blob=value;return 'owned-module';},revokeObjectURL(){}};
    globalThis.AudioContext=class{constructor(){this.audioWorklet={addModule:async()=>vm.runInNewContext(await blob.text(),scope)};}async resume(){}};
    await installNativeObserver(identity);
  }finally{for(const[k,value]of saved)if(value===undefined)delete globalThis[k];else globalThis[k]=value;}
  return {value:new Processor({processorOptions:{identity}}),messages,scope};
}

for(const identity of [{bits:6,version:1},{bits:16,version:2}]){
  test(`native PCM observation preserves samples and decodes version ${identity.version} across render quanta`,async()=>{
    const {value,messages,scope}=await processor(identity);
    const wanted=identity.version===2?0xa123:37,rate=48000;
    for(let block=0;block<100;block++){
      scope.currentTime=block*128/rate;
      const samples=Float32Array.from({length:128},(_,n)=>{
        const t=(block*128+n)/rate;
        const symbol=identity.version===2?(wanted>>(Math.floor(t/.02)*4))&15:wanted;
        return .06*Math.sin(2*Math.PI*440*t)+(t<(identity.version===2?.08:.04)?.42*Math.sin(2*Math.PI*(1000+128*symbol)*t):0);
      });
      const original=samples.slice(),output=new Float32Array(128);
      value.process([[samples]],[[output]]);
      assert.deepEqual(samples,original);
      assert.deepEqual(output,original);
    }
    const decoded=messages.filter(m=>m.kind==='identity');
    assert.equal(decoded.length,1);
    assert.equal(decoded[0].id,wanted);
    assert.ok(decoded[0].confidence>=.7);
  });
}
