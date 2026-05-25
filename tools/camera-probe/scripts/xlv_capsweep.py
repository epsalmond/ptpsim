import importlib.util, requests, base64, json, time
requests.packages.urllib3.disable_warnings()

fj=importlib.util.module_from_spec(spec); spec.loader.exec_module(fj)
TOK=fj.forge(b'p9uOH1RX8d20260524134655', sub='EEEEEE')
H={"XLV_Auth":"Bearer "+TOK}; B="http://192.168.4.169"
S=requests.Session(); S.verify=False
def b64j(t):
    try: return json.loads(base64.b64decode(t+"=="))
    except: return None
out=open("/tmp/xlv_capsweep.jsonl","w")
resp=[]; t0=time.time()
for code in range(0xD000,0xE000):
    h=f"{code:04X}"
    try: r=S.get(f"{B}/camera/functions/{h}/get",headers=H,timeout=3)
    except Exception: continue
    if r.status_code!=200: continue
    g=b64j(r.text); pv=(g or {}).get("property_code_value_list",[]) if g else []
    val=pv[0]["value"] if pv else None
    try: rc=S.get(f"{B}/camera/functions/{h}/cap",headers=H,timeout=3); cap=b64j(rc.text) if rc.status_code==200 else {"status":rc.status_code}
    except Exception: cap={"err":1}
    rec={"code":f"0x{h}","value":val,"cap":cap}
    out.write(json.dumps(rec)+"\n"); out.flush(); resp.append(h)
out.close()
print(f"DONE {len(resp)} responders in {time.time()-t0:.0f}s")
print("responders:", " ".join(resp))
