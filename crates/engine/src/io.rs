//! Import, export, save/load, rendering helpers (bounce, consolidate, commit) and analysis.

use crate::{Engine, EngineError, Result};
use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, EncodeOptions, FileFormat};
use soundcraft_model::{ChannelFormat, Clip, ClipContent, Session, Source, SourceAudio, SourceId, TrackId, TrackKind};
use soundcraft_time::{Range, Samples};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Largest import we accept (samples per channel × channels), ~4 GiB of f32.
const MAX_IMPORT_SAMPLES: usize = 1 << 30;

/// Register decoded audio as a new source. Resamples to the session rate if needed.
pub fn add_source(s: &mut Session, name: &str, mut buf: AudioBuffer, path: Option<&str>, format: FileFormat) -> SourceId {
    let sr = s.sample_rate.hz();
    if buf.sample_rate != sr && buf.sample_rate > 0 {
        buf.channels = soundcraft_dsp::offline::resample(&buf.channels, buf.sample_rate, sr);
        buf.sample_rate = sr;
    }
    let id = SourceId(s.alloc());
    let stem = Path::new(name).file_stem().and_then(|x| x.to_str()).unwrap_or(name).to_string();
    s.sources.push(Source {
        id,
        name: stem.clone(),
        path: path.map_or_else(|| format!("Audio Files/{stem}.wav"), str::to_string),
        channels: u16::try_from(buf.num_channels()).unwrap_or(u16::MAX),
        frames: buf.frames() as u64,
        sample_rate: sr,
        format,
        time_reference: 0,
        unsaved: true,
    });
    s.pool.insert(id, Arc::new(SourceAudio::new(buf)));
    id
}

/// Decode and place an audio file. With no target track, a new track of matching width is made.
pub fn import_audio_bytes(e: &mut Engine, name: &str, bytes: &[u8], path: Option<&str>, target: Option<TrackId>, at: Samples) -> Result<Value> {
    let ext = Path::new(name).extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase);
    let (info, buf) = soundcraft_audio_io::decode(bytes, ext.as_deref()).map_err(|err| EngineError::Io(format!("{name}: {err}")))?;
    if buf.frames().saturating_mul(buf.num_channels().max(1)) > MAX_IMPORT_SAMPLES {
        return Err(EngineError::Io(format!("{name}: file is too large to import")));
    }
    if buf.frames() == 0 {
        return Err(EngineError::Io(format!("{name}: no audio")));
    }
    let channels = buf.num_channels();
    let s = e.session_mut();
    let src = add_source(s, name, buf, path, info.format);
    if let (Some(bwf), Some(sx)) = (info.bwf.as_ref(), s.sources.last_mut()) {
        sx.time_reference = bwf.time_reference;
    }
    let frames = s.source(src).map_or(0, |x| i64::try_from(x.frames).unwrap_or(0));
    let track = match target {
        Some(t) => t,
        None => {
            let stem = Path::new(name).file_stem().and_then(|x| x.to_str()).unwrap_or("Audio").to_string();
            s.add_track(TrackKind::Audio, ChannelFormat::for_channels(channels.min(2)), Some(&stem))
        }
    };
    let cid = s.new_clip_id();
    let stem = s.source(src).map(|x| x.name.clone()).unwrap_or_default();
    crate::edit::place_clip(s, track, Clip::audio(cid, stem, src, 0, at, frames));
    Ok(json!({"track": track, "clip": cid, "source": src, "frames": frames, "channels": channels}))
}

/// Import a movie: a Video track clip spanning the picture (the movie is referenced by `path`,
/// never copied) plus its audio on a new audio track when it has a usable audio stream. Fails
/// only when the file has neither a readable picture nor audio.
pub fn import_video_bytes(e: &mut Engine, path: &str, bytes: Vec<u8>, at: Samples) -> Result<Value> {
    let stem = Path::new(path).file_stem().and_then(|n| n.to_str()).unwrap_or("Video").to_string();
    let ext = Path::new(path).extension().and_then(|x| x.to_str()).unwrap_or("mp4").to_ascii_lowercase();
    let bytes: Arc<[u8]> = bytes.into();
    let movie = soundcraft_video::Movie::open(Arc::clone(&bytes)).map(|m| m.info().clone());
    let audio_name = format!("{stem} audio.{}", if ext == "mov" { "mp4" } else { ext.as_str() });
    let audio = import_audio_bytes(e, &audio_name, &bytes, Some(path), None, at);
    let info = match (movie, &audio) {
        (Ok(info), _) => info,
        (Err(verr), Ok(a)) => {
            e.message(format!("{stem}: imported the audio; no picture ({verr})"));
            return Ok(json!({"audio": a, "video": Value::Null, "video_error": verr.to_string()}));
        }
        (Err(verr), Err(aerr)) => return Err(EngineError::Io(format!("{path}: {verr}; no usable audio stream ({aerr})"))),
    };
    let abs = std::fs::canonicalize(path).map_or_else(|_| path.to_string(), |p| p.to_string_lossy().into_owned());
    let s = e.session_mut();
    let sr = s.sample_rate.as_f64();
    let len = (info.duration * sr).round();
    let len = if len.is_finite() { (len as Samples).clamp(1, sr as Samples * 3600 * 48) } else { 1 };
    let vid = SourceId(s.alloc());
    s.videos.push(soundcraft_model::VideoSource {
        id: vid,
        name: Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("movie").to_string(),
        path: abs,
        width: info.width,
        height: info.height,
        frame_rate: info.frame_rate,
        duration: info.duration,
        codec: info.codec.clone(),
    });
    // Reuse the first Video track when the new clip does not overlap its clips; else add one at the top.
    let range = Range::new(at, at.saturating_add(len));
    let track = match s.tracks.iter().find(|t| t.kind == TrackKind::Video) {
        Some(t) if !t.clips().iter().any(|c| c.range().overlaps(&range)) => t.id,
        _ => {
            let id = s.add_track(TrackKind::Video, ChannelFormat::Mono, Some(&stem));
            if let Some(i) = s.track_index(id) {
                let t = s.tracks.remove(i);
                s.tracks.insert(0, t);
            }
            id
        }
    };
    let cid = s.new_clip_id();
    crate::edit::place_clip(s, track, Clip::video(cid, stem.clone(), vid, 0, at, len));
    if !info.decodable {
        e.message(format!("{stem}: {} pictures cannot be shown yet; the Video track shows the clip only", info.codec_detail));
    }
    if let Err(err) = &audio {
        e.message(format!("{stem}: no usable audio stream ({err})"));
    }
    Ok(json!({
        "video": {"track": track, "clip": cid, "source": vid, "length": len, "info": info},
        "audio": audio.as_ref().ok(),
        "audio_error": audio.as_ref().err().map(ToString::to_string),
    }))
}

