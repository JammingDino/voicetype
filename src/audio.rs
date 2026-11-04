//! Mic capture via cpal on its own thread, downmixed to mono and resampled to
//! 16 kHz for whisper. Ported from MultiZone's `audio.rs`.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

pub const RATE: u32 = 16_000;

pub fn list_input_devices() -> Vec<String> {
    cpal::default_host().input_devices().map(|ds| ds.filter_map(|d| d.name().ok()).collect()).unwrap_or_default()
}

fn resolve_device(host: &cpal::Host, name: Option<&str>) -> Result<cpal::Device, String> {
    if let Some(name) = name {
        if let Some(found) = host.input_devices().ok().and_then(|mut ds| ds.find(|d| d.name().is_ok_and(|n| n == name))) {
            return Ok(found);
        }
        // The saved device is gone: fall back to the default rather than fail.
    }
    host.default_input_device().ok_or_else(|| "no microphone / input device available".into())
}

/// A live recording. Dropping it stops the stream and discards the audio.
pub struct Capture {
    stop_tx: std::sync::mpsc::Sender<()>,
    join: Option<std::thread::JoinHandle<()>>,
    buffer: Arc<Mutex<Vec<f32>>>,
    source_rate: u32,
    /// Peak of the latest callback, as f32 bits; atomic so the UI never blocks the audio callback.
    level: Arc<AtomicU32>,
}

impl Capture {
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    /// Audio captured since the last call, at 16 kHz mono; the recording continues.
    // ponytail: each chunk is resampled on its own, so 44.1 kHz devices get a sub-sample phase step per chunk; inaudible for speech.
    pub fn take_new(&self) -> Vec<f32> {
        let raw = std::mem::take(&mut *self.buffer.lock().unwrap_or_else(|e| e.into_inner()));
        resample_linear(&raw, self.source_rate, RATE)
    }

    /// Stop and return the whole recording at 16 kHz mono.
    pub fn stop(mut self) -> Vec<f32> {
        self.halt();
        let raw = std::mem::take(&mut *self.buffer.lock().unwrap_or_else(|e| e.into_inner()));
        resample_linear(&raw, self.source_rate, RATE)
    }

    fn halt(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.halt();
    }
}

/// cpal streams aren't reliably `Send`, so the stream lives entirely on one thread.
pub fn start_capture(device_name: Option<&str>) -> Result<Capture, String> {
    if let Ok(path) = std::env::var("VOICETYPE_FAKE_MIC") {
        return fake_capture(&path);
    }
    let host = cpal::default_host();
    let device = resolve_device(&host, device_name)?;
    let config = device.default_input_config().map_err(|e| format!("failed to read input device config: {e}"))?;
    let source_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    let sample_format = config.sample_format();
    let stream_config: cpal::StreamConfig = config.into();

    let buffer = Arc::new(Mutex::new(Vec::new()));
    let level = Arc::new(AtomicU32::new(0));
    let (buf_t, level_t) = (buffer.clone(), level.clone());
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    let join = std::thread::spawn(move || {
        let stream = match build_stream(&device, &stream_config, sample_format, channels, buf_t, level_t) {
            Ok(s) => s,
            Err(e) => return drop(ready_tx.send(Err(e))),
        };
        if let Err(e) = stream.play() {
            return drop(ready_tx.send(Err(format!("failed to start capture: {e}"))));
        }
        let _ = ready_tx.send(Ok(()));
        let _ = stop_rx.recv();
        drop(stream);
    });

    ready_rx.recv().map_err(|_| "capture thread exited unexpectedly".to_string())??;
    Ok(Capture { stop_tx, join: Some(join), buffer, source_rate, level })
}

