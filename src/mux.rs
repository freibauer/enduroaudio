//! Minimaler MP4-Leser/-Schreiber.
//!
//! Alle Spuren außer der ersten Audiospur werden unverändert (Sample für Sample) kopiert,
//! ihre Boxen bleiben bis auf die Chunk-Tabellen (stsc/stco/co64) byte-identisch.
//! Die Audiospur wird durch neu kodiertes AAC ersetzt.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};

pub type Result<T> = std::result::Result<T, String>;

const CONTAINERS: &[&[u8; 4]] = &[b"moov", b"trak", b"mdia", b"minf", b"stbl", b"edts", b"dinf"];

#[derive(Clone, Debug)]
pub struct RawBox {
    pub typ: [u8; 4],
    /// Nutzdaten ohne Header (nur bei Blatt-Boxen).
    pub payload: Vec<u8>,
    pub children: Option<Vec<RawBox>>,
}

impl RawBox {
    pub fn leaf(typ: &[u8; 4], payload: Vec<u8>) -> Self {
        Self { typ: *typ, payload, children: None }
    }

    pub fn container(typ: &[u8; 4], children: Vec<RawBox>) -> Self {
        Self { typ: *typ, payload: vec![], children: Some(children) }
    }

    pub fn child(&self, typ: &[u8; 4]) -> Option<&RawBox> {
        self.children.as_ref()?.iter().find(|b| &b.typ == typ)
    }

    pub fn child_mut(&mut self, typ: &[u8; 4]) -> Option<&mut RawBox> {
        self.children.as_mut()?.iter_mut().find(|b| &b.typ == typ)
    }

    pub fn path(&self, path: &[&[u8; 4]]) -> Option<&RawBox> {
        path.iter().try_fold(self, |b, t| b.child(t))
    }

    pub fn path_mut(&mut self, path: &[&[u8; 4]]) -> Option<&mut RawBox> {
        let mut b = self;
        for t in path {
            b = b.child_mut(t)?;
        }
        Some(b)
    }

    fn content_size(&self) -> u64 {
        match &self.children {
            Some(c) => c.iter().map(|b| b.size()).sum(),
            None => self.payload.len() as u64,
        }
    }

    pub fn size(&self) -> u64 {
        let c = self.content_size();
        if c + 8 > u32::MAX as u64 { c + 16 } else { c + 8 }
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        let size = self.size();
        if size > u32::MAX as u64 {
            out.extend_from_slice(&1u32.to_be_bytes());
            out.extend_from_slice(&self.typ);
            out.extend_from_slice(&size.to_be_bytes());
        } else {
            out.extend_from_slice(&(size as u32).to_be_bytes());
            out.extend_from_slice(&self.typ);
        }
        match &self.children {
            Some(c) => c.iter().for_each(|b| b.write(out)),
            None => out.extend_from_slice(&self.payload),
        }
    }
}

pub fn parse_boxes(data: &[u8]) -> Result<Vec<RawBox>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let mut size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as u64;
        let typ: [u8; 4] = data[pos + 4..pos + 8].try_into().unwrap();
        let mut hdr = 8;
        if size == 1 {
            if pos + 16 > data.len() {
                return Err("Defekte MP4-Box".into());
            }
            size = u64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap());
            hdr = 16;
        } else if size == 0 {
            size = (data.len() - pos) as u64;
        }
        let end = pos as u64 + size;
        if size < hdr as u64 || end > data.len() as u64 {
            return Err(format!("Defekte MP4-Box '{}'", String::from_utf8_lossy(&typ)));
        }
        let body = &data[pos + hdr..end as usize];
        if CONTAINERS.contains(&&typ) {
            out.push(RawBox::container(&typ, parse_boxes(body)?));
        } else {
            out.push(RawBox::leaf(&typ, body.to_vec()));
        }
        pos = end as usize;
    }
    Ok(out)
}