/// Resolve each referenced movie: keep it when its path exists, else look for a file of the same
/// name next to the session (moved projects) and relink. Returns the movies still missing.
pub fn relink_videos(s: &mut Session, session_dir: &Path) -> Vec<String> {
    let mut missing = Vec::new();
    for v in &mut s.videos {
        let p = if Path::new(&v.path).is_absolute() { PathBuf::from(&v.path) } else { session_dir.join(&v.path) };
        if p.is_file() {
            continue;
        }
        let name = Path::new(&v.path).file_name().map(PathBuf::from).unwrap_or_default();
        let near =
            [session_dir.join(&name), session_dir.join("Video Files").join(&name)].into_iter().find(|c| !name.as_os_str().is_empty() && c.is_file());
        match near {
            Some(c) => v.path = c.to_string_lossy().into_owned(),
            None => missing.push(format!("{} (video)", p.display())),
        }
    }
    missing
}

/// Import a Standard MIDI File as MIDI tracks (one per SMF track).
pub fn import_midi_bytes(e: &mut Engine, bytes: &[u8], at: Samples, tempo_map: bool) -> Result<Vec<TrackId>> {
    let smf = soundcraft_midi::read_smf(bytes).map_err(|err| EngineError::Io(err.to_string()))?;
    let s = e.session_mut();
    if tempo_map {
        for (tick, bpm) in &smf.tempos {
            let _ = s.tempo.set_tempo(*tick, *bpm);
        }
        for (tick, n, d) in &smf.meters {
            let _ = s.tempo.set_meter(*tick, u32::from(*n), u32::from(*d));
        }
    }
    let base_tick = s.tempo.samples_to_ticks(at, s.sample_rate);
    let mut out = Vec::new();
    for (i, tr) in smf.tracks.iter().enumerate() {
        if tr.sequence.notes.is_empty() {
            continue;
        }
        let name = if tr.name.trim().is_empty() { format!("MIDI {}", i + 1) } else { tr.name.clone() };
        let id = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some(&name));
        let drums = tr.sequence.notes.iter().all(|n| n.channel == 9);
        if let Some(t) = s.track_mut(id) {
            t.instrument = Some(soundcraft_model::Insert::new(if drums { "drum_synth" } else { "subtractive_synth" }));
        }
        let end_tick = base_tick + tr.sequence.end_tick().max(1);
        let len = s.tempo.tick_to_samples(end_tick, s.sample_rate) - at;
        let cid = s.new_clip_id();
        crate::edit::place_clip(s, id, Clip::midi(cid, name, at, len.max(1), tr.sequence.clone()));
        out.push(id);
    }
    Ok(out)
}

/// Export all MIDI clips as a format-1 SMF.
pub fn export_midi(e: &Engine) -> Result<Vec<u8>> {
    let s = e.session();
    let mut smf = soundcraft_midi::Smf {
        format: 1,
        tracks: Vec::new(),
        tempos: s.tempo.tempos().iter().map(|t| (t.tick, t.bpm)).collect(),
        meters: s.tempo.meters().iter().map(|m| (m.tick, u8::try_from(m.numerator).unwrap_or(4), u8::try_from(m.denominator).unwrap_or(4))).collect(),
        markers: s.markers.iter().map(|m| (s.tempo.samples_to_ticks(m.start, s.sample_rate), m.name.clone())).collect(),
        key_sigs: Vec::new(),
    };
    for t in s.tracks.iter().filter(|t| t.kind.is_midi()) {
        let mut seq = soundcraft_midi::Sequence::default();
        for c in t.clips() {
            if let ClipContent::Midi { sequence } = &c.content {
                let base = s.tempo.samples_to_ticks(c.start, s.sample_rate);
                for n in &sequence.notes {
                    let mut n = *n;
                    n.start += base;
                    seq.notes.push(n);
                }
            }
        }
        seq.sort();
        smf.tracks.push(soundcraft_midi::SmfTrack { name: t.name.clone(), sequence: seq });
    }
    soundcraft_midi::write_smf(&smf).map_err(|err| EngineError::Io(err.to_string()))
}

