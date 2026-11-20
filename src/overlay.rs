//! Оверлей для OBS: страница с пианино, на которой видно, какие клавиши
//! ты сейчас жмёшь.
//!
//! Программа поднимает маленький веб-сервер только для этого компьютера
//! (127.0.0.1). В OBS добавляется источник «Браузер» с адресом
//! http://localhost:ПОРТ/piano — фон прозрачный, клавиши подсвечиваются.
//! Ноты приходят в страницу потоком (Server-Sent Events), задержка ~15 мс.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use crate::state::{LockExt, Shared};

#[derive(Debug, Clone, Default, PartialEq)]
pub enum OverlayState {
    #[default]
    Off,
    Running(u16),
    Error(String),
}

pub fn spawn(shared: Arc<Shared>) {
    thread::Builder::new()
        .name("overlay".into())
        .spawn(move || run(shared))
        .expect("не удалось запустить сервер оверлея");
}

/// Следит за настройками: включает/выключает сервер и меняет порт.
fn run(shared: Arc<Shared>) {
    let mut current: Option<(u16, Arc<std::sync::atomic::AtomicBool>)> = None;
    loop {
        let enabled = shared.overlay_enabled.load(Ordering::Relaxed);
        let port = shared.overlay_port.load(Ordering::Relaxed) as u16;
        let want = enabled.then_some(port);
        if current.as_ref().map(|(p, _)| *p) != want {
            if let Some((_, stop)) = current.take() {
                stop.store(true, Ordering::Relaxed);
            }
            match want {
                None => *shared.overlay_state.lock_ok() = OverlayState::Off,
                Some(p) => match TcpListener::bind(("127.0.0.1", p)) {
                    Ok(listener) => {
                        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let (sh, st) = (shared.clone(), stop.clone());
                        thread::spawn(move || serve(listener, sh, st));
                        current = Some((p, stop));
                        *shared.overlay_state.lock_ok() = OverlayState::Running(p);
                    }
                    Err(e) => {
                        *shared.overlay_state.lock_ok() =
                            OverlayState::Error(format!("порт {p} занят или недоступен: {e}"));
                        // Пробуем снова через секунду (вдруг порт освободится).
                        thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                },
            }
        }
        thread::sleep(Duration::from_millis(300));
    }
}

fn serve(listener: TcpListener, shared: Arc<Shared>, stop: Arc<std::sync::atomic::AtomicBool>) {
    let _ = listener.set_nonblocking(true);
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let (sh, st) = (shared.clone(), stop.clone());
                thread::spawn(move || {
                    let _ = handle(stream, &sh, &st);
                });
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn handle(stream: TcpStream, shared: &Shared, stop: &std::sync::atomic::AtomicBool) -> std::io::Result<()> {
    let _ = stream.set_nonblocking(false);
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    // Дочитываем заголовки.
    let mut line = String::new();
    while reader.read_line(&mut line)? > 2 {
        line.clear();
    }
    let path = request.split_whitespace().nth(1).unwrap_or("/");
    let path = path.split('?').next().unwrap_or("/");
    let mut out = stream;
    match path {
        "/" | "/piano" => {
            let body = PAGE;
            write!(
                out,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
                 Cache-Control: no-cache\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        }
        "/events" => stream_events(out, shared, stop),
        _ => write!(out, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
    }
}

/// Текущее состояние для страницы: диапазон клавиш и зажатые ноты.
fn state_json(shared: &Shared) -> String {
    let notes: Vec<String> = (0..128u8).filter(|&n| shared.note_is_down(n)).map(|n| n.to_string()).collect();
    format!(
        "{{\"from\":{},\"to\":{},\"notes\":[{}],\"pedal\":{},\"names\":{}}}",
        shared.overlay_from.load(Ordering::Relaxed),
        shared.overlay_to.load(Ordering::Relaxed),
        notes.join(","),
        shared.pedal_down.load(Ordering::Relaxed),
        shared.overlay_names.load(Ordering::Relaxed),
    )
}

fn stream_events(mut out: TcpStream, shared: &Shared, stop: &std::sync::atomic::AtomicBool) -> std::io::Result<()> {
    write!(
        out,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\
         Connection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\nretry: 1000\n\n"
    )?;
    let mut last = String::new();
    let mut last_sent = Instant::now();
    while !stop.load(Ordering::Relaxed) && shared.overlay_enabled.load(Ordering::Relaxed) {
        let now = state_json(shared);
        // Отправляем при изменении и раз в 2 секунды (чтобы соединение не уснуло).
        if now != last || last_sent.elapsed() > Duration::from_secs(2) {
            write!(out, "data: {now}\n\n")?;
            out.flush()?;
            last = now;
            last_sent = Instant::now();
        }
        thread::sleep(Duration::from_millis(12));
    }
    Ok(())
}

/// Страница оверлея. Фон прозрачный — в OBS видно только пианино.
const PAGE: &str = r##"<!doctype html>
<html lang="ru">
<head>
<meta charset="utf-8">
<title>Пианино — оверлей</title>
<style>
  html, body { margin: 0; height: 100%; background: transparent; overflow: hidden; }
  #wrap { position: absolute; inset: 0; display: flex; align-items: flex-end; }
  svg { width: 100%; height: 100%; display: block; }
  .w { fill: #eef1f6; stroke: #1b2130; stroke-width: 1.2; }
  .b { fill: #151a24; stroke: #0c0f15; stroke-width: 1; }
  .w.on { fill: url(#gw); }
  .b.on { fill: url(#gb); }
  .glow { filter: drop-shadow(0 0 6px rgba(60,200,180,.95)); }
  text { font: 600 13px "Segoe UI", sans-serif; fill: #0f3d38; text-anchor: middle; }
  .pedal { fill: #3cc8b4; opacity: 0; transition: opacity .08s; }
  .pedal.on { opacity: .9; }
</style>
</head>
<body>
<div id="wrap">
<svg id="kb" preserveAspectRatio="xMidYMax meet">
  <defs>
    <linearGradient id="gw" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#bff3ea"/><stop offset="1" stop-color="#3cc8b4"/>
    </linearGradient>
    <linearGradient id="gb" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#2a8f82"/><stop offset="1" stop-color="#3cc8b4"/>
    </linearGradient>
  </defs>
</svg>
</div>
<script>
const NAMES = ["C","C#","D","D#","E","F","F#","G","G#","A","A#","B"];
const isBlack = n => [1,3,6,8,10].includes(n % 12);
let range = null, keys = {}, pedal = null, showNames = false;

function build(from, to) {
  const svg = document.getElementById("kb");
  [...svg.querySelectorAll(".k, .pedal, text")].forEach(e => e.remove());
  keys = {};
  const whites = [];
  for (let n = from; n <= to; n++) if (!isBlack(n)) whites.push(n);
  const W = 24, H = 120, total = whites.length * W;
  svg.setAttribute("viewBox", `0 -12 ${total} ${H + 12}`);
  const ns = "http://www.w3.org/2000/svg";
  whites.forEach((n, i) => {
    const r = document.createElementNS(ns, "rect");
    Object.entries({x: i * W + .6, y: 0, width: W - 1.2, height: H, rx: 3}).forEach(([k, v]) => r.setAttribute(k, v));
    r.setAttribute("class", "k w");
    svg.appendChild(r); keys[n] = r;
  });
  for (let n = from; n <= to; n++) {
    if (!isBlack(n)) continue;
    const i = whites.indexOf(n - 1);
    if (i < 0) continue;
    const r = document.createElementNS(ns, "rect");
    Object.entries({x: (i + 1) * W - W * .3, y: 0, width: W * .6, height: H * .62, rx: 2}).forEach(([k, v]) => r.setAttribute(k, v));
    r.setAttribute("class", "k b");
    svg.appendChild(r); keys[n] = r;
  }
  pedal = document.createElementNS(ns, "rect");
  Object.entries({x: 0, y: -10, width: total, height: 5, rx: 2.5}).forEach(([k, v]) => pedal.setAttribute(k, v));
  pedal.setAttribute("class", "pedal");
  svg.appendChild(pedal);
  range = from + ":" + to;
}

function render(s) {
  if (range !== s.from + ":" + s.to) build(s.from, s.to);
  showNames = s.names;
  const on = new Set(s.notes);
  document.querySelectorAll("text").forEach(t => t.remove());
  for (const [n, el] of Object.entries(keys)) {
    const pressed = on.has(+n);
    el.classList.toggle("on", pressed);
    el.classList.toggle("glow", pressed);
    if (pressed && showNames && !isBlack(+n)) {
      const t = document.createElementNS("http://www.w3.org/2000/svg", "text");
      t.setAttribute("x", +el.getAttribute("x") + (+el.getAttribute("width")) / 2);
      t.setAttribute("y", 112);
      t.textContent = NAMES[n % 12] + (Math.floor(n / 12) - 1);
      el.parentNode.appendChild(t);
    }
  }
  pedal.classList.toggle("on", s.pedal);
}

const q = new URLSearchParams(location.search);
function connect() {
  const es = new EventSource("/events");
  es.onmessage = e => {
    const s = JSON.parse(e.data);
    if (q.has("from")) s.from = +q.get("from");
    if (q.has("to")) s.to = +q.get("to");
    render(s);
  };
  es.onerror = () => { es.close(); setTimeout(connect, 1000); };
}
connect();
</script>
</body>
</html>
"##;
