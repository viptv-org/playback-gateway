#!/usr/bin/env python3
"""Inspect private external HTTPS references with bounded FFprobe jobs."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import subprocess
import time


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--manifest',type=Path,required=True)
    p.add_argument('--output',type=Path,required=True)
    p.add_argument('--ffprobe',default=os.environ.get('LAB_FFPROBE','/opt/ffmpeg/bin/ffprobe'))
    args=p.parse_args();os.umask(0o077);args.output.mkdir(parents=True,exist_ok=True)
    items=json.loads(args.manifest.read_text())['fixtures']
    def inspect(item):
        target=args.output/(item['id']+'.json')
        if target.exists():return json.loads(target.read_text())
        raw=args.output/(item['id']+'.private-probe.json');log=args.output/(item['id']+'.private-stderr.log')
        started=time.monotonic();row=dict(id=item['id'],status='failed')
        command=[args.ffprobe,'-v','error','-protocol_whitelist','http,https,httpproxy,tcp,tls,crypto','-rw_timeout','15000000','-request_size','8388608','-initial_request_size','1048576','-reconnect','1','-reconnect_streamed','1','-reconnect_delay_max','2','-analyzeduration','5000000','-probesize','5000000','-show_streams','-show_format','-of','json','-i',item['url']]
        try:
            with raw.open('wb') as out,log.open('wb') as err:
                job=subprocess.run(command,stdout=out,stderr=err,timeout=60)
            row['exit_code']=job.returncode
            if job.returncode:raise ValueError('inspection_failed')
            if raw.stat().st_size>4*1024*1024:raise ValueError('inspection_output_limit')
            data=json.loads(raw.read_text());fmt=data.get('format',{})
            row['format']={k:fmt.get(k) for k in ['format_name','duration','start_time','bit_rate','size']}
            row['streams']=[{k:v for k,v in stream.items() if k not in ('filename','url')} for stream in data.get('streams',[])]
            video=next((s for s in row['streams'] if s.get('codec_type')=='video' and not s.get('disposition',{}).get('attached_pic')),None)
            if not video or not video.get('width') or not video.get('height'):raise ValueError('video_not_inspected')
            if float(fmt.get('duration',0))<30:raise ValueError('source_too_short')
            row['status']='valid'
        except Exception as error:
            row['failure']=str(error) if isinstance(error,ValueError) else type(error).__name__
        row['elapsed_ms']=round((time.monotonic()-started)*1000,2)
        target.write_text(json.dumps(row,indent=2));return row
    with ThreadPoolExecutor(max_workers=2) as pool:rows=list(pool.map(inspect,items))
    result=dict(planned=len(items),inspected=len(rows),valid=sum(r['status']=='valid' for r in rows),results=rows)
    (args.output/'summary.json').write_text(json.dumps(result,indent=2))
    print(json.dumps({k:v for k,v in result.items() if k!='results'}))

if __name__=='__main__':main()