/// Render the main mix. Returns the encoded file and (peak dBFS, integrated LUFS).
pub fn bounce_bytes(e: &Engine, r: Range, opts: &EncodeOptions, normalize: bool) -> Result<(Vec<u8>, (f32, f32, f32))> {
    bounce_bytes_with(e, r, opts, normalize, false)
}

/// Render the main mix in its own format (every main channel, SMPTE/WAV order, with the WAV
/// speaker mask of the main format), or an ITU stereo fold-down when `fold_stereo`.
pub fn bounce_bytes_with(e: &Engine, r: Range, opts: &EncodeOptions, normalize: bool, fold_stereo: bool) -> Result<(Vec<u8>, (f32, f32, f32))> {
    let s = e.session();
    if r.len() > s.sample_rate.samples(4.0 * 3600.0) {
        return Err(EngineError::BadParams("file.bounce_mix".into(), "bounces are limited to 4 hours".into()));
    }
    let (mut ch, fmt) = render_main(s, r, fold_stereo);
    if normalize {
        soundcraft_dsp::offline::normalize(&mut ch, -0.1, false);
    }
    let peak = soundcraft_dsp::offline::peak_db(&ch);
    let mut lm = soundcraft_dsp::meter::LoudnessMeter::new(s.sample_rate.as_f64() as f32, ch.len());
    let frames = ch.first().map_or(0, Vec::len);
    lm.process(&ch, frames);
    let lufs = lm.integrated_lufs();
    let true_peak = lm.true_peak_db();
    let buf = AudioBuffer { sample_rate: s.sample_rate.hz(), channels: ch };
    let bytes = soundcraft_audio_io::encode_with_channel_mask(&buf, opts, fmt.channel_mask()).map_err(|err| EngineError::Io(err.to_string()))?;
    Ok((bytes, (peak, lufs, true_peak)))
}

/// The main mix over `r` and its format (folded to stereo on request).
fn render_main(s: &Session, r: Range, fold_stereo: bool) -> (Vec<Vec<f32>>, ChannelFormat) {
    let ch = soundcraft_mix::render_range(s, r, 1024);
    let fmt = if ch.len() <= 2 { ChannelFormat::Stereo } else { s.main_format() };
    if fold_stereo && ch.len() > 2 { (soundcraft_mix::fold_down_stereo(fmt, &ch), ChannelFormat::Stereo) } else { (ch, fmt) }
}

/// Bounce one file per audio/instrument track (each soloed, so its sends and auxes are included)
/// into `dir`. Returns the written paths.
pub fn bounce_stems(e: &Engine, dir: &str, r: Range, opts: &EncodeOptions) -> Result<Vec<String>> {
    bounce_stems_with(e, dir, r, opts, false)
}

/// [`bounce_stems`] in the main format, or folded down to stereo when `fold_stereo`.
pub fn bounce_stems_with(e: &Engine, dir: &str, r: Range, opts: &EncodeOptions, fold_stereo: bool) -> Result<Vec<String>> {
    let s = e.session();
    std::fs::create_dir_all(dir).map_err(|err| EngineError::Io(format!("{dir}: {err}")))?;
    let ext = match opts.format {
        FileFormat::Aiff => "aif",
        FileFormat::Flac => "flac",
        _ => "wav",
    };
    let mut out = Vec::new();
    let stems: Vec<(TrackId, String)> = s
        .tracks
        .iter()
        .filter(|t| matches!(t.kind, TrackKind::Audio | TrackKind::Instrument | TrackKind::Midi) && !t.inactive && !t.mixer.mute)
        .map(|t| (t.id, t.name.clone()))
        .collect();
    for (id, name) in stems {
        let mut solo = s.clone();
        for t in &mut solo.tracks {
            t.mixer.solo = t.id == id;
        }
        let (ch, fmt) = render_main(&solo, r, fold_stereo);
        let buf = AudioBuffer { sample_rate: s.sample_rate.hz(), channels: ch };
        let bytes = soundcraft_audio_io::encode_with_channel_mask(&buf, opts, fmt.channel_mask()).map_err(|err| EngineError::Io(err.to_string()))?;
        let path = Path::new(dir).join(format!("{} - {}.{ext}", sanitize_name(&s.name), sanitize_name(&name)));
        std::fs::write(&path, bytes).map_err(|err| EngineError::Io(format!("{}: {err}", path.display())))?;
        out.push(path.to_string_lossy().into_owned());
    }
    Ok(out)
}

/// Limit a range used for rendering to the track's material (plus a second), so a huge
/// selection never renders hours of silence.
pub fn bounded_to_track(s: &Session, t: TrackId, r: Range) -> Range {
    let Some(tr) = s.track(t) else { return Range::point(r.start) };
    let end = tr.clips().iter().map(Clip::end).max().unwrap_or(0).saturating_add(s.sample_rate.samples(1.0));
    let start = r.start.max(0).min(end);
    Range::new(start, r.end.min(end).max(start))
}

/// Transient positions (absolute) in a track's clips within `r`.
pub fn transients_in(s: &Session, t: TrackId, r: Range, sens: f32) -> Vec<Samples> {
    let r = if r.is_empty() { Range::new(0, s.content_end()) } else { r };
    let r = bounded_to_track(s, t, r);
    let audio = soundcraft_mix::render_clips(s, t, r);
    soundcraft_dsp::offline::detect_transients(&audio, s.sample_rate.as_f64() as f32, sens).into_iter().map(|i| r.start + i as i64).collect()
}