/// Top-Level-Boxen der Datei: (Typ, Offset der Nutzdaten, Größe der Nutzdaten).
fn top_level(f: &mut BufReader<File>) -> Result<Vec<([u8; 4], u64, u64)>> {
    let len = f.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
    let mut pos = 0u64;
    let mut out = Vec::new();
    while pos + 8 <= len {
        f.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
        let mut h = [0u8; 16];
        f.read_exact(&mut h[..8]).map_err(|e| e.to_string())?;
        let mut size = u32::from_be_bytes(h[0..4].try_into().unwrap()) as u64;
        let typ: [u8; 4] = h[4..8].try_into().unwrap();
        let mut hdr = 8;
        if size == 1 {
            f.read_exact(&mut h[8..16]).map_err(|e| e.to_string())?;
            size = u64::from_be_bytes(h[8..16].try_into().unwrap());
            hdr = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < hdr {
            return Err("Keine gültige MP4-Datei".into());
        }
        out.push((typ, pos + hdr, size.min(len - pos) - hdr));
        pos += size;
    }
    Ok(out)
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}
fn be64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}

/// Lage eines Samples in der Quelldatei.
#[derive(Clone, Copy)]
pub struct SampleLoc {
    pub offset: u64,
    pub size: u32,
    pub dts: u64,
}

pub struct Track {
    pub trak: RawBox,
    pub handler: [u8; 4],
    pub timescale: u32,
    pub samples: Vec<SampleLoc>,
}

pub struct SourceFile {
    pub ftyp: Option<RawBox>,
    pub moov: RawBox,
    pub tracks: Vec<Track>,
}

fn parse_track(trak: RawBox) -> Result<Track> {
    let err = || "Unvollständige Spur in der MP4-Datei".to_string();
    let hdlr = trak.path(&[b"mdia", b"hdlr"]).ok_or_else(err)?;
    let handler: [u8; 4] = hdlr.payload.get(8..12).ok_or_else(err)?.try_into().unwrap();
    let mdhd = &trak.path(&[b"mdia", b"mdhd"]).ok_or_else(err)?.payload;
    let timescale = if mdhd[0] == 1 { be32(mdhd, 20) } else { be32(mdhd, 12) };
    let stbl = trak.path(&[b"mdia", b"minf", b"stbl"]).ok_or_else(err)?;

    let stsz = &stbl.child(b"stsz").ok_or_else(err)?.payload;
    let uniform = be32(stsz, 4);
    let count = be32(stsz, 8) as usize;
    let sizes: Vec<u32> =
        (0..count).map(|i| if uniform != 0 { uniform } else { be32(stsz, 12 + 4 * i) }).collect();

    let offsets: Vec<u64> = if let Some(b) = stbl.child(b"stco") {
        (0..be32(&b.payload, 4) as usize).map(|i| be32(&b.payload, 8 + 4 * i) as u64).collect()
    } else if let Some(b) = stbl.child(b"co64") {
        (0..be32(&b.payload, 4) as usize).map(|i| be64(&b.payload, 8 + 8 * i)).collect()
    } else {
        return Err(err());
    };

    let stsc = &stbl.child(b"stsc").ok_or_else(err)?.payload;
    let stsc: Vec<(usize, usize)> = (0..be32(stsc, 4) as usize)
        .map(|i| (be32(stsc, 8 + 12 * i) as usize, be32(stsc, 12 + 12 * i) as usize))
        .collect();

    let stts = &stbl.child(b"stts").ok_or_else(err)?.payload;
    let mut dts_list = Vec::with_capacity(count);
    let mut t = 0u64;
    for i in 0..be32(stts, 4) as usize {
        let (n, d) = (be32(stts, 8 + 8 * i), be32(stts, 12 + 8 * i));
        for _ in 0..n {
            dts_list.push(t);
            t += d as u64;
        }
    }

    let mut samples = Vec::with_capacity(count);
    let mut s = 0usize;
    for (ci, &chunk_off) in offsets.iter().enumerate() {
        let chunk_no = ci + 1;
        let per_chunk = stsc.iter().rev().find(|e| e.0 <= chunk_no).map_or(0, |e| e.1);
        let mut off = chunk_off;
        for _ in 0..per_chunk {
            if s >= count {
                break;
            }
            samples.push(SampleLoc { offset: off, size: sizes[s], dts: *dts_list.get(s).unwrap_or(&t) });
            off += sizes[s] as u64;
            s += 1;
        }
    }
    if samples.len() != count {
        return Err("Inkonsistente Sample-Tabellen in der MP4-Datei".into());
    }
    Ok(Track { trak, handler, timescale, samples })
}

