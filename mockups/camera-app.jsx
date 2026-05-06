/* global React */
// Camera control — main App component (used by all 3 platforms)
// Props:
//   variant: 'mobile' | 'desktop'  — controls layout density and chrome
//   compact: boolean               — extra-tight (small phones)
//   showHistogram, showGrid, showLevel, showShutter — extras flags
//   initialState: 'disconnected' | 'scanning' | 'pairing' | 'gps' | 'wifi-warning' | 'wifi' | 'live'
//   accent: string (CSS color)     — accent override
//   onStateChange?: (s) => void

const ISO_VALUES   = ['L 100', '125', '160', '200', '250', '320', '400', '500', '640', '800', '1000', '1250', '1600', '2000', '2500', '3200', '4000', '5000', '6400', 'H 12800'];
const SHUTTER_VALUES = ['30"', '15"', '8"', '4"', '2"', '1"', '1/2', '1/4', '1/8', '1/15', '1/30', '1/60', '1/125', '1/250', '1/500', '1/1000', '1/2000', '1/4000', '1/8000'];
const APERTURE_VALUES = ['F1.4', 'F1.8', 'F2.0', 'F2.8', 'F3.5', 'F4.0', 'F5.6', 'F6.3', 'F8.0', 'F11', 'F13', 'F16', 'F22'];

const SETTING_DEFS = {
  iso:      { label: 'ISO',   sub: 'Sensitivity', values: ISO_VALUES,      defaultIdx: 7 }, // 500
  shutter:  { label: 'SS',    sub: 'Shutter',     values: SHUTTER_VALUES,  defaultIdx: 12 }, // 1/125
  aperture: { label: 'F',     sub: 'Aperture',    values: APERTURE_VALUES, defaultIdx: 5 }, // F4.0
};

// ─────────────────────────────────────────────────────────────────────
// SettingTile — tap to open modal, press-and-hold to scrub.
// Distinguishes between tap and hold using a 280ms timer.
// ─────────────────────────────────────────────────────────────────────
function SettingTile({ kind, idx, onTap, onHoldStart, active }) {
  const def = SETTING_DEFS[kind];
  const value = def.values[idx];
  const holdTimer = React.useRef(null);
  const startedHold = React.useRef(false);
  const startPos = React.useRef({ x: 0, y: 0 });

  const onPointerDown = (e) => {
    e.preventDefault();
    startedHold.current = false;
    startPos.current = { x: e.clientX, y: e.clientY };
    const target = e.currentTarget;
    try { target.setPointerCapture(e.pointerId); } catch (_) {}
    holdTimer.current = setTimeout(() => {
      startedHold.current = true;
      onHoldStart(kind, e.clientX, e.clientY, e.pointerId, target);
    }, 260);
  };
  const onPointerUp = (e) => {
    if (holdTimer.current) clearTimeout(holdTimer.current);
    if (!startedHold.current) {
      // count as tap
      const dx = e.clientX - startPos.current.x;
      const dy = e.clientY - startPos.current.y;
      if (Math.hypot(dx, dy) < 8) onTap(kind);
    }
    startedHold.current = false;
  };
  const onPointerCancel = () => {
    if (holdTimer.current) clearTimeout(holdTimer.current);
    startedHold.current = false;
  };

  return (
    <div className={`setting${active ? ' active' : ''}`}
      onPointerDown={onPointerDown}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerCancel}
      onContextMenu={(e) => e.preventDefault()}
    >
      <div className="lbl">{def.label}</div>
      <div className="val">{value}</div>
      <div className="sub">{def.sub}</div>
    </div>
  );
}

