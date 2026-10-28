//! Grafische Oberfläche.


use crate::dsp::{self, Analysis, GainCurve, Params};
use crate::media;
use crate::{default_output, fmt_time};
use eframe::egui;
use egui_plot::{Corner, Legend, Line, Plot, PlotPoints};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------------------
// Hintergrundjobs

enum JobKind {
    Analyze,
    Process,
}

enum JobResult {
    Analyzed(Analysis),
    Processed,
}

#[derive(Default)]
struct JobState {
    progress: f32,
    result: Option<Result<JobResult, String>>,
}

struct Job {
    kind: JobKind,
    state: Arc<Mutex<JobState>>,
    cancel: Arc<AtomicBool>,
    started: std::time::Instant,
}

impl Job {
    fn spawn(
        kind: JobKind,
        ctx: &egui::Context,
        work: impl FnOnce(&AtomicBool, &dyn Fn(f32)) -> Result<JobResult, String> + Send + 'static,
    ) -> Self {
        let state = Arc::new(Mutex::new(JobState::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let (st, c, ctx) = (state.clone(), cancel.clone(), ctx.clone());
        std::thread::spawn(move || {
            let report = |p: f32| {
                let mut s = st.lock().unwrap();
                // Nur bei sichtbarer Änderung neu zeichnen
                if (p - s.progress).abs() > 0.002 {
                    s.progress = p;
                    ctx.request_repaint();
                }
            };
            let r = work(&c, &report);
            st.lock().unwrap().result = Some(r);
            ctx.request_repaint();
        });
        Self { kind, state, cancel, started: std::time::Instant::now() }
    }
}

// ---------------------------------------------------------------------------------------
// Dateidialoge

fn pick_input() -> Option<String> {
    rfd::FileDialog::new()
        .add_filter("Video/Audio (MP4)", &["mp4", "m4a", "m4v", "mov"])
        .pick_file()
        .map(|p| p.to_string_lossy().into_owned())
}

fn pick_output(current: &str) -> Option<String> {
    let p = Path::new(current);
    let mut dlg = rfd::FileDialog::new().add_filter("MP4", &["mp4", "m4a", "m4v", "mov"]);
    if let Some(d) = p.parent() {
        dlg = dlg.set_directory(d);
    }
    if let Some(n) = p.file_name() {
        dlg = dlg.set_file_name(n.to_string_lossy());
    }
    dlg.save_file().map(|p| p.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------------------
// GUI

struct PlotData {
    before: Vec<[f64; 2]>,
    after: Vec<[f64; 2]>,
    gain: Vec<[f64; 2]>,
}

struct App {
    input: String,
    output: String,
    params: Params,
    analysis: Option<Arc<Analysis>>,
    curve: Option<(Params, GainCurve, media::Summary, PlotData)>,
    job: Option<Job>,
    status: String,
    error: bool,
}

impl App {
    fn new(input: Option<String>) -> Self {
        let mut app = Self {
            input: String::new(),
            output: String::new(),
            params: Params::default(),
            analysis: None,
            curve: None,
            job: None,
            status: "Bitte eine MP4-Datei wählen oder ins Fenster ziehen.".into(),
            error: false,
        };
        if let Some(i) = input {
            app.set_input(i);
        }
        app
    }

    fn set_input(&mut self, path: String) {
        self.output = default_output(&path);
        self.input = path;
        self.analysis = None;
        self.curve = None;
        self.status = "Datei gewählt. Als Nächstes: „Analysieren“.".into();
        self.error = false;
    }

    fn busy(&self) -> bool {
        self.job.is_some()
    }

    fn start_analyze(&mut self, ctx: &egui::Context) {
        let input = self.input.clone();
        self.analysis = None;
        self.curve = None;
        self.job = Some(Job::spawn(JobKind::Analyze, ctx, move |cancel, report| {
            media::analyze(&input, cancel, report).map(JobResult::Analyzed)
        }));
    }

    fn start_process(&mut self, ctx: &egui::Context) {
        let (Some(an), Some((p, curve, ..))) = (self.analysis.clone(), self.curve.as_ref()) else { return };
        let (input, output, curve, mut p) = (self.input.clone(), self.output.clone(), curve.clone(), p.clone());
        p.ceiling_db = self.params.ceiling_db;
        p.bitrate_kbps = self.params.bitrate_kbps;
        self.job = Some(Job::spawn(JobKind::Process, ctx, move |cancel, report| {
            media::process(&input, &output, &an, &curve, &p, cancel, report).map(|_| JobResult::Processed)
        }));
    }

    /// Verstärkungskurve neu berechnen, wenn sich relevante Parameter geändert haben.
    fn update_curve(&mut self) {
        let Some(an) = &self.analysis else { return };
        let mut key = self.params.clone();
        key.ceiling_db = 0.0;
        key.bitrate_kbps = 0;
        if self.curve.as_ref().is_some_and(|c| c.0 == key) {
            return;
        }
        let curve = dsp::compute_gain(an, &self.params);
        let summary = media::summarize(an, &curve, &self.params);
        // Für die Anzeige auf 1 Punkt pro Sekunde reduzieren
        let fps = dsp::FRAMES_PER_SEC;
        let mut plot = PlotData { before: vec![], after: vec![], gain: vec![] };
        for (i, (l, g)) in curve.level_db.chunks(fps).zip(curve.gain_db.chunks(fps)).enumerate() {
            let l = l.iter().sum::<f32>() / l.len() as f32;
            let g = g.iter().sum::<f32>() / g.len() as f32;
            let t = i as f64;
            plot.before.push([t, l as f64]);
            plot.after.push([t, (l + g) as f64]);
            plot.gain.push([t, g as f64]);
        }
        self.curve = Some((key, curve, summary, plot));
    }

    fn poll_job(&mut self) {
        let Some(job) = &self.job else { return };
        let Some(result) = job.state.lock().unwrap().result.take() else { return };
        let secs = job.started.elapsed().as_secs_f64();
        match result {
            Ok(JobResult::Analyzed(an)) => {
                self.status = format!(
                    "Analyse fertig ({:.0} s): {}, {} Hz, {} Kanal/Kanäle, Spitzenpegel {:.1} dBFS.",
                    secs,
                    fmt_time(an.total_samples as f64 / an.sample_rate as f64),
                    an.sample_rate,
                    an.channels,
                    an.peak_db
                );
                self.error = false;
                self.analysis = Some(Arc::new(an));
            }
            Ok(JobResult::Processed) => {
                self.status = format!("Fertig ({:.0} s). Gespeichert: {}", secs, self.output);
                self.error = false;
            }
            Err(e) => {
                if matches!(job.kind, JobKind::Process) && e != "Abgebrochen" {
                    let _ = std::fs::remove_file(&self.output);
                }
                self.status = format!("Fehler: {e}");
                self.error = true;
                if e == "Abgebrochen" {
                    self.status = "Abgebrochen.".into();
                    self.error = false;
                    if matches!(job.kind, JobKind::Process) {
                        let _ = std::fs::remove_file(&self.output);
                    }
                }
            }
        }
        self.job = None;
    }

    fn files_ui(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("files").num_columns(3).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.label("Eingabe:");
            let w = (ui.available_width() - 110.0).max(200.0);
            ui.add(egui::TextEdit::singleline(&mut self.input.clone()).desired_width(w).interactive(false));
            if ui.add_enabled(!self.busy(), egui::Button::new("Öffnen …")).clicked()
                && let Some(p) = pick_input()
            {
                self.set_input(p);
            }
            ui.end_row();

            ui.label("Ausgabe:");
            ui.add_enabled(!self.busy(), egui::TextEdit::singleline(&mut self.output).desired_width(w));
            if ui.add_enabled(!self.busy() && !self.input.is_empty(), egui::Button::new("Speichern unter …")).clicked() {
                if let Some(p) = pick_output(&self.output) {
                    self.output = p;
                }
            }
            ui.end_row();
        });
    }

    fn params_ui(&mut self, ui: &mut egui::Ui) {
        let enabled = !self.busy();
        ui.add_enabled_ui(enabled, |ui| {
            egui::Grid::new("params").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
                let p = &mut self.params;
                ui.label("Ziel-Sprachpegel").on_hover_text("Auf diesen Pegel (RMS der Sprache, ohne Pausen) wird geregelt.");
                ui.add(egui::Slider::new(&mut p.target_db, -30.0..=-14.0).suffix(" dBFS").step_by(0.5));
                ui.end_row();
                ui.label("Regelfenster").on_hover_text(
                    "Zeitraum, über den der Sprachpegel gemittelt wird. Kürzer = leise Stellen werden schneller \
                     ausgeglichen, aber die Dynamik innerhalb von Sätzen wird stärker eingeebnet.",
                );
                ui.add(egui::Slider::new(&mut p.window_s, 3.0..=60.0).suffix(" s").step_by(1.0));
                ui.end_row();
                ui.label("Max. Anhebung");
                ui.add(egui::Slider::new(&mut p.max_boost_db, 0.0..=30.0).suffix(" dB").step_by(1.0));
                ui.end_row();
                ui.label("Max. Absenkung").on_hover_text("Zu laute Stellen werden höchstens so weit abgesenkt.");
                ui.add(egui::Slider::new(&mut p.max_cut_db, 0.0..=20.0).suffix(" dB").step_by(1.0));
                ui.end_row();
                ui.label("Stille-Schwelle").on_hover_text(
                    "Alles darunter gilt als Pause und wird bei der Pegelmessung ignoriert (nicht angehoben).",
                );
                ui.add(egui::Slider::new(&mut p.gate_db, -70.0..=-35.0).suffix(" dBFS").step_by(1.0));
                ui.end_row();
                ui.label("Limiter-Grenze").on_hover_text("Spitzen werden auf diesen Wert begrenzt, damit nichts übersteuert.");
                ui.add(egui::Slider::new(&mut p.ceiling_db, -6.0..=0.0).suffix(" dBFS").step_by(0.1));
                ui.end_row();
                ui.label("AAC-Bitrate (Stereo)");
                egui::ComboBox::from_id_salt("br").selected_text(format!("{} kbit/s", p.bitrate_kbps)).show_ui(ui, |ui| {
                    for b in [128, 160, 192, 256, 320] {
                        ui.selectable_value(&mut p.bitrate_kbps, b, format!("{b} kbit/s"));
                    }
                });
                ui.end_row();
            });
            if ui.button("Standardwerte").clicked() {
                self.params = Params::default();
            }
        });
    }