pub fn read_source(path: &str) -> Result<SourceFile> {
    let f = File::open(path).map_err(|e| format!("Datei kann nicht geöffnet werden: {e}"))?;
    let mut f = BufReader::new(f);
    let boxes = top_level(&mut f)?;
    let read = |f: &mut BufReader<File>, off: u64, len: u64| -> Result<Vec<u8>> {
        f.seek(SeekFrom::Start(off)).map_err(|e| e.to_string())?;
        let mut v = vec![0u8; len as usize];
        f.read_exact(&mut v).map_err(|e| e.to_string())?;
        Ok(v)
    };
    let mut ftyp = None;
    let mut moov = None;
    for (typ, off, len) in boxes {
        match &typ {
            b"ftyp" => ftyp = Some(RawBox::leaf(b"ftyp", read(&mut f, off, len)?)),
            b"moov" => moov = Some(RawBox::container(b"moov", parse_boxes(&read(&mut f, off, len)?)?)),
            b"moof" => return Err("Fragmentierte MP4-Dateien werden nicht unterstützt".into()),
            _ => {}
        }
    }
    let moov = moov.ok_or("Keine gültige MP4-Datei (moov fehlt)")?;
    let tracks = moov
        .children
        .as_ref()
        .unwrap()
        .iter()
        .filter(|b| &b.typ == b"trak")
        .map(|b| parse_track(b.clone()))
        .collect::<Result<Vec<_>>>()?;
    Ok(SourceFile { ftyp, moov, tracks })
}

// ---------------------------------------------------------------------------------------
// Schreiben

fn full_box(typ: &[u8; 4], version: u8, body: impl FnOnce(&mut Vec<u8>)) -> RawBox {
    let mut p = vec![version, 0, 0, 0];
    body(&mut p);
    RawBox::leaf(typ, p)
}

struct Chunk {
    track: usize,
    first_sample: usize,
    count: usize,
}

/// Neu geschriebene Chunk-Tabellen einer Spur.
#[derive(Default)]
struct ChunkTable {
    offsets: Vec<u64>,
    counts: Vec<u32>,
}

impl ChunkTable {
    fn boxes(&self) -> [RawBox; 2] {
        let stsc = full_box(b"stsc", 0, |p| {
            let mut entries: Vec<(u32, u32)> = Vec::new();
            for (i, &c) in self.counts.iter().enumerate() {
                if entries.last().map(|e| e.1) != Some(c) {
                    entries.push((i as u32 + 1, c));
                }
            }
            p.extend_from_slice(&(entries.len() as u32).to_be_bytes());
            for (first, n) in entries {
                p.extend_from_slice(&first.to_be_bytes());
                p.extend_from_slice(&n.to_be_bytes());
                p.extend_from_slice(&1u32.to_be_bytes());
            }
        });
        let co64 = full_box(b"co64", 0, |p| {
            p.extend_from_slice(&(self.offsets.len() as u32).to_be_bytes());
            for o in &self.offsets {
                p.extend_from_slice(&o.to_be_bytes());
            }
        });
        [stsc, co64]
    }
}

/// Encoder-Informationen für die neue Audiospur.
pub struct AudioInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub audio_specific_config: Vec<u8>,
    pub avg_bitrate: u32,
    /// Encoder-Vorlauf in Samples (wird per Edit-List übersprungen).
    pub encoder_delay: u32,
    /// Länge des Originalaudios in Samples.
    pub duration_samples: u64,
}

pub struct Muxer {
    src: BufReader<File>,
    /// Aktuelle Leseposition in `src`.
    src_pos: u64,
    out: BufWriter<File>,
    pos: u64,
    mdat_start: u64,
    copy_tracks: Vec<Track>,
    copy_tables: Vec<ChunkTable>,
    pending: std::collections::VecDeque<(f64, Chunk)>,
    audio_template: RawBox,
    audio_table: ChunkTable,
    audio_sizes: Vec<u32>,
    audio_frame_len: u32,
    audio_rate: u32,
    audio_chunk: Vec<u8>,
    audio_chunk_frames: u32,
    moov: RawBox,
    copy_buf: Vec<u8>,
}

const CHUNK_SECONDS: f64 = 1.0;