/// Strip Silence: keep only the non-silent ranges of clips inside `r`.
pub fn strip_silence(s: &mut Session, t: TrackId, r: Range, thr: f32, min_len: Samples, pre: Samples, post: Samples) -> usize {
    let r = bounded_to_track(s, t, r);
    let audio = soundcraft_mix::render_clips(s, t, r);
    let keep = soundcraft_dsp::offline::non_silent_ranges(
        &audio,
        thr,
        usize::try_from(min_len).unwrap_or(0),
        usize::try_from(pre).unwrap_or(0),
        usize::try_from(post).unwrap_or(0),
    );
    let mut cut_points = vec![r.start];
    for (a, b) in &keep {
        cut_points.push(r.start + *a as i64);
        cut_points.push(r.start + *b as i64);
    }
    cut_points.push(r.end);
    for p in &cut_points {
        crate::edit::separate_at(s, t, *p);
    }
    // Remove pieces that fall in the gaps.
    let mut gaps = Vec::new();
    let mut prev = r.start;
    for (a, b) in &keep {
        let a = r.start + *a as i64;
        if a > prev {
            gaps.push(Range::new(prev, a));
        }
        prev = r.start + *b as i64;
    }
    if prev < r.end {
        gaps.push(Range::new(prev, r.end));
    }
    if let Some(pl) = s.track_mut(t).and_then(|x| x.playlist_mut()) {
        pl.clips.retain(|c| !gaps.iter().any(|g| c.start >= g.start && c.end() <= g.end));
    }
    keep.len()
}

/// Consolidate the selection on a track into one new clip backed by a rendered source.
pub fn consolidate(e: &mut Engine, t: TrackId, r: Range) -> Result<bool> {
    let s = e.session();
    if s.track(t).is_none_or(|tr| !tr.kind.has_playlist() || tr.kind.is_midi()) {
        return Ok(false);
    }
    let r = bounded_to_track(s, t, r);
    if r.is_empty() {
        return Ok(false);
    }
    let audio = soundcraft_mix::render_clips(s, t, r);
    let name = format!("{}_consolidated", s.track(t).map_or("Audio", |x| x.name.as_str()));
    let buf = AudioBuffer { sample_rate: s.sample_rate.hz(), channels: audio };
    let s = e.session_mut();
    let src = add_source(s, &name, buf, None, FileFormat::Wav);
    crate::edit::clear_range(s, t, r, false);
    let cid = s.new_clip_id();
    crate::edit::place_clip(s, t, Clip::audio(cid, name, src, 0, r.start, r.len()));
    Ok(true)
}

/// Render clip gain into a new source and reset the clip's gain.
pub fn render_clip_gain(e: &mut Engine, id: soundcraft_model::ClipId) -> Result<bool> {
    let s = e.session();
    let Some((t, c)) = s.find_clip(id).map(|(t, c)| (t, c.clone())) else { return Ok(false) };
    if !c.is_audio() {
        return Ok(false);
    }
    let mut c2 = c.clone();
    c2.fade_in = Default::default();
    c2.fade_out = Default::default();
    let mut tmp = s.clone();
    if let Some(pl) = tmp.track_mut(t).and_then(|x| x.playlist_mut()) {
        pl.clips = vec![c2];
    }
    let audio = soundcraft_mix::render_clips(&tmp, t, c.range());
    let buf = AudioBuffer { sample_rate: s.sample_rate.hz(), channels: audio };
    let s = e.session_mut();
    let src = add_source(s, &format!("{}_gain", c.name), buf, None, FileFormat::Wav);
    if let Some(cl) = s.find_clip_mut(id) {
        cl.content = ClipContent::Audio { source: src, offset: 0 };
        cl.gain_db = 0.0;
        cl.gain_env.clear();
    }
    Ok(true)
}

/// Commit a track's inserts into a new audio track (Track › Commit).
pub fn commit_track(e: &mut Engine, t: TrackId) -> Result<Option<TrackId>> {
    let s = e.session();
    let Some(tr) = s.track(t) else { return Ok(None) };
    if !tr.kind.is_audio_path() || tr.kind == TrackKind::Master {
        return Ok(None);
    }
    let end = tr.clips().iter().map(Clip::end).max().unwrap_or(0);
    if end <= 0 {
        return Ok(None);
    }
    let range = Range::new(0, end + s.sample_rate.samples(2.0));
    let audio = soundcraft_mix::render_track_pre_fader(s, t, range);
    let name = format!("{}.cm", tr.name);
    let fmt = ChannelFormat::for_channels(audio.len().clamp(1, 2));
    let mixer = tr.mixer.clone();
    let buf = AudioBuffer { sample_rate: s.sample_rate.hz(), channels: audio };
    let s = e.session_mut();
    let src = add_source(s, &name, buf, None, FileFormat::Wav);
    let nt = s.add_track(TrackKind::Audio, fmt, Some(&name));
    if let (Some(idx), Some(new_idx)) = (s.track_index(t), s.track_index(nt)) {
        let tr = s.tracks.remove(new_idx);
        s.tracks.insert((idx + 1).min(s.tracks.len()), tr);
    }
    let cid = s.new_clip_id();
    if let Some(ntr) = s.track_mut(nt) {
        ntr.mixer.volume_db = mixer.volume_db;
        ntr.mixer.output = mixer.output.clone();
        ntr.mixer.sends = mixer.sends.clone();
        if let Some(pl) = ntr.playlist_mut() {
            pl.clips.push(Clip::audio(cid, name, src, 0, 0, range.len()));
        }
    }
    if let Some(old) = s.track_mut(t) {
        old.inactive = true;
        old.hidden = true;
    }
    Ok(Some(nt))
}

