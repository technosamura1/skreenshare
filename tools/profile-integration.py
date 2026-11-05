# Real launcher polling/curl/AEAD integration, isolated synthetic profiles and localhost relay.
import base64, json, os, subprocess, threading, time, urllib.parse, sys, tempfile
from pathlib import Path
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
from io import BytesIO
from PIL import Image
exe=Path(sys.argv[1]).resolve(strict=True)
root=Path(tempfile.mkdtemp(prefix='screenshare-profile-integration-'))
lock=threading.Lock(); topics={}; counter=0
class Relay(BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def do_POST(self):
  global counter
  body=self.rfile.read(int(self.headers['Content-Length'])).decode()
  with lock:
   counter+=1;m={'id':'m'+str(counter),'time':int(time.time()),'event':'message','message':body}
   topics.setdefault(self.path.strip('/'),[]).append(m)
  self.send_response(200);self.end_headers();self.wfile.write(json.dumps(m).encode())
 def do_GET(self):
  u=urllib.parse.urlparse(self.path);topic=u.path.strip('/').removesuffix('/json');since=urllib.parse.parse_qs(u.query).get('since',['all'])[0]
  with lock: rows=list(topics.get(topic,[]))
  if since!='all':
   ids=[v['id'] for v in rows]
   if since in ids:rows=rows[ids.index(since)+1:]
  self.send_response(200);self.end_headers();self.wfile.write(('\n'.join(json.dumps(v,separators=(",",":")) for v in rows)).encode())
server=ThreadingHTTPServer(('127.0.0.1',0),Relay);threading.Thread(target=server.serve_forever,daemon=True).start()
b64=lambda x:base64.urlsafe_b64encode(x).decode().rstrip('=')
photo=BytesIO();Image.new('RGB',(48,48),(230,160,30)).save(photo,format='JPEG');photo=photo.getvalue()
procs=[]
try:
 for n in (1,2):
  other=3-n; folder=root/str(n)/'screenshare';folder.mkdir(parents=True,exist_ok=True)
  ident=bytes([n]).hex()*16;peer=bytes([other]).hex()*16
  (folder/'identity.txt').write_text('v1\nid '+ident+'\nkey '+bytes([n+10]).hex()*32+'\nname Tester'+str(n)+'\n')
  (folder/'friends.txt').write_text('v1 '+peer+' '+'22'*32+' '+bytes([other+10]).hex()*32+' 0 Tester'+str(other)+'\n')
  wire='v1\n'+b64(('Tester'+str(n)).encode())+'\n'+b64(('Status '+str(n)).encode())+'\n'+str(n)+'\n'+b64(photo)
  (folder/'profiles.txt').write_text(ident+' '+b64(wire.encode())+'\n')
  (folder/'gui.ini').write_text('welcome=1\nsounds=0\n')
  env=os.environ.copy();env['SCREENSHARE_PROFILE']=str(folder);env['LOCALAPPDATA']=str(folder.parent);env['SCREENSHARE_RELAY']='http://127.0.0.1:'+str(server.server_port)
  si=subprocess.STARTUPINFO();si.dwFlags|=subprocess.STARTF_USESHOWWINDOW;si.wShowWindow=7
  procs.append(subprocess.Popen([str(exe)],env=env,startupinfo=si,creationflags=subprocess.CREATE_NO_WINDOW))
 deadline=time.monotonic()+90
 while time.monotonic()<deadline:
  okay=True
  for n in (1,2):
   other=3-n;peer=bytes([other]).hex()*16
   lines=(root/str(n)/'screenshare'/'profiles.txt').read_text().splitlines()
   found=[v.split(' ',1)[1] for v in lines if v.startswith(peer+' ')]
   if not found:okay=False;continue
   wire=base64.urlsafe_b64decode(found[0]+'===').decode().split('\n')
   assert base64.urlsafe_b64decode(wire[1]+'===').decode()=='Tester'+str(other)
   assert base64.urlsafe_b64decode(wire[2]+'===').decode()=='Status '+str(other)
   assert base64.urlsafe_b64decode(wire[4]+'===')==photo
  if okay:
   print('PASS: two minimized launchers exchanged name, status and JPEG avatar through real polling/curl/AEAD; both caches saved.')
   print('Relay messages:',counter);break
  time.sleep(.5)
 else:raise RuntimeError('profile exchange timed out')
finally:
 for p in procs:
  p.terminate();p.wait(timeout=5)
 print("Total relay messages:",counter); server.shutdown()
