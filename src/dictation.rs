//! The dictation engine: one thread owns the mic and runs voice detection for
//! auto-stop, split-on-pause and the wake word; one worker transcribes and types
//! segments in order, and does live previews when it has nothing else to do.

use crate::{Config, audio, engine, output};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SR: usize = audio::RATE as usize;
/// Audio kept from before speech starts, so soft first syllables aren't cut.
const PRE_ROLL: usize = SR / 2;
/// Whisper's window is 30 s; cut before it even without a pause.
const MAX_SEGMENT: usize = SR * 25;

pub enum Cmd {
    /// Hold-to-talk press. Never auto-stops; the release ends it.
    Hold,
    Release,
    Toggle,
}

#[derive(Default)]
pub struct State {
    pub recording: AtomicBool,
    /// Segments waiting for (or in) transcription.
    pub pending: AtomicUsize,
    /// The UI asks for the mic to stay open to show the level while calibrating.
    pub monitor: AtomicBool,
    level: AtomicU32,
    rms: AtomicU32,
    threshold: AtomicU32,
    pub preview: Mutex<String>,
    pub status: Mutex<String>,
}

fn load(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Ordering::Relaxed))
}

impl State {
    pub fn level(&self) -> f32 {
        load(&self.level)
    }
    /// (current RMS, speech threshold), for the calibration meter.
    pub fn vad(&self) -> (f32, f32) {
        (load(&self.rms), load(&self.threshold))
    }
    pub fn busy(&self) -> bool {
        self.recording.load(Ordering::Relaxed) || self.pending.load(Ordering::Relaxed) > 0
    }
    fn say(&self, s: impl Into<String>) {
        let s = s.into();
        eprintln!("[dictation] {s}");
        *self.status.lock().unwrap() = s;
    }
}

type Notify = Arc<dyn Fn() + Send + Sync>;

pub fn spawn(cfg: Arc<Mutex<Config>>, state: Arc<State>, notify: Notify) -> Sender<Cmd> {
    let (tx, rx) = channel();
    let (jobs, partial) = spawn_worker(cfg.clone(), state.clone(), notify.clone());
    std::thread::spawn(move || mic_loop(rx, cfg, state, notify, jobs, partial));
    tx
}

struct Session {
    audio: Vec<f32>,
    auto_stop: bool,
    manual: bool,
    heard: bool,
    seg_heard: bool,
    silence: usize,
    since_partial: usize,
    strip_wake: bool,
    committed_any: bool,
}

impl Session {
    fn new(auto_stop: bool, manual: bool) -> Self {
        Session { audio: Vec::new(), auto_stop, manual, heard: false, seg_heard: false, silence: 0, since_partial: 0, strip_wake: false, committed_any: false }
    }
}

struct Job {
    samples: Vec<f32>,
    strip_wake: bool,
}

type Partial = Arc<Mutex<Option<Vec<f32>>>>;

