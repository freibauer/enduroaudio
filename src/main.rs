#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod dsp;
mod media;
mod mux;

#[cfg(feature = "ui")]
mod gui;

use dsp::Params;
use std::path::Path;
use std::sync::atomic::AtomicBool;

pub(crate) fn fmt_time(s: f64) -> String {
    let s = s.max(0.0) as u64;
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

pub(crate) fn default_output(input: &str) -> String {
    let p = Path::new(input);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "mp4".into());
    p.with_file_name(format!("{stem}_laut.{ext}")).to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------------------
// Kommandozeile (zum Testen): enduroaudio --cli <eingabe> [ausgabe]

fn run_cli(args: &[String]) -> Result<(), String> {
    let input = args.first().ok_or("Aufruf: enduroaudio --cli <eingabe.mp4> [ausgabe.mp4]")?;
    let output = args.get(1).cloned().unwrap_or_else(|| default_output(input));
    let cancel = AtomicBool::new(false);
    let p = Params::default();
    let t = std::time::Instant::now();
    let an = media::analyze(input, &cancel, |_| {})?;
    eprintln!("Analyse: {:.1} s, {} Frames, Spitze {:.1} dBFS", t.elapsed().as_secs_f32(), an.frame_db.len(), an.peak_db);
    let curve = dsp::compute_gain(&an, &p);
    let s = media::summarize(&an, &curve, &p);
    eprintln!("Schwankung vorher ±{:.2} dB, nachher ±{:.2} dB", s.spread_before, s.spread_after);
    for (a, b, g) in &s.quiet_sections {
        eprintln!("  leise: {} – {} +{:.1} dB", fmt_time(*a), fmt_time(*b), g);
    }
    if let Ok(path) = std::env::var("ENDUROAUDIO_DUMP_GAIN") {
        let txt: String = curve.gain_db.iter().zip(&curve.level_db).map(|(g, l)| format!("{l:.2} {g:.2}\n")).collect();
        std::fs::write(path, txt).map_err(|e| e.to_string())?;
    }
    let t = std::time::Instant::now();
    media::process(input, &output, &an, &curve, &p, &cancel, |_| {})?;
    eprintln!("Verarbeitung: {:.1} s -> {output}", t.elapsed().as_secs_f32());
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--cli") {
        if let Err(e) = run_cli(&args[1..]) {
            eprintln!("Fehler: {e}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(feature = "screenshot")]
    if args.first().map(String::as_str) == Some("--screenshots") {
        if let Err(e) = gui::screenshot::run(&args[1..]) {
            eprintln!("Fehler: {e}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(feature = "gui")]
    if let Err(e) = gui::run(args.into_iter().next()) {
        eprintln!("Fehler: {e}");
        std::process::exit(1);
    }
    #[cfg(not(feature = "gui"))]
    eprintln!("Ohne GUI gebaut. Aufruf: enduroaudio --cli <eingabe.mp4> [ausgabe.mp4]");
}