    fn plot_ui(&self, ui: &mut egui::Ui) {
        let Some((_, _, summary, plot)) = &self.curve else { return };
        ui.horizontal_wrapped(|ui| {
            ui.label(format!(
                "Schwankung des Sprachpegels (30-s-Blöcke): vorher ±{:.1} dB → nachher ±{:.1} dB",
                summary.spread_before, summary.spread_after
            ));
        });
        let link = egui::Id::unique("pegel-link");
        let fmt_x = |m: egui_plot::GridMark, _: &std::ops::RangeInclusive<f64>| fmt_time(m.value);
        let h = ((ui.available_height() - 130.0) * 0.62).max(120.0);
        Plot::new("level")
            .height(h)
            .legend(Legend::default().position(Corner::RightBottom))
            .link_axis(link, [true, false])
            .link_cursor(link, [true, false])
            .x_axis_formatter(fmt_x)
            .y_axis_label("Sprachpegel dBFS")
            .y_axis_min_width(48.0)
            .label_formatter(|_| None)
            .default_y_bounds(-50.0, -10.0)
            .allow_scroll(false)
            .show(ui, |pu| {
                pu.line(Line::new("vorher", PlotPoints::from(plot.before.clone())).color(egui::Color32::from_rgb(220, 90, 70)));
                pu.line(Line::new("nachher", PlotPoints::from(plot.after.clone())).color(egui::Color32::from_rgb(70, 160, 230)));
            });
        Plot::new("gain")
            .height((h * 0.55).max(80.0))
            .link_axis(link, [true, false])
            .link_cursor(link, [true, false])
            .x_axis_formatter(fmt_x)
            .y_axis_label("Verstärkung dB")
            .y_axis_min_width(48.0)
            .label_formatter(|_| None)
            .default_y_bounds(-10.0, 22.0)
            .allow_scroll(false)
            .show(ui, |pu| {
                pu.line(Line::new("Verstärkung", PlotPoints::from(plot.gain.clone())).color(egui::Color32::from_rgb(120, 200, 120)));
            });
    }

