//! Rendert Screenshots der Oberfläche ohne Fenster und ohne GPU (für die Dokumentation).
//!
//! egui tesselliert die Oberfläche zu Dreiecken; diese werden hier per CPU gerastert.
//! Aufruf: enduroaudio --screenshots <eingabe.mp4> <ausgabeordner> [verarbeitungsdauer_s]

use super::{App, Job, JobKind, JobState};
use crate::media;
use egui::epaint;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SIZE: [f32; 2] = [1100.0, 860.0];

struct Texture {
    w: usize,
    h: usize,
    /// Vormultiplizierte Farben, 0..1
    px: Vec<[f32; 4]>,
}

impl Texture {
    fn sample(&self, u: f32, v: f32) -> [f32; 4] {
        // Bilinear
        let x = (u * self.w as f32 - 0.5).clamp(0.0, (self.w - 1) as f32);
        let y = (v * self.h as f32 - 0.5).clamp(0.0, (self.h - 1) as f32);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let p = |x: usize, y: usize| self.px[y * self.w + x];
        let mut out = [0.0; 4];
        for (i, o) in out.iter_mut().enumerate() {
            let a = p(x0, y0)[i] * (1.0 - fx) + p(x1, y0)[i] * fx;
            let b = p(x0, y1)[i] * (1.0 - fx) + p(x1, y1)[i] * fx;
            *o = a * (1.0 - fy) + b * fy;
        }
        out
    }
}

struct Renderer {
    ctx: egui::Context,
    textures: HashMap<egui::TextureId, Texture>,
    ppp: f32,
}

impl Renderer {
    fn new(ppp: f32) -> Self {
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(ppp);
        ctx.set_theme(egui::Theme::Dark);
        Self { ctx, textures: HashMap::new(), ppp }
    }

    fn apply_textures(&mut self, delta: &epaint::textures::TexturesDelta) {
        for (id, deltas) in &delta.set {
            for d in deltas {
                let egui::ImageData::Color(img) = &d.image;
                let px: Vec<[f32; 4]> = img
                    .pixels
                    .iter()
                    .map(|c| [c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0, c.a() as f32 / 255.0])
                    .collect();
                match d.pos {
                    None => {
                        self.textures.insert(*id, Texture { w: img.size[0], h: img.size[1], px });
                    }
                    Some([ox, oy]) => {
                        let t = self.textures.get_mut(id).expect("Teilupdate einer unbekannten Textur");
                        for y in 0..img.size[1] {
                            for x in 0..img.size[0] {
                                t.px[(oy + y) * t.w + ox + x] = px[y * img.size[0] + x];
                            }
                        }
                    }
                }
            }
        }
    }

    /// Mehrere Durchläufe, damit Layout und Plot-Grenzen eingeschwungen sind; der letzte wird gerastert.
    fn frame(&mut self, app: &mut App) -> (usize, usize, Vec<[f32; 3]>) {
        let mut out = None;
        for i in 0..6 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SIZE.into())),
                time: Some(i as f64 * 0.1),
                ..Default::default()
            };
            let o = self.ctx.run_ui(input, |ui| app.draw(ui));
            self.apply_textures(&o.textures_delta);
            out = Some(o);
        }
        let out = out.unwrap();
        let prims = self.ctx.tessellate(out.shapes, self.ppp);
        self.rasterize(&prims)
    }

    fn rasterize(&self, prims: &[epaint::ClippedPrimitive]) -> (usize, usize, Vec<[f32; 3]>) {
        let w = (SIZE[0] * self.ppp) as usize;
        let h = (SIZE[1] * self.ppp) as usize;
        let mut buf = vec![[0.0f32; 3]; w * h];
        for p in prims {
            let epaint::Primitive::Mesh(mesh) = &p.primitive else { continue };
            let Some(tex) = self.textures.get(&mesh.texture_id) else { continue };
            let cx0 = (p.clip_rect.min.x * self.ppp).max(0.0).floor() as i64;
            let cy0 = (p.clip_rect.min.y * self.ppp).max(0.0).floor() as i64;
            let cx1 = ((p.clip_rect.max.x * self.ppp).ceil() as i64).min(w as i64);
            let cy1 = ((p.clip_rect.max.y * self.ppp).ceil() as i64).min(h as i64);
            for tri in mesh.indices.chunks_exact(3) {
                let v = [&mesh.vertices[tri[0] as usize], &mesh.vertices[tri[1] as usize], &mesh.vertices[tri[2] as usize]];
                let pos: Vec<[f32; 2]> = v.iter().map(|v| [v.pos.x * self.ppp, v.pos.y * self.ppp]).collect();
                let area = (pos[1][0] - pos[0][0]) * (pos[2][1] - pos[0][1]) - (pos[2][0] - pos[0][0]) * (pos[1][1] - pos[0][1]);
                if area.abs() < 1e-9 {
                    continue;
                }
                let x0 = (pos.iter().map(|p| p[0]).fold(f32::MAX, f32::min).floor() as i64).max(cx0);
                let x1 = (pos.iter().map(|p| p[0]).fold(f32::MIN, f32::max).ceil() as i64).min(cx1);
                let y0 = (pos.iter().map(|p| p[1]).fold(f32::MAX, f32::min).floor() as i64).max(cy0);
                let y1 = (pos.iter().map(|p| p[1]).fold(f32::MIN, f32::max).ceil() as i64).min(cy1);
                let col: Vec<[f32; 4]> = v
                    .iter()
                    .map(|v| {
                        let c = v.color;
                        [c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0, c.a() as f32 / 255.0]
                    })
                    .collect();
                let edge = |a: [f32; 2], b: [f32; 2], x: f32, y: f32| (b[0] - a[0]) * (y - a[1]) - (b[1] - a[1]) * (x - a[0]);
                for y in y0..y1 {
                    for x in x0..x1 {
                        let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                        let w0 = edge(pos[1], pos[2], px, py) / area;
                        let w1 = edge(pos[2], pos[0], px, py) / area;
                        let w2 = edge(pos[0], pos[1], px, py) / area;
                        if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                            continue;
                        }
                        let u = w0 * v[0].uv.x + w1 * v[1].uv.x + w2 * v[2].uv.x;
                        let t = w0 * v[0].uv.y + w1 * v[1].uv.y + w2 * v[2].uv.y;
                        let s = tex.sample(u, t);
                        let mut src = [0.0f32; 4];
                        for i in 0..4 {
                            src[i] = (w0 * col[0][i] + w1 * col[1][i] + w2 * col[2][i]) * s[i];
                        }
                        let d = &mut buf[y as usize * w + x as usize];
                        for i in 0..3 {
                            d[i] = src[i] + d[i] * (1.0 - src[3]);
                        }
                    }
                }
            }
        }
        (w, h, buf)
    }
}