// ─────────────────────────────────────────────────────────────────────
// HoldStrip — overlay: horizontal value strip that scrubs with finger
// ─────────────────────────────────────────────────────────────────────
function HoldStrip({ kind, idx, onChange, onEnd, startX, pointerId }) {
  const def = SETTING_DEFS[kind];
  const values = def.values;
  const TICK_W = 64;
  const innerRef = React.useRef(null);
  const [currentIdx, setCurrentIdx] = React.useState(idx);
  const startIdx = React.useRef(idx);
  const startXRef = React.useRef(startX);

  React.useEffect(() => {
    const handleMove = (e) => {
      if (pointerId != null && e.pointerId !== pointerId) return;
      const dx = e.clientX - startXRef.current;
      const delta = Math.round(-dx / (TICK_W * 0.85));
      let next = startIdx.current + delta;
      next = Math.max(0, Math.min(values.length - 1, next));
      if (next !== currentIdx) {
        setCurrentIdx(next);
        onChange(next);
      }
    };
    const handleUp = (e) => {
      if (pointerId != null && e.pointerId !== pointerId) return;
      onEnd();
    };
    window.addEventListener('pointermove', handleMove);
    window.addEventListener('pointerup', handleUp);
    window.addEventListener('pointercancel', handleUp);
    return () => {
      window.removeEventListener('pointermove', handleMove);
      window.removeEventListener('pointerup', handleUp);
      window.removeEventListener('pointercancel', handleUp);
    };
  }, [currentIdx, onChange, onEnd, pointerId, values.length]);

  // Position inner so currentIdx tick is centered
  const containerWidth = innerRef.current?.parentElement?.offsetWidth || 0;
  const offset = containerWidth / 2 - currentIdx * TICK_W - TICK_W / 2;

  return (
    <div className="hold-overlay">
      <div className="hold-readout">
        <span>{def.label} · {def.sub}</span>
        <span className="v mono">{values[currentIdx]}</span>
        <span>HOLD · SLIDE</span>
      </div>
      <div className="hold-strip">
        <div className="hold-strip-inner" ref={innerRef}
          style={{ transform: `translateX(${offset}px)` }}>
          {values.map((v, i) => {
            const dist = Math.abs(i - currentIdx);
            const cls = dist === 0 ? 'center' : dist <= 2 ? 'near' : '';
            return <div key={i} className={`hold-tick ${cls}`}>{v}</div>;
          })}
        </div>
        <div className="hold-cursor" />
      </div>
    </div>
  );
}

// ─────────────────────────────────────────────────────────────────────
// ModalPicker — wheel-style scroll picker (tap to open)
// ─────────────────────────────────────────────────────────────────────
function ModalPicker({ kind, idx, onCommit, onCancel }) {
  const def = SETTING_DEFS[kind];
  const values = def.values;
  const [selIdx, setSelIdx] = React.useState(idx);
  const ITEM_H = 36;

  // Render 9 visible items centered on selIdx
  const winSize = 9;
  const half = Math.floor(winSize / 2);

  const wheelRef = React.useRef(null);
  const dragRef = React.useRef({ active: false, startY: 0, startIdx: 0, accumDelta: 0 });

  const onPointerDown = (e) => {
    e.preventDefault();
    dragRef.current = { active: true, startY: e.clientY, startIdx: selIdx, accumDelta: 0 };
    try { e.currentTarget.setPointerCapture(e.pointerId); } catch (_) {}
  };
  const onPointerMove = (e) => {
    if (!dragRef.current.active) return;
    const dy = e.clientY - dragRef.current.startY;
    const delta = Math.round(-dy / ITEM_H);
    let next = dragRef.current.startIdx + delta;
    next = Math.max(0, Math.min(values.length - 1, next));
    if (next !== selIdx) setSelIdx(next);
  };
  const onPointerUp = () => { dragRef.current.active = false; };

  // Wheel event for desktop
  const onWheel = (e) => {
    e.preventDefault();
    const dir = e.deltaY > 0 ? 1 : -1;
    setSelIdx((i) => Math.max(0, Math.min(values.length - 1, i + dir)));
  };

  return (
    <div className="modal-backdrop" onClick={onCancel}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="mhead">
          <div>
            <div className="mtitle">{def.label} · {def.sub}</div>
          </div>
          <button className="mclose mono" onClick={onCancel}>CANCEL</button>
        </div>
        <div className="mwheel"
          ref={wheelRef}
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          onPointerCancel={onPointerUp}
          onWheel={onWheel}
        >
          <div className="selector" />
          <div className="mwheel-inner"
            style={{ transform: `translateY(${-selIdx * ITEM_H}px)` }}>
            {values.map((v, i) => {
              const dist = Math.abs(i - selIdx);
              const cls = dist === 0 ? 'center' : dist <= 2 ? 'near' : '';
              return (
                <div key={i} className={`mitem ${cls}`}
                  onClick={() => setSelIdx(i)}>
                  {v}
                </div>
              );
            })}
          </div>
        </div>
        <div className="mfooter">
          <button className="btn ghost" onClick={onCancel}>CANCEL</button>
          <button className="btn" onClick={() => onCommit(selIdx)}>SET {values[selIdx]}</button>
        </div>
      </div>
    </div>
  );
}