impl Muxer {
    pub fn new(src_path: &str, src: SourceFile, out_path: &str, audio_frame_len: u32, audio_rate: u32) -> Result<Self> {
        let audio_idx = src
            .tracks
            .iter()
            .position(|t| &t.handler == b"soun")
            .ok_or("Die Datei enthält keine Audiospur")?;
        let mut copy_tracks = Vec::new();
        let mut audio_template = None;
        for (i, t) in src.tracks.into_iter().enumerate() {
            if i == audio_idx {
                audio_template = Some(t.trak);
            } else if &t.handler != b"soun" {
                copy_tracks.push(t);
            }
        }

        // Chunks der kopierten Spuren (je ca. 1 s) in zeitlicher Reihenfolge vorplanen
        let mut pending = Vec::new();
        for (ti, t) in copy_tracks.iter().enumerate() {
            let mut first = 0;
            while first < t.samples.len() {
                let start = t.samples[first].dts;
                let limit = start + (CHUNK_SECONDS * t.timescale as f64) as u64;
                let mut end = first + 1;
                while end < t.samples.len() && t.samples[end].dts < limit {
                    end += 1;
                }
                let time = start as f64 / t.timescale.max(1) as f64;
                pending.push((time, Chunk { track: ti, first_sample: first, count: end - first }));
                first = end;
            }
        }
        pending.sort_by(|a, b| a.0.total_cmp(&b.0));

        let src_file = BufReader::with_capacity(
            256 << 10,
            File::open(src_path).map_err(|e| format!("Quelle kann nicht geöffnet werden: {e}"))?,
        );
        let mut out = BufWriter::with_capacity(
            1 << 20,
            File::create(out_path).map_err(|e| format!("Ausgabedatei kann nicht erstellt werden: {e}"))?,
        );
        let mut head = Vec::new();
        src.ftyp.unwrap_or_else(|| RawBox::leaf(b"ftyp", b"isom\0\0\x02\0isomiso2mp41".to_vec())).write(&mut head);
        let mdat_start = head.len() as u64;
        // mdat mit 64-Bit-Größe, wird am Ende nachgetragen
        head.extend_from_slice(&1u32.to_be_bytes());
        head.extend_from_slice(b"mdat");
        head.extend_from_slice(&0u64.to_be_bytes());
        out.write_all(&head).map_err(|e| e.to_string())?;

        let n = copy_tracks.len();
        Ok(Self {
            src: src_file,
            src_pos: 0,
            out,
            pos: head.len() as u64,
            mdat_start,
            copy_tracks,
            copy_tables: (0..n).map(|_| ChunkTable::default()).collect(),
            pending: pending.into(),
            audio_template: audio_template.unwrap(),
            audio_table: ChunkTable::default(),
            audio_sizes: Vec::new(),
            audio_frame_len,
            audio_rate: audio_rate.max(1),
            audio_chunk: Vec::new(),
            audio_chunk_frames: 0,
            moov: src.moov,
            copy_buf: Vec::new(),
        })
    }

    fn write_copy_chunks_until(&mut self, time: f64) -> Result<()> {
        while self.pending.front().is_some_and(|(t, _)| *t <= time) {
            let (_, c) = self.pending.pop_front().unwrap();
            let t = &self.copy_tracks[c.track];
            let table = &mut self.copy_tables[c.track];
            table.offsets.push(self.pos);
            table.counts.push(c.count as u32);
            for s in &t.samples[c.first_sample..c.first_sample + c.count] {
                self.copy_buf.resize(s.size as usize, 0);
                // seek_relative behält den Lesepuffer, wenn das Ziel darin liegt
                if self.src_pos != s.offset {
                    self.src.seek_relative(s.offset as i64 - self.src_pos as i64).map_err(|e| e.to_string())?;
                }
                self.src.read_exact(&mut self.copy_buf).map_err(|e| format!("Lesefehler: {e}"))?;
                self.src_pos = s.offset + s.size as u64;
                self.out.write_all(&self.copy_buf).map_err(|e| format!("Schreibfehler: {e}"))?;
                self.pos += s.size as u64;
            }
        }
        Ok(())
    }

