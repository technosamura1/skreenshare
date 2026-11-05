"""Local moving-window stream benchmark. No relay, real identities, audio or screenshots.
Usage: python tools/perf-smoke.py EXE [seconds] [output.json] [window|screen]
The source requests ~120 updates/s; host requests 1440p60; viewer renders fullscreen.
The test-only SS_PERF_EXCLUDE_VIEWER switch excludes the viewer from monitor capture.
"""
import ctypes, json, os, queue, subprocess, sys, tempfile, threading, time, tkinter as tk
from pathlib import Path
mode=sys.argv[4] if len(sys.argv)>4 else 'window'
if mode not in ('window','screen'):raise SystemExit('mode must be window or screen')
exe=str(Path(sys.argv[1]).resolve(strict=True)); seconds=int(sys.argv[2]) if len(sys.argv)>2 else 30
out=Path(sys.argv[3]) if len(sys.argv)>3 else Path('perf-result.json')
ctypes.windll.user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))
root=tk.Tk(); root.title('screenshare local 1440p performance source');root.overrideredirect(True)
root.geometry('2560x1440+0+0'); canvas=tk.Canvas(root,width=2560,height=1440,bg='#14202c',highlightthickness=0);canvas.pack()
for x in range(0,2560,80):
 for y in range(0,1440,80):
  canvas.create_rectangle(x,y,x+40,y+40,fill='#34485c' if (x+y)%160 else '#52687c',outline='')
bar=canvas.create_rectangle(0,0,100,1440,fill='#df943a',outline='')
label=canvas.create_text(1280,720,text='2560 x 1440 | moving source',fill='white',font=('Arial',40))
root.update()
ctypes.windll.user32.GetAncestor.restype=ctypes.c_void_p
hwnd=ctypes.windll.user32.GetAncestor(ctypes.c_void_p(root.winfo_id()),2)
ctypes.windll.winmm.timeBeginPeriod(1)
q=queue.Queue();procs={}; rows=[];errors=[];messages=[];start=time.monotonic();connected=set(); source_frames=0;run_start=None
profile=Path(tempfile.mkdtemp(prefix='skreenshare-perf-'))
env=os.environ.copy();env['SS_PERF_EXCLUDE_VIEWER']='1';env['SCREENSHARE_PROFILE']=str(profile);env['LOCALAPPDATA']=str(profile)
def launch(name,args):
 p=subprocess.Popen([exe,*args,'--ipc'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,encoding='utf-8',errors='replace',bufsize=1,env=env,creationflags=subprocess.CREATE_NO_WINDOW)
 procs[name]=p
 def read():
  for line in p.stdout:q.put((name,line.strip()))
 threading.Thread(target=read,daemon=True).start()
def send(name,line):
 p=procs[name]
 if p.poll() is None:p.stdin.write(line+'\n');p.stdin.flush()
def tick():
 global source_frames,run_start
 t=time.monotonic();source_frames+=1
 x=int(((t-start)*720) % 2460);canvas.coords(bar,x,0,x+100,1440)
 canvas.itemconfigure(label,text=f'2560 x 1440 | source frame {source_frames}')
 while True:
  try:who,line=q.get_nowait()
  except queue.Empty:break
  if line.startswith('@CODE '):send('view' if who=='host' else 'host',line[6:])
  elif line=='@CONNECTED':
   connected.add(who)
   if len(connected)==2 and run_start is None:run_start=t
  elif line.startswith(('@STATS ','@DIAG ','@ENCODER ')):
   rows.append({'side':who,'elapsed':round(t-start,3),'text':line})
  elif line.startswith('@ERROR') or 'panicked at' in line:errors.append((who,line))
  elif ('NVENC' in line or 'HDR' in line or 'Цвет и масштаб' in line or 'Декодирование' in line):messages.append((who,line))
 if errors or (run_start and t-run_start>=seconds) or t-start>seconds+60:root.quit();return
 root.after(8,tick)
try:
 launch('host',['host',*(['--window',str(hwnd)] if mode=='window' else []),'--monitor','0','--fps','60','--bitrate','45','--preset','p4','--max-send','90','--no-audio'])
 launch('view',['client','--no-audio'])
 root.after(8,tick);root.mainloop()
finally:
 for who,p in procs.items():
  if p.poll() is None:
   try:send(who,'STOP');p.wait(timeout=5)
   except (OSError,subprocess.TimeoutExpired):p.kill();p.wait(timeout=5)
 root.destroy();ctypes.windll.winmm.timeEndPeriod(1)
 if not any('картинка 2560x1440 → 2560x1440' in x['text'] for x in rows):errors.append(('test','expected 2560x1440 input AND fullscreen output; check primary monitor resolution'))
 result={'connected':sorted(connected),'errors':errors,'messages':messages,'source_updates':source_frames,'elapsed':round(time.monotonic()-start,3),'rows':rows}
 out.write_text(json.dumps(result,ensure_ascii=False,indent=2),encoding='utf-8')
 print(json.dumps(result,ensure_ascii=False,indent=2))
 if errors or len(connected)!=2:sys.exit(1)
