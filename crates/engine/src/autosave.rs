//! Crash-recovery copies of open sessions, one folder each, written in the background, and the
//! recovery of the ones left behind with unsaved work.

use crate::io::{open_session, sanitize_name};
use crate::{Engine, EngineError, Result};
use soundcraft_audio_io::{BitDepth, EncodeOptions, FileFormat};
use soundcraft_model::{Session, SourceAudio, SourceId};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant, SystemTime};

const SESSION: &str = "session.scraft";
const ORIGIN: &str = "origin";
const LOCK: &str = "lock";
/// While changes keep coming, the newest is written at most this often.
const INTERVAL: Duration = Duration::from_secs(1);
const STOPPED: &str = "the autosave thread stopped";

/// Writes autosaves on a thread of its own, so the UI never waits on the disk.
pub struct Autosaver {
    jobs: Option<Sender<Job>>,
    thread: Option<std::thread::JoinHandle<()>>,
    failure: Arc<Mutex<Option<String>>>,
}

enum Job {
    Write(Arc<Session>, Option<String>),
    /// Done with the session's folder: keep it for the next launch, or delete it.
    Close(bool),
    Flush(Sender<std::result::Result<(), String>>),
}

impl Autosaver {
    /// A writer for autosave folders under `root`.
    pub fn new(root: PathBuf) -> Autosaver {
        let (tx, rx) = channel();
        let failure = Arc::new(Mutex::new(None));
        let writer = Writer { root, failure: Arc::clone(&failure), folder: None, pending: None, last: None, written: Ok(()) };
        let thread = std::thread::Builder::new().name("soundcraft-autosave".into()).spawn(move || writer.run(&rx));
        if let Err(e) = &thread {
            report(&failure, format!("{STOPPED}: {e}"));
        }
        Autosaver { jobs: Some(tx), thread: thread.ok(), failure }
    }

    /// Autosave `session`, opened from the session file `origin`, soon.
    pub fn write(&self, session: Arc<Session>, origin: Option<String>) {
        self.send(Job::Write(session, origin));
    }

    /// Done with the open session's folder: keep it for the next launch, or delete it.
    pub fn close(&self, keep: bool) {
        self.send(Job::Close(keep));
    }

    /// Wait until everything handed over so far is done. A failure returned here isn't reported
    /// again by `failure`.
    pub fn flush(&self) -> Result<()> {
        let (tx, rx) = channel();
        self.send(Job::Flush(tx));
        let done = rx.recv().unwrap_or_else(|_| Err(STOPPED.into()));
        if done.is_err() {
            self.failure();
        }
        done.map_err(EngineError::Io)
    }

