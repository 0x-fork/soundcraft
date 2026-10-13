//! Clips: windows onto audio sources or MIDI sequences, placed on a playlist.

use crate::{ClipId, MAX_NAME_CHARS, SourceId, truncate_chars};
use soundcraft_midi::Sequence;
use soundcraft_time::{MAX_POSITION, Range, Samples};

/// Fade curve shapes (Pro Tools offers standard, equal-power, S-curve and custom presets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
pub enum FadeShape {
    Linear,
    #[default]
    EqualPower,
    SCurve,
    Exponential,
    Logarithmic,
}

impl FadeShape {
    pub const ALL: [FadeShape; 5] = [FadeShape::Linear, FadeShape::EqualPower, FadeShape::SCurve, FadeShape::Exponential, FadeShape::Logarithmic];

    /// Fade-in gain at `t` ∈ [0,1] (fade-outs use `gain(1 - t)`).
    pub fn gain(self, t: f32) -> f32 {
        let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
        match self {
            FadeShape::Linear => t,
            FadeShape::EqualPower => (t * std::f32::consts::FRAC_PI_2).sin(),
            FadeShape::SCurve => 0.5 - 0.5 * (t * std::f32::consts::PI).cos(),
            FadeShape::Exponential => t * t * t,
            FadeShape::Logarithmic => 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FadeShape::Linear => "Linear",
            FadeShape::EqualPower => "Equal Power",
            FadeShape::SCurve => "S-Curve",
            FadeShape::Exponential => "Exponential",
            FadeShape::Logarithmic => "Logarithmic",
        }
    }

    pub fn from_id(s: &str) -> Option<FadeShape> {
        let s = s.to_ascii_lowercase().replace(['-', '_', ' '], "");
        FadeShape::ALL.into_iter().find(|f| f.label().to_ascii_lowercase().replace(['-', ' '], "") == s)
    }
}

/// A fade at a clip boundary, `len` samples long.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Fade {
    pub len: Samples,
    pub shape: FadeShape,
}

/// What a clip plays.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ClipContent {
    /// Audio from `source`, starting `offset` samples into the file.
    Audio { source: SourceId, offset: Samples },
    /// MIDI notes, with ticks relative to the clip start.
    Midi { sequence: Sequence },
    /// Picture from the movie `source` ([`crate::VideoSource`], `Session::videos`), starting
    /// `offset` samples into the movie. Video clips carry no audio.
    Video { source: SourceId, offset: Samples },
}

/// A clip on a playlist.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Clip {
    pub id: ClipId,
    pub name: String,
    /// Timeline start (samples).
    pub start: Samples,
    /// Length (samples, > 0).
    pub length: Samples,
    pub content: ClipContent,
    /// Static clip gain in dB.
    #[serde(default)]
    pub gain_db: f32,
    /// Clip gain envelope breakpoints: (offset from clip start, dB). Empty = flat `gain_db`.
    #[serde(default)]
    pub gain_env: Vec<(Samples, f32)>,
    #[serde(default)]
    pub fade_in: Fade,
    #[serde(default)]
    pub fade_out: Fade,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub edit_locked: bool,
    #[serde(default)]
    pub time_locked: bool,
    #[serde(default)]
    pub rating: u8,
    /// Sync point offset from clip start.
    #[serde(default)]
    pub sync_point: Samples,
    /// Optional colour override (RGB).
    #[serde(default)]
    pub color: Option<[u8; 3]>,
    /// Elastic / Vari-speed ratio (1.0 = none) — applied by rendered processing.
    #[serde(default = "one")]
    pub stretch: f64,
    /// Reversed playback (AudioSuite Reverse renders into a new source; this is for previews).
    #[serde(default)]
    pub group: Option<u64>,
    /// Marks the clips made by `clip.loop` (the origin and its copies share one id), so
    /// `clip.unloop` still recognises the loop after the source's content is edited.
    #[serde(default)]
    pub loop_id: Option<u64>,
}

fn one() -> f64 {
    1.0
}

impl Clip {
    pub fn audio(id: ClipId, name: impl Into<String>, source: SourceId, offset: Samples, start: Samples, length: Samples) -> Clip {
        Clip {
            id,
            name: name.into(),
            start,
            length: length.max(1),
            content: ClipContent::Audio { source, offset },
            gain_db: 0.0,
            gain_env: Vec::new(),
            fade_in: Fade::default(),
            fade_out: Fade::default(),
            muted: false,
            edit_locked: false,
            time_locked: false,
            rating: 0,
            sync_point: 0,
            color: None,
            stretch: 1.0,
            group: None,
            loop_id: None,
        }
    }