pub fn export_clips(e: &Engine, ids: &[soundcraft_model::ClipId], dir: &str) -> Result<usize> {
    let s = e.session();
    std::fs::create_dir_all(dir).map_err(|err| EngineError::Io(format!("{dir}: {err}")))?;
    let mut n = 0;
    for id in ids {
        let Some((t, c)) = s.find_clip(*id) else { continue };
        let audio = soundcraft_mix::render_clips(s, t, c.range());
        let buf = AudioBuffer { sample_rate: s.sample_rate.hz(), channels: audio };
        let bytes = soundcraft_audio_io::encode(&buf, &EncodeOptions::default()).map_err(|err| EngineError::Io(err.to_string()))?;
        let stem = sanitize_name(&c.name);
        let mut path = Path::new(dir).join(format!("{stem}.wav"));
        let mut k = 2u32;
        while path.exists() {
            path = Path::new(dir).join(format!("{stem}-{k}.wav"));
            k = k.saturating_add(1);
            if k > 10_000 {
                return Err(EngineError::Io(format!("{dir}: too many files named {stem}")));
            }
        }
        std::fs::write(&path, bytes).map_err(|err| EngineError::Io(format!("{}: {err}", path.display())))?;
        n += 1;
    }
    Ok(n)
}

pub fn sanitize_name(n: &str) -> String {
    let s: String = n.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | ' ' | '.') { c } else { '_' }).collect();
    let s = s.trim().trim_start_matches('.').to_string();
    if s.is_empty() { "untitled".into() } else { s.chars().take(100).collect() }
}

/// Save the session to `path` (a `.scraft` file). Audio that only exists in memory is written
/// as 32-bit float WAV into `Audio Files/` next to it. Returns how many audio files were written.
pub fn save_session(e: &mut Engine, path: &str) -> Result<usize> {
    let (fresh, written, p) = write_session(e.session(), path, false)?;
    // Keep the in-memory document in sync (paths, unsaved flags) without an undo step.
    *e.session_mut() = fresh;
    e.path = Some(p.to_string_lossy().into_owned());
    e.mark_clean();
    Ok(written)
}

/// Write a copy of the session (and any audio it needs) without touching the open document —
/// used for autosave / crash recovery. Audio already written for a source is reused.
pub fn save_copy(e: &Engine, path: &str) -> Result<usize> {
    write_session(e.session(), path, true).map(|(_, n, _)| n)
}

/// Write `session` to `path`; returns the session with updated media paths, the number of audio
/// files written and the final path.
fn write_session(session: &Session, path: &str, overwrite: bool) -> Result<(Session, usize, PathBuf)> {
    let p = session_file(path);
    let dir = p.parent().map(Path::to_path_buf).unwrap_or_default();
    let audio_dir = dir.join("Audio Files");
    let mut s = session.clone();
    let from = s.folder.clone();
    let same_dir = from.as_deref().is_some_and(|f| same_path(f, &dir));
    let mut written = 0;
    for src in &mut s.sources {
        if !same_dir
            && s.pool.get(src.id).is_none()
            && Path::new(&src.path).is_relative()
            && let Some(f) = &from
        {
            src.path = f.join(&src.path).to_string_lossy().into_owned();
        }
        let exists_rel = same_dir && !Path::new(&src.path).is_absolute() && dir.join(&src.path).exists();
        if exists_rel && !src.unsaved {
            continue;
        }
        let Some(audio) = s.pool.get(src.id) else { continue };
        std::fs::create_dir_all(&audio_dir).map_err(|err| EngineError::Io(format!("{}: {err}", audio_dir.display())))?;
        let mut stem = sanitize_name(&src.name);
        if overwrite {
            // One file per source id so autosaves never collide or pile up.
            stem = format!("{stem}_{}", src.id.0);
            if audio_dir.join(format!("{stem}.wav")).exists() {
                src.path = format!("Audio Files/{stem}.wav");
                src.unsaved = false;
                continue;
            }
        }
        let opts = EncodeOptions { format: FileFormat::Wav, bit_depth: soundcraft_audio_io::BitDepth::Float32, dither: false, bwf: None };
        let bytes = soundcraft_audio_io::encode(&audio.buffer, &opts).map_err(|err| EngineError::Io(err.to_string()))?;
        let (name, new) = store_wav(&audio_dir, &stem, &bytes)?;
        src.path = format!("Audio Files/{name}");
        src.unsaved = false;
        src.format = FileFormat::Wav;
        written += usize::from(new);
    }
    if written > 0 {
        sync_dir(&audio_dir);
    }
    s.folder = Some(dir.clone());
    if !overwrite && let Some(stem) = p.file_stem().and_then(|x| x.to_str()) {
        s.name = stem.to_string();
    }
    let text = s.to_json().map_err(|err| EngineError::Io(err.to_string()))?;
    std::fs::create_dir_all(&dir).map_err(|err| EngineError::Io(format!("{}: {err}", dir.display())))?;
    // Atomic-ish write: temp file then rename.
    let tmp = p.with_extension("scraft.tmp");
    if !overwrite && std::fs::read(&p).is_ok_and(|old| old != text.as_bytes()) {
        back_up(&p);
    }
    replace_file(&tmp, &p, text.as_bytes())?;
    Ok((s, written, p))
}

