//! Signalverarbeitung: Pegelmessung, Berechnung der Verstärkungskurve und Limiter.
//!
//! Ablauf:
//! 1. Das Audiosignal wird in 50-ms-Frames zerlegt. Pro Frame wird die Energie des
//!    (hochpassgefilterten) Mono-Mixes gemessen.
//! 2. Für jeden Frame wird der lokale *Sprachpegel* bestimmt: energetischer Mittelwert
//!    aller aktiven Frames (über der Stille-Schwelle und max. 20 dB unter dem lokalen
//!    Pegel) in einem um den Frame zentrierten, gaußähnlich gewichteten Fenster.
//!    Stille (Noise-Gate, Pausen) wird dabei ignoriert.
//! 3. Verstärkung = Zielpegel - Sprachpegel, begrenzt auf [-max_cut, +max_boost].
//!    Da die ganze Datei vorab analysiert wird, ist das Fenster nicht-kausal: nach einer
//!    Pause stimmt die Verstärkung sofort, ohne Einregelzeit.
//! 4. Ein Lookahead-Limiter verhindert Übersteuerungen durch die Anhebung.

pub const FRAMES_PER_SEC: usize = 20;

#[derive(Clone, Debug, PartialEq)]
pub struct Params {
    /// Ziel-Sprachpegel in dBFS (RMS über aktive Frames).
    pub target_db: f32,
    /// Länge des Regelfensters in Sekunden.
    pub window_s: f32,
    /// Maximale Anhebung in dB.
    pub max_boost_db: f32,
    /// Maximale Absenkung in dB.
    pub max_cut_db: f32,
    /// Frames unterhalb dieser Schwelle gelten als Stille.
    pub gate_db: f32,
    /// Obergrenze des Limiters in dBFS.
    pub ceiling_db: f32,
    /// AAC-Bitrate in kbit/s.
    pub bitrate_kbps: u32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            target_db: -21.0,
            window_s: 10.0,
            max_boost_db: 20.0,
            max_cut_db: 8.0,
            gate_db: -52.0,
            ceiling_db: -1.0,
            bitrate_kbps: 192,
        }
    }
}

/// Ergebnis der Analyse (Durchlauf 1).
#[derive(Clone)]
pub struct Analysis {
    pub sample_rate: u32,
    pub channels: usize,
    /// Samples pro Frame (pro Kanal).
    pub hop: usize,
    /// Gesamtzahl Samples pro Kanal.
    pub total_samples: u64,
    /// Energie pro Frame in dBFS.
    pub frame_db: Vec<f32>,
    pub peak_db: f32,
}

/// Biquad-Hochpass (Butterworth), um tieffrequentes Rumpeln aus der Messung herauszuhalten.
#[derive(Clone, Copy)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn highpass(fc: f64, sr: f64) -> Self {
        let w0 = 2.0 * std::f64::consts::PI * fc / sr;
        let alpha = w0.sin() / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
        let cos = w0.cos();
        let a0 = 1.0 + alpha;
        Self {
            b0: (1.0 + cos) / 2.0 / a0,
            b1: -(1.0 + cos) / a0,
            b2: (1.0 + cos) / 2.0 / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// Sammelt Frame-Energien aus interleaved Samples.
pub struct Analyzer {
    channels: usize,
    hop: usize,
    hp: Biquad,
    acc: f64,
    count: usize,
    total: u64,
    peak: f32,
    frame_db: Vec<f32>,
    sample_rate: u32,
}

impl Analyzer {
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        Self {
            channels,
            hop: sample_rate as usize / FRAMES_PER_SEC,
            hp: Biquad::highpass(80.0, sample_rate as f64),
            acc: 0.0,
            count: 0,
            total: 0,
            peak: 0.0,
            frame_db: Vec::new(),
            sample_rate,
        }
    }

    pub fn push(&mut self, interleaved: &[f32]) {
        let ch = self.channels;
        for frame in interleaved.chunks_exact(ch) {
            let mut sum = 0.0f32;
            for &s in frame {
                sum += s;
                self.peak = self.peak.max(s.abs());
            }
            let y = self.hp.process((sum / ch as f32) as f64);
            self.acc += y * y;
            self.count += 1;
            self.total += 1;
            if self.count == self.hop {
                self.flush_frame();
            }
        }
    }

    fn flush_frame(&mut self) {
        let ms = self.acc / self.count.max(1) as f64;
        self.frame_db.push((10.0 * (ms + 1e-20).log10()) as f32);
        self.acc = 0.0;
        self.count = 0;
    }

    pub fn finish(mut self) -> Analysis {
        if self.count > 0 {
            self.flush_frame();
        }
        Analysis {
            sample_rate: self.sample_rate,
            channels: self.channels,
            hop: self.hop,
            total_samples: self.total,
            frame_db: self.frame_db,
            peak_db: 20.0 * (self.peak.max(1e-10)).log10(),
        }
    }
}