    pub fn midi(id: ClipId, name: impl Into<String>, start: Samples, length: Samples, sequence: Sequence) -> Clip {
        let mut c = Clip::audio(id, name, SourceId(0), 0, start, length);
        c.content = ClipContent::Midi { sequence };
        c
    }

    /// A clip showing the movie `source` from `offset` samples into it.
    pub fn video(id: ClipId, name: impl Into<String>, source: SourceId, offset: Samples, start: Samples, length: Samples) -> Clip {
        let mut c = Clip::audio(id, name, SourceId(0), 0, start, length);
        c.content = ClipContent::Video { source, offset };
        c
    }

    pub fn end(&self) -> Samples {
        self.start.saturating_add(self.length)
    }

    pub fn range(&self) -> Range {
        Range { start: self.start, end: self.end() }
    }

    pub fn is_audio(&self) -> bool {
        matches!(self.content, ClipContent::Audio { .. })
    }

    pub fn is_video(&self) -> bool {
        matches!(self.content, ClipContent::Video { .. })
    }

    /// The audio source this clip plays (None for MIDI and video clips).
    pub fn source(&self) -> Option<SourceId> {
        match self.content {
            ClipContent::Audio { source, .. } => Some(source),
            ClipContent::Midi { .. } | ClipContent::Video { .. } => None,
        }
    }

    /// The movie this clip shows (video clips only).
    pub fn video_source(&self) -> Option<SourceId> {
        match self.content {
            ClipContent::Video { source, .. } => Some(source),
            ClipContent::Audio { .. } | ClipContent::Midi { .. } => None,
        }
    }

    /// Offset into the audio file or movie (0 for MIDI).
    pub fn source_offset(&self) -> Samples {
        match self.content {
            ClipContent::Audio { offset, .. } | ClipContent::Video { offset, .. } => offset,
            ClipContent::Midi { .. } => 0,
        }
    }

    /// Gain (linear) at `rel` samples from the clip start: clip gain × envelope × fades.
    pub fn gain_at(&self, rel: Samples) -> f32 {
        let mut db = self.gain_db;
        if !self.gain_env.is_empty() {
            db += env_value(&self.gain_env, rel);
        }
        let mut g = db_to_gain(db);
        if self.fade_in.len > 0 && rel < self.fade_in.len {
            g *= self.fade_in.shape.gain(rel as f32 / self.fade_in.len as f32);
        }
        let from_end = self.length - rel;
        if self.fade_out.len > 0 && from_end <= self.fade_out.len {
            g *= self.fade_out.shape.gain(from_end as f32 / self.fade_out.len as f32);
        }
        g
    }

    /// Move the start edge to `new_start` keeping material anchored (trim start).
    pub fn trim_start_to(&mut self, new_start: Samples) {
        let new_start = new_start.min(self.end() - 1);
        let delta = new_start - self.start;
        if let ClipContent::Audio { offset, .. } | ClipContent::Video { offset, .. } = &mut self.content {
            *offset = offset.saturating_add(delta);
        }
        // MIDI note ticks are re-anchored by the engine, which knows the tempo map.
        self.length -= delta;
        self.start = new_start;
        self.clamp_fades();
        // Keep the gain at the cut when earlier breakpoints are dropped.
        if self.gain_env.iter().any(|&(t, _)| t < delta) && self.gain_env.iter().all(|&(t, _)| t != delta) {
            self.gain_env.insert(0, (delta, env_value(&self.gain_env, delta)));
        }
        self.gain_env.retain(|&(t, _)| t >= delta);
        self.gain_env.iter_mut().for_each(|(t, _)| *t = t.saturating_sub(delta));
    }

    /// Move the end edge to `new_end`.
    pub fn trim_end_to(&mut self, new_end: Samples) {
        let new_end = new_end.max(self.start + 1);
        self.length = new_end - self.start;
        self.clamp_fades();
    }

    /// Keep both fades within the clip.
    pub fn clamp_fades(&mut self) {
        self.fade_in.len = self.fade_in.len.clamp(0, self.length);
        self.fade_out.len = self.fade_out.len.clamp(0, self.length);
    }

    /// Repair a clip read from a file: positions and lengths within ±[`MAX_POSITION`], as commands
    /// keep theirs, fades within the clip, a name that can be drawn, and a usable gain and stretch.
    pub fn sanitize(&mut self) {
        bound(&mut self.start);
        self.length = self.length.clamp(1, MAX_POSITION);
        bound(&mut self.sync_point);
        self.gain_env.iter_mut().for_each(|(at, _)| bound(at));
        match &mut self.content {
            ClipContent::Audio { offset, .. } | ClipContent::Video { offset, .. } => bound(offset),
            ClipContent::Midi { sequence } => bound_sequence(sequence),
        }
        self.clamp_fades();
        truncate_chars(&mut self.name, MAX_NAME_CHARS);
        if !self.gain_db.is_finite() {
            self.gain_db = 0.0;
        }
        if !self.stretch.is_finite() || self.stretch <= 0.0 {
            self.stretch = 1.0;
        }
    }
}

