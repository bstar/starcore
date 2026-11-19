#!/usr/bin/env python3
"""Measure decoded flash/beep sync across sustained HLS, seeking and renditions."""
import argparse,array,json,math,re,subprocess,tempfile,urllib.request,os,socket,time
from pathlib import Path
parser=argparse.ArgumentParser(); parser.add_argument('--probe',required=True);parser.add_argument('--fps',default='24000/1001');parser.add_argument('--output');args=parser.parse_args()
from fractions import Fraction
fps=float(Fraction(args.fps));step=round(6*fps)/fps

def run(command,**kwargs): return subprocess.check_output(command,**kwargs)
def measure(path,minimum,reference=None,clock_origin=0):
    video=json.loads(run(['ffprobe','-v','error','-select_streams','v:0','-show_frames','-show_entries','frame=best_effort_timestamp_time','-of','json',str(path)]))['frames']
    pixels=run(['ffmpeg','-v','error','-i',str(path),'-an','-vf','scale=8:8','-pix_fmt','gray','-fps_mode','passthrough','-f','rawvideo','-'])
    assert len(pixels)//64==len(video),(len(pixels)//64,len(video))
    video_onsets=[];white=False
    for i,frame in enumerate(video):
        chunk=pixels[i*64:(i+1)*64]; lit=sum(chunk)/max(1,len(chunk))>128
        if lit and not white:video_onsets.append(float(frame['best_effort_timestamp_time']))
        white=lit
    # Packet-level continuity must be exact, including at HLS joins. An
    # accumulated sample gap can be hidden by measuring only the first clip.
    packets=json.loads(run(['ffprobe','-v','error','-select_streams','a:0','-show_packets','-show_entries','packet=pts,duration','-of','json',str(path)]))['packets']
    if path.suffix!='.mkv':  # Matroska source timestamps are millisecond-quantized.
        for previous,current in zip(packets,packets[1:]):
            assert current['pts']==previous['pts']+previous['duration'],(path,'audio timestamp discontinuity',previous,current)
    audio=json.loads(run(['ffprobe','-v','error','-select_streams','a:0','-show_frames','-show_entries','frame=best_effort_timestamp_time','-of','json',str(path)]))['frames']
    origin=float(audio[0]['best_effort_timestamp_time'])
    samples=array.array('f',run(['ffmpeg','-v','error','-i',str(path),'-vn','-ac','1','-ar','48000','-f','f32le','-']))
    audio_onsets=[];last=-10
    for i in range(0,len(samples)-128,128):
        level=math.sqrt(sum(v*v for v in samples[i:i+128])/128)
        time=origin+i/48000
        if level>0.04 and time-last>.5:audio_onsets.append(time);last=time
    # Compare matching events by source-second, excluding partial edge pulses.
    pairs=[];events={}
    for t in video_onsets[1:-1]:
        nearest=min(audio_onsets,key=lambda a:abs(a-t))
        pairs.append(nearest-t);events[round(nearest-clock_origin)]=nearest-t
    assert len(pairs)>=minimum,(path,len(pairs),video_onsets,audio_onsets)
    worst=max(abs(v) for v in pairs)
    drift=abs(sum(pairs[-min(10,len(pairs)):])/min(10,len(pairs))-sum(pairs[:min(10,len(pairs))])/min(10,len(pairs)))
    assert worst<1/fps+.025,(path,'A/V offset',worst,'video',video_onsets,'audio',audio_onsets,'origin',origin)
    assert drift<1/fps+.005,(path,'accumulated drift',drift)
    if reference is not None:
        errors=[offset-reference[event] for event,offset in sorted(events.items()) if event in reference]
        assert len(errors)>=minimum,(path,'Missing reference events')
        assert max(abs(v) for v in errors)<.008,(path,'A/V timing changed relative to source',errors)
        print(path.name,'events',len(errors),'max sync error relative to source ms',round(max(abs(v) for v in errors)*1000,2),flush=True)
    else:
        print(path.name,'reference events',len(pairs),'frame quantization ms',round(worst*1000,2),flush=True)
    return events