/// Verstärkungskurve (pro Frame).
#[derive(Clone)]
pub struct GainCurve {
    /// Gemessener lokaler Sprachpegel in dBFS.
    pub level_db: Vec<f32>,
    /// Anzuwendende Verstärkung in dB.
    pub gain_db: Vec<f32>,
}

/// Zentrierte gleitende Summe mit Fensterbreite `w` (Frames), Ränder werden abgeschnitten.
fn box_sum(x: &[f64], w: usize) -> Vec<f64> {
    let n = x.len();
    let h = w / 2;
    let mut prefix = Vec::with_capacity(n + 1);
    prefix.push(0.0);
    let mut acc = 0.0;
    for &v in x {
        acc += v;
        prefix.push(acc);
    }
    (0..n)
        .map(|i| prefix[(i + h + 1).min(n)] - prefix[i.saturating_sub(h)])
        .collect()
}

/// Dreifache Box-Filterung ≈ Gaußfenster mit Gesamtbreite `w`.
fn gauss_sum(x: &[f64], w: usize) -> Vec<f64> {
    let w3 = (w / 3).max(1);
    let a = box_sum(x, w3);
    let b = box_sum(&a, w3);
    box_sum(&b, w3)
}

fn gauss_mean(x: &[f64], w: usize) -> Vec<f64> {
    let s = gauss_sum(x, w);
    let c = gauss_sum(&vec![1.0; x.len()], w);
    s.iter().zip(&c).map(|(s, c)| s / c).collect()
}

/// Lokaler Pegel über aktive Frames; `None`, wo das Fenster zu wenig Aktivität enthält.
fn local_level(e: &[f64], active: &[f64], w: usize) -> Vec<Option<f64>> {
    let s = gauss_sum(&e.iter().zip(active).map(|(e, a)| e * a).collect::<Vec<_>>(), w);
    let c = gauss_sum(active, w);
    let total = gauss_sum(&vec![1.0; e.len()], w);
    (0..e.len())
        .map(|i| {
            // Mindestens 15 % Aktivität im Fenster, sonst gilt der Pegel als unbekannt
            if c[i] < 0.15 * total[i] || c[i] <= 0.0 {
                None
            } else {
                Some(10.0 * (s[i] / c[i] + 1e-20).log10())
            }
        })
        .collect()
}

/// Lineare Interpolation über unbekannte Werte, Ränder werden gehalten.
fn fill_gaps(v: &[Option<f64>], fallback: f64) -> Vec<f64> {
    let known: Vec<usize> = (0..v.len()).filter(|&i| v[i].is_some()).collect();
    if known.is_empty() {
        return vec![fallback; v.len()];
    }
    let mut out = vec![0.0; v.len()];
    let mut k = 0;
    for i in 0..v.len() {
        while k + 1 < known.len() && known[k + 1] <= i {
            k += 1;
        }
        let a = known[k];
        out[i] = if i <= a || k + 1 == known.len() {
            v[a].unwrap()
        } else {
            let b = known[k + 1];
            let t = (i - a) as f64 / (b - a) as f64;
            v[a].unwrap() * (1.0 - t) + v[b].unwrap() * t
        };
    }
    out
}