pub(crate) fn bound(at: &mut Samples) {
    *at = (*at).clamp(-MAX_POSITION, MAX_POSITION);
}

pub(crate) fn bound_sequence(s: &mut Sequence) {
    for n in &mut s.notes {
        bound(&mut n.start);
        n.length = n.length.clamp(1, MAX_POSITION);
    }
    s.ctrls.iter_mut().for_each(|c| bound(&mut c.tick));
}

fn env_value(env: &[(Samples, f32)], at: Samples) -> f32 {
    let mut prev: Option<(Samples, f32)> = None;
    for &(t, v) in env {
        if t >= at {
            return match prev {
                Some((pt, pv)) if t > pt => pv + (v - pv) * ((at - pt) as f32 / (t - pt) as f32),
                _ => v,
            };
        }
        prev = Some((t, v));
    }
    prev.map_or(0.0, |(_, v)| v)
}

pub(crate) fn db_to_gain(db: f32) -> f32 {
    if db.is_nan() || db <= -144.0 { 0.0 } else { 10f32.powf(db.min(48.0) / 20.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fade_shapes_hit_endpoints() {
        for s in FadeShape::ALL {
            assert!(s.gain(0.0).abs() < 1e-6, "{s:?}");
            assert!((s.gain(1.0) - 1.0).abs() < 1e-6, "{s:?}");
            assert_eq!(s.gain(f32::NAN), 0.0);
            assert_eq!(FadeShape::from_id(s.label()), Some(s));
        }
    }

    #[test]
    fn gain_with_fades() {
        let mut c = Clip::audio(ClipId(1), "a", SourceId(1), 0, 100, 1000);
        c.fade_in = Fade { len: 100, shape: FadeShape::Linear };
        c.fade_out = Fade { len: 100, shape: FadeShape::Linear };
        assert!(c.gain_at(0).abs() < 1e-6);
        assert!((c.gain_at(50) - 0.5).abs() < 1e-6);
        assert!((c.gain_at(500) - 1.0).abs() < 1e-6);
        assert!((c.gain_at(950) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn gain_envelope_interpolates() {
        let mut c = Clip::audio(ClipId(1), "a", SourceId(1), 0, 0, 1000);
        c.gain_env = vec![(0, 0.0), (1000, -20.0)];
        assert!((c.gain_at(500) - db_to_gain(-10.0)).abs() < 1e-4);
    }

    #[test]
    fn trim_start_keeps_gain_envelope_at_cut() {
        for (env, cut, expected) in [
            (vec![(0, 0.0), (1000, -20.0)], 500, vec![(0, -10.0), (500, -20.0)]),
            (vec![(0, 0.0), (100, -20.0)], 500, vec![(0, -20.0)]),
            (vec![(0, 0.0), (500, -10.0), (1000, -20.0)], 500, vec![(0, -10.0), (500, -20.0)]),
            (vec![], 500, vec![]),
            (vec![(100, -3.0)], 0, vec![(100, -3.0)]),
            (vec![(100, -3.0)], -100, vec![(200, -3.0)]),
        ] {
            let mut clip = Clip::audio(ClipId(1), "a", SourceId(1), 100, 0, 1000);
            clip.gain_env = env;
            let gain_at_cut = clip.gain_at(cut);
            clip.trim_start_to(cut);
            assert_eq!(clip.gain_env, expected);
            if cut > 0 {
                assert!((clip.gain_at(0) - gain_at_cut).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn trims_keep_material_anchored() {
        let mut c = Clip::audio(ClipId(1), "a", SourceId(1), 1000, 100, 1000);
        c.trim_start_to(300);
        assert_eq!((c.start, c.length, c.source_offset()), (300, 800, 1200));
        c.trim_end_to(500);
        assert_eq!(c.length, 200);
        c.trim_end_to(-50);
        assert_eq!(c.length, 1);
        // Regression: trimming the start must not leave a fade-out longer than the clip.
        let mut c = Clip::audio(ClipId(2), "b", SourceId(1), 0, 0, 1000);
        c.fade_out = Fade { len: 800, shape: FadeShape::Linear };
        c.trim_start_to(900);
        assert!(c.fade_out.len <= c.length);
    }
}