/// Write `bytes` to `tmp`, flush it to disk and rename it over `file`, so `file` holds either the
/// old or the new contents, never a mix.
pub fn replace_file(tmp: &Path, file: &Path, bytes: &[u8]) -> Result<()> {
    if let Err(err) = write_synced(tmp, bytes).and_then(|()| std::fs::rename(tmp, file)) {
        let _ = std::fs::remove_file(tmp);
        return Err(EngineError::Io(format!("{}: {err}", file.display())));
    }
    sync_dir(file.parent().unwrap_or(Path::new("")));
    Ok(())
}

fn sync_dir(dir: &Path) {
    let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
}

fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Copy the session file about to be replaced to `Backups/<name> backup <n>.<ext>`, keeping the
/// newest ten.
fn back_up(file: &Path) {
    let (Some(dir), Some(stem), Some(ext)) = (file.parent(), file.file_stem().and_then(|x| x.to_str()), file.extension().and_then(|x| x.to_str()))
    else {
        return;
    };
    let backups = dir.join("Backups");
    let prefix = format!("{stem} backup ");
    let suffix = format!(".{ext}");
    let numbers: Vec<u32> = std::fs::read_dir(&backups)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|f| f.file_name().to_str()?.strip_prefix(&prefix)?.strip_suffix(&suffix)?.parse().ok())
        .collect();
    let next = numbers.iter().max().map_or(1, |n| n.saturating_add(1));
    let copied = std::fs::create_dir_all(&backups).and_then(|()| std::fs::copy(file, backups.join(format!("{prefix}{next}{suffix}"))));
    if let Err(err) = copied {
        log::warn!("no backup of {}: {err}", file.display());
        return;
    }
    for n in numbers.into_iter().filter(|n| n.saturating_add(10) <= next) {
        let _ = std::fs::remove_file(backups.join(format!("{prefix}{n}{suffix}")));
    }
}

/// The file a session saved to `path` goes to: `path`, with `.scraft` added when it has no
/// extension and isn't an existing file.
pub fn session_file(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.extension().is_none() && !p.is_file() { p.with_extension(soundcraft_model::SESSION_EXTENSION) } else { p }
}

/// True when `command` would write a session over the existing `file`. Save, Save As and Save
/// Copy In may write the open session's own file; other commands never ask.
pub fn replaces(e: &Engine, command: &str, file: &Path) -> bool {
    match command {
        "session.save" | "session.save_as" | "session.save_copy" => {
            file.exists() && !e.path.as_deref().is_some_and(|own| same_path(Path::new(own), file))
        }
        "session.save_template" | "file.export_selected_tracks_as_session" => file.exists(),
        _ => false,
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    let full = |p: &Path| if p.as_os_str().is_empty() { Path::new(".").canonicalize() } else { p.canonicalize() };
    a == b || matches!((full(a), full(b)), (Ok(x), Ok(y)) if x == y)
}

/// Store `bytes` as the first of `<stem>.wav`, `<stem>_01.wav`, … in `dir` that is free or already
/// holds exactly these bytes, so saving never replaces audio; returns the name and whether it was
/// written.
fn store_wav(dir: &Path, stem: &str, bytes: &[u8]) -> Result<(String, bool)> {
    use std::io::Write;
    for k in 0..10_000u32 {
        let name = if k == 0 { format!("{stem}.wav") } else { format!("{stem}_{k:02}.wav") };
        let path = dir.join(&name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                if let Err(err) = f.write_all(bytes).and_then(|()| f.sync_all()) {
                    let _ = std::fs::remove_file(&path);
                    return Err(EngineError::Io(format!("{}: {err}", path.display())));
                }
                return Ok((name, true));
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if std::fs::metadata(&path).is_ok_and(|m| m.len() == bytes.len() as u64) && std::fs::read(&path).is_ok_and(|b| b == bytes) {
                    return Ok((name, false));
                }
            }
            Err(err) => return Err(EngineError::Io(format!("{}: {err}", path.display()))),
        }
    }
    Err(EngineError::Io(format!("{}: no free file name for {stem}", dir.display())))
}

/// Open a `.scraft` session and decode its audio files (missing files are reported, not fatal).
pub fn open_session(e: &mut Engine, path: &str) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path).map_err(|err| EngineError::Io(format!("{path}: {err}")))?;
    let mut s = Session::from_json(&text).map_err(|err| EngineError::Io(err.to_string()))?;
    let dir = Path::new(path).parent().map(Path::to_path_buf).unwrap_or_default();
    let mut missing = Vec::new();
    for src in s.sources.clone() {
        let p = if Path::new(&src.path).is_absolute() { PathBuf::from(&src.path) } else { dir.join(&src.path) };
        match std::fs::read(&p) {
            Ok(bytes) => match soundcraft_audio_io::decode(&bytes, p.extension().and_then(|x| x.to_str())) {
                Ok((_, mut buf)) => {
                    if buf.sample_rate != s.sample_rate.hz() && buf.sample_rate > 0 {
                        buf.channels = soundcraft_dsp::offline::resample(&buf.channels, buf.sample_rate, s.sample_rate.hz());
                        buf.sample_rate = s.sample_rate.hz();
                    }
                    s.pool.insert(src.id, Arc::new(SourceAudio::new(buf)));
                }
                Err(err) => missing.push(format!("{}: {err}", p.display())),
            },
            Err(_) => missing.push(p.display().to_string()),
        }
    }
    missing.extend(relink_videos(&mut s, &dir));
    s.folder = Some(dir);
    e.replace_session(s);
    e.path = Some(path.to_string());
    for m in &missing {
        e.message(format!("missing media: {m}"));
    }
    Ok(missing)
}