fn save_ppm(path: &std::path::Path, (w, h, buf): (usize, usize, Vec<[f32; 3]>)) -> Result<(), String> {
    let mut data = format!("P6\n{w} {h}\n255\n").into_bytes();
    for p in buf {
        data.extend(p.iter().map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8));
    }
    std::fs::write(path, data).map_err(|e| e.to_string())
}

pub fn run(args: &[String]) -> Result<(), String> {
    let [input, dir, ..] = args else {
        return Err("Aufruf: enduroaudio --screenshots <eingabe.mp4> <ausgabeordner>".into());
    };
    let dir = std::path::Path::new(dir);
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut r = Renderer::new(1.0);
    // Anzeigename wie unter Windows
    let shown = format!(r"D:\Videos\{}", std::path::Path::new(input).file_name().unwrap().to_string_lossy());

    // 1) Datei gewählt
    let mut app = App::new(Some(shown.clone()));
    save_ppm(&dir.join("1_datei_gewaehlt.ppm"), r.frame(&mut app))?;

    // 2) Analyse läuft
    let job = |kind, progress: f32, secs: u64| Job {
        kind,
        state: Arc::new(Mutex::new(JobState { progress, result: None })),
        cancel: Arc::new(AtomicBool::new(false)),
        started: Instant::now() - Duration::from_secs(secs),
    };
    app.job = Some(job(JobKind::Analyze, 0.37, 13));
    save_ppm(&dir.join("2_analyse.ppm"), r.frame(&mut app))?;

    // 3) Ergebnis der echten Analyse
    let t = Instant::now();
    let an = media::analyze(input, &AtomicBool::new(false), |_| {})?;
    app.job = None;
    app.status = format!(
        "Analyse fertig ({:.0} s): {}, {} Hz, {} Kanal/Kanäle, Spitzenpegel {:.1} dBFS.",
        t.elapsed().as_secs_f64(),
        crate::fmt_time(an.total_samples as f64 / an.sample_rate as f64),
        an.sample_rate,
        an.channels,
        an.peak_db
    );
    app.analysis = Some(Arc::new(an));
    save_ppm(&dir.join("3_analyse_ergebnis.ppm"), r.frame(&mut app))?;

    // 4) Verarbeitung läuft
    app.job = Some(job(JobKind::Process, 0.58, 75));
    save_ppm(&dir.join("4_verarbeitung.ppm"), r.frame(&mut app))?;

    // 5) Fertig
    app.job = None;
    // Tatsächliche Verarbeitungsdauer aus einem echten Lauf (optional übergeben)
    let secs = args.get(2).map_or("?", String::as_str);
    app.status = format!("Fertig ({secs} s). Gespeichert: {}", app.output);
    save_ppm(&dir.join("5_fertig.ppm"), r.frame(&mut app))?;
    Ok(())
}
