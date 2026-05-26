# Wired tether — camera control surface (WIRE-CONFIRMED 2026-05-23)

Source: full desktop-tether-app walkthrough over **wired gigabit Ethernet** (camera 192.168.4.169

exercised ~every setting in the app. Connection = same PCSS knock + PTP-IP/15740 as Wi-Fi; wired is
faster + smoother (the app's choppiness is its single-threaded UI, not the protocol).

## Big-3 — absolute control via STANDARD PTP props (SetDevicePropValue 0x1016)
The desktop tether sets the Big-3 with standard absolute PTP properties — NOT the reference app's relative
step-ops (0x902C/0x902D). Much cleaner.
- **ISO  = 0x500f** (ExposureIndex), uint32 LE. Literal ISO (e.g. 200,320,2000,3200) or
  0xffffffff/0xfffffffe (-1/-2) = Auto sentinels.
- **Shutter = 0x500d** (ExposureTime), uint32 LE **microseconds** (19686≈1/50s, 99212≈1/10s,
  198425≈1/5s).
- **Aperture = 0x5007** (FNumber), set 5× (standard PTP FNumber = uint16 ×100, e.g. f/2.8=280).
- PASM mode = 0x500e (ExposureProgram).

## Other decoded sets
- 0x5003 ImageSize = PTP string "4000x3000"
- 0x5011 DateTime  = PTP string "20260523T091637"
- 0xd039 (enum, 4 values: 0x00010000/0x00020000/0x1/0x2)
- 0xd395 = PTP strings "x,y,z" adjustment triplets (e.g. "0,0,2","3,0,2","-1,0,2")

## Full property surface touched by the app
SET (0x1016), 36 distinct: 5003 5007 500a 500b 500d 500e 500f 5010 5011 d001 d007 d018 d01b d025
  d026 d02e d039 d16f d171 d174 d1bc d201 d207 d208 d215 d216 d21c d230 d23c d304 d351 d359 d375
  d376 d38c d395
GetDevicePropDesc (0x1014), 46 distinct (descriptors w/ type+range+enum): 5003 5005 500a 500b 500d
  500e 500f 5010 5015 d001 d007 d008 d00a d00b d00c d017 d018 d01c d020 d023 d024 d025 d029 d02e
  d031 d037 d039 d104 d16f d170 d171 d189 d1b8 d1bd d1bf d201 d208 d228 d304 d320 d321 d322 d351
  d359 d38c d395
GetDevicePropValue (0x1015) polled, 48 distinct: 5005 500a 500d d01b d023 d026 d031 d037 d100 d104
  d136 d16f d170 d171 d180 d201 d207 d209 d20c d20d d20e d211 d212 d215 d216 d21c d224 d226 d227
  d235 d23f d33f d347 d34b d365 d366 d36a d36b d36d d372 d374 d375 d376 d38a d38b d38d d38e d395

## Other opcodes in the session
- 0x9018 SDK_GetLiveViewData ×10707 = THIS app's live-view method (vs our 0x101C+GetObject 0x80000001)
- 0x1009 GetObject ×25 = RAW/HEIF downloads; 0x100a GetThumb ×11; 0x100b DeleteObject ×51
- 0x100e ×47, 0x1018 ×13, 0x100c/0x100d ×1 — TODO identify

## TODO (deeper passes available)
- Decode the 46 GetDevicePropDesc RESPONSES → full enum/range per prop (the complete "every menu
  option" map). Capture has them all.
- Characterize 0x9018 live-view (params, frame format, rate over gigabit) — may give a smoother feed
  than our 0x101C loop.
- Map vendor props 0xdxxx to camera menu names (film sim, DR, NR, etc.).