/// Turn a recorded take into clips on the record-armed tracks (one undo step: "Record").
/// `channels` is the planar input capture at `rate`; each track takes the input channels its
/// input route names ("In 3" → channel 2), defaulting to the first channels.
pub fn add_recording(e: &mut Engine, start: Samples, channels: Vec<Vec<f32>>, rate: u32) -> Result<Vec<soundcraft_model::ClipId>> {
    let frames = channels.first().map_or(0, Vec::len);
    if frames == 0 {
        return Ok(Vec::new());
    }
    let armed: Vec<(TrackId, usize, usize, String)> = e
        .session()
        .tracks
        .iter()
        .filter(|t| t.mixer.record_arm && t.kind == TrackKind::Audio && !t.inactive)
        .map(|t| {
            let first = match &t.mixer.input {
                soundcraft_model::Route::Hardware(h) => {
                    h.trim_start_matches("In ").split(['-', ' ']).next().and_then(|n| n.parse::<usize>().ok()).map_or(0, |n| n.saturating_sub(1))
                }
                _ => 0,
            };
            (t.id, first, t.channels(), t.name.clone())
        })
        .collect();
    if armed.is_empty() {
        return Err(EngineError::BadParams("record".into(), "no tracks are record-enabled".into()));
    }
    let before = e.session_arc();
    let mut out = Vec::new();
    let take_no = e.session().sources.len() + 1;
    for (tid, first, n, name) in armed {
        let chans: Vec<Vec<f32>> = (0..n).map(|k| channels.get((first + k) % channels.len().max(1)).cloned().unwrap_or_default()).collect();
        let buf = AudioBuffer { sample_rate: rate, channels: chans };
        let clip_name = format!("{name}_{take_no:02}");
        let s = e.session_mut();
        let src = add_source(s, &clip_name, buf, None, FileFormat::Wav);
        let len = s.source(src).map_or(0, |x| i64::try_from(x.frames).unwrap_or(0));
        let cid = s.new_clip_id();
        crate::edit::place_clip(s, tid, Clip::audio(cid, clip_name, src, 0, start, len));
        out.push(cid);
    }
    e.push_undo("Record", before);
    Ok(out)
}

#[cfg(test)]
mod save_tests {
    use super::*;