pub fn compute_gain(an: &Analysis, p: &Params) -> GainCurve {
    let n = an.frame_db.len();
    if n == 0 {
        return GainCurve { level_db: vec![], gain_db: vec![] };
    }
    let w = ((p.window_s.max(1.0) * FRAMES_PER_SEC as f32) as usize).max(3);
    let db: Vec<f64> = an.frame_db.iter().map(|&d| d as f64).collect();
    let e: Vec<f64> = db.iter().map(|d| 10f64.powf(d / 10.0)).collect();
    let gate = p.gate_db as f64;

    // Stufe 1: absolute Schwelle
    let act0: Vec<f64> = db.iter().map(|&d| if d > gate { 1.0 } else { 0.0 }).collect();
    let l0 = local_level(&e, &act0, w);
    // Stufe 2: zusätzlich relative Schwelle (20 dB unter lokalem Pegel), damit
    // Hintergrundgeräusche und Ausklingen den Sprachpegel nicht nach unten ziehen
    let act1: Vec<f64> = (0..n)
        .map(|i| {
            let thr = l0[i].map_or(gate, |l| (l - 20.0).max(gate));
            if db[i] > thr { 1.0 } else { 0.0 }
        })
        .collect();
    let level = fill_gaps(&local_level(&e, &act1, w), p.target_db as f64);

    let gain: Vec<f64> = level
        .iter()
        .map(|l| (p.target_db as f64 - l).clamp(-p.max_cut_db as f64, p.max_boost_db as f64))
        .collect();
    // Nachglätten (1 s), damit keine hörbaren Sprünge entstehen
    let gain = gauss_mean(&gain, FRAMES_PER_SEC);

    GainCurve {
        level_db: level.iter().map(|&v| v as f32).collect(),
        gain_db: gain.iter().map(|&v| v as f32).collect(),
    }
}

/// Wendet die Verstärkungskurve sampleweise (linear interpoliert) an.
pub struct GainApplier {
    gains: Vec<f32>,
    hop: f64,
    channels: usize,
    pos: u64,
}

impl GainApplier {
    pub fn new(curve: &GainCurve, hop: usize, channels: usize) -> Self {
        Self {
            gains: curve.gain_db.iter().map(|g| 10f32.powf(g / 20.0)).collect(),
            hop: hop as f64,
            channels,
            pos: 0,
        }
    }

    fn gain_at(&self, n: u64) -> f32 {
        if self.gains.is_empty() {
            return 1.0;
        }
        // Frame-Mittelpunkte liegen bei (i + 0.5) * hop
        let x = (n as f64 / self.hop - 0.5).max(0.0);
        let i = x.floor() as usize;
        if i + 1 >= self.gains.len() {
            return *self.gains.last().unwrap();
        }
        let t = (x - i as f64) as f32;
        self.gains[i] * (1.0 - t) + self.gains[i + 1] * t
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        for frame in buf.chunks_exact_mut(self.channels) {
            let g = self.gain_at(self.pos);
            for s in frame {
                *s *= g;
            }
            self.pos += 1;
        }
    }
}

/// Lookahead-Peak-Limiter mit gekoppelten Kanälen.
///
/// Garantiert |y| <= ceiling: Die Verstärkung wird aus dem Minimum über 2·D Samples
/// gebildet und über D Samples gemittelt; das Signal wird um D Samples verzögert.
pub struct Limiter {
    channels: usize,
    ceiling: f32,
    la: usize,
    release: f32,
    // Gleitendes Minimum (monotone Deque über (Index, Wert))
    minq: std::collections::VecDeque<(u64, f32)>,
    env: f32,
    // Gleitender Mittelwert der Hüllkurve
    avg_buf: std::collections::VecDeque<f32>,
    avg_sum: f64,
    delay: std::collections::VecDeque<f32>,
    idx: u64,
}