    fn flush_audio_chunk(&mut self) -> Result<()> {
        if self.audio_chunk_frames == 0 {
            return Ok(());
        }
        let first_frame = self.audio_sizes.len() as u64 - self.audio_chunk_frames as u64;
        // Zeitstempel des Chunks (nur für die Verschachtelung mit den anderen Spuren)
        let time = first_frame as f64 * self.audio_frame_len as f64 / self.audio_rate as f64;
        self.write_copy_chunks_until(time)?;
        self.audio_table.offsets.push(self.pos);
        self.audio_table.counts.push(self.audio_chunk_frames);
        self.out.write_all(&self.audio_chunk).map_err(|e| format!("Schreibfehler: {e}"))?;
        self.pos += self.audio_chunk.len() as u64;
        self.audio_chunk.clear();
        self.audio_chunk_frames = 0;
        Ok(())
    }

    /// Fügt einen kodierten AAC-Frame hinzu.
    pub fn push_audio_frame(&mut self, data: &[u8]) -> Result<()> {
        self.audio_sizes.push(data.len() as u32);
        self.audio_chunk.extend_from_slice(data);
        self.audio_chunk_frames += 1;
        let per_chunk = (CHUNK_SECONDS * self.audio_rate as f64 / self.audio_frame_len as f64).ceil() as u32;
        if self.audio_chunk_frames >= per_chunk {
            self.flush_audio_chunk()?;
        }
        Ok(())
    }

    pub fn audio_frames(&self) -> u64 {
        self.audio_sizes.len() as u64
    }