// ─────────────────────────────────────────────────────────────────────
// Histogram (deterministic procedural curve)
// ─────────────────────────────────────────────────────────────────────
function Histogram() {
  const bins = React.useMemo(() => {
    const N = 40;
    const arr = [];
    for (let i = 0; i < N; i++) {
      const x = i / (N - 1);
      // Mix two gaussians for a typical-looking image histogram
      const g1 = Math.exp(-Math.pow((x - 0.32) / 0.14, 2)) * 0.85;
      const g2 = Math.exp(-Math.pow((x - 0.72) / 0.10, 2)) * 0.55;
      arr.push(Math.min(1, g1 + g2 + 0.04));
    }
    return arr;
  }, []);
  return (
    <div className="histogram">
      {bins.map((h, i) => (
        <div key={i} className="bin" style={{ height: `${h * 100}%` }} />
      ))}
    </div>
  );
}

// ─────────────────────────────────────────────────────────────────────
// LiveView — fills available area; AF box, grid, level, histogram, hint
// ─────────────────────────────────────────────────────────────────────
function LiveView({ afBox, onTapToFocus, showGrid, showLevel, showHistogram, info, recording }) {
  const ref = React.useRef(null);
  const [tapHint, setTapHint] = React.useState(null);
  const [afState, setAfState] = React.useState('searching'); // searching → locked

  React.useEffect(() => {
    setAfState('searching');
    const t = setTimeout(() => setAfState('locked'), 420);
    return () => clearTimeout(t);
  }, [afBox.x, afBox.y]);

  const handleClick = (e) => {
    const rect = ref.current.getBoundingClientRect();
    const x = ((e.clientX - rect.left) / rect.width) * 100;
    const y = ((e.clientY - rect.top) / rect.height) * 100;
    onTapToFocus({ x, y });
    setTapHint({ x, y, k: Date.now() });
    setTimeout(() => setTapHint(null), 900);
  };

  return (
    <div className="liveview" ref={ref} onClick={handleClick}
      style={{ position: 'relative', flex: 1, minHeight: 0, cursor: 'crosshair' }}>
      <div className="liveview-label">LIVE · 6240 × 4160 · RAW+JPG</div>
      {showGrid && (
        <div className="grid-overlay">
          <div className="gv1" /><div className="gv2" />
          <div className="gh1" /><div className="gh2" />
        </div>
      )}
      {showLevel && <div className="level" />}

      {/* AF box */}
      <div
        className={`af-box${afState === 'locked' ? ' locked' : ''}`}
        style={{
          left: `${afBox.x}%`,
          top: `${afBox.y}%`,
          width: `${afBox.w}%`,
          height: `${afBox.h}%`,
          transform: 'translate(-50%, -50%)',
        }}
      >
        <div className="corner tl" />
        <div className="corner tr" />
        <div className="corner bl" />
        <div className="corner br" />
        <div className="label mono">{afState === 'locked' ? 'AF · LOCK' : 'AF · WIDE'}</div>
      </div>

      {tapHint && (
        <div className="af-tap-hint" style={{
          left: `${tapHint.x}%`, top: `${tapHint.y}%`,
        }} key={tapHint.k} />
      )}

      {/* Top HUD */}
      <div style={{
        position: 'absolute', top: 12, left: 12, right: 12,
        display: 'flex', justifyContent: 'space-between',
        alignItems: 'flex-start', gap: 8, zIndex: 2, pointerEvents: 'none',
      }}>
        <div style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
            <span style={{
              width: 6, height: 6, borderRadius: '50%',
              background: recording ? '#D8593F' : '#7BA67A',
              boxShadow: recording ? '0 0 0 3px rgba(216,89,63,0.18)' : '0 0 0 3px rgba(123,166,122,0.18)',
            }} />
            <span className="mono" style={{ fontSize: 9, letterSpacing: '0.18em', color: '#EDE7DD' }}>
              {recording ? 'REC' : 'LINK'}
            </span>
            <span className="mono" style={{ fontSize: 9, letterSpacing: '0.16em', color: 'rgba(237,231,221,0.55)' }}>
              {info.cameraModel}
            </span>
          </div>
          <div className="mono" style={{ fontSize: 9, letterSpacing: '0.14em', color: 'rgba(237,231,221,0.45)' }}>
            {info.lens} · {info.profile}
          </div>
        </div>
        {showHistogram && <Histogram />}
      </div>

      {/* Bottom-left exposure meter */}
      <div style={{
        position: 'absolute', bottom: 12, left: 12,
        zIndex: 2, pointerEvents: 'none',
        display: 'flex', alignItems: 'center', gap: 8,
      }}>
        <div className="mono" style={{
          fontSize: 9, letterSpacing: '0.16em', color: 'rgba(237,231,221,0.55)',
        }}>EV</div>
        <div style={{
          width: 110, height: 4, background: 'rgba(255,255,255,0.10)',
          position: 'relative', borderRadius: 1,
        }}>
          {[-2,-1,0,1,2].map(t => (
            <div key={t} style={{
              position: 'absolute',
              left: `${50 + t * 22}%`,
              top: -2, bottom: -2,
              width: 1, background: t === 0 ? '#EDE7DD' : 'rgba(255,255,255,0.25)',
            }} />
          ))}
          <div style={{
            position: 'absolute',
            left: `${50 + 0.3 * 22}%`,
            top: -3, bottom: -3,
            width: 2, background: '#E8B36B', transform: 'translateX(-1px)',
          }} />
        </div>
        <div className="mono" style={{
          fontSize: 10, color: '#E8B36B', fontFeatureSettings: '"tnum" 1',
        }}>+0.3</div>
      </div>

      {/* Bottom-right battery / shots */}
      <div style={{
        position: 'absolute', bottom: 12, right: 12,
        zIndex: 2, pointerEvents: 'none',
        display: 'flex', alignItems: 'center', gap: 12,
      }}>
        <div className="mono" style={{
          fontSize: 10, color: 'rgba(237,231,221,0.75)',
          fontFeatureSettings: '"tnum" 1',
        }}>{info.shotsRemaining} <span style={{ color: 'rgba(237,231,221,0.4)', fontSize: 9, letterSpacing: '0.14em' }}>SHOTS</span></div>
        <div className="battery">
          <div className="cell"><div className="fill" style={{ width: `${info.battery}%` }} /></div>
          <span>{info.battery}%</span>
        </div>
      </div>
    </div>
  );
}