    fn quiet_list_ui(&self, ui: &mut egui::Ui) {
        let Some((_, _, summary, _)) = &self.curve else { return };
        ui.label(format!("Leise Stellen (Anhebung > 6 dB): {}", summary.quiet_sections.len()));
        egui::ScrollArea::vertical().id_salt("quiet").max_height(ui.available_height()).show(ui, |ui| {
            for (a, b, g) in &summary.quiet_sections {
                ui.monospace(format!("{} – {}  +{:.0} dB", fmt_time(*a), fmt_time(*b), g));
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}

impl App {
    fn draw(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.poll_job();

        if !self.busy() {
            let dropped = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()));
            if let Some(p) = dropped {
                self.set_input(p.to_string_lossy().into_owned());
            }
        }
        self.update_curve();

        egui::Frame::central_panel(ui.style()).show(ui, |ui| {
            ui.heading("Leise Stellen anheben");
            ui.label(
                "Misst den Sprachpegel über den ganzen Stream und gleicht leise Abschnitte (z. B. nach Pausen) \
                 automatisch an. Das Video wird unverändert übernommen.",
            );
            ui.add_space(6.0);
            self.files_ui(ui);
            ui.separator();

            ui.horizontal(|ui| {
                let can = !self.busy() && !self.input.is_empty();
                if ui.add_enabled(can, egui::Button::new("1. Analysieren")).clicked() {
                    self.start_analyze(&ctx);
                }
                let can2 = can && self.curve.is_some() && !self.output.trim().is_empty();
                if ui.add_enabled(can2, egui::Button::new("2. Verarbeiten & speichern")).clicked() {
                    self.start_process(&ctx);
                }
                if let Some(job) = &self.job {
                    let p = job.state.lock().unwrap().progress;
                    let label = match job.kind {
                        JobKind::Analyze => "Analysiere",
                        JobKind::Process => "Verarbeite",
                    };
                    let el = job.started.elapsed().as_secs_f32();
                    let eta = if p > 0.02 { format!(", noch ca. {:.0} s", el / p - el) } else { String::new() };
                    ui.add(egui::ProgressBar::new(p).desired_width(260.0).text(format!("{label} … {:.0} %{eta}", p * 100.0)));
                    if ui.button("Abbrechen").clicked() {
                        job.cancel.store(true, Ordering::Relaxed);
                    }
                }
            });
            let color = if self.error { egui::Color32::from_rgb(230, 80, 80) } else { ui.visuals().text_color() };
            ui.colored_label(color, &self.status);
            ui.separator();

            ui.columns(2, |cols| {
                cols[0].set_max_width(430.0);
                self.params_ui(&mut cols[0]);
                cols[1].set_min_height(200.0);
                self.quiet_list_ui(&mut cols[1]);
            });
            ui.separator();
            if self.curve.is_some() {
                self.plot_ui(ui);
            } else if self.analysis.is_none() && !self.input.is_empty() && !self.busy() {
                ui.label("Nach der Analyse erscheint hier der Pegelverlauf.");
            }
        });
    }
}

pub fn run(input: Option<String>) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Enduro Audio – leise Stellen anheben")
            .with_inner_size([980.0, 800.0])
            .with_min_inner_size([700.0, 560.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("enduroaudio", options, Box::new(|_cc| Ok(Box::new(App::new(input)))))
}