fn mic_loop(rx: Receiver<Cmd>, cfg: Arc<Mutex<Config>>, state: Arc<State>, notify: Notify, jobs: Sender<Job>, partial: Partial) {
    let mut cap: Option<(audio::Capture, Option<String>)> = None;
    let mut vad = audio::Vad::new(0.01);
    let mut session: Option<Session> = None;
    let mut pending_frame: Vec<f32> = Vec::new();
    let mut wake = Wake::default();

    let commit = |s: &mut Session| {
        let samples = std::mem::take(&mut s.audio);
        *partial.lock().unwrap() = None;
        s.seg_heard = false;
        s.since_partial = 0;
        if samples.len() < SR * 3 / 10 {
            return;
        }
        state.pending.fetch_add(1, Ordering::Relaxed);
        let _ = jobs.send(Job { samples, strip_wake: std::mem::take(&mut s.strip_wake) });
        s.committed_any = true;
    };
    let finish = |s: Option<Session>| {
        if let Some(mut s) = s {
            // A manual recording the detector never "heard" is still sent once, in case the threshold is too high.
            if s.seg_heard || (s.manual && !s.committed_any) {
                commit(&mut s);
            }
            state.recording.store(false, Ordering::Relaxed);
            if !s.committed_any {
                state.say("Heard nothing");
            }
            notify();
        }
    };

    loop {
        let c = cfg.lock().unwrap().clone();
        let cmd = match rx.recv_timeout(Duration::from_millis(40)) {
            Ok(c) => Some(c),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let start = |auto_stop, manual| {
            state.recording.store(true, Ordering::Relaxed);
            state.say("Listening…");
            notify();
            Session::new(auto_stop, manual)
        };
        match cmd {
            Some(Cmd::Hold) if session.is_none() => session = Some(start(false, true)),
            Some(Cmd::Release) if session.as_ref().is_some_and(|s| s.manual) => finish(session.take()),
            Some(Cmd::Toggle) if session.is_some() => finish(session.take()),
            Some(Cmd::Toggle) => session = Some(start(c.auto_stop, true)),
            _ => {}
        }

        let need_mic = session.is_some() || c.wake_word || state.monitor.load(Ordering::Relaxed);
        if !need_mic || cap.as_ref().is_some_and(|(_, d)| *d != c.device) {
            cap = None;
            wake = Wake::default();
        }
        if need_mic && cap.is_none() {
            match audio::start_capture(c.device.as_deref()) {
                Ok(k) => cap = Some((k, c.device.clone())),
                Err(e) => {
                    state.say(format!("Mic error: {e}"));
                    if session.take().is_some() {
                        state.recording.store(false, Ordering::Relaxed);
                    }
                    notify();
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
            }
        }
        let Some((k, _)) = &cap else { continue };
        state.level.store(k.level().to_bits(), Ordering::Relaxed);
        vad.min_threshold = c.silence_threshold;
        pending_frame.extend(k.take_new());

        let frames = pending_frame.len() / audio::FRAME;
        for i in 0..frames {
            let frame = &pending_frame[i * audio::FRAME..(i + 1) * audio::FRAME];
            let speech = vad.is_speech(frame);
            let n = frame.len();
            if let Some(s) = session.as_mut() {
                s.audio.extend_from_slice(frame);
                if speech {
                    s.heard = true;
                    s.seg_heard = true;
                    s.silence = 0;
                } else {
                    s.silence += n;
                }
                if !s.seg_heard && (!s.manual || s.committed_any) && s.audio.len() > PRE_ROLL {
                    s.audio.drain(..s.audio.len() - PRE_ROLL);
                }
                s.since_partial += n;
                let paused = s.seg_heard && s.silence >= (c.pause_secs * SR as f32) as usize;
                if (c.split_on_pause && paused) || s.audio.len() >= MAX_SEGMENT {
                    commit(s);
                } else if c.live_preview && c.overlay && s.seg_heard && s.since_partial >= SR {
                    s.since_partial = 0;
                    *partial.lock().unwrap() = Some(s.audio.clone());
                }
                let quiet = (c.auto_stop_secs * SR as f32) as usize;
                if s.auto_stop && ((s.heard && s.silence >= quiet) || (!s.heard && s.silence >= SR * 10)) {
                    finish(session.take());
                }
            } else if c.wake_word {
                if let Some(audio) = wake.feed(frame, speech, |a| heard_wake(a, &c)) {
                    let mut s = start(true, false);
                    s.audio = audio;
                    s.heard = true;
                    s.seg_heard = true;
                    s.strip_wake = true;
                    session = Some(s);
                }
            }
        }
        pending_frame.drain(..frames * audio::FRAME);
        let (rms, th) = (vad.rms, vad.threshold());
        state.rms.store(rms.to_bits(), Ordering::Relaxed);
        state.threshold.store(th.to_bits(), Ordering::Relaxed);
    }
}

/// Wake-word listening: each burst of speech is transcribed once (at its
/// pause, or after 2.5 s if it keeps going) and checked for the phrase.
// ponytail: wake detection is whisper on VAD-gated bursts, so it costs a transcription per burst of room speech.
// A dedicated keyword model (openWakeWord) would be cheaper if the mic is always on in a noisy room.
#[derive(Default)]
struct Wake {
    buf: Vec<f32>,
    speech: bool,
    silence: usize,
    checked: bool,
}

impl Wake {
    /// Returns the burst's audio when it contained the wake phrase.
    fn feed(&mut self, frame: &[f32], speech: bool, check: impl FnOnce(&[f32]) -> bool) -> Option<Vec<f32>> {
        self.buf.extend_from_slice(frame);
        if speech {
            self.speech = true;
            self.silence = 0;
        } else {
            self.silence += frame.len();
        }
        if !self.speech {
            if self.buf.len() > SR * 3 / 10 {
                self.buf.drain(..self.buf.len() - SR * 3 / 10);
            }
            return None;
        }
        let ended = self.silence >= SR * 35 / 100;
        if !self.checked && (ended || self.buf.len() >= SR * 5 / 2) {
            self.checked = true;
            if check(&self.buf) {
                let audio = std::mem::take(&mut self.buf);
                *self = Wake::default();
                return Some(audio);
            }
        }
        if ended {
            *self = Wake::default();
        }
        None
    }
}

/// `wake`: the clip may hold the wake phrase, so prompt whisper with it; short
/// names in a short clip are otherwise often heard as other words.
fn transcribe(c: &Config, samples: &[f32], wake: bool) -> Result<String, String> {
    let prompt = if wake { format!("{}.", c.wake_phrase) } else { String::new() };
    engine::ensure_running(&c.model, c.gpu).and_then(|u| engine::transcribe(&u, audio::encode_wav(samples), &c.model, &c.language, &prompt))
}

fn heard_wake(samples: &[f32], c: &Config) -> bool {
    let text = transcribe(c, samples, true);
    eprintln!("[wake] heard {text:?}");
    text.is_ok_and(|t| after_wake(&t, &c.wake_phrase).is_some())
}

/// Byte index just past the wake phrase's last word ("jarvis" in "hey jarvis"),
/// which whisper spells reliably; "hey" often comes out as "Hey," "A" or nothing.
fn after_wake(text: &str, phrase: &str) -> Option<usize> {
    let word = phrase.split_whitespace().last()?.to_ascii_lowercase();
    let lower = text.to_ascii_lowercase();
    lower.find(&word).map(|i| i + word.len())
}

fn strip_wake(text: &str, phrase: &str) -> String {
    let Some(i) = after_wake(text, phrase) else { return text.to_string() };
    let rest = text[i..].trim_start_matches(|c: char| !c.is_alphanumeric());
    let mut chars = rest.chars();
    chars.next().map(|f| f.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

fn spawn_worker(cfg: Arc<Mutex<Config>>, state: Arc<State>, notify: Notify) -> (Sender<Job>, Partial) {
    let (tx, rx) = channel::<Job>();
    let partial: Partial = Arc::default();
    let slot = partial.clone();
    std::thread::spawn(move || loop {
        // Committed segments always go before a preview.
        let job = match rx.try_recv() {
            Ok(j) => Some(j),
            Err(TryRecvError::Disconnected) => return,
            // Taken in its own statement: a guard in the match scrutinee would hold
            // the lock through the transcription and the wait below, starving the mic thread.
            Err(TryRecvError::Empty) => match { let p = slot.lock().unwrap().take(); p } {
                Some(p) => {
                    let c = cfg.lock().unwrap().clone();
                    if let Ok(t) = transcribe(&c, &p, c.wake_word) {
                        *state.preview.lock().unwrap() = if c.wake_word { strip_wake(&t, &c.wake_phrase) } else { t };
                        notify();
                    }
                    continue;
                }
                None => match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(j) => Some(j),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
            },
        };
        let Some(job) = job else { continue };
        let c = cfg.lock().unwrap().clone();
        state.say("Transcribing…");
        notify();
        let t0 = std::time::Instant::now();
        let text = transcribe(&c, &job.samples, job.strip_wake)
            .map(|t| if job.strip_wake { strip_wake(&t, &c.wake_phrase) } else { t });
        let msg = match text {
            Ok(t) if t.is_empty() => "Heard nothing".to_string(),
            Ok(mut t) => {
                if c.trailing_space {
                    t.push(' ');
                }
                match output::emit(&t, c.output) {
                    Ok(()) => format!("Typed {} words ({:.2} s)", t.split_whitespace().count(), t0.elapsed().as_secs_f32()),
                    Err(e) => format!("Error: {e}"),
                }
            }
            Err(e) => format!("Error: {e}"),
        };
        state.preview.lock().unwrap().clear();
        state.pending.fetch_sub(1, Ordering::Relaxed);
        state.say(msg);
        notify();
    });
    (tx, partial)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_phrase_is_found_and_stripped() {
        assert_eq!(strip_wake("Hey Jarvis, write this down.", "hey jarvis"), "Write this down.");
        assert_eq!(strip_wake("A Jarvis. ok", "hey jarvis"), "Ok");
        assert_eq!(strip_wake("Hey, Jarvis!", "hey jarvis"), "");
        assert_eq!(strip_wake("no wake here", "hey jarvis"), "no wake here");
        assert!(after_wake("Hey Travis", "hey jarvis").is_none());
    }

    #[test]
    fn wake_checks_each_burst_once_and_hands_over_its_audio() {
        let (loud, quiet) = (vec![0.3f32; audio::FRAME], vec![0.0f32; audio::FRAME]);
        let mut w = Wake::default();
        let mut checks = 0;
        for _ in 0..20 {
            assert!(w.feed(&quiet, false, |_| unreachable!()).is_none());
        }
        for _ in 0..40 {
            assert!(w.feed(&loud, true, |_| unreachable!()).is_none());
        }
        let mut got = None;
        for _ in 0..30 {
            got = got.or(w.feed(&quiet, false, |a| {
                checks += 1;
                a.len() > SR
            }));
        }
        assert_eq!(checks, 1);
        let audio = got.expect("wake heard");
        assert!(audio.len() >= 40 * audio::FRAME, "the burst and its pre-roll are kept");
    }
}
