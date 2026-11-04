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
    /// Seconds of silence that end the recording, if any do.
    stop_after: Option<f32>,
    manual: bool,
    heard: bool,
    seg_heard: bool,
    silence: usize,
    since_partial: usize,
    strip_wake: bool,
    committed_any: bool,
}

impl Session {
    fn new(stop_after: Option<f32>, manual: bool) -> Self {
        Session { audio: Vec::new(), stop_after, manual, heard: false, seg_heard: false, silence: 0, since_partial: 0, strip_wake: false, committed_any: false }
    }
}

enum Job {
    Clip { samples: Vec<f32>, wake: bool },
    /// The recording is over: follow up what it typed.
    End,
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
        let _ = jobs.send(Job::Clip { samples, wake: std::mem::take(&mut s.strip_wake) });
        s.committed_any = true;
    };
    let finish = |s: Option<Session>| {
        if let Some(mut s) = s {
            // A manual recording the detector never "heard" is still sent once, in case the threshold is too high.
            if s.seg_heard || (s.manual && !s.committed_any) {
                commit(&mut s);
            }
            state.recording.store(false, Ordering::Relaxed);
            if s.committed_any {
                state.pending.fetch_add(1, Ordering::Relaxed);
                let _ = jobs.send(Job::End);
            } else {
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
        let start = |stop_after, manual| {
            state.recording.store(true, Ordering::Relaxed);
            state.say("Listening…");
            notify();
            Session::new(stop_after, manual)
        };
        match cmd {
            Some(Cmd::Hold) if session.is_none() => session = Some(start(None, true)),
            Some(Cmd::Release) if session.as_ref().is_some_and(|s| s.manual) => finish(session.take()),
            Some(Cmd::Toggle) if session.is_some() => finish(session.take()),
            Some(Cmd::Toggle) => session = Some(start(c.auto_stop.then_some(c.auto_stop_secs), true)),
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
                } else if c.live_preview && c.overlay && s.seg_heard && s.since_partial >= (c.preview_secs * SR as f32) as usize {
                    s.since_partial = 0;
                    *partial.lock().unwrap() = Some(s.audio.clone());
                }
                if s.stop_after.is_some_and(|secs| (s.heard && s.silence >= (secs * SR as f32) as usize) || (!s.heard && s.silence >= SR * 10)) {
                    finish(session.take());
                }
            } else if c.wake_word {
                if let Some(audio) = wake.feed(frame, speech, |a| heard_wake(a, &c)) {
                    if c.wake_chime {
                        output::chime();
                    }
                    let mut s = start(Some(c.wake_stop_secs), false);
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
    text.is_ok_and(|t| after_wake(&t, &c.wake_phrase, c.wake_match).is_some())
}

/// How much of the wake phrase has to be heard, and where.
#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WakeMatch {
    /// Just the last word ("jarvis" in "hey jarvis"), which whisper spells
    /// reliably; "hey" often comes out as "Hey," "A" or nothing.
    LastWord,
    Phrase,
    /// The whole phrase, as the first thing said.
    Start,
}

impl WakeMatch {
    pub const ALL: [(WakeMatch, &str); 3] = [
        (WakeMatch::LastWord, "Forgiving: just its last word, anywhere in what I say"),
        (WakeMatch::Phrase, "Whole phrase, anywhere in what I say"),
        (WakeMatch::Start, "Strict: whole phrase, as the first thing I say"),
    ];
}

/// Byte index just past the wake phrase. Whole words only, ignoring case and punctuation.
fn after_wake(text: &str, phrase: &str, mode: WakeMatch) -> Option<usize> {
    let want: Vec<String> = phrase.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect();
    let want = if mode == WakeMatch::LastWord { &want[want.len().checked_sub(1)?..] } else { &want[..] };
    if want.is_empty() {
        return None;
    }
    let (mut words, mut start) = (Vec::new(), None);
    for (i, ch) in text.char_indices().chain([(text.len(), ' ')]) {
        match (ch.is_alphanumeric(), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                words.push((text[s..i].to_lowercase(), i));
                start = None;
            }
            _ => {}
        }
    }
    let hit = words.windows(want.len()).position(|w| w.iter().zip(want).all(|((a, _), b)| a == b))?;
    (mode != WakeMatch::Start || hit == 0).then(|| words[hit + want.len() - 1].1)
}

fn strip_wake(text: &str, phrase: &str, mode: WakeMatch) -> String {
    let Some(i) = after_wake(text, phrase, mode) else { return text.to_string() };
    let rest = text[i..].trim_start_matches(|c: char| !c.is_alphanumeric());
    let mut chars = rest.chars();
    chars.next().map(|f| f.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// Removes the sentence-ending period if asked to; other punctuation stays.
fn tidy(text: String, c: &Config) -> String {
    if c.drop_period { text.trim_end_matches(['.', '。']).to_string() } else { text }
}

/// The separator is typed in front of the next phrase rather than after each
/// one, so that a key pressed at the end doesn't send a trailing space with it.
fn with_separator(text: String, c: &Config, typed: bool) -> String {
    if typed { format!("{}{text}", c.separator()) } else { text }
}

/// After a recording's last phrase: the chosen key, or else the separator so the next dictation doesn't run into it.
fn end_output(c: &Config) -> Result<(), String> {
    if c.end_key != output::EndKey::None {
        std::thread::sleep(Duration::from_secs_f32(c.end_key_delay_secs));
        output::press(c.end_key)
    } else if c.separator().is_empty() {
        Ok(())
    } else {
        output::emit(c.separator(), c.output)
    }
}

fn spawn_worker(cfg: Arc<Mutex<Config>>, state: Arc<State>, notify: Notify) -> (Sender<Job>, Partial) {
    let (tx, rx) = channel::<Job>();
    let partial: Partial = Arc::default();
    let slot = partial.clone();
    // Whether the current recording has typed anything yet.
    let mut typed = false;
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
                        *state.preview.lock().unwrap() = if c.wake_word { strip_wake(&t, &c.wake_phrase, c.wake_match) } else { t };
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
        let Job::Clip { samples, wake } = job else {
            // Nothing typed (all silence, say) means nothing to send with Enter either.
            if std::mem::take(&mut typed) {
                if let Err(e) = end_output(&c) {
                    state.say(format!("Error: {e}"));
                }
            }
            state.pending.fetch_sub(1, Ordering::Relaxed);
            notify();
            continue;
        };
        state.say("Transcribing…");
        notify();
        let t0 = std::time::Instant::now();
        let text = transcribe(&c, &samples, wake)
            .map(|t| if wake { strip_wake(&t, &c.wake_phrase, c.wake_match) } else { t })
            .map(|t| tidy(t, &c));
        let msg = match text {
            Ok(t) if t.is_empty() => "Heard nothing".to_string(),
            Ok(t) => {
                let t = with_separator(t, &c, typed);
                match output::emit(&t, c.output) {
                    Ok(()) => {
                        typed = true;
                        format!("Typed {} words ({:.2} s)", t.split_whitespace().count(), t0.elapsed().as_secs_f32())
                    }
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
        use WakeMatch::*;
        assert_eq!(strip_wake("Hey Jarvis, write this down.", "hey jarvis", LastWord), "Write this down.");
        assert_eq!(strip_wake("A Jarvis. ok", "hey jarvis", LastWord), "Ok");
        assert_eq!(strip_wake("Hey, Jarvis!", "hey jarvis", LastWord), "");
        assert_eq!(strip_wake("no wake here", "hey jarvis", LastWord), "no wake here");
        assert!(after_wake("Hey Travis", "hey jarvis", LastWord).is_none());
    }

    #[test]
    fn wake_match_modes_trade_forgiveness_for_false_starts() {
        use WakeMatch::*;
        let hit = |text, mode| after_wake(text, "hey jarvis", mode).is_some();
        assert!(hit("A Jarvis, go", LastWord) && !hit("A Jarvis, go", Phrase));
        assert!(hit("so, hey, Jarvis!", Phrase) && !hit("so, hey, Jarvis!", Start));
        assert!(hit("Hey, Jarvis! Go", Start));
        assert_eq!(strip_wake("Hey, Jarvis! go on", "hey jarvis", Start), "Go on");
        // Whole words only, and a phrase of no words never matches.
        assert!(!hit("jarvises everywhere", LastWord));
        assert!(after_wake("anything", " ", LastWord).is_none());
        assert!(after_wake("anything", "!", Phrase).is_none());
    }

    #[test]
    fn separator_goes_between_phrases_and_never_before_the_first() {
        let c = Config::default();
        assert_eq!(with_separator("One.".into(), &c, false), "One.");
        assert_eq!(with_separator("Two.".into(), &c, true), " Two.");
        let c = Config { after_phrase: output::After::Custom, custom_after: " / ".into(), drop_period: true, ..c };
        assert_eq!(with_separator("Two".into(), &c, true), " / Two");
        assert_eq!(tidy("Done...".into(), &c), "Done");
        assert_eq!(tidy("Done?".into(), &c), "Done?");
    }

    /// The real path from mic to output, on the sample clip, with the installed tiny.en.
    /// `cargo test -- --ignored` (needs samples/jfk.wav, the model and the engine).
    #[test]
    #[ignore]
    fn session_types_phrases_then_presses_the_end_key() {
        let out = std::env::temp_dir().join("voicetype-e2e.txt");
        let _ = std::fs::remove_file(&out);
        unsafe {
            std::env::set_var("VOICETYPE_FAKE_MIC", "samples/jfk.wav");
            std::env::set_var("VOICETYPE_FAKE_OUTPUT", &out);
        }
        let cfg = Config {
            model: "tiny.en".into(),
            split_on_pause: true,
            end_key: output::EndKey::Enter,
            after_phrase: output::After::Custom,
            custom_after: "|".into(),
            drop_period: true,
            auto_stop_secs: 1.5,
            ..Default::default()
        };
        let state = Arc::new(State::default());
        let tx = spawn(Arc::new(Mutex::new(cfg)), state.clone(), Arc::new(|| {}));
        tx.send(Cmd::Toggle).unwrap();
        std::thread::sleep(Duration::from_secs(1));
        for _ in 0..600 {
            if !state.busy() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        engine::shutdown();
        let typed = std::fs::read_to_string(&out).unwrap();
        println!("{typed}");
        assert!(typed.ends_with("<Enter>"), "{typed}");
        assert!(typed.contains("|"), "real-time phrases are joined by the separator: {typed}");
        assert!(typed.to_lowercase().contains("country"), "{typed}");
        assert!(!typed.contains(".<Enter>") && !typed.contains(" <Enter>"), "{typed}");
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