    #[test]
    fn autosave_copy_leaves_document_untouched_and_reuses_audio() {
        let mut e = crate::demo::demo_engine();
        e.execute("mix.volume", &serde_json::json!({"track": "Kick", "db": -7.0})).unwrap();
        let dir = std::env::temp_dir().join(format!("soundcraft-autosave-{}", std::process::id()));
        let path = dir.join("Midnight Groove.scraft");
        let p = path.to_string_lossy().into_owned();
        let n1 = save_copy(&e, &p).unwrap();
        let n2 = save_copy(&e, &p).unwrap();
        assert!(n1 >= 5);
        assert_eq!(n2, 0, "second autosave must not rewrite audio");
        assert!(e.is_dirty() && e.path.is_none());
        let mut r = Engine::default();
        open_session(&mut r, &p).unwrap();
        assert_eq!(r.session().track_by_name("Kick").unwrap().mixer.volume_db, -7.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn with_take(level: f32) -> Engine {
        let mut e = Engine::default();
        let s = e.session_mut();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        let src = add_source(s, "Audio 1_02", AudioBuffer { sample_rate: 48_000, channels: vec![vec![level; 4_800]] }, None, FileFormat::Wav);
        let id = s.new_clip_id();
        crate::edit::place_clip(s, t, Clip::audio(id, "Audio 1_02", src, 0, 0, 4_800));
        e
    }

    fn reopen(path: &str) -> Engine {
        let mut r = Engine::default();
        assert_eq!(open_session(&mut r, path).unwrap(), Vec::<String>::new());
        r
    }

    #[test]
    fn saving_never_replaces_audio_of_another_session() {
        let dir = std::env::temp_dir().join(format!("soundcraft-no-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = |p: &str| dir.join(p).to_string_lossy().into_owned();
        save_session(&mut with_take(0.5), &path("A/A.scraft")).unwrap();
        save_session(&mut with_take(-0.25), &path("A/A v2.scraft")).unwrap();
        save_session(&mut with_take(0.75), &path("B/B.scraft")).unwrap();
        save_session(&mut reopen(&path("B/B.scraft")), &path("A/B.scraft")).unwrap();
        let levels = ["A/A.scraft", "A/A v2.scraft", "B/B.scraft", "A/B.scraft"].map(|p| {
            let r = reopen(&path(p));
            r.session().pool.get(r.session().sources[0].id).unwrap().buffer.channels[0][0]
        });
        assert_eq!(levels, [0.5, -0.25, 0.75, 0.75]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn consolidating_twice_keeps_both_renders() {
        let dir = std::env::temp_dir().join(format!("soundcraft-consolidate-twice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("Song.scraft").to_string_lossy().into_owned();
        let mut e = crate::demo::demo_engine();
        let kick = e.session().track_by_name("Kick").unwrap().id;
        let end = e.session().content_end();
        let render = |e: &Engine| soundcraft_mix::render_clips(e.session(), kick, Range::new(0, end));
        e.execute("edit.consolidate", &json!({"tracks": [kick.0], "start": 500_000, "end": 560_000})).unwrap();
        save_session(&mut e, &path).unwrap();
        e.execute("edit.consolidate", &json!({"tracks": [kick.0], "start": 1_400_000, "end": 1_500_000})).unwrap();
        save_session(&mut e, &path).unwrap();
        assert!(render(&reopen(&path)) == render(&e));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_after_undo_reuses_the_saved_audio() {
        let dir = std::env::temp_dir().join(format!("soundcraft-undo-resave-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("Song.scraft").to_string_lossy().into_owned();
        let mut e = with_take(0.5);
        e.execute("track.new", &json!({})).unwrap();
        assert_eq!(save_session(&mut e, &path).unwrap(), 1);
        e.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(save_session(&mut e, &path).unwrap(), 0);
        assert_eq!(std::fs::read_dir(dir.join("Audio Files")).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saved_audio_is_trusted_only_in_the_folder_it_was_saved_in() {
        let dir = std::env::temp_dir().join(format!("soundcraft-own-folder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = |p: &str| dir.join(p).to_string_lossy().into_owned();
        let level = |r: &Engine| r.session().pool.get(r.session().sources[0].id).unwrap().buffer.channels[0][0];
        save_session(&mut with_take(-0.25), &path("B/Other.scraft")).unwrap();
        let mut e = with_take(0.5);
        save_session(&mut e, &path("A/A.scraft")).unwrap();
        e.execute("track.new", &json!({})).unwrap();
        save_session(&mut e, &path("B/B.scraft")).unwrap();
        e.execute("edit.undo", &json!({})).unwrap();
        e.execute("session.save", &json!({})).unwrap();
        assert_eq!(level(&reopen(&path("B/B.scraft"))), 0.5);
        e.execute("session.save_copy", &json!({"path": path("A/Copy.scraft")})).unwrap();
        e.execute("session.save", &json!({})).unwrap();
        assert_eq!(level(&reopen(&path("B/B.scraft"))), 0.5);
        std::fs::remove_file(dir.join("A/Audio Files/Audio 1_02.wav")).unwrap();
        let mut missing = Engine::default();
        open_session(&mut missing, &path("A/A.scraft")).unwrap();
        save_session(&mut missing, &path("B/Moved.scraft")).unwrap();
        let mut r = Engine::default();
        assert_eq!(open_session(&mut r, &path("B/Moved.scraft")).unwrap(), vec![dir.join("A/Audio Files/Audio 1_02.wav").display().to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_over_another_session_needs_replace() {
        let dir = std::env::temp_dir().join(format!("soundcraft-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("Song.scraft").to_string_lossy().into_owned();
        let mut first = with_take(0.5);
        first.execute("session.save_as", &json!({"path": path})).unwrap();
        first.execute("session.save", &json!({})).unwrap();
        first.execute("session.save_as", &json!({"path": dir.join("Song")})).unwrap();
        assert!(first.execute("session.save_template", &json!({"path": path})).is_err());
        let mut second = with_take(-0.25);
        let track = second.session().tracks[0].id.0;
        for id in ["session.save", "session.save_as", "session.save_copy", "session.save_template", "file.export_selected_tracks_as_session"] {
            let err = second.execute(id, &json!({"path": path, "tracks": [track]})).unwrap_err();
            assert!(err.to_string().contains("`replace: true`"), "{id}: {err}");
        }
        assert_eq!(std::fs::read_dir(dir.join("Audio Files")).unwrap().count(), 1);
        Engine::default().execute("session.save_as", &json!({"path": dir.join("New/New.scraft")})).unwrap();
        std::fs::copy(&path, dir.join("Song")).unwrap();
        let mut bare = Engine::default();
        open_session(&mut bare, &dir.join("Song").to_string_lossy()).unwrap();
        bare.execute("session.save", &json!({})).unwrap();
        second.execute("session.save_as", &json!({"path": path, "replace": true})).unwrap();
        let r = reopen(&path);
        assert_eq!(r.session().pool.get(r.session().sources[0].id).unwrap().buffer.channels[0][0], -0.25);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_keeps_the_last_ten_versions_as_backups() {
        let dir = std::env::temp_dir().join(format!("soundcraft-backups-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Song.scraft");
        let mut e = Engine::default();
        let mut versions = Vec::new();
        for i in 0..12 {
            e.execute("track.new", &json!({"name": format!("T{i}")})).unwrap();
            save_session(&mut e, &path.to_string_lossy()).unwrap();
            save_session(&mut e, &path.to_string_lossy()).unwrap();
            versions.push(std::fs::read(&path).unwrap());
        }
        let backup = |n: usize| std::fs::read(dir.join(format!("Backups/Song backup {n}.scraft"))).ok();
        assert_eq!(std::fs::read_dir(dir.join("Backups")).unwrap().count(), 10);
        assert_eq!((backup(1), backup(2), backup(11)), (None, Some(versions[1].clone()), Some(versions[10].clone())));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod stem_tests {
    #[test]
    fn stems_one_file_per_track() {
        let mut e = crate::demo::demo_engine();
        let dir = std::env::temp_dir().join(format!("soundcraft-stems-{}", std::process::id()));
        let r = e
            .execute("file.bounce_stems", &serde_json::json!({"dir": dir.to_string_lossy(), "start": {"seconds": 10.0}, "end": {"seconds": 11.0}}))
            .unwrap();
        let n = r["files"].as_array().map_or(0, Vec::len);
        assert_eq!(n, 7, "{r}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
