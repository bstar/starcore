#!/usr/bin/env python3
"""Real MKV/FLAC -> authenticated ABR HLS -> decoded audio/video proof."""
import argparse,array,json,math,os,re,subprocess,tempfile,urllib.request,urllib.error
from pathlib import Path
from fractions import Fraction
parser=argparse.ArgumentParser();parser.add_argument('--probe');parser.add_argument('--fps',default='24000/1001');args=parser.parse_args()
fps=float(Fraction(args.fps)); step=round(6*fps)/fps
root=Path(__file__).resolve().parents[1]
probe=Path(args.probe) if args.probe else root/'target/release/examples/streaming-probe'
with tempfile.TemporaryDirectory(prefix='star-hls-proof-') as temp:
    work=Path(temp);source=work/'Movie with spaces.mkv'
    subprocess.run(['ffmpeg','-v','error','-f','lavfi','-i',f'testsrc2=size=1280x720:rate={args.fps}','-f','lavfi','-i','sine=frequency=440:sample_rate=48000','-t','14','-c:v','libx264','-preset','ultrafast','-c:a','flac',str(source)],check=True)
    with (work/'server.log').open('w') as log:
        server=subprocess.Popen([str(probe),str(source)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=log,text=True)
        try:
            session=json.loads(server.stdout.readline());base='http://127.0.0.1:'+str(session['port']);token=session['token']
            def get(path,headers=None):
                with urllib.request.urlopen(urllib.request.Request(base+path,headers=headers or {}),timeout=60) as response:return response.read()
            catalog=json.loads(get('/v1/catalog?token='+token));movie=catalog['id'];assert catalog['streaming']['hls']
            try:get('/v1/hls/'+movie+'/master.m3u8');raise AssertionError('Unauthenticated HLS succeeded')
            except urllib.error.HTTPError as error:assert error.code==401
            master=get('/v1/hls/'+movie+'/master.m3u8?token='+token).decode();variants=[line for line in master.splitlines() if line.startswith('/v1/')]
            assert len(variants)==2,master
            assert all('token='+token in url for url in variants)
            for variant in variants:
                playlist=get(variant).decode();assert '#EXT-X-ENDLIST' in playlist and '#EXT-X-DISCONTINUITY' not in playlist
                segments=re.findall(r'^(/v1/.+media.m4s.+)$',playlist,re.M);inits=re.findall(r'URI="([^"]+)"',playlist)
                assert len(segments)==3 and len(inits)==1
                inits=inits*len(segments)
                for index in [0,1,2]:
                    init=get(inits[index]);media=get(segments[index]);assert b'moov' in init and b'moof' in media[:512], (len(init),init[:64],len(media),media[:64])
                    partial=get(segments[index],{'Range':'bytes=0-63'});assert partial==media[:64]
                    clip=work/(variant.split('/')[4]+'-'+str(index)+'.mp4');clip.write_bytes(init+media)
                    info=json.loads(subprocess.check_output(['ffprobe','-v','error','-show_entries','stream=codec_type,codec_name,width,height','-of','json',str(clip)]))
                    assert any(s['codec_name']=='aac' for s in info['streams']),info
                    # Later fragments retain their original timeline; only the
                    # first encode has AAC priming, rather than restarting it.
                    packets=json.loads(subprocess.check_output(['ffprobe','-v','error','-select_streams','a:0','-show_packets','-read_intervals','%+0.05','-of','json',str(clip)]))['packets']
                    if index>0:
                        assert float(packets[0]['pts_time'])>=index*step-0.05, packets[0]
                    decoded=subprocess.run(['ffmpeg','-v','error','-xerror','-i',str(clip),'-f','null','-'],capture_output=True,check=True)
                    assert not decoded.stderr,decoded.stderr.decode()
                    # Some GPU drivers emit visibly corrupt HEVC with no decoder
                    # errors. Compare actual pixels with the source, not exit codes.
                    def thumbnail(path,offset):
                        return subprocess.check_output(['ffmpeg','-v','error','-ss',str(offset),'-i',str(path),'-frames:v','1','-vf','scale=64:64','-pix_fmt','rgb24','-f','rawvideo','-'])
                    expected=thumbnail(source,index*step);actual=thumbnail(clip,0)
                    assert len(expected)==len(actual)==64*64*3
                    mse=sum((a-b)**2 for a,b in zip(expected,actual))/len(expected)
                    psnr=10*math.log10(255**2/max(mse,1e-10))
                    assert psnr>24, f'Corrupt picture: {variant.split("/")[4]} segment {index}: PSNR {psnr:.2f} dB'
                print('Decoded rendition',variant.split('/')[4],': first, next and final/seek segments with AAC audio')
            print('PASS: MKV, incompatible audio conversion, multiple qualities, authentication, byte ranges, discontinuities and seek segments')
        finally:
            server.stdin.close()
            try:server.wait(timeout=15)
            except subprocess.TimeoutExpired:server.kill();server.wait()
            if server.returncode:
                print((work/'server.log').read_text())