    /// The newest failure since the last call.
    pub fn failure(&self) -> Option<String> {
        self.failure.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    fn send(&self, job: Job) {
        if self.jobs.as_ref().is_none_or(|tx| tx.send(job).is_err()) {
            report(&self.failure, STOPPED.into());
        }
    }
}

impl Drop for Autosaver {
    /// What was handed over is written, and an open folder stays for the next launch.
    fn drop(&mut self) {
        drop(self.jobs.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The writer thread.
struct Writer {
    root: PathBuf,
    failure: Arc<Mutex<Option<String>>>,
    /// The open session's folder.
    folder: Option<Autosave>,
    /// The newest session not written yet.
    pending: Option<(Arc<Session>, Option<String>)>,
    last: Option<Instant>,
    /// How the newest write into the open folder went, for `Flush`.
    written: std::result::Result<(), String>,
}

impl Writer {
    fn run(mut self, jobs: &Receiver<Job>) {
        loop {
            let wait = match (&self.pending, self.last) {
                (None, _) => Duration::MAX,
                (Some(_), last) => last.map_or(Duration::ZERO, |t| INTERVAL.saturating_sub(t.elapsed())),
            };
            match jobs.recv_timeout(wait) {
                Ok(Job::Write(session, origin)) => self.pending = Some((session, origin)),
                Ok(Job::Flush(reply)) => {
                    self.write_pending();
                    let _ = reply.send(self.written.clone());
                }
                Ok(Job::Close(keep)) => {
                    if keep {
                        self.write_pending();
                    }
                    self.pending = None;
                    self.written = Ok(());
                    if let Some(a) = self.folder.take()
                        && !keep
                    {
                        a.discard();
                    }
                }
                Err(RecvTimeoutError::Timeout) => self.write_pending(),
                Err(RecvTimeoutError::Disconnected) => {
                    self.write_pending();
                    return;
                }
            }
        }
    }

    fn write_pending(&mut self) {
        let Some((session, origin)) = self.pending.take() else { return };
        self.last = Some(Instant::now());
        let written = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
            if self.folder.is_none() {
                self.folder = Some(Autosave::create(&self.root, &session.name)?);
            }
            self.folder.as_mut().map_or(Ok(()), |a| a.write(&session, origin.as_deref()))
        }));
        self.written = match written {
            Ok(written) => written.map_err(|e| e.to_string()),
            Err(_) => Err("internal error (see the log)".into()),
        };
        if let Err(e) = &self.written {
            report(&self.failure, e.clone());
        }
    }
}

fn report(failure: &Mutex<Option<String>>, e: String) {
    log::warn!("autosave failed: {e}");
    *failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(e);
}

/// An open session's autosave folder, locked while the session is open so a later launch can tell
/// it from one a crash left behind.
struct Autosave {
    dir: PathBuf,
    lock: Option<std::fs::File>,
    copies: BTreeMap<SourceId, (Weak<SourceAudio>, String)>,
    next: u64,
}

impl Autosave {
    /// A new folder under `root` for the session called `name`.
    fn create(root: &Path, name: &str) -> Result<Autosave> {
        let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis();
        let dir = root.join(format!("{} {stamp}-{}", sanitize_name(name), std::process::id()));
        let lock = std::fs::create_dir_all(&dir).and_then(|()| std::fs::File::create(dir.join(LOCK))).map_err(|err| io_error(&dir, err))?;
        lock.try_lock().map_err(|err| EngineError::Io(format!("{}: {err}", dir.display())))?;
        Ok(Autosave { dir, lock: Some(lock), copies: BTreeMap::new(), next: 1 })
    }

    /// Write `session`, opened from the session file `origin`. Audio saved with it is referenced
    /// where it is; audio that exists only in memory is copied, once per recording or render.
    fn write(&mut self, session: &Session, origin: Option<&str>) -> Result<()> {
        let base = origin.and_then(|p| Path::new(p).parent()).and_then(|p| std::path::absolute(p).ok());
        let audio_dir = self.dir.join("Audio Files");
        let mut s = session.clone();
        let mut copies = BTreeMap::new();
        for src in &mut s.sources {
            let file = if Path::new(&src.path).is_absolute() { Some(PathBuf::from(&src.path)) } else { base.as_ref().map(|b| b.join(&src.path)) };
            let audio = s.pool.get(src.id);
            if let Some(f) = &file
                && (!src.unsaved && f.is_file() || audio.is_none())
            {
                src.path = f.to_string_lossy().into_owned();
                continue;
            }
            let Some(audio) = audio else { continue };
            let name = match self.copies.remove(&src.id) {
                Some((copy, name)) if copy.upgrade().is_some_and(|a| Arc::ptr_eq(&a, audio)) => name,
                _ => {
                    let name = format!("{} {}.wav", sanitize_name(&src.name), self.next);
                    self.next += 1;
                    let opts = EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Float32, dither: false, bwf: None };
                    let bytes = soundcraft_audio_io::encode(&audio.buffer, &opts).map_err(|err| EngineError::Io(err.to_string()))?;
                    std::fs::create_dir_all(&audio_dir)
                        .and_then(|()| write_synced(&audio_dir.join(&name), &bytes))
                        .map_err(|err| io_error(&audio_dir, err))?;
                    name
                }
            };
            src.path = audio_dir.join(&name).to_string_lossy().into_owned();
            src.unsaved = true;
            copies.insert(src.id, (Arc::downgrade(audio), name));
        }
        let text = s.to_json().map_err(|err| EngineError::Io(err.to_string()))?;
        write_atomically(&self.dir.join(ORIGIN), origin.unwrap_or_default().as_bytes())?;
        write_atomically(&self.dir.join(SESSION), text.as_bytes())?;
        for (_, name) in std::mem::replace(&mut self.copies, copies).into_values() {
            let _ = std::fs::remove_file(audio_dir.join(name));
        }
        Ok(())
    }

