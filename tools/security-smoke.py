"""Loopback streaming smoke test; no audio, no recording, no real friend identities."""
import base64, queue, re, socket, subprocess, sys, threading, time

sys.stdout.reconfigure(encoding="utf-8")
exe = sys.argv[1]
for friends in (False, True):
    events = queue.Queue()
    procs = {}
    def launch(who, args):
        p = subprocess.Popen([exe, *args, '--ipc'], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT, text=True, encoding='utf-8', errors='replace', bufsize=1,
            creationflags=subprocess.CREATE_NO_WINDOW)
        procs[who] = p
        def read():
            for line in p.stdout:
                events.put((who, line.strip()))
        threading.Thread(target=read, daemon=True).start()
        return p
    def send(who, line):
        p=procs[who]
        p.stdin.write(line+'\n'); p.stdin.flush()
    try:
        launch('host', ['host', '--monitor', '1', '--fps', '30', '--no-audio'])
        launch('view', ['client', '--windowed', '--no-audio'])
        key='ab'*32
        if friends:
            send('view', 'MEDIAKEY '+key)
        connected=set(); stats=[]; failures=[]; host_code=None
        until=time.monotonic()+65
        while time.monotonic()<until and len(stats)<4:
            try: who,line=events.get(timeout=.2)
            except queue.Empty: continue
            if line.startswith('@CODE '):
                if who=='host':
                    host_code=line[6:]
                    if friends: send('host','MEDIAKEY '+key)
                send('view' if who=='host' else 'host',line[6:])
            elif line=='@CONNECTED': connected.add(who)
            elif who=='view' and line.startswith('@STATS '):
                stats.append(line[7:])
                if len(stats)==1:
                    # An observer sees this token, but a forged clear disconnect must do nothing.
                    raw=base64.urlsafe_b64decode(host_code[1:]+'===')
                    off=38 if raw[0]==2 else 6
                    ip=socket.inet_ntoa(raw[off:off+4]); port=int.from_bytes(raw[off+4:off+6],'big')
                    with socket.socket(socket.AF_INET,socket.SOCK_DGRAM) as s:
                        for _ in range(3): s.sendto(bytes([0xA3])+raw[2:6],(ip,port))
            elif line.startswith('@ERROR') or 'panicked at' in line:
                failures.append((who,line))
        assert connected=={'host','view'}, ('connection failed',connected,failures)
        assert len(stats)>=4, ('stream failed after forged BYE',stats,failures)
        assert not failures, failures
        send('host','STOP')
        procs['host'].wait(timeout=12)
        procs['view'].wait(timeout=12)
        print(('friend-IPC' if friends else 'ordinary')+': connected, frames received after forged BYE, authenticated host STOP disconnected viewer')
        print('last stats:',stats[-1])
    finally:
        for p in procs.values():
            if p.poll() is None:
                try: p.stdin.write('STOP\n');p.stdin.flush();p.wait(timeout=5)
                except (OSError,subprocess.TimeoutExpired): p.kill();p.wait(timeout=5)