    pub fn finish(mut self, info: &AudioInfo) -> Result<()> {
        self.flush_audio_chunk()?;
        self.write_copy_chunks_until(f64::INFINITY)?;

        // mdat-Größe nachtragen
        let mdat_size = self.pos - self.mdat_start;
        self.out.flush().map_err(|e| e.to_string())?;
        let mut f = self.out.into_inner().map_err(|e| e.to_string())?;
        f.seek(SeekFrom::Start(self.mdat_start + 8)).map_err(|e| e.to_string())?;
        f.write_all(&mdat_size.to_be_bytes()).map_err(|e| e.to_string())?;
        f.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;

        let movie_ts = self
            .moov
            .child(b"mvhd")
            .map(|b| if b.payload[0] == 1 { be32(&b.payload, 20) } else { be32(&b.payload, 12) })
            .unwrap_or(1000)
            .max(1);

        let mut traks = Vec::new();
        for (t, table) in self.copy_tracks.iter().zip(&self.copy_tables) {
            let mut trak = t.trak.clone();
            let stbl = trak.path_mut(&[b"mdia", b"minf", b"stbl"]).unwrap();
            let children = stbl.children.as_mut().unwrap();
            children.retain(|b| !matches!(&b.typ, b"stsc" | b"stco" | b"co64"));
            children.extend(table.boxes());
            traks.push(trak);
        }
        traks.push(build_audio_trak(&self.audio_template, &self.audio_table, &self.audio_sizes, self.audio_frame_len, info, movie_ts)?);

        let mut moov_children: Vec<RawBox> = Vec::new();
        let mut traks_written = false;
        for b in self.moov.children.as_ref().unwrap() {
            if &b.typ == b"trak" {
                if !traks_written {
                    moov_children.append(&mut traks);
                    traks_written = true;
                }
            } else {
                moov_children.push(b.clone());
            }
        }
        let mut buf = Vec::new();
        RawBox::container(b"moov", moov_children).write(&mut buf);
        f.write_all(&buf).map_err(|e| format!("Schreibfehler: {e}"))?;
        f.sync_all().map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn esds(info: &AudioInfo, max_frame: u32) -> RawBox {
    let desc = |tag: u8, body: Vec<u8>| -> Vec<u8> {
        let mut v = vec![tag];
        // Länge im 4-Byte-Format (0x80-Fortsetzung), wie üblich
        let l = body.len() as u32;
        v.extend_from_slice(&[0x80 | (l >> 21) as u8 & 0x7f, 0x80 | (l >> 14) as u8 & 0x7f, 0x80 | (l >> 7) as u8 & 0x7f, l as u8 & 0x7f]);
        v.extend(body);
        v
    };
    let dsi = desc(0x05, info.audio_specific_config.clone());
    let mut dc = vec![0x40, 0x15];
    dc.extend_from_slice(&max_frame.to_be_bytes()[1..4]);
    dc.extend_from_slice(&info.avg_bitrate.max(1).to_be_bytes());
    dc.extend_from_slice(&info.avg_bitrate.max(1).to_be_bytes());
    dc.extend(dsi);
    let mut es = vec![0, 0, 0];
    es.extend(desc(0x04, dc));
    es.extend(desc(0x06, vec![0x02]));
    full_box(b"esds", 0, |p| p.extend(desc(0x03, es)))
}

fn build_audio_trak(
    template: &RawBox,
    table: &ChunkTable,
    sizes: &[u32],
    frame_len: u32,
    info: &AudioInfo,
    movie_ts: u32,
) -> Result<RawBox> {
    let media_duration = sizes.len() as u64 * frame_len as u64;
    let presented = info.duration_samples.min(media_duration.saturating_sub(info.encoder_delay as u64));
    let movie_duration = presented * movie_ts as u64 / info.sample_rate as u64;
    let sr = info.sample_rate;
    if sr > 65535 {
        return Err("Abtastraten über 65535 Hz werden nicht unterstützt".into());
    }

    let mut mp4a = vec![0u8; 6];
    mp4a.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    mp4a.extend_from_slice(&[0u8; 8]);
    mp4a.extend_from_slice(&info.channels.to_be_bytes());
    mp4a.extend_from_slice(&16u16.to_be_bytes());
    mp4a.extend_from_slice(&[0u8; 4]);
    mp4a.extend_from_slice(&(sr << 16).to_be_bytes());
    let max_frame = sizes.iter().copied().max().unwrap_or(0);
    esds(info, max_frame).write(&mut mp4a);
    let stsd = full_box(b"stsd", 0, |p| {
        p.extend_from_slice(&1u32.to_be_bytes());
        RawBox::leaf(b"mp4a", mp4a).write(p);
    });
    let stts = full_box(b"stts", 0, |p| {
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
        p.extend_from_slice(&frame_len.to_be_bytes());
    });
    let stsz = full_box(b"stsz", 0, |p| {
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
        for s in sizes {
            p.extend_from_slice(&s.to_be_bytes());
        }
    });
    let [stsc, co64] = table.boxes();
    let stbl = RawBox::container(b"stbl", vec![stsd, stts, stsc, stsz, co64]);

    let elst = full_box(b"elst", 1, |p| {
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&movie_duration.to_be_bytes());
        p.extend_from_slice(&(info.encoder_delay as u64).to_be_bytes());
        p.extend_from_slice(&1u16.to_be_bytes());
        p.extend_from_slice(&0u16.to_be_bytes());
    });

    let mut trak = template.clone();
    let err = || "Unvollständige Audiospur".to_string();
    // tkhd: Dauer anpassen
    {
        let tkhd = trak.child_mut(b"tkhd").ok_or_else(err)?;
        let p = &mut tkhd.payload;
        if p[0] == 1 {
            p[28..36].copy_from_slice(&movie_duration.to_be_bytes());
        } else {
            p[20..24].copy_from_slice(&(movie_duration.min(u32::MAX as u64) as u32).to_be_bytes());
        }
    }
    // mdhd: Zeitbasis = Abtastrate, Dauer = alle kodierten Samples
    {
        let mdhd = trak.path_mut(&[b"mdia", b"mdhd"]).ok_or_else(err)?;
        let lang = mdhd.payload[if mdhd.payload[0] == 1 { 32 } else { 20 }..][..2].to_vec();
        *mdhd = full_box(b"mdhd", 1, |p| {
            p.extend_from_slice(&[0u8; 16]);
            p.extend_from_slice(&sr.to_be_bytes());
            p.extend_from_slice(&media_duration.to_be_bytes());
            p.extend_from_slice(&lang);
            p.extend_from_slice(&[0, 0]);
        });
    }
    *trak.path_mut(&[b"mdia", b"minf", b"stbl"]).ok_or_else(err)? = stbl;
    let children = trak.children.as_mut().unwrap();
    children.retain(|b| &b.typ != b"edts");
    let pos = children.iter().position(|b| &b.typ == b"tkhd").map_or(0, |p| p + 1);
    children.insert(pos, RawBox::container(b"edts", vec![elst]));
    Ok(trak)
}