    /// Delete the folder: the session closed with nothing left to recover.
    fn discard(mut self) {
        drop(self.lock.take());
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Unsaved work left behind in an autosave folder.
#[derive(Debug, Clone, PartialEq)]
pub struct Leftover {
    pub dir: PathBuf,
    /// The session's name.
    pub name: String,
    /// The session file it was autosaved from, if it had one.
    pub origin: Option<String>,
    pub saved: SystemTime,
}

/// Leftover autosaves under `root`, newest first: folders no open session holds, written after the
/// session file they came from. Empty and outdated ones are deleted.
pub fn leftovers(root: &Path) -> Vec<Leftover> {
    let mut out = Vec::new();
    for dir in std::fs::read_dir(root).into_iter().flatten().flatten().map(|d| d.path()) {
        let Ok(lock) = std::fs::OpenOptions::new().write(true).open(dir.join(LOCK)) else { continue };
        if lock.try_lock().is_err() {
            continue;
        }
        let origin = std::fs::read_to_string(dir.join(ORIGIN)).ok().filter(|o| !o.is_empty());
        let origin_saved = origin.as_ref().and_then(|o| std::fs::metadata(o).and_then(|m| m.modified()).ok());
        let saved = std::fs::metadata(dir.join(SESSION)).and_then(|m| m.modified());
        drop(lock);
        match saved {
            Ok(saved) if origin_saved.is_none_or(|o| saved > o) => {
                let text = std::fs::read_to_string(dir.join(SESSION)).unwrap_or_default();
                let named = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v.get("name")?.as_str().map(str::to_string));
                let name = named.unwrap_or_else(|| dir.file_name().unwrap_or_default().to_string_lossy().into_owned());
                out.push(Leftover { dir, name, origin, saved });
            }
            _ => {
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }
    out.sort_by_key(|l| std::cmp::Reverse(l.saved));
    out
}

/// Open `left` as an unsaved version of the session it came from, so Save writes it back there.
/// Returns the media that couldn't be loaded.
pub fn recover(e: &mut Engine, left: &Leftover) -> Result<Vec<String>> {
    let mut r = Engine::default();
    let missing = open_session(&mut r, &left.dir.join(SESSION).to_string_lossy())?;
    let mut s = r.session().clone();
    if let Some(home) = left.origin.as_deref().and_then(|o| Path::new(o).parent()) {
        for src in s.sources.iter_mut().filter(|src| !src.unsaved) {
            if let Ok(rel) = Path::new(&src.path).strip_prefix(home) {
                src.path = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
            }
        }
    }
    e.replace_session(Session::default());
    *e.session_mut() = s;
    e.path = left.origin.clone();
    Ok(missing)
}

/// Delete a leftover autosave.
pub fn discard(left: &Leftover) {
    let _ = std::fs::remove_dir_all(&left.dir);
}

fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    write_synced(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path)).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        io_error(path, err)
    })
}

