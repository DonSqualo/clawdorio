use axum::{
    middleware,
    response::{Html, IntoResponse},
    routing::get,
    routing::post,
    Json, Router,
};
use clawdorio_engine::Engine;
use clawdorio_protocol::{targets, Patch, Swap, UiUpdate};
use serde::Deserialize;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::{AllowOrigin, CorsLayer};

#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/health", get(health))
        .route("/api/ui/demo", post(ui_demo))
        .with_state(Arc::new(state))
        // Local security: allow only loopback + Tailscale by default.
        .layer(middleware::from_fn(ip_allowlist))
        // This service is expected to be local-only and may control a local agent swarm.
        // Never use `Access-Control-Allow-Origin: *` here; it makes it easier for a random
        // website in your browser to probe/exfiltrate local state.
        .layer(local_only_cors())
}

async fn health() -> &'static str {
    "ok"
}

async fn dashboard() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

#[derive(Debug, Deserialize)]
struct DemoInput {
    #[serde(default)]
    pub selected: Option<String>,
}

async fn ui_demo(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(input): Json<DemoInput>,
) -> Json<UiUpdate> {
    // Touch engine so the process fails fast if sqlite is unavailable.
    let _ = state.engine.open();

    let selected = input.selected.unwrap_or_else(|| "none".to_string());
    let patches = vec![
        Patch {
            target: targets::PANEL_BOTTOM_BAR.to_string(),
            swap: Swap::Replace,
            html: Some(format!(
                "<div class=\"card\"><strong>Bottom Bar</strong><div>Selected: {}</div></div>",
                html_escape::encode_text(&selected)
            )),
            payload: None,
            trigger: None,
        },
        Patch {
            target: targets::PANEL_RIGHT.to_string(),
            swap: Swap::Replace,
            html: Some(
                "<div class=\"card\"><strong>Right Panel</strong><div>Demo patch ok.</div></div>"
                    .to_string(),
            ),
            payload: None,
            trigger: None,
        },
    ];
    Json(UiUpdate::new("ui.demo", patches))
}

pub async fn serve(addr: SocketAddr, db_path: PathBuf) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_listener(listener, db_path, async {
        std::future::pending::<()>().await
    })
    .await?;
    Ok(())
}

pub async fn serve_listener(
    listener: tokio::net::TcpListener,
    db_path: PathBuf,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<SocketAddr> {
    let state = AppState {
        engine: Engine::new(db_path),
    };
    let app = build_router(state);
    let addr = listener.local_addr()?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await?;
    Ok(addr)
}

async fn ip_allowlist(
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let ip = peer.ip();
    if is_allowed_peer_ip(ip) {
        return next.run(req).await;
    }
    (axum::http::StatusCode::FORBIDDEN, "forbidden").into_response()
}

fn is_allowed_peer_ip(ip: IpAddr) -> bool {
    if ip.is_loopback() {
        return true;
    }

    // Tailscale CGNAT range (100.64.0.0/10).
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            // 100.64.0.0 - 100.127.255.255
            o[0] == 100 && (64..=127).contains(&o[1])
        }
        IpAddr::V6(_v6) => false,
    }
}

fn local_only_cors() -> CorsLayer {
    use axum::http::header;
    use axum::http::HeaderValue;
    use axum::http::Method;

    CorsLayer::new()
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::CONTENT_TYPE])
        .allow_origin(AllowOrigin::predicate(|origin: &HeaderValue, _req| {
            is_allowed_local_origin(origin)
        }))
}

fn is_allowed_local_origin(origin: &axum::http::HeaderValue) -> bool {
    let Ok(s) = origin.to_str() else {
        return false;
    };

    // Tauri WebView origin (production).
    if s == "tauri://localhost" {
        return true;
    }

    // Dev server and local reverse proxies.
    is_http_origin_for_host(s, "localhost") || is_http_origin_for_host(s, "127.0.0.1")
}