impl Limiter {
    pub fn new(sample_rate: u32, channels: usize, ceiling_db: f32) -> Self {
        let la = (sample_rate as f32 * 0.005) as usize; // 5 ms Lookahead
        let release_s = 0.15;
        Self {
            channels,
            ceiling: 10f32.powf(ceiling_db / 20.0),
            la: la.max(1),
            release: 1.0 - (-1.0 / (release_s * sample_rate as f32)).exp(),
            minq: Default::default(),
            env: 1.0,
            avg_buf: Default::default(),
            avg_sum: 0.0,
            delay: Default::default(),
            idx: 0,
        }
    }

    /// Latenz in Samples (pro Kanal). Die Ausgabe ist bereits ausgerichtet: Ausgabe-Sample k
    /// gehört zu Eingabe-Sample k, es fehlen nur die letzten `latency()` Samples bis zum Leeren.
    pub fn latency(&self) -> usize {
        self.la
    }

    /// Verarbeitet interleaved Samples; die Ausgabe hinkt um `latency()` Samples hinterher.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.channels;
        for frame in input.chunks_exact(ch) {
            let peak = frame.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            let r = if peak > self.ceiling { self.ceiling / peak } else { 1.0 };

            while let Some(&(_, v)) = self.minq.back() {
                if v >= r {
                    self.minq.pop_back();
                } else {
                    break;
                }
            }
            self.minq.push_back((self.idx, r));
            let win = 2 * self.la as u64;
            while let Some(&(i, _)) = self.minq.front() {
                if i + win < self.idx {
                    self.minq.pop_front();
                } else {
                    break;
                }
            }
            let m = self.minq.front().unwrap().1;
            if m < self.env {
                self.env = m;
            } else {
                self.env += (m - self.env) * self.release;
            }

            self.avg_buf.push_back(self.env);
            self.avg_sum += self.env as f64;
            if self.avg_buf.len() > self.la + 1 {
                self.avg_sum -= self.avg_buf.pop_front().unwrap() as f64;
            }
            let g = (self.avg_sum / self.avg_buf.len() as f64) as f32;

            self.delay.extend(frame.iter().copied());
            if self.delay.len() > self.la * ch {
                for _ in 0..ch {
                    let s = self.delay.pop_front().unwrap() * g;
                    out.push(s.clamp(-self.ceiling, self.ceiling));
                }
            }
            self.idx += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_respects_ceiling_and_latency() {
        let mut lim = Limiter::new(44100, 2, -1.0);
        let ceil = 10f32.powf(-1.0 / 20.0);
        let input: Vec<f32> = (0..100_000)
            .flat_map(|i| {
                let v = ((i as f32) * 0.05).sin() * if i < 300 { 3.0 } else { 0.3 };
                [v, v]
            })
            .collect();
        let mut out = Vec::new();
        lim.process(&input, &mut out);
        assert_eq!(out.len(), input.len() - lim.latency() * 2);
        assert!(out.iter().all(|s| s.abs() <= ceil + 1e-6));
        // Leise Passagen bleiben (nach dem Release) unverändert
        let i = 90_000 * 2;
        assert!((out[i] - input[i]).abs() < 1e-3);
    }

    #[test]
    fn quiet_passage_gets_boosted() {
        // 60 s laut (-20 dB), 30 s leise (-40 dB), 60 s laut
        let mut frame_db = vec![-20.0; 1200];
        frame_db.extend(vec![-40.0; 600]);
        frame_db.extend(vec![-20.0; 1200]);
        let an = Analysis {
            sample_rate: 44100,
            channels: 2,
            hop: 2205,
            total_samples: 3000 * 2205,
            frame_db,
            peak_db: 0.0,
        };
        let p = Params { target_db: -20.0, ..Params::default() };
        let g = compute_gain(&an, &p);
        assert!(g.gain_db[600].abs() < 0.5);
        assert!((g.gain_db[1500] - 20.0).abs() < 0.5);
    }
}
