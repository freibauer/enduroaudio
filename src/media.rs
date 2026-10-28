//! Dekodieren, Analysieren und Verarbeiten einer MP4-Datei.

use crate::dsp::{self, Analysis, Analyzer, GainApplier, GainCurve, Limiter, Params};
use crate::mux::{self, AudioInfo, Muxer};
use fdk_aac::enc::{AudioObjectType, BitRate, ChannelMode, Encoder, EncoderParams, Transport};
use std::sync::atomic::{AtomicBool, Ordering};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymErr;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

pub type Result<T> = std::result::Result<T, String>;

const CANCELLED: &str = "Abgebrochen";

/// Dekodiert die erste Audiospur und ruft `sink(sample_rate, channels, samples, fortschritt)`
/// für jeden Block interleaved Samples auf.
fn decode_audio(
    path: &str,
    cancel: &AtomicBool,
    mut sink: impl FnMut(u32, usize, &[f32], f32) -> Result<()>,
) -> Result<()> {
    let file = std::fs::File::open(path).map_err(|e| format!("Datei kann nicht geöffnet werden: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("mp4");
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("Format wird nicht unterstützt: {e}"))?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.sample_rate.is_some())
        .ok_or("Keine Audiospur gefunden")?
        .clone();
    let total = track.codec_params.n_frames.unwrap_or(0).max(1) as f32;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("Audio-Codec wird nicht unterstützt: {e}"))?;

    let mut buf: Option<SampleBuffer<f32>> = None;
    let mut spec: Option<(u32, usize)> = None;
    let mut silence = Vec::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCELLED.into());
        }
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymErr::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymErr::ResetRequired) => break,
            Err(e) => return Err(format!("Lesefehler: {e}")),
        };
        if packet.track_id() != track.id {
            continue;
        }
        let progress = packet.ts() as f32 / total;
        match decoder.decode(&packet) {
            Ok(decoded) => {
                let s = *decoded.spec();
                if buf.as_ref().is_none_or(|b| b.capacity() < decoded.capacity() * s.channels.count()) {
                    buf = Some(SampleBuffer::new(decoded.capacity() as u64, s));
                }
                let b = buf.as_mut().unwrap();
                b.copy_interleaved_ref(decoded);
                let ch = s.channels.count();
                spec = Some((s.rate, ch));
                sink(s.rate, ch, b.samples(), progress)?;
            }
            Err(SymErr::DecodeError(_)) => {
                // Defektes Paket: durch Stille gleicher Länge ersetzen, damit A/V synchron bleibt
                if let Some((rate, ch)) = spec {
                    silence.resize(packet.dur as usize * ch, 0.0);
                    sink(rate, ch, &silence, progress)?;
                }
            }
            Err(e) => return Err(format!("Dekodierfehler: {e}")),
        }
    }
    Ok(())
}

/// Durchlauf 1: Pegel der gesamten Datei messen.
pub fn analyze(path: &str, cancel: &AtomicBool, mut progress: impl FnMut(f32)) -> Result<Analysis> {
    let mut analyzer: Option<Analyzer> = None;
    decode_audio(path, cancel, |sr, ch, samples, p| {
        analyzer.get_or_insert_with(|| Analyzer::new(sr, ch)).push(samples);
        progress(p);
        Ok(())
    })?;
    let an = analyzer.ok_or("Die Audiospur enthält keine Daten")?.finish();
    Ok(an)
}

struct AacSink {
    encoder: Encoder,
    channels: usize,
    frame_len: usize,
    pcm: Vec<i16>,
    out: Vec<u8>,
}

impl AacSink {
    fn push(&mut self, samples: &[f32], mux: &mut Muxer) -> Result<()> {
        self.pcm.extend(samples.iter().map(|s| (s * 32767.0).round().clamp(-32768.0, 32767.0) as i16));
        let block = self.frame_len * self.channels;
        let mut start = 0;
        while self.pcm.len() - start >= block {
            self.encode(start, start + block, mux)?;
            start += block;
        }
        self.pcm.drain(..start);
        Ok(())
    }

    fn encode(&mut self, from: usize, to: usize, mux: &mut Muxer) -> Result<()> {
        let mut off = from;
        while off < to {
            let info = self
                .encoder
                .encode(&self.pcm[off..to], &mut self.out)
                .map_err(|e| format!("AAC-Encoder: {e}"))?;
            off += info.input_consumed;
            if info.output_size > 0 {
                mux.push_audio_frame(&self.out[..info.output_size])?;
            }
            if info.input_consumed == 0 && info.output_size == 0 {
                break;
            }
        }
        Ok(())
    }
}