fn is_http_origin_for_host(origin: &str, host: &str) -> bool {
    for scheme in ["http://", "https://"] {
        if let Some(rest) = origin.strip_prefix(scheme) {
            if let Some(after) = rest.strip_prefix(host) {
                // Origin is just scheme://host[:port]
                return after.is_empty() || after.starts_with(':');
            }
        }
    }
    false
}

const DASHBOARD_HTML: &str = r###"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <meta name="theme-color" content="#081427" />
  <title>Clawdorio Command Grid</title>
  <link rel="preconnect" href="https://fonts.googleapis.com">
  <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
  <link href="https://fonts.googleapis.com/css2?family=Orbitron:wght@500;700&family=Inter:wght@400;500;600;700&family=Geist+Mono&display=swap" rel="stylesheet">
  <style>
    :root{
      --bg-a:#050913;
      --bg-b:#081325;
      --bg-c:#0a1a31;
      --ice:#e6fbff;
      --teal:#6ff8ff;
      --blue:#68c7ff;
      --line:#7fcbff5a;
      --panel:#0b1a2dcc;
      --panel-edge:#73c7ff55;
      --muted:#8aa3be;
      --ok:#4df5bf;
      --warn:#ffd06b;
      --bad:#ff7198;
      --dock-w:min(300px, 26vw);
      --command-h:140px;
      --screen-pad:12px;
    }
    *{box-sizing:border-box;margin:0;padding:0}
    html,body{width:100%;height:100%;overflow:hidden}
    body{
      font-family:Inter,system-ui,sans-serif;
      color:var(--ice);
      background:
        radial-gradient(circle at 12% 12%, #1e3258 0%, #0b1528 24%, transparent 52%),
        radial-gradient(circle at 88% 18%, #163755 0%, #0a1426 25%, transparent 58%),
        linear-gradient(165deg,var(--bg-c) 0%,var(--bg-b) 45%,var(--bg-a) 100%);
    }
    .layout{position:relative;width:100vw;height:100vh}
    .topbar{
      position:absolute;left:var(--screen-pad);right:var(--screen-pad);top:var(--screen-pad);
      height:54px;display:flex;gap:12px;align-items:center;justify-content:space-between;
      padding:10px 12px;border:1px solid var(--panel-edge);border-radius:14px;
      background:linear-gradient(160deg,#0c223b 0%, #081427 100%);
      box-shadow:0 12px 30px #020c1888;
      z-index:50;
    }
    .brand{display:flex;align-items:center;gap:10px}
    .sig{
      width:10px;height:10px;border-radius:3px;background:linear-gradient(160deg,var(--teal),var(--blue));
      box-shadow:0 0 0 3px #6ff8ff22;
    }
    .brand h1{font-family:Orbitron,system-ui,sans-serif;font-size:14px;letter-spacing:.7px}
    .brand .sub{font-size:11px;color:var(--muted)}
    .pill{display:flex;align-items:center;gap:8px;font-size:12px;color:var(--muted)}
    .dot{width:8px;height:8px;border-radius:99px;background:var(--warn);box-shadow:0 0 0 3px #ffd06b22}
    .dot.ok{background:var(--ok);box-shadow:0 0 0 3px #4df5bf22}
    .btn{
      border:1px solid #4f799f;background:#0b1b30;color:var(--ice);
      border-radius:10px;padding:8px 10px;font-weight:600;cursor:pointer;
    }
    .btn:hover{border-color:#8de7ff;box-shadow:0 0 0 1px #95e6ff44 inset}

    .dock{
      position:absolute;top:calc(var(--screen-pad) + 64px);bottom:calc(var(--screen-pad) + var(--command-h));
      width:var(--dock-w);padding:10px;border:1px solid var(--panel-edge);border-radius:16px;
      background:var(--panel);backdrop-filter:blur(10px);
      box-shadow:0 14px 40px #0008;
      overflow:hidden;
      z-index:40;
    }
    .dock.left{left:var(--screen-pad)}
    .dock.right{right:var(--screen-pad)}
    .dock h2{font-family:Orbitron,system-ui,sans-serif;font-size:13px;letter-spacing:.6px;margin-bottom:10px}
    .dock .scroll{height:100%;overflow:auto;padding-right:6px}
    .card{
      border:1px solid #5fa5d655;border-radius:14px;
      background:linear-gradient(160deg,#0c223bdd 0%, #081427dd 100%);
      padding:10px;margin-bottom:10px;
    }
    .card .k{font-size:11px;color:var(--muted);margin-bottom:6px}
    .card .v{font-size:13px}
    .list{display:flex;flex-direction:column;gap:8px}
    .item{
      display:flex;align-items:center;justify-content:space-between;gap:10px;
      padding:10px;border-radius:12px;border:1px solid #4f799f55;background:#061325aa;
      cursor:pointer;
    }
    .item:hover{border-color:#8de7ff}
    .item strong{font-size:13px}
    .item span{font-size:11px;color:var(--muted)}

    .commandbar{
      position:absolute;left:var(--screen-pad);right:var(--screen-pad);
      bottom:var(--screen-pad);height:var(--command-h);
      border:1px solid var(--panel-edge);border-radius:18px;
      background:linear-gradient(160deg,#0c223bcc 0%, #081427cc 100%);
      backdrop-filter:blur(10px);
      padding:12px;
      box-shadow:0 18px 48px #0009;
      z-index:45;
      display:grid;
      grid-template-columns: 1fr 320px;
      gap:12px;
    }
    .cmdgrid{display:grid;grid-template-columns:repeat(4,1fr);gap:10px}
    .cmd{
      padding:10px;border-radius:14px;border:1px solid #4f799f55;background:#061325aa;
      min-height:68px;
      display:flex;flex-direction:column;justify-content:space-between;
    }
    .cmd .t{font-family:Geist Mono, ui-monospace, SFMono-Regular, Menlo, monospace;font-size:11px;color:#cfefff}
    .cmd .d{font-size:11px;color:var(--muted)}
    .mini{
      border-radius:14px;border:1px solid #4f799f55;background:#061325aa;
      padding:10px;display:flex;flex-direction:column;gap:10px;
    }
    .mini .row{display:flex;align-items:center;justify-content:space-between;font-size:12px;color:var(--muted)}

    .viewport{
      position:absolute;
      left:calc(var(--screen-pad) + var(--dock-w) + 12px);
      right:calc(var(--screen-pad) + var(--dock-w) + 12px);
      top:calc(var(--screen-pad) + 64px);
      bottom:calc(var(--screen-pad) + var(--command-h));
      border-radius:18px;border:1px solid var(--panel-edge);
      background:radial-gradient(circle at 50% 40%, #0d2a4a 0%, #061325 55%, #04070f 100%);
      box-shadow: inset 0 0 0 1px #0007, 0 22px 70px #0008;
      overflow:hidden;
      z-index:10;
    }
    #rtsCanvas{width:100%;height:100%;display:block}
    .hint{
      position:absolute;left:14px;bottom:14px;z-index:20;
      padding:8px 10px;border-radius:12px;border:1px solid #5fa5d655;
      background:#081427bb;color:var(--muted);font-size:11px;
      pointer-events:none;
    }

    /* Small screens: collapse to single column */
    @media (max-width: 980px){
      :root{--dock-w: min(320px, 92vw);}
      .dock.right{display:none}
      .viewport{left:var(--screen-pad);right:var(--screen-pad)}
      .commandbar{grid-template-columns:1fr}
    }
  </style>
</head>
<body>
  <div class="layout">
    <header class="topbar">
      <div class="brand">
        <div class="sig"></div>
        <div>
          <h1>CLAWDORIO</h1>
          <div class="sub">Command Grid (local)</div>
        </div>
      </div>
      <div style="display:flex;align-items:center;gap:10px">
        <div class="pill"><span id="connDot" class="dot"></span><span id="connText">connecting</span></div>
        <button id="demoBtn" class="btn" type="button">demo patch</button>
      </div>
    </header>

    <aside class="dock left">
      <h2>Build Queue</h2>
      <div class="scroll">
        <div class="card">
          <div class="k">Draft</div>
          <div class="v">Select a structure and place it in the grid.</div>
        </div>
        <div id="buildList" class="list"></div>
      </div>
    </aside>

    <main class="viewport">
      <canvas id="rtsCanvas"></canvas>
      <div class="hint">Drag: pan | Wheel: zoom | Double-click: center | Camera persists across reloads</div>
    </main>

    <aside class="dock right">
      <h2>Intel</h2>
      <div class="scroll">
        <div class="card">
          <div class="k">Selection</div>
          <div id="selectionText" class="v">none</div>
        </div>
        <div class="card">
          <div class="k">API</div>
          <div class="v">GET <span style="font-family:Geist Mono,monospace">/health</span></div>
          <div class="v">POST <span style="font-family:Geist Mono,monospace">/api/ui/demo</span></div>
        </div>
        <div class="card">
          <div class="k">Patches</div>
          <div id="panel.right" class="v">waiting</div>
        </div>
      </div>
    </aside>

    <footer class="commandbar">
      <section class="cmdgrid">
        <div class="cmd">
          <div class="t">RALLY</div>
          <div class="d">Assign agents to a run.</div>
        </div>
        <div class="cmd">
          <div class="t">DRAFT</div>
          <div class="d">Place structures, plan belts.</div>
        </div>
        <div class="cmd">
          <div class="t">OPS</div>
          <div class="d">Run queue and tasks.</div>
        </div>
        <div class="cmd">
          <div class="t">INTEL</div>
          <div class="d">Inspect selected entity.</div>
        </div>
      </section>
      <section class="mini">
        <div class="row"><span>Camera</span><span id="camText">0,0 @ 1.00</span></div>
        <div class="row"><span>Pointer</span><span id="ptrText">-</span></div>
        <div class="row"><span>Bottom</span><span id="panel.bottom.bar">idle</span></div>
      </section>
    </footer>
  </div>

  <script>
  (function(){
    const $ = (id) => document.getElementById(id);

    const connDot = $("connDot");
    const connText = $("connText");
    const selectionText = $("selectionText");
    const camText = $("camText");
    const ptrText = $("ptrText");
    const buildList = $("buildList");
    const demoBtn = $("demoBtn");

    const buildItems = [
      { id: "base.core", name: "Core", desc: "Command node" },
      { id: "feature.research", name: "Research", desc: "Unlock tech" },
      { id: "feature.warehouse", name: "Warehouse", desc: "Storage" },
      { id: "feature.factory", name: "Factory", desc: "Production" },
      { id: "feature.power", name: "Power", desc: "Energy supply" },
    ];
    let draftKind = buildItems[0].id;
    let selected = null;

    function renderBuildList(){
      buildList.innerHTML = "";
      for (const it of buildItems){
        const el = document.createElement("div");
        el.className = "item";
        el.innerHTML = `<div><strong>${esc(it.name)}</strong><div><span>${esc(it.desc)}</span></div></div><span>${esc(it.id)}</span>`;
        el.addEventListener("click", () => {
          draftKind = it.id;
          selected = null;
          selectionText.textContent = `drafting ${it.id}`;
        });
        buildList.appendChild(el);
      }
    }

    function esc(s){
      return String(s).replace(/[&<>"]/g, (c) => ({ "&":"&amp;", "<":"&lt;", ">":"&gt;", "\"":"&quot;" }[c]));
    }

    async function healthLoop(){
      for(;;){
        try{
          const r = await fetch("/health", { cache: "no-store" });
          if (!r.ok) throw new Error("bad");
          connDot.classList.add("ok");
          connText.textContent = "online";
        }catch(_e){
          connDot.classList.remove("ok");
          connText.textContent = "offline";
        }
        await new Promise(res => setTimeout(res, 1200));
      }
    }

    demoBtn.addEventListener("click", async () => {
      const body = { selected: selected ? selected.kind : draftKind };
      const r = await fetch("/api/ui/demo", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
      const j = await r.json();
      if (j && Array.isArray(j.patches)){
        for (const p of j.patches){
          const t = document.getElementById(p.target);
          if (!t) continue;
          if (p.swap === "Replace" || !p.swap){
            t.innerHTML = p.html || "";
          }
        }
      }
    });

    // RTS-ish canvas: isometric grid, draft placement, camera persist.
    const canvas = $("rtsCanvas");
    const ctx = canvas.getContext("2d");
    let w = 0, h = 0, dpr = 1;

    const CAMERA_KEY = "clawdorio.camera.v1";
    const cam = { x: 0, y: 0, z: 1.0 };
    const grid = { tile: 38, cols: 64, rows: 64 };
    const placed = [];

    const state = {
      isPanning: false,
      panStart: { x: 0, y: 0, camx: 0, camy: 0 },
      mouse: { x: 0, y: 0 },
      hover: null,
    };

    function resize(){
      const r = canvas.getBoundingClientRect();
      dpr = Math.max(1, Math.min(2, window.devicePixelRatio || 1));
      w = Math.floor(r.width * dpr);
      h = Math.floor(r.height * dpr);
      canvas.width = w;
      canvas.height = h;
    }

    function loadCamera(){
      try{
        const raw = localStorage.getItem(CAMERA_KEY);
        if (!raw) return;
        const j = JSON.parse(raw);
        if (typeof j.x === "number") cam.x = j.x;
        if (typeof j.y === "number") cam.y = j.y;
        if (typeof j.z === "number") cam.z = clamp(j.z, 0.5, 2.2);
      }catch(_e){}
    }
    let lastSave = 0;
    function saveCameraThrottled(){
      const now = performance.now();
      if (now - lastSave < 250) return;
      lastSave = now;
      try{ localStorage.setItem(CAMERA_KEY, JSON.stringify(cam)); }catch(_e){}
    }

    function clamp(v, a, b){ return Math.max(a, Math.min(b, v)); }

    function worldToScreen(wx, wy){
      // 2:1 isometric projection with camera.
      const s = grid.tile * cam.z;
      const isoX = (wx - wy) * (s * 0.5);
      const isoY = (wx + wy) * (s * 0.25);
      return {
        x: (w * 0.5) + isoX - cam.x,
        y: (h * 0.42) + isoY - cam.y,
      };
    }

    function screenToWorld(sx, sy){
      const s = grid.tile * cam.z;
      const x = (sx + cam.x - w*0.5) / (s*0.5);
      const y = (sy + cam.y - h*0.42) / (s*0.25);
      // Solve:
      // x = wx - wy
      // y = wx + wy
      const wx = (x + y) * 0.5;
      const wy = (y - x) * 0.5;
      return { wx, wy };
    }

    function snapCell(wx, wy){
      const gx = Math.floor(wx + 0.5);
      const gy = Math.floor(wy + 0.5);
      return { gx, gy };
    }

    function draw(){
      ctx.clearRect(0,0,w,h);

      // Background haze.
      ctx.fillStyle = "#050913";
      ctx.fillRect(0,0,w,h);

      // Grid.
      const maxCells = 22;
      const center = screenToWorld(w*0.5, h*0.42);
      const cx = Math.floor(center.wx);
      const cy = Math.floor(center.wy);

      for (let y = cy - maxCells; y <= cy + maxCells; y++){
        for (let x = cx - maxCells; x <= cx + maxCells; x++){
          const p = worldToScreen(x, y);
          const s = grid.tile * cam.z;
          const half = s*0.5;
          const quarter = s*0.25;

          // Diamond
          ctx.beginPath();
          ctx.moveTo(p.x, p.y - quarter);
          ctx.lineTo(p.x + half, p.y);
          ctx.lineTo(p.x, p.y + quarter);
          ctx.lineTo(p.x - half, p.y);
          ctx.closePath();

          ctx.strokeStyle = "rgba(127,203,255,0.18)";
          ctx.lineWidth = 1 * dpr;
          ctx.stroke();
        }
      }

      // Placed "buildings" as filled diamonds.
      for (const b of placed){
        const p = worldToScreen(b.x, b.y);
        const s = grid.tile * cam.z;
        const half = s*0.5;
        const quarter = s*0.25;
        ctx.beginPath();
        ctx.moveTo(p.x, p.y - quarter);
        ctx.lineTo(p.x + half, p.y);
        ctx.lineTo(p.x, p.y + quarter);
        ctx.lineTo(p.x - half, p.y);
        ctx.closePath();
        ctx.fillStyle = "rgba(111,248,255,0.14)";
        ctx.fill();
        ctx.strokeStyle = "rgba(111,248,255,0.55)";
        ctx.stroke();

        ctx.fillStyle = "rgba(230,251,255,0.85)";
        ctx.font = `${Math.max(10, 11*cam.z)}px Inter, system-ui, sans-serif`;
        ctx.fillText(b.kind.split(".").pop(), p.x - half + 6, p.y - quarter - 6);
      }

      // Hover/draft ghost.
      if (state.hover){
        const p = worldToScreen(state.hover.x, state.hover.y);
        const s = grid.tile * cam.z;
        const half = s*0.5;
        const quarter = s*0.25;
        ctx.beginPath();
        ctx.moveTo(p.x, p.y - quarter);
        ctx.lineTo(p.x + half, p.y);
        ctx.lineTo(p.x, p.y + quarter);
        ctx.lineTo(p.x - half, p.y);
        ctx.closePath();
        ctx.fillStyle = "rgba(255,208,107,0.10)";
        ctx.fill();
        ctx.strokeStyle = "rgba(255,208,107,0.70)";
        ctx.stroke();
      }

      camText.textContent = `${cam.x.toFixed(0)},${cam.y.toFixed(0)} @ ${cam.z.toFixed(2)}`;
      requestAnimationFrame(draw);
    }

    function updateHover(clientX, clientY){
      const r = canvas.getBoundingClientRect();
      const sx = (clientX - r.left) * dpr;
      const sy = (clientY - r.top) * dpr;
      const { wx, wy } = screenToWorld(sx, sy);
      const { gx, gy } = snapCell(wx, wy);
      state.hover = { x: gx, y: gy };
      ptrText.textContent = `${gx},${gy}`;
    }

    canvas.addEventListener("mousemove", (e) => {
      state.mouse.x = e.clientX;
      state.mouse.y = e.clientY;
      if (state.isPanning){
        const dx = (e.clientX - state.panStart.x) * dpr;
        const dy = (e.clientY - state.panStart.y) * dpr;
        cam.x = state.panStart.camx - dx;
        cam.y = state.panStart.camy - dy;
        saveCameraThrottled();
        return;
      }
      updateHover(e.clientX, e.clientY);
    });

    canvas.addEventListener("mousedown", (e) => {
      if (e.button !== 0) return;
      state.isPanning = true;
      state.panStart.x = e.clientX;
      state.panStart.y = e.clientY;
      state.panStart.camx = cam.x;
      state.panStart.camy = cam.y;
    });
    window.addEventListener("mouseup", () => { state.isPanning = false; });

    canvas.addEventListener("dblclick", () => {
      cam.x = 0; cam.y = 0;
      saveCameraThrottled();
    });

    canvas.addEventListener("wheel", (e) => {
      e.preventDefault();
      const dz = (e.deltaY > 0) ? -0.08 : 0.08;
      cam.z = clamp(cam.z + dz, 0.5, 2.2);
      saveCameraThrottled();
    }, { passive: false });

    canvas.addEventListener("click", (e) => {
      if (!state.hover) return;
      // Place a building (draft).
      placed.push({ x: state.hover.x, y: state.hover.y, kind: draftKind });
      selected = { x: state.hover.x, y: state.hover.y, kind: draftKind };
      selectionText.textContent = `${selected.kind} @ ${selected.x},${selected.y}`;
    });

    window.addEventListener("resize", () => resize());
    window.addEventListener("beforeunload", () => { try{ localStorage.setItem(CAMERA_KEY, JSON.stringify(cam)); }catch(_e){} });

    renderBuildList();
    loadCamera();
    resize();
    requestAnimationFrame(draw);
    healthLoop();
  })();
  </script>
</body>
</html>
"###;
