import sys, time, struct, requests

requests.packages.urllib3.disable_warnings()
from xlv_socketio import XLVSocket as SioSession
TOK="eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.eyJmcmVzaCI6ZmFsc2UsImlhdCI6MTc3OTYzMzYzMywianRpIjoiNDZmNGZmZTItZjAzOS00OTgzLWE2MzQtNjMxMzgzMzkxZDhmIiwidHlwZSI6ImFjY2VzcyIsInN1YiI6IkVFRUVFRSIsIm5iZiI6MTc3OTYzMzYzMywiZXhwIjoxNzc5NjM1NDMzfQ.vPVsPbzUf-tLAlh5LtV6_PM1XYC-5Y7Mr74qMVgW1nc"
BASE="http://192.168.4.169"; H={"XLV_Auth":"Bearer "+TOK}
S=requests.Session(); S.verify=False
def jdims(b):
    i=2
    while i<len(b)-9:
        if b[i]!=0xFF: i+=1; continue
        if b[i+1] in (0xC0,0xC1,0xC2,0xC3): return struct.unpack(">H",b[i+7:i+9])[0],struct.unpack(">H",b[i+5:i+7])[0]
        i+=2+struct.unpack(">H",b[i+2:i+4])[0]
    return None
def setp(c,v):
    try: return S.post(f"{BASE}/camera/functions/{c}/set",headers=H,json={"value":v},timeout=8).status_code
    except Exception as e: return "err"
def measure(sec=3.0):
    try: r=S.get(f"{BASE}/camera/functions/liveview",params={"xlrat":TOK},stream=True,timeout=10)
    except Exception as e: return "stream-err"
    buf=bytearray(); fr=[]; t0=time.time()
    for ch in r.iter_content(8192):
        buf+=ch
        while True:
            s=buf.find(b'\xff\xd8'); e=buf.find(b'\xff\xd9',s+2) if s>=0 else -1
            if s>=0 and e>=0: j=bytes(buf[s:e+2]); del buf[:e+2]; fr.append((len(j),jdims(j)))
            else: break
        if time.time()-t0>sec: break
    r.close(); dt=time.time()-t0; n=len(fr)
    if not n: return "NO FRAMES"
    avg=sum(f[0] for f in fr)//n
    d=fr[0][1]
    return f"{d[0]}x{d[1]}  {n/dt:.1f}fps  {avg//1024}KB/fr  ~{avg*8*(n/dt)/1e6:.1f}Mbps"
for prio,label in ((1,"REALTIME"),(0,"QUALITY")):
    print(f"\n===== FpsValue={prio} ({label} priority) =====")
    try:
        with SioSession(BASE, TOK, init_fps_priority=prio) as s:
            time.sleep(1.0)
            print("  default          :", measure())
            for sz in (1,2,3):
                print(f"  size D174={sz} set={setp('D174',sz)} :", measure(2.5))
            setp("D174",1)
            for q in (1,2,3):
                print(f"  qual D173={q} set={setp('D173',q)} :", measure(2.5))
            setp("D173",3)
    except Exception as e:
        print("  session err:", repr(e)[:120])
