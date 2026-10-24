// Transport and synthetic content producer only. Backend playback routes stay real.
import http from 'node:http';
import {readFileSync} from 'node:fs';
import {createHash} from 'node:crypto';
const config=JSON.parse(readFileSync(process.argv[2]));
http.createServer((req,res)=>{
  const headers={...req.headers};
  const selected=JSON.parse(readFileSync(config.selection_file));
  if(!headers['x-playback-trace']&&selected.length===1)headers['x-playback-trace']=BigInt('0x'+createHash('sha256').update(selected[0].id).digest('hex').slice(0,16)).toString();
  const upstream=http.request({socketPath:config.gateway_socket,path:req.url,method:req.method,headers},response=>{
    res.writeHead(response.statusCode,response.headers);response.pipe(res);
    response.on('error',()=>res.destroy());
  });
  upstream.on('error',()=>{if(!res.headersSent)res.writeHead(502);res.end();});
  req.on('aborted',()=>upstream.destroy());res.on('close',()=>upstream.destroy());req.pipe(upstream);
}).listen(config.control_port,'127.0.0.1');
http.createServer((req,res)=>{
  const path=new URL(req.url,'http://fixture').pathname;
  const selected=JSON.parse(readFileSync(config.selection_file));
  const meta=item=>({id:item.id,type:'movie',name:item.id,description:'Owned media compatibility fixture',runtime:'1',year:2026});
  let value;
  if(path==='/manifest.json')value={id:'fixture.media.lab',version:'1.0.0',name:'Media laboratory',description:'Owned qualification media',types:['movie'],resources:['catalog','meta','stream'],catalogs:[{type:'movie',id:'media-lab',name:'Media laboratory'}]};
  else if(path.startsWith('/catalog/'))value={metas:selected.map(meta)};
  else {
    const id=decodeURIComponent(path.split('/').at(-1).replace(/\.json$/,''));
    const item=selected.find(i=>i.id===id);
    if(!item){res.writeHead(404);res.end();return;}
    if(path.startsWith('/meta/'))value={meta:meta(item)};
    else if(path.startsWith('/stream/'))value={streams:[{name:'Owned HTTPS fixture',title:item.container+' / '+item.video,url:item.url??config.source_origin+'/'+item.file,behaviorHints:{...(item.headers?{proxyHeaders:{request:item.headers}}:{})}}]};
  }
  if(!value){res.writeHead(404);res.end();return;}
  res.writeHead(200,{'content-type':'application/json'});res.end(JSON.stringify(value));
}).listen(config.addon_port,'127.0.0.1');