/// Durchlauf 2: Verstärkung anwenden, neu kodieren und mit dem Originalvideo muxen.
pub fn process(
    input: &str,
    output: &str,
    analysis: &Analysis,
    curve: &GainCurve,
    params: &Params,
    cancel: &AtomicBool,
    mut progress: impl FnMut(f32),
) -> Result<()> {
    if same_file(input, output) {
        return Err("Ausgabedatei darf nicht die Eingabedatei sein".into());
    }
    let source = mux::read_source(input)?;
    let sr = analysis.sample_rate;
    let src_ch = analysis.channels;
    let channels = src_ch.min(2);

    let encoder = Encoder::new(EncoderParams {
        bit_rate: BitRate::Cbr(params.bitrate_kbps * 1000 * channels as u32 / 2),
        sample_rate: sr,
        transport: Transport::Raw,
        channels: if channels == 1 { ChannelMode::Mono } else { ChannelMode::Stereo },
        audio_object_type: AudioObjectType::Mpeg4LowComplexity,
    })
    .map_err(|e| format!("AAC-Encoder kann nicht initialisiert werden: {e}"))?;
    let info = encoder.info().map_err(|e| format!("AAC-Encoder: {e}"))?;
    let frame_len = info.frameLength as usize;
    let mut sink = AacSink {
        encoder,
        channels,
        frame_len,
        pcm: Vec::new(),
        out: vec![0u8; (info.maxOutBufBytes as usize).max(8192)],
    };

    let mut mux = Muxer::new(input, source, output, frame_len as u32, sr)?;
    let mut gain = GainApplier::new(curve, analysis.hop, channels);
    let mut limiter = Limiter::new(sr, channels, params.ceiling_db);
    let mut total: u64 = 0;
    let mut work = Vec::new();
    let mut limited = Vec::new();

    let mut stage = |samples: &[f32], limiter: &mut Limiter, sink: &mut AacSink, mux: &mut Muxer| -> Result<()> {
        limited.clear();
        limiter.process(samples, &mut limited);
        sink.push(&limited, mux)
    };

    decode_audio(input, cancel, |rate, ch, samples, p| {
        if rate != sr || ch != src_ch {
            return Err("Audioformat ändert sich innerhalb der Datei".into());
        }
        work.clear();
        if ch == channels {
            work.extend_from_slice(samples);
        } else {
            // Mehr als zwei Kanäle: die ersten beiden übernehmen
            for f in samples.chunks_exact(ch) {
                work.extend_from_slice(&f[..channels]);
            }
        }
        total += (work.len() / channels) as u64;
        gain.process(&mut work);
        stage(&work, &mut limiter, &mut sink, &mut mux)?;
        progress(p * 0.98);
        Ok(())
    })?;

    // Limiter leeren, dann mit Stille auffüllen, bis der Encoder alles ausgegeben hat
    let latency = limiter.latency();
    stage(&vec![0.0; latency * channels], &mut limiter, &mut sink, &mut mux)?;
    let needed = info.nDelay as u64 + total;
    let zeros = vec![0.0f32; frame_len * channels];
    let mut guard = 0;
    while mux.audio_frames() * (frame_len as u64) < needed + frame_len as u64 && guard < 64 {
        sink.push(&zeros, &mut mux)?;
        guard += 1;
    }

    let audio_info = AudioInfo {
        sample_rate: sr,
        channels: channels as u16,
        audio_specific_config: info.confBuf[..info.confSize as usize].to_vec(),
        avg_bitrate: params.bitrate_kbps * 1000 * channels as u32 / 2,
        encoder_delay: info.nDelay,
        duration_samples: total,
    };
    mux.finish(&audio_info)?;
    progress(1.0);
    Ok(())
}

fn same_file(a: &str, b: &str) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Zusammenfassung für die Anzeige.
pub struct Summary {
    pub quiet_sections: Vec<(f64, f64, f32)>,
    pub spread_before: f32,
    pub spread_after: f32,
}

pub fn summarize(an: &Analysis, curve: &GainCurve, p: &Params) -> Summary {
    let fps = dsp::FRAMES_PER_SEC as f64;
    // Abschnitte mit deutlicher Anhebung (> 6 dB) zusammenfassen
    let mut quiet = Vec::new();
    let mut start: Option<usize> = None;
    let mut maxg = 0.0f32;
    for (i, &g) in curve.gain_db.iter().chain(std::iter::once(&0.0)).enumerate() {
        if g > 6.0 {
            if start.is_none() {
                start = Some(i);
                maxg = 0.0;
            }
            maxg = maxg.max(g);
        } else if let Some(s) = start.take() {
            if (i - s) as f64 / fps >= 3.0 {
                quiet.push((s as f64 / fps, i as f64 / fps, maxg));
            }
        }
    }
    // Streuung des Sprachpegels in 30-s-Blöcken vor/nach der Verarbeitung
    let spread = |with_gain: bool| -> f32 {
        let w = 30 * dsp::FRAMES_PER_SEC;
        let levels: Vec<f64> = an
            .frame_db
            .chunks(w)
            .enumerate()
            .filter_map(|(bi, c)| {
                let act: Vec<f64> = c
                    .iter()
                    .enumerate()
                    .filter(|(_, d)| **d > p.gate_db.max(-45.0))
                    .map(|(j, d)| {
                        let g = if with_gain { curve.gain_db[bi * w + j] } else { 0.0 };
                        10f64.powf((d + g) as f64 / 10.0)
                    })
                    .collect();
                (act.len() > w / 10).then(|| 10.0 * (act.iter().sum::<f64>() / act.len() as f64).log10())
            })
            .collect();
        if levels.is_empty() {
            return 0.0;
        }
        let mean = levels.iter().sum::<f64>() / levels.len() as f64;
        (levels.iter().map(|l| (l - mean).powi(2)).sum::<f64>() / levels.len() as f64).sqrt() as f32
    };
    Summary {
        quiet_sections: quiet,
        spread_before: spread(false),
        spread_after: spread(true),
    }
}
