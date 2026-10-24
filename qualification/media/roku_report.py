#!/usr/bin/env python3
"""Summarize private Roku qualification receipts without publishing source URLs."""
import argparse
import csv
from collections import Counter
import hashlib
import json
from pathlib import Path
import re
import statistics


def read(path):
    try:return json.loads(path.read_text())
    except (OSError,ValueError):return None


def distribution(values):
    values=sorted(float(x) for x in values if x is not None)
    if not values:return dict(count=0)
    def quantile(q):
        position=(len(values)-1)*q;low=int(position);high=min(low+1,len(values)-1)
        return values[low]+(values[high]-values[low])*(position-low)
    return dict(count=len(values),median=statistics.median(values),p95=quantile(.95),maximum=max(values))


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args();root=args.root;job=read(root/'intel-job.json') or {};local=Path(job.get('local_run',root/'intel-qsv-hdr-six'))
    definitions=[('roku-roku50','manifest.json'),('roku-owned-formats','owned-formats.json'),('roku-roku4k','roku4k.json'),('roku-roku720','roku720.json'),('roku-roku-long-copy','roku-long-copy.json'),('roku-post-qualification','post-campaign.json')]
    rows=[];phases=[];secrets=set()
    for phase,manifest_name in definitions:
        manifest=read(root/'corpus'/manifest_name)
        if not manifest:
            phases.append(dict(phase=phase,complete=False,planned=None));continue
        fixtures=manifest['fixtures'];summary=read(local/phase/'summary.json') or {};campaign=read(local/phase/'campaign.json') or {};phase_rows=[]
        for fixture in fixtures:
            url=fixture.get('url','')
            if '/resolve/torbox/' in url:secrets.add(url.split('/resolve/torbox/')[1].split('/')[0])
            row=read(local/phase/(fixture['id']+'.json'))
            if not row:
                phase_rows.append(dict(case_id=fixture['id'],status='not_run'));continue
            metrics=row.get('metrics',{});delivery=row.get('delivery',{});streams=row.get('output_streams',[])
            selected=[track for track in delivery.get('audio_tracks',[]) if track.get('selected')]
            record=dict(phase=phase,case_id=fixture['id'],source_id=fixture.get('source_id',fixture.get('validation_id',fixture['id'])),title=fixture.get('title',fixture['id']),input=dict(container=fixture['container'],video=fixture['video'],audio=fixture.get('audio'),width=fixture.get('width'),height=fixture.get('height'),transfer=fixture.get('transfer')),profile=row.get('client'),status=row['status'],compatibility_passed=row.get('compatibility_passed') is True,performance_failures=row.get('performance_failures',[]),failure=re.sub(r'https?://\S+','<private-url>',str(row.get('failure',''))) or None,capacity_reclaimed=row.get('capacity_reclaimed'),tone_map_sdr_verified=row.get('tone_map_sdr_verified') is True,subtitle_payload_verified=row.get('subtitle_payload_verified') is True,selected_audio_codecs=[track['codec'] for track in selected],first_decoded_frame_ms=metrics.get('first_decoded_frame_ms'),backend_ready_ms=metrics.get('backend_ready_ms'),decoded_media_seconds=metrics.get('decoded_seconds'),decode_elapsed_seconds=metrics.get('decode_elapsed_seconds'),max_decoded_video_progress_gap_ms=metrics.get('max_video_progress_gap_ms'),renewals=metrics.get('renewals'),releases=metrics.get('releases'),driver_source_matches=row.get('driver_sha256')==campaign.get('harness_sha256',{}).get('roku_flow.py'))
            record['output_streams']=[{k:s.get(k) for k in ['codec_type','codec_name','profile','level','width','height','pix_fmt','color_transfer','color_primaries','color_space','channels','sample_rate']} for s in streams if s.get('codec_type') in ['audio','video']]
            commands=read(local/phase/(fixture['id']+'.private-commands.json')) or {};pipelines=[]
            for pid,argv in commands.items():
                if not argv or Path(argv[0]).name!='ffmpeg' or not argv[-1].endswith('index.m3u8') or '-i' not in argv:continue
                before=argv[:argv.index('-i')];after=argv[argv.index('-i')+2:]
                def option(values,name):return values[values.index(name)+1] if name in values else None
                filters=option(after,'-vf') or ''
                pipelines.append(dict(pid=int(pid),input_acceleration=option(before,'-hwaccel'),explicit_decoder=option(before,'-c:v'),encoder=option(after,'-c:v'),cpu_mobius_tone_map='tonemap=tonemap=mobius' in filters,hardware_downloads=filters.count('hwdownload'),hardware_uploads=filters.count('hwupload')))
            record['observed_pipeline_attempts']=pipelines
            phase_rows.append(record);rows.append(record)
        phases.append(dict(phase=phase,planned=len(fixtures),completed=sum(x['status']!='not_run' for x in phase_rows),compatibility_passed=sum(x.get('compatibility_passed') is True for x in phase_rows),strict_passed=sum(x['status']=='passed' for x in phase_rows),failed=sum(x['status']=='failed' for x in phase_rows),not_run=sum(x['status']=='not_run' for x in phase_rows),complete=summary.get('planned')==len(fixtures) and summary.get('not_run')==0))
    primary=[row for row in rows if row['phase']=='roku-roku50'];provenance=read(local/'provenance.json') or {}
    source=read(root/'corpus/manifest.json') or dict(fixtures=[]);source=source['fixtures']
    report=dict(complete=all(phase['complete'] for phase in phases),accepted=False,qualification='Existing Roku request/profile emulation, real backend and gateway, trusted HTTPS, paced independent output decoding. Physical Roku rendering is not asserted.',candidate_sha256=provenance.get('binary_sha256'),image_id=provenance.get('image_id'),cpu_budget=provenance.get('cpu_limit'),primary=dict(planned=50,completed=len(primary),compatibility_passed=sum(row['compatibility_passed'] for row in primary),strict_passed=sum(row['status']=='passed' for row in primary),performance_failures=sum(bool(row['performance_failures']) for row in primary),other_failures=sum(row['status']=='failed' and not row['performance_failures'] for row in primary),hdr_sdr_verified=sum(row['tone_map_sdr_verified'] for row in primary),first_decoded_frame_ms=distribution(row['first_decoded_frame_ms'] for row in primary)),input_coverage=dict(video_codecs=dict(Counter(f['video'] for f in source)),containers=dict(Counter(f['container'] for f in source)),pq_hdr=sum(f.get('transfer')=='smpte2084' for f in source),ten_bit=sum(f.get('pix_fmt')=='yuv420p10le' for f in source),dolby_vision_hdr10_base_layers=sum(bool(f.get('source_dovi')) for f in source)),selected_audio_codecs=dict(Counter(codec for row in rows if row['compatibility_passed'] for codec in row['selected_audio_codecs'])),phases=phases,cases=rows,limitations=['Paced decoder progress gaps are recorded as performance failures; browser rendering is measured separately.','External network timings are functional observations, not controlled before/after performance gains.','Dolby Vision coverage verifies compatible HDR10 base-layer conversion, not dynamic Dolby Vision processing.','Every failed attempt is retained; post-campaign retries do not overwrite primary failures.'])
    report['accepted']=report['complete'] and all(row['status']=='passed' and row['capacity_reclaimed'] and row['driver_source_matches'] for row in rows)
    primary_ids={row['source_id'] for row in primary}
    resolved={row['source_id'] for row in primary if row['compatibility_passed']}
    retained_retries=[row for row in rows if row['phase']=='roku-post-qualification' and row['case_id'].endswith(('-retry','-probe-retry'))]
    resolved.update(row['source_id'] for row in retained_retries if row['compatibility_passed'] and row['source_id'] in primary_ids)
    report['primary']['resolved_compatible_sources_after_retakes']=len(resolved)
    report['primary']['retakes']=len(retained_retries)
    report['primary']['initial_failures_remain_recorded']=True
    report['totals']=dict(trials=len(rows),strict_passed=sum(row['status']=='passed' for row in rows),compatibility_passed=sum(row['compatibility_passed'] for row in rows),performance_failures=sum(bool(row['performance_failures']) for row in rows),all_capacity_reclaimed=all(row['capacity_reclaimed'] is True for row in rows),driver_integrity=all(row['driver_source_matches'] for row in rows))
    encoded=json.dumps(report,indent=2)+'\n'
    assert not any(secret and secret in encoded for secret in secrets),'source credential in report'
    assert '/resolve/torbox/' not in encoded,'credential-bearing source reference in report'
    args.output.write_text(encoded)
    with args.output.with_suffix('.failures.csv').open('w',newline='') as output:
        writer=csv.DictWriter(output,fieldnames=['phase','case_id','title','failure','compatibility_passed','first_decoded_frame_ms','max_decoded_video_progress_gap_ms','capacity_reclaimed'])
        writer.writeheader()
        for row in rows:
            if row['status']=='failed':writer.writerow({key:row.get(key) for key in writer.fieldnames})
    text=['Roku playback qualification','',f"Complete: {report['complete']}; accepted: {report['accepted']}.",f"Primary 50: {report['primary']['completed']} attempted, {report['primary']['compatibility_passed']} compatible decoded outputs, {report['primary']['strict_passed']} strict passes, {report['primary']['performance_failures']} performance failures, {report['primary']['other_failures']} other failures.",'','| Stage | Planned | Compatible | Strict passes | Failed | Not run |','|---|---:|---:|---:|---:|---:|']
    text.extend(f"| {p['phase']} | {p.get('planned')} | {p.get('compatibility_passed',0)} | {p.get('strict_passed',0)} | {p.get('failed',0)} | {p.get('not_run','pending')} |" for p in phases)
    text+=['',f"Primary compatibility after retained retakes: {len(resolved)}/50 distinct sources. Initial failures remain recorded.",'These are Roku wire/profile and output-decoding checks. Physical Roku rendering is not asserted. External timings are not controlled optimization comparisons. All failures remain visible.']
    args.output.with_suffix('.md').write_text('\n'.join(text)+'\n')
    print(json.dumps({key:report[key] for key in ['complete','accepted','primary','selected_audio_codecs']}))


if __name__=='__main__':main()
