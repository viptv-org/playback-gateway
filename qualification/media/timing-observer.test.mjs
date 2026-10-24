import test from 'node:test';
import assert from 'node:assert/strict';
import {compareIdentities,installIdentityObserver} from './timing-observer.mjs';

test('an integer-second audio error cannot disappear through nearest-pulse matching',()=>{
  const video=Array.from({length:24},(_,i)=>({id:i+1,t:(i+1)*1000}));
  const audio=video.map(x=>({...x,t:x.t+1000,confidence:1}));
  const result=compareIdentities(video,audio);
  assert.equal(result.verified,true);
  assert.equal(result.maximum_drift_ms,1000);
});

test('ambiguous and low-confidence identities never qualify',()=>{
  const video=Array.from({length:24},(_,i)=>({id:i+1,t:(i+1)*1000}));
  const audio=video.map(x=>({...x,confidence:1}));
  assert.equal(compareIdentities(video,[...audio,audio[0]]).verified,false);
  assert.equal(compareIdentities(video,audio.map(x=>({...x,confidence:.1}))).verified,false);
});

test('PCM identification crosses packet boundaries without changing playback calls',()=>{
  const saved={window:globalThis.window,canvas:globalThis.CanvasRenderingContext2D,audio:globalThis.AudioBufferSourceNode};
  try{
    globalThis.CanvasRenderingContext2D=class {drawImage(){return 'unchanged';}};
    globalThis.AudioBufferSourceNode=class {
      start(...args){this.started=args;return 'original-start';}
      stop(...args){this.stopped=args;return 'original-stop';}
    };
    const context={currentTime:0};
    globalThis.window={__mediaLab:{},__mediaLabAudioContexts:[context]};
    installIdentityObserver();
    const rate=48000,id=37,frequency=1000+128*id;
    const values=Float32Array.from({length:2048},(_,i)=>.5*Math.sin(2*Math.PI*frequency*i/rate)+.02*Math.sin(2*Math.PI*440*i/rate));
    const original=values.slice();
    const nodes=[];
    for(let at=0;at<values.length;at+=128){
      const node=new AudioBufferSourceNode();nodes.push(node);node.context=context;
      const samples=values.subarray(at,at+128);
      node.buffer={sampleRate:rate,getChannelData:()=>samples};
      assert.equal(node.start(at/rate),'original-start');
      assert.deepEqual(node.started,[at/rate]);
    }
    const markers=window.__mediaLab.identityAudioMarkers;
    assert.equal(markers.length,1);
    assert.equal(markers[0].id,id);
    assert.ok(markers[0].confidence>.7);
    assert.deepEqual(values,original);
    assert.equal(nodes[0].stop(.1),'original-stop');
    assert.deepEqual(nodes[0].stopped,[.1]);
    assert.equal(window.__mediaLab.identityAudioNodes[0].stop,.1);
  }finally{
    globalThis.window=saved.window;globalThis.CanvasRenderingContext2D=saved.canvas;globalThis.AudioBufferSourceNode=saved.audio;
  }
});