with tempfile.TemporaryDirectory(prefix='star-sync-proof-') as temp:
    work=Path(args.output or temp);work.mkdir(parents=True,exist_ok=True);source=work/'flash-beep.mkv'
    pulse='0.12*sin(2*PI*1000*t)*lt(mod(t\\,1)\\,0.08)'
    channels='|'.join([pulse,pulse,pulse,'0',pulse,pulse])
    subprocess.run(['ffmpeg','-v','error','-y','-f','lavfi','-i',f"color=c=black:s=960x540:r={args.fps},drawbox=c=white:t=fill:enable='lt(mod(t,1),0.08)'",'-f','lavfi','-i',f'aevalsrc={channels}:s=48000:c=5.1','-t','180','-c:v','libx264','-preset','ultrafast','-c:a','flac',str(source)],check=True)
    reference=measure(source,170)
    with (work/'server.log').open('w') as log:
        runtime=work/'runtime';runtime.mkdir(mode=0o700,exist_ok=True)
        server=subprocess.Popen([str(Path(args.probe).resolve()),str(source)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=log,text=True,env={**os.environ,'XDG_RUNTIME_DIR':str(runtime)})
        try:
            session=json.loads(server.stdout.readline());base='http://127.0.0.1:'+str(session['port']);token=session['token']
            def get(path):return urllib.request.urlopen(base+path,timeout=90).read()
            catalog=json.loads(get('/v1/catalog?token='+token));id=catalog['id']
            master=get('/v1/hls/'+id+'/master.m3u8?token='+token).decode()
            variants=[line for line in master.splitlines() if line.startswith('/v1/')];assert len(variants)>=2
            def cache_status():
                endpoint=next((runtime/'starkit-streaming').glob('*.ctl'))
                with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as client:
                    client.settimeout(2);client.connect(str(endpoint));client.sendall(b'{"version":1,"op":"status"}\n')
                    data=bytearray()
                    while b'\n' not in data:data.extend(client.recv(65536))
                return json.loads(data)['application']['hls']['movies'][0]
            first=get(variants[0]).decode();initial=re.findall(r'URI="([^"]+)"',first)[0]
            get(initial);time.sleep(1)
            before=cache_status();time.sleep(.3);after=cache_status()
            assert 1<=after['cached_segments']<=4,after
            assert before['cached_segments']==after['cached_segments'],'Encoder did not pause its read-ahead'
            assert after['cache_bytes']<=384*1024*1024,after
            # Decode the playlist as a whole: isolated fragment checks miss drift.
            capture=work/'continuous.mp4'
            subprocess.run(['ffmpeg','-v','error','-y','-i',base+variants[0],'-t','120','-map','0:v:0','-map','0:a:0','-c','copy',str(capture)],check=True,timeout=240)
            measure(capture,110,reference)
            audio_phases=set()
            for quality,indices in [(variants[0],[25,26,3]),(variants[1],[5,6])]:
                playlist=get(quality).decode();assert '#EXT-X-DISCONTINUITY' not in playlist
                media=re.findall(r'^(/v1/.+media.m4s.+)$',playlist,re.M);inits=re.findall(r'URI="([^"]+)"',playlist)
                assert len(inits)==1;inits=inits*len(media)
                for index in indices:
                    clip=work/f'{quality.split("/")[4]}-seek-{index}.mp4';clip.write_bytes(get(inits[index])+get(media[index]))
                    measure(clip,3,reference,clock_origin=1)
                    audio_packets=json.loads(run(['ffprobe','-v','error','-select_streams','a:0','-show_packets','-show_entries','packet=pts','-of','json',str(clip)]))['packets']
                    audio_phases.add(audio_packets[0]['pts']%1024)
                    packets=json.loads(run(['ffprobe','-v','error','-select_streams','v:0','-show_packets','-show_entries','packet=pts_time','-of','json',str(clip)]))['packets']
                    assert abs(float(packets[0]['pts_time'])-(1+index*step))<.06,packets[0]
            assert len(audio_phases)==1,('Renditions use different AAC sample grids',audio_phases)
            print('PASS: 120-second flash/beep sync, fractional frame rate, 5.1 downmix, forward/backward uncached seek, rendition switch and absolute timestamps',flush=True)
        finally:
            server.stdin.close()
            try:server.wait(timeout=10)
            except subprocess.TimeoutExpired:server.kill();server.wait()
            if server.returncode:print((work/'server.log').read_text())
