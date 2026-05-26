import hmac, hashlib, base64, json, secrets, requests, sys, time
requests.packages.urllib3.disable_warnings()
def b64u(b): return base64.urlsafe_b64encode(b).rstrip(b'=').decode()
def mint():
    hdr={"typ":"JWT","alg":"HS256"}
    pl={"fresh":False,"iat":0,"nbf":0,"jti":secrets.token_hex(8),"type":"access","sub":"EEEEEE","exp":2050000000}
    si=b64u(json.dumps(hdr,separators=(',',':')).encode())+'.'+b64u(json.dumps(pl,separators=(',',':')).encode())
    return si+'.'+b64u(hmac.new(b'p9uOH1RX8d',si.encode(),hashlib.sha256).digest())
H={"XLV_Auth":"Bearer "+mint()}; B="http://192.168.4.169"; S=requests.Session(); S.verify=False
def b64j(t):
    try: return json.loads(base64.b64decode(t+"=="))
    except: return None
# curated KNOWN-valid codes (prior responders + spot-read movie props) — never blind range
CODES=[0xD01B,0xD028,0xD037,0xD039,0xD112,0xD136,0xD15A,0xD170,0xD171,0xD173,0xD174,0xD18A,0xD18B,
       0xD1B7,0xD1C6,0xD208,0xD209,0xD20A,0xD211,0xD224,0xD225,0xD226,0xD227,0xD228,0xD229,0xD22A,
       0xD22B,0xD22C,0xD22D,0xD22E,0xD230,0xD247,0xD268,0xD277,0xD27F,0xD304,0xD305,0xD307,0xD02A]
def getv(c):
    try:
        r=S.get(f"{B}/camera/functions/{c:04X}/get",headers=H,timeout=6)
        if r.status_code!=200: return r.status_code,None,None
        g=b64j(r.text); pv=(g or {}).get("property_code_value_list",[]); val=pv[0]["value"] if pv else None
        rc=S.get(f"{B}/camera/functions/{c:04X}/cap",headers=H,timeout=6)
        d=(b64j(rc.text) or {}).get("property_value_desc",[{}])[0] if rc.status_code==200 else {}
        return 200,val,d
    except Exception: return "timeout",None,None
mode=getv(0xD037)[1]
out=open(f"/tmp/xlv_mode_{mode}.jsonl","w")
print(f"=== MODE D037={mode} ===")
for c in CODES:
    st,val,d=getv(c)
    enum=d.get("value") if d else None
    print(f"  0x{c:04X}: {st} val={val} enum={enum} min/max={d.get('minimum_value') if d else ''}/{d.get('maximum_value') if d else ''}")
    out.write(json.dumps({"code":f"0x{c:04X}","status":st,"value":val,"cap":d})+"\n")
out.close(); print(f"saved /tmp/xlv_mode_{mode}.jsonl")