// ─────────────────────────────────────────────────────────────────────
// Connection screens
// ─────────────────────────────────────────────────────────────────────
function ConnectionHeader({ title, subtitle, step, totalSteps }) {
  return (
    <div style={{
      padding: '20px 20px 14px',
      borderBottom: '1px solid var(--hairline)',
    }}>
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 10 }}>
        <div className="app-icon" style={{ width: 28, height: 28, borderRadius: 6 }} />
        <div className="steps">
          {Array.from({ length: totalSteps }).map((_, i) => (
            <div key={i} className={`dot${i === step ? ' active' : i < step ? ' done' : ''}`} />
          ))}
        </div>
      </div>
      <div className="tag" style={{ marginBottom: 4 }}>{subtitle}</div>
      <div style={{
        fontSize: 22, fontWeight: 500, letterSpacing: '-0.01em',
        lineHeight: 1.15,
      }}>{title}</div>
    </div>
  );
}

function ScreenScanning({ onSelect }) {
  const cameras = [
    { id: 'x-h2', name: 'X-H2 · 7B41', signal: 'Strong', distance: '0.4 m', primary: true },
    { id: 'gfx', name: 'GFX 100 · A2C9', signal: 'Weak', distance: '5.1 m' },
  ];
  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%' }}>
      <ConnectionHeader
        title="Scanning for cameras"
        subtitle="STEP 01 · BLUETOOTH"
        step={0}
        totalSteps={4}
      />
      <div className="scan-radar">
        <div className="ring" /><div className="ring r2" /><div className="ring r3" /><div className="ring r4" />
        <div className="sweep" />
        <div className="center-dot" />
        <div className="blip" style={{ left: '32%', top: '38%' }} />
        <div className="blip" style={{ left: '70%', top: '64%' }} />
      </div>
      <div style={{ padding: '20px', flex: 1 }}>
        <div className="tag" style={{ marginBottom: 10 }}>NEARBY · 2 FOUND</div>
        <div className="card">
          {cameras.map((c, i) => (
            <div key={c.id} className="row" onClick={() => onSelect(c)}>
              <div>
                <div className="mono" style={{ fontSize: 13, fontWeight: 500 }}>{c.name}</div>
                <div className="tag" style={{ marginTop: 4 }}>
                  {c.signal === 'Strong' ? '●●●●' : '●●○○'} · {c.distance}
                </div>
              </div>
              <div className="tag warn">PAIR ›</div>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}

function ScreenPairing({ onComplete, cameraName = 'X-H2 · 7B41' }) {
  const [progress, setProgress] = React.useState(0);
  React.useEffect(() => {
    const id = setInterval(() => {
      setProgress((p) => {
        if (p >= 100) { clearInterval(id); setTimeout(onComplete, 300); return 100; }
        return p + 4;
      });
    }, 80);
    return () => clearInterval(id);
  }, [onComplete]);
  const code = '482 619';
  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%' }}>
      <ConnectionHeader
        title="Confirm pairing code"
        subtitle="STEP 02 · BLUETOOTH"
        step={1}
        totalSteps={4}
      />
      <div style={{ padding: '24px 20px', flex: 1 }}>
        <div className="tag">CONFIRM ON {cameraName.split(' · ')[0]}</div>
        <div style={{
          display: 'flex', justifyContent: 'center', alignItems: 'baseline',
          gap: 10, margin: '24px 0 8px',
        }}>
          {code.split(' ').map((seg, i) => (
            <div key={i} className="mono" style={{
              fontSize: 36,
              fontWeight: 500,
              letterSpacing: '0.16em',
              color: 'var(--accent)',
              fontFeatureSettings: '"tnum" 1',
            }}>{seg}</div>
          ))}
        </div>
        <div style={{ textAlign: 'center', color: 'var(--muted)', fontSize: 12, marginBottom: 24 }}>
          Press OK on camera body to confirm.
        </div>
        <div className="progress"><div className="bar" style={{ width: `${progress}%` }} /></div>
        <div style={{ display: 'flex', justifyContent: 'space-between', marginTop: 6 }}>
          <span className="tag">PAIRING</span>
          <span className="tag mono">{progress}%</span>
        </div>
      </div>
    </div>
  );
}

function ScreenGPS({ onComplete }) {
  const [stage, setStage] = React.useState(0);
  React.useEffect(() => {
    const a = setTimeout(() => setStage(1), 700);
    const b = setTimeout(() => setStage(2), 1500);
    const c = setTimeout(() => setStage(3), 2300);
    const d = setTimeout(onComplete, 2900);
    return () => { [a,b,c,d].forEach(clearTimeout); };
  }, [onComplete]);
  const items = [
    { key: 'gps',  label: 'GPS COORDINATES', val: '37.7752° N, 122.4194° W' },
    { key: 'time', label: 'CAMERA CLOCK',    val: 'May 03, 2026 · 14:22:08' },
    { key: 'tz',   label: 'TIME ZONE',       val: 'PDT · UTC−07:00' },
  ];
  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%' }}>
      <ConnectionHeader
        title="Syncing camera"
        subtitle="STEP 03 · TRANSFER"
        step={2}
        totalSteps={4}
      />
      <div style={{ padding: '20px', flex: 1, display: 'flex', flexDirection: 'column', gap: 12 }}>
        {items.map((it, i) => (
          <div key={it.key} className="card" style={{
            padding: '14px 16px',
            display: 'flex', justifyContent: 'space-between', alignItems: 'center',
            opacity: stage > i ? 1 : 0.4,
            transition: 'opacity 200ms ease',
          }}>
            <div>
              <div className="tag" style={{ marginBottom: 4 }}>{it.label}</div>
              <div className="mono" style={{ fontSize: 13, fontWeight: 500 }}>{it.val}</div>
            </div>
            {stage > i ? (
              <span className="tag good">✓ SENT</span>
            ) : stage === i ? (
              <div className="spinner" />
            ) : (
              <span className="tag">QUEUED</span>
            )}
          </div>
        ))}
        <div style={{ flex: 1 }} />
        <div className="banner">
          <div className="spinner" style={{ width: 12, height: 12, borderWidth: 1.2 }} />
          <div>Bluetooth link · {stage >= 3 ? 'all data sent' : 'streaming over BLE'}</div>
        </div>
      </div>
    </div>
  );
}

function ScreenWifiWarning({ onContinue, onCancel }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%' }}>
      <ConnectionHeader
        title="Switch to camera Wi-Fi?"
        subtitle="STEP 04 · WI-FI"
        step={3}
        totalSteps={4}
      />
      <div style={{ padding: '20px', flex: 1, display: 'flex', flexDirection: 'column', gap: 14 }}>
        <div className="banner warn">
          <span className="mono" style={{ fontSize: 16 }}>!</span>
          <div>
            <div style={{ fontWeight: 500, marginBottom: 2 }}>Internet may be disrupted</div>
            <div style={{ color: 'var(--muted)' }}>This device will join the camera's local Wi-Fi for live view. Cellular and other networks remain available.</div>
          </div>
        </div>
        <div className="card">
          <div className="row" style={{ cursor: 'default' }}>
            <div>
              <div className="tag">CAMERA NETWORK</div>
              <div className="mono" style={{ marginTop: 4, fontSize: 13 }}>FUJI-XH2-7B41</div>
            </div>
            <div className="tag good">5 GHz</div>
          </div>
          <div className="row" style={{ cursor: 'default' }}>
            <div>
              <div className="tag">CURRENT NETWORK</div>
              <div className="mono" style={{ marginTop: 4, fontSize: 13, color: 'var(--muted)' }}>home-fiber-5G</div>
            </div>
            <div className="tag bad">WILL DROP</div>
          </div>
          <div className="row" style={{ cursor: 'default' }}>
            <div>
              <div className="tag">EXPECTED LATENCY</div>
              <div className="mono" style={{ marginTop: 4, fontSize: 13 }}>~ 80 ms</div>
            </div>
            <div className="tag">LOW</div>
          </div>
        </div>
        <div style={{ flex: 1 }} />
        <div style={{ display: 'flex', gap: 8 }}>
          <button className="btn ghost" onClick={onCancel}>NOT NOW</button>
          <button className="btn" onClick={onContinue}>CONNECT</button>
        </div>
      </div>
    </div>
  );
}

