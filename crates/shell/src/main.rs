//! Phase S browser shell: a thin eframe window over a separate renderer
//! process.
//!
//! It owns the window and the input, spawns `renderer`, forwards scroll/click
//! over stdin, and paints the frame the renderer mapped into shared memory.
//! The point of this binary is to measure the split, not to be the final UI.

use eframe::egui;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Instant;

enum Ev {
    Frame {
        path: String,
        w: usize,
        h: usize,
        seq: u32,
    },
    Title(String),
    Nav(String),
    Error(String),
    Exited,
}

struct Renderer {
    child: Child,
    stdin: std::process::ChildStdin,
    rx: Receiver<Ev>,
}

impl Renderer {
    fn spawn(url: &str) -> Option<Self> {
        let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
        let exe = dir.join(if cfg!(windows) { "renderer.exe" } else { "renderer" });
        let mut child = Command::new(exe)
            .arg(url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let ev = if let Some(rest) = line.strip_prefix("frame ") {
                    let mut it = rest.split_whitespace();
                    let path = it.next().unwrap_or("").to_string();
                    let w = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                    let h = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                    let _doc_h: f32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                    let seq = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                    Ev::Frame { path, w, h, seq }
                } else if let Some(t) = line.strip_prefix("title ") {
                    Ev::Title(t.to_string())
                } else if let Some(n) = line.strip_prefix("nav ") {
                    Ev::Nav(n.to_string())
                } else if let Some(e) = line.strip_prefix("error ") {
                    Ev::Error(e.to_string())
                } else {
                    continue;
                };
                if tx.send(ev).is_err() {
                    break;
                }
            }
            let _ = tx.send(Ev::Exited);
        });
        Some(Self { child, stdin, rx })
    }

    fn send(&mut self, cmd: &str) {
        let _ = writeln!(self.stdin, "{cmd}");
        let _ = self.stdin.flush();
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        let _ = self.send("quit");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Shell {
    r: Renderer,
    map: Option<mazda_ipc::FrameMap>,
    map_path: String,
    tex: Option<egui::TextureHandle>,
    seq: u32,
    scroll: f32,
    /// time the last scroll command was written; a frame arriving after it
    /// measures the round trip
    sent: Option<Instant>,
    lat: Vec<f32>,
    title: String,
    err: Option<String>,
    /// content size last sent to the renderer
    size: (usize, usize),
}

impl Shell {
    fn new(url: &str) -> Option<Self> {
        let r = Renderer::spawn(url)?;
        Some(Self {
            r,
            map: None,
            map_path: String::new(),
            tex: None,
            seq: 0,
            scroll: 0.0,
            sent: None,
            lat: Vec::new(),
            title: url.to_string(),
            err: None,
            size: (0, 0),
        })
    }

    fn drain(&mut self) {
        while let Ok(ev) = self.r.rx.try_recv() {
            match ev {
                Ev::Frame { path, w, h, seq } => {
                    if seq == self.seq {
                        continue;
                    }
                    if path != self.map_path {
                        self.map = mazda_ipc::FrameMap::open(std::path::Path::new(&path)).ok();
                        self.map_path = path;
                        self.tex = None;
                    }
                    if let Some(m) = self.map.as_ref() {
                        if m.dims() == (w, h) {
                            self.seq = seq;
                            // the pixels changed: re-upload on the next frame
                            self.tex = None;
                            if let Some(t0) = self.sent.take() {
                                self.lat.push(t0.elapsed().as_secs_f32() * 1000.0);
                            }
                        }
                    }
                }
                Ev::Title(t) => self.title = t,
                Ev::Nav(url) => {
                    self.scroll = 0.0;
                    self.r.send(&format!("load {url}"));
                }
                Ev::Error(e) => self.err = Some(e),
                Ev::Exited => {
                    self.err = Some("renderer exited".into());
                    break;
                }
            }
        }
    }

    fn upload(&mut self, ctx: &egui::Context) {
        let Some(m) = self.map.as_ref() else { return };
        let (w, h) = m.dims();
        let px = m.pixels();
        let mut rgba = Vec::with_capacity(w * h * 4);
        for p in px.chunks_exact(4) {
            rgba.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
        let img = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
        self.tex = Some(ctx.load_texture("page", img, egui::TextureOptions::LINEAR));
    }
}

impl eframe::App for Shell {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain();

        // the renderer paints at the content size, so tell it when it changes
        let rect = ui.max_rect();
        let size = (
            rect.width().max(1.0).round() as usize,
            rect.height().max(1.0).round() as usize,
        );
        if size != self.size {
            self.size = size;
            self.r.send(&format!("resize {} {}", size.0, size.1));
        }

        // input -> renderer
        let wheel = ui.input(|i| i.smooth_scroll_delta().y);
        let click = ui.input(|i| {
            i.pointer
                .primary_clicked()
                .then(|| i.pointer.interact_pos())
                .flatten()
        });
        if wheel != 0.0 {
            self.scroll = (self.scroll - wheel).max(0.0);
            self.r.send(&format!("scroll {:.1}", self.scroll));
            self.sent = Some(Instant::now());
        }
        if let Some(p) = click {
            self.r.send(&format!("click {:.1} {:.1}", p.x, p.y));
        }

        if self.seq != 0 && self.tex.is_none() {
            self.upload(ui.ctx());
        }

        if let Some(t) = &self.tex {
            let rect = ui.max_rect();
            ui.painter().image(
                t.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }

        // status overlay (the spike's only chrome)
        let mut status = self.title.clone();
        if let Some(e) = &self.err {
            status = format!("{status}  ·  {e}");
        }
        if !self.lat.is_empty() {
            let mut v = self.lat.clone();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let p50 = v[v.len() / 2];
            let p95 = v[(v.len() * 95 / 100).min(v.len() - 1)];
            status = format!("{status}  ·  scroll p50 {p50:.1} ms · p95 {p95:.1} ms");
        }
        ui.painter().text(
            egui::pos2(8.0, 6.0),
            egui::Align2::LEFT_TOP,
            status,
            egui::FontId::proportional(14.0),
            egui::Color32::from_rgb(20, 20, 20),
        );

        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(8));
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        if !self.lat.is_empty() {
            let mut v = self.lat.clone();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let p = |q: usize| v[(q * v.len() / 100).min(v.len() - 1)];
            println!(
                "[spike] scroll latency p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms ({} samples)",
                p(50),
                p(95),
                v[v.len() - 1],
                v.len()
            );
        }
    }
}

fn main() -> eframe::Result {
    let url = std::env::args()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .unwrap_or_else(|| "about:blank".to_string());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Mazda (Phase S)")
            .with_inner_size([1000.0, 700.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Mazda (Phase S)",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(
                Shell::new(&url).expect("failed to spawn renderer"),
            ))
        }),
    )
}