fn io_error(path: &Path, err: std::io::Error) -> EngineError {
    EngineError::Io(format!("{}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use soundcraft_audio_io::AudioBuffer;
    use soundcraft_model::{ChannelFormat, Clip, TrackKind};

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("soundcraft-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn record(e: &mut Engine, level: f32) {
        let before = e.session_arc();
        let s = e.session_mut();
        let t = match s.tracks.first() {
            Some(t) => t.id,
            None => s.add_track(TrackKind::Audio, ChannelFormat::Mono, None),
        };
        let src =
            crate::io::add_source(s, "Audio 1_01", AudioBuffer { sample_rate: 48_000, channels: vec![vec![level; 4_800]] }, None, FileFormat::Wav);
        let id = s.new_clip_id();
        crate::edit::place_clip(s, t, Clip::audio(id, "Audio 1_01", src, 0, 0, 4_800));
        e.push_undo("Record", before);
    }

    fn levels(e: &Engine) -> Vec<f32> {
        let s = e.session();
        s.sources.iter().map(|src| s.pool.get(src.id).map_or(f32::NAN, |a| a.buffer.channels[0][0])).collect()
    }

    #[test]
    fn saved_audio_is_referenced_and_takes_are_copied_once() {
        let dir = folder("autosave-copies");
        let path = dir.join("Song/Song.scraft");
        let mut e = Engine::default();
        record(&mut e, 0.5);
        crate::io::save_session(&mut e, &path.to_string_lossy()).unwrap();
        record(&mut e, -0.25);
        let mut a = Autosave::create(&dir.join("Autosave"), "Song").unwrap();
        a.write(e.session(), e.path.as_deref()).unwrap();
        a.write(e.session(), e.path.as_deref()).unwrap();
        assert_eq!(std::fs::read_dir(a.dir.join("Audio Files")).unwrap().count(), 1);
        drop(a);
        let left = leftovers(&dir.join("Autosave"));
        let mut r = Engine::default();
        assert_eq!(recover(&mut r, &left[0]).unwrap(), Vec::<String>::new());
        let paths: Vec<_> = r.session().sources.iter().map(|s| (s.path.starts_with("Audio Files/"), s.unsaved)).collect();
        assert_eq!(paths, [(true, false), (false, true)]);
        assert_eq!(levels(&r), [0.5, -0.25]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_take_recorded_after_undo_gets_its_own_copy() {
        let dir = folder("autosave-undo");
        let mut e = Engine::default();
        record(&mut e, 0.5);
        let mut a = Autosave::create(&dir, "Untitled").unwrap();
        a.write(e.session(), None).unwrap();
        e.execute("edit.undo", &json!({})).unwrap();
        record(&mut e, -0.25);
        a.write(e.session(), None).unwrap();
        drop(a);
        let mut r = Engine::default();
        recover(&mut r, &leftovers(&dir)[0]).unwrap();
        assert_eq!(levels(&r), [-0.25]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_writer_saves_the_newest_change_and_drops_the_folder_when_done() {
        let root = folder("autosave-writer");
        let w = Autosaver::new(root.clone());
        let mut e = Engine::default();
        for _ in 0..5 {
            record(&mut e, 0.5);
            w.write(e.session_arc(), None);
        }
        w.flush().unwrap();
        let file = std::fs::read_dir(&root).unwrap().flatten().map(|d| d.path().join(SESSION)).collect::<Vec<_>>();
        let mut r = Engine::default();
        open_session(&mut r, &file[0].to_string_lossy()).unwrap();
        assert_eq!((file.len(), r.session().sources.len()), (1, 5));
        w.close(false);
        w.flush().unwrap();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        record(&mut e, 0.5);
        w.write(e.session_arc(), None);
        drop(w);
        let mut r = Engine::default();
        recover(&mut r, &leftovers(&root)[0]).unwrap();
        assert_eq!(r.session().sources.len(), 6);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unsaved_work_is_offered_once_its_session_closes() {
        let dir = folder("autosave-leftovers");
        let path = dir.join("Song.scraft").to_string_lossy().into_owned();
        let root = dir.join("Autosave");
        std::fs::create_dir_all(&dir).unwrap();
        let mut e = Engine::default();
        crate::io::save_session(&mut e, &path).unwrap();
        record(&mut e, 0.5);
        let mut a = Autosave::create(&root, "Song").unwrap();
        a.write(e.session(), e.path.as_deref()).unwrap();
        assert!(leftovers(&root).is_empty());
        drop(a);
        let left = leftovers(&root);
        assert_eq!((left.len(), left[0].name.as_str(), left[0].origin.as_deref()), (1, "Song", Some(path.as_str())));
        let mut r = Engine::default();
        recover(&mut r, &left[0]).unwrap();
        assert!(r.is_dirty());
        r.execute("session.save", &json!({})).unwrap();
        assert!(leftovers(&root).is_empty());
        assert!(!left[0].dir.exists());
        let mut reopened = Engine::default();
        open_session(&mut reopened, &path).unwrap();
        assert_eq!(levels(&reopened), [0.5]);
        Autosave::create(&root, "Song").unwrap().discard();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
