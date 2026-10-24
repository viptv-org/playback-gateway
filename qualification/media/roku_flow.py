#!/usr/bin/env python3
"""Exercise existing Roku backend requests and decode real HTTPS HLS off-server.

This is a wire/profile qualification driver, not a physical Roku decoder.
"""
import contextlib
import hashlib
import json
import os
from pathlib import Path
import queue
import ssl
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


def run(config):
    fixture=config['fixture'];origin=config['origin'];seconds=config['seconds']
    result=dict(fixture=fixture['id'],driver='roku-api',status='failed',events=[],metrics={},limitations=['Roku request/profile emulation plus real FFmpeg decoding; no physical Roku rendering or speaker observation.'])
    result['driver_sha256']=hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    ca=Path(config['request_ca']);tls=ssl.create_default_context(cafile=str(ca))
    client=fixture.get('client',dict(platform='roku',can_play_direct=False,max_width=1920,max_height=1080,video_codecs=['h264'],audio_codecs=['aac']))
    result['client']=client
    if fixture.get('source_dovi'):
        result['limitations'].append('Dolby Vision sources qualify their compatible HDR10 base layer; dynamic Dolby Vision processing is not asserted.')
    ffmpeg=Path(config.get('consumer_ffmpeg','/usr/bin/ffmpeg'));ffprobe=ffmpeg.with_name('ffprobe')
    result['decoder_sha256']=hashlib.sha256(ffmpeg.read_bytes()).hexdigest()
    result['decoder_version']=subprocess.check_output([str(ffmpeg),'-version'],text=True).splitlines()[0]
    rowpath=Path(config['result']);session=None;decoder=None;worker=None;inspection_thread=None;inspection={};done=threading.Event();renewals=[];renew_errors=[];started=None
    def call(path,method='GET',body=None,timeout=65):
        headers={'Authorization':'Bearer member-token-1'}
        if body is not None:headers['Content-Type']='application/json'
        request=urllib.request.Request(origin+path,method=method,headers=headers,data=json.dumps(body).encode() if body is not None else None)
        try:
            with urllib.request.urlopen(request,context=tls,timeout=timeout) as response:
                data=json.load(response);status=response.status
        except urllib.error.HTTPError as error:
            try:code=json.load(error).get('error_code','request_failed')
            except (ValueError,AttributeError):code='request_failed'
            raise RuntimeError(f'{method} {path.split("?")[0]} {error.code} {code}') from None
        result['events'].append(dict(path=path.split('?')[0],method=method,status=status,wall_ns=time.time_ns()))
        return data
    def playback_event(kind,sequence,position):
        return call('/api/profiles/1/playback-events','POST',dict(attempt_id=session['id'],sequence=sequence,identity=card['identity'],parent_identity=card.get('parent_identity'),event=kind,position_millis=int(position*1000),duration_millis=int((session['delivery'].get('duration') or 0)*1000),ended=False))
    observer=contextlib.nullcontext(None)
    observer_file=config.get('observer_config')
    if observer_file and Path(observer_file).is_file():
        from remote_sample import RemoteSample
        observer=RemoteSample(Path(observer_file),rowpath.parent.name+'/'+fixture['id'])
    try:
        boot=call('/api/bootstrap');assert boot['contract_version']==3
        home=boot.get('home');until=time.monotonic()+40
        while not home or not any(row.get('cards') for row in home.get('rows',[])):
            if time.monotonic()>until:raise RuntimeError('fixture_catalog_not_ready')
            time.sleep(.2);home=call('/api/profiles/1/home')
        cards=[card for row in home['rows'] for card in row.get('cards',[])]
        card=next(card for card in cards if card['identity']['id']==fixture['id'])
        if card.get('metadata_origin'):
            fields=dict(metadata_origin=card['metadata_origin'],card=card)
            query=dict(metadata_origin=json.dumps(card['metadata_origin']),card=json.dumps(card))
        else:
            provider=card['provider_id'];addon=int(provider.split(':')[1]) if provider.startswith('addon:') else None
            fields=dict(metadata_provider_id='stremio' if addon else provider)
            if addon:fields['metadata_addon_id']=addon
            query=dict(provider_id=fields['metadata_provider_id'])
            if addon:query['addon_id']=addon
        identity=card['identity'];quoted=urllib.parse.quote(identity['id'],safe='')
        call('/api/meta/'+identity['kind']+'/'+quoted+'?'+urllib.parse.urlencode(query))
        job=call('/api/streams','POST',dict(type=identity['kind'],id=identity['id'],**fields))
        sources=[];after=0;until=time.monotonic()+65
        while time.monotonic()<until:
            page=call('/api/streams/'+job['id']+'?after='+str(after)+'&wait_ms=10000')
            for event in page['events']:sources.extend(event.get('streams',[]))
            after+=len(page['events'])
            if page.get('done'):break
        if not sources:raise RuntimeError('fixture_source_not_granted')
        ranked=call('/api/profiles/1/source-ranking','POST',dict(ids=[s['id'] for s in sources],device_max_height=client['max_height']))
        source=ranked['streams'][0]
        with observer as sampling:
            started=time.monotonic();started_wall=time.time_ns();result['play_requested_wall_ns']=started_wall
            body=dict(request_id=str(uuid.uuid4()),stream_id=source['id'],client=client,force_gateway=True,position=0,resume=False)
            if fixture.get('multi_audio'):body['audio_track']=2
            if fixture.get('audio_track_override') is not None:body['audio_track']=fixture['audio_track_override']
            if fixture.get('subtitles'):body['subtitle_track']=2
            session=call('/api/playback','POST',body)
            until=time.monotonic()+65
            while session['status']=='starting' and time.monotonic()<until:
                time.sleep(.1);session=call('/api/playback/'+session['id'])
            if session['status']!='ready':raise RuntimeError('playback_'+str(session.get('error_code',session['status'])))
            delivery=session['delivery']
            if delivery['kind']!='gateway' or delivery['format']!='hls':raise RuntimeError('roku_hls_delivery_required')
            result['delivery']={key:delivery.get(key) for key in ['kind','format','live','mode','video_mode','audio_mode','audio_tracks','subtitle_tracks','duration']}
            result['metrics']['backend_ready_ms']=(time.monotonic()-started)*1000
            def renew():
                delay=max(1,session.get('renew_after_seconds',20))
                while not done.wait(delay):
                    try:
                        updated=call('/api/playback/'+session['id']+'/heartbeat','POST',{},timeout=10)
                        if updated.get('status')!='ready' or updated.get('delivery',{}).get('url')!=delivery['url']:raise RuntimeError('renewal_changed_delivery')
                        renewals.append(time.time_ns());delay=max(1,updated.get('renew_after_seconds',20))
                    except Exception as error:renew_errors.append(type(error).__name__);return
            worker=threading.Thread(target=renew,daemon=True);worker.start()
            command=[str(ffmpeg),'-nostdin','-hide_banner','-v','error','-xerror','-re','-tls_verify','1','-ca_file',str(ca),'-threads','2','-live_start_index','0','-i',delivery['url'],'-t',str(seconds),'-map','0:v:0','-map','0:a:0?','-progress','pipe:1','-stats_period','0.1','-f','null','-']
            def inspect_first_segment():
                # A later playlist can begin on a copied non-IDR VOD slice.
                # Observe the actual initial segment while its decoder headers
                # are present, independently of playback and without a seek.
                url=urllib.parse.urljoin(delivery['url'],'segment-000000000.ts')
                probe_command=[str(ffprobe),'-v','error','-tls_verify','1','-ca_file',str(ca),'-analyzeduration','1000000','-probesize','1000000','-show_streams','-of','json','-i',url]
                try:
                    with rowpath.with_suffix('.probe-private.log').open('wb') as log:
                        measured=subprocess.run(probe_command,stdout=subprocess.PIPE,stderr=log,timeout=25)
                    if measured.returncode:inspection['failure']='delivered_stream_inspection_failed'
                    else:inspection['streams']=json.loads(measured.stdout)['streams']
                except Exception:inspection['failure']='delivered_stream_inspection_failed'
            progress=[];first_frame=None;last_out=0;frame_count=0;frame_updates=[]
            with rowpath.with_suffix('.decode-private.log').open('wb') as log:
                decoder=subprocess.Popen(command,stdout=subprocess.PIPE,stderr=log,text=True,bufsize=1)
                lines=queue.Queue(maxsize=1000)
                def receive_progress():
                    for line in decoder.stdout:
                        lines.put((time.monotonic(),line))
                    lines.put((time.monotonic(),None))
                reader=threading.Thread(target=receive_progress,daemon=True);reader.start()
                deadline=time.monotonic()+seconds+70
                while True:
                    if time.monotonic()>deadline:raise TimeoutError('output_decoder_deadline')
                    try:arrived,line=lines.get(timeout=.25)
                    except queue.Empty:continue
                    if line is None:break
                    key,_,value=line.strip().partition('=')
                    if key=='frame':
                        if int(value)>frame_count:frame_updates.append(arrived)
                        frame_count=max(frame_count,int(value))
                        if frame_count>0 and first_frame is None:
                            first_frame=arrived;result['metrics']['first_decoded_frame_ms']=(first_frame-started)*1000
                            ack=call('/api/playback/'+session['id']+'/first-frame','POST',{},timeout=10)
                            if ack.get('ok') is not True:raise RuntimeError('first_frame_ack_failed')
                            playback_event('start',1,0)
                            inspection_thread=threading.Thread(target=inspect_first_segment,daemon=True);inspection_thread.start()
                    if key=='out_time_us':
                        try:last_out=max(last_out,int(value)/1e6)
                        except ValueError:continue
                        progress.append(dict(wall=arrived,position=last_out))
                try:
                    stat=Path('/proc/'+str(decoder.pid)+'/stat').read_text();fields=stat[stat.rfind(')')+2:].split()
                    exact_decoder_cpu=(int(fields[11])+int(fields[12]))/os.sysconf('SC_CLK_TCK')
                except (OSError,ValueError,IndexError):exact_decoder_cpu=None
                decoder.wait(timeout=10)
            result['metrics'].update(decoded_seconds=last_out,decoded_video_frames=frame_count,renewals=len(renewals),decode_elapsed_seconds=time.monotonic()-(first_frame or started))
            result['metrics']['decoder_cpu_seconds']=exact_decoder_cpu
            gaps=[(b-a)*1000 for a,b in zip(frame_updates,frame_updates[1:])]
            result['metrics'].update(max_video_progress_gap_ms=max(gaps,default=0),video_progress_gaps_over_1s=sum(gap>1000 for gap in gaps))
            result['progress']=progress
            if decoder.returncode:raise RuntimeError('output_decode_failed')
            expected=min(seconds,float(delivery.get('duration') or fixture['duration'])-1)
            if last_out<expected*.95:raise RuntimeError('insufficient_decoded_progress')
            if first_frame is None:raise RuntimeError('video_decode_missing')
            performance_failures=['decoded_video_progress_gap'] if any(gap>1000 for gap in gaps) else []
            if seconds>=25 and not renewals:raise RuntimeError('renewal_missing')
            if renew_errors:raise RuntimeError('renewal_failed')
            if inspection_thread:inspection_thread.join(timeout=27)
            if inspection.get('failure') or 'streams' not in inspection:raise RuntimeError('delivered_stream_inspection_failed')
            streams=inspection['streams'];result['output_streams']=streams
            video=next(s for s in streams if s['codec_type']=='video');audio=next((s for s in streams if s['codec_type']=='audio'),None)
            if video['codec_name'] not in client['video_codecs']:raise RuntimeError('output_video_codec_mismatch')
            if video['width']>client['max_width'] or video['height']>client['max_height']:raise RuntimeError('output_dimensions_exceed_roku_profile')
            if fixture.get('width') and fixture.get('height'):
                scale=min(1,client['max_width']/fixture['width'],client['max_height']/fixture['height'])
                expected=(max(2,int(fixture['width']*scale)//2*2),max(2,int(fixture['height']*scale)//2*2))
                if (video['width'],video['height'])!=expected:raise RuntimeError('output_resolution_changed_from_source_fit')
            if fixture.get('audio') and (not audio or audio['codec_name'] not in client['audio_codecs']):raise RuntimeError('output_audio_codec_mismatch')
            expected_audio=fixture.get('audio_track_override',2 if fixture.get('multi_audio') else None)
            if expected_audio is not None and not any(track.get('selected') and track.get('input_index')==expected_audio for track in delivery.get('audio_tracks',[])):raise RuntimeError('selected_audio_mismatch')
            if fixture.get('subtitles'):
                if not any(track.get('selected') and track.get('input_index')==2 for track in delivery.get('subtitle_tracks',[])):raise RuntimeError('selected_subtitle_mismatch')
                import re
                def media_text(url):
                    if urllib.parse.urlsplit(url).netloc!=urllib.parse.urlsplit(origin).netloc:raise RuntimeError('subtitle_delivery_origin_changed')
                    with urllib.request.urlopen(url,context=tls,timeout=15) as response:
                        data=response.read(128*1024+1)
                    if len(data)>128*1024:raise RuntimeError('subtitle_playlist_limit')
                    return data.decode('utf-8')
                master=media_text(delivery['url'])
                match=re.search(r'#EXT-X-MEDIA:TYPE=SUBTITLES[^\n]*URI="([^"]+)"',master)
                if not match:raise RuntimeError('subtitle_rendition_missing')
                playlist_url=urllib.parse.urljoin(delivery['url'],match[1])
                playlist=media_text(playlist_url)
                segment=next(line for line in playlist.splitlines() if line and not line.startswith('#'))
                caption=media_text(urllib.parse.urljoin(playlist_url,segment))
                if not caption.startswith('WEBVTT') or '-->' not in caption:raise RuntimeError('subtitle_payload_missing')
                result['subtitle_payload_verified']=True
            if fixture.get('transfer') in ['smpte2084','arib-std-b67']:
                if delivery.get('video_mode')!='encode':raise RuntimeError('hdr_not_converted')
                if any(video.get(key)!='bt709' for key in ['color_transfer','color_primaries','color_space']):raise RuntimeError('tone_map_sdr_color_tags_missing')
                if video.get('pix_fmt')!='yuv420p':raise RuntimeError('tone_map_not_8bit_sdr')
                if any('mastering display' in str(side).lower() or 'content light' in str(side).lower() or 'dovi' in str(side).lower() for side in video.get('side_data_list',[])):raise RuntimeError('hdr_side_data_leaked_to_sdr')
                result['tone_map_sdr_verified']=True
                result['tone_map_pixel_quality_measured']=False
            playback_event('stop',2,last_out)
            result['compatibility_passed']=True
            result['performance_failures']=performance_failures
            result['status']='failed' if performance_failures else 'passed'
            if performance_failures:result['failure']=';'.join(performance_failures)
        if sampling is not None:
            result['server_resources_file']=sampling.resources_file
            result['server_commands_file']=sampling.resources_file.replace('.resources.json','.private-commands.json')
    except Exception as error:
        # Omit URL-bearing exception strings and any original source response body.
        safe=str(error) if isinstance(error,(RuntimeError,TimeoutError)) else type(error).__name__
        import re
        result['failure']=re.sub(r'https?://\S+','<private-url>',safe)[:1000]
    finally:
        done.set()
        if worker:worker.join(timeout=70)
        if inspection_thread:inspection_thread.join(timeout=27)
        if decoder and decoder.poll() is None:
            decoder.kill();decoder.wait(timeout=10)
        if session and session.get('id'):
            try:
                call('/api/playback/'+session['id'],'DELETE',timeout=10);result['metrics']['releases']=1
            except Exception:result.update(status='failed',release_failure=True)
        result['finished_wall_ns']=time.time_ns()
        rowpath.write_text(json.dumps(result,indent=2))
    print(json.dumps({key:result.get(key) for key in ['fixture','status','failure','metrics']}))
    return 0 if result['status']=='passed' else 1


if __name__=='__main__':
    os.umask(0o077)
    raise SystemExit(run(json.loads(Path(sys.argv[1]).read_text())))