/// Plays a WAV as if it were the mic, in real time, then silence. For testing
/// the whole dictation path without speaking: `VOICETYPE_FAKE_MIC=clip.wav`.
fn fake_capture(path: &str) -> Result<Capture, String> {
    let clip = load_wav(std::path::Path::new(path))?;
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let level = Arc::new(AtomicU32::new(0));
    let (buf_t, level_t) = (buffer.clone(), level.clone());
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let join = std::thread::spawn(move || {
        let silence = [0.0f32; 320];
        let mut chunks = clip.chunks(320);
        while stop_rx.try_recv().is_err() {
            let chunk = chunks.next().unwrap_or(&silence);
            level_t.store(chunk.iter().fold(0.0f32, |m, s| m.max(s.abs())).to_bits(), Ordering::Relaxed);
            buf_t.lock().unwrap().extend_from_slice(chunk);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
    Ok(Capture { stop_tx, join: Some(join), buffer, source_rate: RATE, level })
}

/// Energy voice-activity detection over 20 ms frames, with a noise floor that
/// adapts so a fan or room hum doesn't count as speech.
pub struct Vad {
    floor: f32,
    /// The user's calibration knob: RMS below this is always silence.
    pub min_threshold: f32,
    pub rms: f32,
}

pub const FRAME: usize = 320;

impl Vad {
    pub fn new(min_threshold: f32) -> Self {
        Vad { floor: min_threshold / 3.0, min_threshold, rms: 0.0 }
    }

    pub fn threshold(&self) -> f32 {
        self.min_threshold.max(self.floor * 3.0)
    }

    pub fn is_speech(&mut self, frame: &[f32]) -> bool {
        self.rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len().max(1) as f32).sqrt();
        let speech = self.rms > self.threshold();
        // Fall fast, rise slowly, and never learn from speech.
        if self.rms < self.floor {
            self.floor = self.floor * 0.9 + self.rms * 0.1;
        } else if !speech {
            self.floor = self.floor * 0.995 + self.rms * 0.005;
        }
        speech
    }
}

fn build_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    channels: usize,
    buffer: Arc<Mutex<Vec<f32>>>,
    level: Arc<AtomicU32>,
) -> Result<cpal::Stream, String> {
    macro_rules! build {
        ($ty:ty, $convert:expr) => {
            device.build_input_stream(
                config,
                move |data: &[$ty], _| {
                    let mono = to_mono(data, channels, $convert);
                    let peak = mono.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                    level.store(peak.to_bits(), Ordering::Relaxed);
                    if let Ok(mut buf) = buffer.lock() {
                        buf.extend_from_slice(&mono);
                    }
                },
                |e| eprintln!("audio stream error: {e}"),
                None,
            )
        };
    }
    let stream = match sample_format {
        cpal::SampleFormat::F32 => build!(f32, |s: f32| s),
        cpal::SampleFormat::I16 => build!(i16, |s: i16| s as f32 / i16::MAX as f32),
        cpal::SampleFormat::U16 => build!(u16, |s: u16| (s as f32 - 32768.0) / 32768.0),
        other => return Err(format!("unsupported sample format: {other:?}")),
    };
    stream.map_err(|e| format!("failed to open input stream: {e}"))
}

fn to_mono<T: Copy>(data: &[T], channels: usize, convert: impl Fn(T) -> f32) -> Vec<f32> {
    if channels <= 1 {
        return data.iter().map(|&s| convert(s)).collect();
    }
    data.chunks(channels).map(|f| f.iter().map(|&s| convert(s)).sum::<f32>() / channels as f32).collect()
}

/// Linear interpolation; adequate for speech.
fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from as f64 / to as f64;
    let out_len = (input.len() as f64 / ratio).floor() as usize;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let idx = pos.floor() as usize;
            let a = input[idx];
            let b = *input.get(idx + 1).unwrap_or(&a);
            a + (b - a) * (pos - idx as f64) as f32
        })
        .collect()
}

/// 16-bit 16 kHz mono WAV, which is what whisper-server wants.
pub fn encode_wav(samples: &[f32]) -> Vec<u8> {
    let spec = hound::WavSpec { channels: 1, sample_rate: RATE, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut w = hound::WavWriter::new(&mut buf, spec).expect("wav to memory");
    for &s in samples {
        w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).expect("wav to memory");
    }
    w.finalize().expect("wav to memory");
    buf.into_inner()
}

/// Any PCM WAV file, as 16 kHz mono samples.
pub fn load_wav(path: &std::path::Path) -> Result<Vec<f32>, String> {
    let mut r = hound::WavReader::open(path).map_err(|e| e.to_string())?;
    let spec = r.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>(),
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>().map(|s| s.map(|s| s as f32 / max)).collect::<Result<_, _>>()
        }
    }
    .map_err(|e| e.to_string())?;
    let mono = to_mono(&samples, spec.channels as usize, |s| s);
    Ok(resample_linear(&mono, spec.sample_rate, RATE))
}

#[cfg(test)]
mod tests {
    #[test]
    fn vad_separates_speech_from_noise() {
        let mut v = super::Vad::new(0.01);
        let noise: Vec<f32> = (0..320).map(|i| if i % 2 == 0 { 0.003 } else { -0.003 }).collect();
        let voice: Vec<f32> = (0..320).map(|i| (i as f32 / 5.0).sin() * 0.2).collect();
        for _ in 0..200 {
            assert!(!v.is_speech(&noise));
        }
        assert!(v.is_speech(&voice));
        assert!(!v.is_speech(&noise));
    }

    #[test]
    fn wav_round_trip_and_resample() {
        let tone: Vec<f32> = (0..48_000).map(|i| (i as f32 / 10.0).sin() * 0.5).collect();
        assert_eq!(super::resample_linear(&tone, 48_000, 16_000).len(), 16_000);
        let path = std::env::temp_dir().join("voicetype-test.wav");
        std::fs::write(&path, super::encode_wav(&tone[..16_000])).unwrap();
        let back = super::load_wav(&path).unwrap();
        assert_eq!(back.len(), 16_000);
        assert!((back[100] - tone[100]).abs() < 1e-3);
    }
}