function ScreenWifiConnecting({ onComplete }) {
  const [stage, setStage] = React.useState(0);
  const stages = [
    'Releasing internet network',
    'Joining FUJI-XH2-7B41',
    'Negotiating live-view stream',
    'Buffering first frame',
  ];
  React.useEffect(() => {
    const id = setInterval(() => {
      setStage((s) => {
        if (s >= stages.length) { clearInterval(id); setTimeout(onComplete, 300); return s; }
        return s + 1;
      });
    }, 700);
    return () => clearInterval(id);
  }, [onComplete, stages.length]);
  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%' }}>
      <ConnectionHeader
        title="Connecting"
        subtitle="STEP 04 · WI-FI"
        step={3}
        totalSteps={4}
      />
      <div style={{ padding: '20px', flex: 1, display: 'flex', flexDirection: 'column', gap: 10 }}>
        {stages.map((s, i) => (
          <div key={s} style={{ display: 'flex', alignItems: 'center', gap: 10, padding: '10px 0' }}>
            <div style={{ width: 18, display: 'flex', justifyContent: 'center' }}>
              {i < stage ? (
                <span className="tag good" style={{ fontSize: 11 }}>✓</span>
              ) : i === stage ? (
                <div className="spinner" style={{ width: 12, height: 12, borderWidth: 1.2 }} />
              ) : (
                <div style={{ width: 6, height: 6, background: 'var(--surface-3)', borderRadius: '50%' }} />
              )}
            </div>
            <div className="mono" style={{
              fontSize: 12,
              color: i < stage ? 'var(--fg-2)' : i === stage ? 'var(--accent)' : 'var(--muted-2)',
              letterSpacing: '0.04em',
            }}>{s}</div>
          </div>
        ))}
        <div style={{ flex: 1 }} />
        <div className="banner warn">
          <div className="spinner" style={{ width: 12, height: 12, borderWidth: 1.2 }} />
          <div>Internet bridge dropped at 14:22:14 · will reconnect on disconnect</div>
        </div>
      </div>
    </div>
  );
}

// ─────────────────────────────────────────────────────────────────────
// Live screen — assembled from LiveView + bottom controls
// ─────────────────────────────────────────────────────────────────────
function LiveScreen({
  variant, compact,
  showHistogram, showGrid, showLevel, showShutter,
  onDisconnect,
}) {
  const [iso, setIso]           = React.useState(SETTING_DEFS.iso.defaultIdx);
  const [shutter, setShutter]   = React.useState(SETTING_DEFS.shutter.defaultIdx);
  const [aperture, setAperture] = React.useState(SETTING_DEFS.aperture.defaultIdx);

  const setterFor = (kind) => ({
    iso: setIso, shutter: setShutter, aperture: setAperture,
  }[kind]);
  const idxFor = { iso, shutter, aperture };

  const [modal, setModal] = React.useState(null);
  const [hold, setHold]   = React.useState(null); // { kind, startX, pointerId }
  const [afBox, setAfBox] = React.useState({ x: 50, y: 52, w: 26, h: 18 });

  const onTapToFocus = ({ x, y }) => {
    setAfBox({ x, y, w: 18, h: 14 });
  };

  const onSettingTap = (kind) => setModal(kind);
  const onHoldStart  = (kind, sx, sy, pid) => setHold({ kind, startX: sx, pointerId: pid });
  const onHoldEnd    = () => setHold(null);

  const info = {
    cameraModel: 'X-H2 · 7B41',
    lens: 'XF 33mm F1.4',
    profile: 'CLASSIC NEG',
    battery: 78,
    shotsRemaining: 412,
  };

  return (
    <div style={{
      position: 'relative',
      display: 'flex', flexDirection: 'column',
      flex: 1,
      minHeight: 0,
    }}>
      <LiveView
        afBox={afBox}
        onTapToFocus={onTapToFocus}
        showGrid={showGrid}
        showLevel={showLevel}
        showHistogram={showHistogram}
        info={info}
        recording={false}
      />

      {hold && (
        <HoldStrip
          kind={hold.kind}
          idx={idxFor[hold.kind]}
          startX={hold.startX}
          pointerId={hold.pointerId}
          onChange={(i) => setterFor(hold.kind)(i)}
          onEnd={onHoldEnd}
        />
      )}

      {modal && (
        <ModalPicker
          kind={modal}
          idx={idxFor[modal]}
          onCommit={(i) => { setterFor(modal)(i); setModal(null); }}
          onCancel={() => setModal(null)}
        />
      )}

      {/* Bottom control bar */}
      <div style={{
        flex: '0 0 auto',
        position: 'relative',
        zIndex: 10,
        background: 'var(--bg-2)',
        borderTop: '1px solid var(--hairline)',
      }}>
        {/* Top status row */}
        <div style={{
          display: 'flex', alignItems: 'center', justifyContent: 'space-between',
          padding: '8px 14px',
          borderBottom: '1px solid var(--hairline)',
          fontSize: 10,
        }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
            <span style={{
              width: 6, height: 6, borderRadius: '50%', background: 'var(--good)',
            }} />
            <span className="tag">Wi-Fi · 5GHz · 78ms</span>
          </div>
          <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
            <span className="tag">RAW+JPG</span>
            <span className="tag warn">M</span>
            <button className="tag" style={{
              background: 'transparent', border: 'none', color: 'var(--muted)',
              cursor: 'pointer', padding: 0,
            }} onClick={onDisconnect}>DISCONNECT ›</button>
          </div>
        </div>

        {/* Setting tiles */}
        <div style={{ display: 'flex', height: compact ? 64 : 72, borderBottom: '1px solid var(--hairline)' }}>
          <SettingTile kind="iso"      idx={iso}      onTap={onSettingTap} onHoldStart={onHoldStart} active={modal==='iso' || hold?.kind==='iso'} />
          <SettingTile kind="shutter"  idx={shutter}  onTap={onSettingTap} onHoldStart={onHoldStart} active={modal==='shutter' || hold?.kind==='shutter'} />
          <SettingTile kind="aperture" idx={aperture} onTap={onSettingTap} onHoldStart={onHoldStart} active={modal==='aperture' || hold?.kind==='aperture'} />
        </div>

        {/* Toolbar (drive mode + WB + shutter) */}
        {showShutter && (
          <div style={{
            display: 'flex', alignItems: 'center', justifyContent: 'space-between',
            padding: variant === 'desktop' ? '14px 18px' : '12px 16px',
            gap: 12,
          }}>
            <div style={{ display: 'flex', alignItems: 'center', gap: 14 }}>
              <ToolbarBtn label="WB" value="AUTO" />
              <ToolbarBtn label="DRIVE" value="CH" />
              <ToolbarBtn label="EXP" value="+0.3" />
            </div>
            <div className="shutter" />
            <div style={{ display: 'flex', alignItems: 'center', gap: 14 }}>
              <ToolbarBtn label="FOCUS" value="AF-S" />
              <ToolbarBtn label="FILM" value="C-NEG" />
              <ToolbarBtn label="MENU" value="•••" />
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

function ToolbarBtn({ label, value }) {
  return (
    <button style={{
      display: 'flex', flexDirection: 'column', alignItems: 'center',
      gap: 2, padding: '4px 6px',
    }}>
      <span className="tag">{label}</span>
      <span className="mono" style={{ fontSize: 11, color: 'var(--fg-2)' }}>{value}</span>
    </button>
  );
}

// ─────────────────────────────────────────────────────────────────────
// CameraApp — top-level component, orchestrates the flow
// ─────────────────────────────────────────────────────────────────────
function CameraApp({
  variant = 'mobile',
  compact = false,
  showHistogram = true,
  showGrid = true,
  showLevel = true,
  showShutter = true,
  initialState = 'live',
}) {
  const [state, setState] = React.useState(initialState);
  React.useEffect(() => { setState(initialState); }, [initialState]);

  const renderState = () => {
    switch (state) {
      case 'disconnected':
        return <ScreenDisconnected onScan={() => setState('scanning')} />;
      case 'scanning':
        return <ScreenScanning onSelect={() => setState('pairing')} />;
      case 'pairing':
        return <ScreenPairing onComplete={() => setState('gps')} />;
      case 'gps':
        return <ScreenGPS onComplete={() => setState('wifi-warning')} />;
      case 'wifi-warning':
        return <ScreenWifiWarning
          onContinue={() => setState('wifi')}
          onCancel={() => setState('disconnected')}
        />;
      case 'wifi':
        return <ScreenWifiConnecting onComplete={() => setState('live')} />;
      case 'live':
      default:
        return <LiveScreen
          variant={variant}
          compact={compact}
          showHistogram={showHistogram}
          showGrid={showGrid}
          showLevel={showLevel}
          showShutter={showShutter}
          onDisconnect={() => setState('disconnected')}
        />;
    }
  };

  return (
    <div className={`app${compact ? ' compact' : ''}`}>
      {renderState()}
    </div>
  );
}

function ScreenDisconnected({ onScan }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%' }}>
      <div style={{
        padding: '24px 20px 18px',
        borderBottom: '1px solid var(--hairline)',
        display: 'flex', justifyContent: 'space-between', alignItems: 'center',
      }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
          <div className="app-icon" />
          <div>
            <div style={{ fontSize: 17, fontWeight: 500, letterSpacing: '-0.005em' }}>Frame Remote</div>
            <div className="tag" style={{ marginTop: 2 }}>v 2.4 · NO CAMERA LINKED</div>
          </div>
        </div>
        <span className="tag">⚙</span>
      </div>
      <div style={{ flex: 1, display: 'flex', flexDirection: 'column',
        alignItems: 'center', justifyContent: 'center', padding: '24px 20px',
        textAlign: 'center', gap: 18 }}>
        <div style={{
          width: 72, height: 72, borderRadius: '50%',
          border: '1px dashed var(--hairline-2)',
          display: 'flex', alignItems: 'center', justifyContent: 'center',
          color: 'var(--muted)', fontSize: 28,
        }}><span className="glyph bt" style={{ width: 28, height: 28 }} /></div>
        <div>
          <div style={{ fontSize: 17, fontWeight: 500, marginBottom: 6 }}>Pair a camera over Bluetooth</div>
          <div style={{ color: 'var(--muted)', fontSize: 12, maxWidth: 280, lineHeight: 1.5 }}>
            Turn your camera on and enable Bluetooth pairing in the Wireless menu. The app will sync GPS &amp; clock, then switch to Wi-Fi for live view.
          </div>
        </div>
      </div>
      <div style={{ padding: '16px 20px 20px', display: 'flex', flexDirection: 'column', gap: 8 }}>
        <button className="btn" onClick={onScan}>SCAN FOR CAMERAS</button>
        <button className="btn ghost">USE QR PAIRING</button>
      </div>
    </div>
  );
}

Object.assign(window, { CameraApp });
