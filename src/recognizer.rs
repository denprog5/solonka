//! Поток распознавания: VAD → GigaAM (sherpa-onnx) → матчер → счётчик.
//!
//! GigaAM — офлайн CTC-модель с посимвольным выводом: словаря нет, поэтому она может
//! написать любую форму мата. Распознаёт фразу целиком, когда VAD её закрыл
//! (или когда фраза дошла до `MAX_PHRASE_MS`).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use sherpa_onnx::{
    LinearResampler, OfflineNemoEncDecCtcModelConfig, OfflineRecognizer, OfflineRecognizerConfig,
};

use crate::counter::SharedCounter;
use crate::matcher::Matcher;
use crate::status::{Engine, SharedStatus};
use crate::win;

/// Частота, на которой обучена модель.
const MODEL_RATE: u32 = 16_000;

pub enum Ctrl {
    /// Новый источник звука (моно f32, `rate` Гц).
    Source {
        consumer: rtrb::Consumer<f32>,
        rate: u32,
    },
    /// Захват остановлен.
    Stop,
    SetMatcher(Matcher),
    SetVadLevel(f32),
}

pub struct Recognizer {
    thread: Thread,
    tx: Sender<Ctrl>,
}

impl Recognizer {
    pub fn spawn(
        model_dir: PathBuf,
        matcher: Matcher,
        vad_level: f32,
        counter: SharedCounter,
        status: SharedStatus,
    ) -> Recognizer {
        let (tx, rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("recognizer".into())
            .spawn(move || {
                win::lower_current_thread();
                match Asr::load(&model_dir) {
                    Ok(asr) => {
                        set_engine(&status, Engine::Ready);
                        Worker {
                            matcher,
                            vad_level,
                            counter,
                        }
                        .run(&asr, rx);
                    }
                    Err(e) => set_engine(&status, Engine::Error(e)),
                }
            })
            .expect("spawn recognizer");
        Recognizer {
            thread: handle.thread().clone(),
            tx,
        }
    }

    pub fn thread(&self) -> Thread {
        self.thread.clone()
    }

    pub fn send(&self, c: Ctrl) {
        let _ = self.tx.send(c);
        self.thread.unpark();
    }
}

fn set_engine(status: &SharedStatus, e: Engine) {
    status.lock().unwrap().engine = e;
    win::notify_main(win::EV_REFRESH);
}

// ---------------------------------------------------------------------------------------------
// GigaAM через sherpa-onnx

struct Asr {
    rec: OfflineRecognizer,
}

impl Asr {
    fn load(model_dir: &Path) -> Result<Asr, String> {
        let model = model_dir.join("model.int8.onnx");
        let tokens = model_dir.join("tokens.txt");
        if !model.is_file() || !tokens.is_file() {
            return Err(format!("Нет модели GigaAM: {}", model_dir.display()));
        }
        let mut cfg = OfflineRecognizerConfig::default();
        // Короткие 8.3-пути: sherpa-onnx открывает файлы через `char*`, кириллица в пути ломает загрузку.
        cfg.model_config.nemo_ctc = OfflineNemoEncDecCtcModelConfig {
            model: Some(win::short_path(&model)),
        };
        cfg.model_config.tokens = Some(win::short_path(&tokens));
        // Один поток: два быстрее по времени, но суммарно едят больше CPU, а задержка нам не критична.
        cfg.model_config.num_threads = 1;
        cfg.model_config.provider = Some("cpu".into());
        let rec = OfflineRecognizer::create(&cfg)
            .ok_or_else(|| format!("Не удалось загрузить модель: {}", model_dir.display()))?;
        Ok(Asr { rec })
    }

    /// `pcm` — моно 16 кГц.
    fn decode(&self, pcm: &[f32]) -> String {
        let stream = self.rec.create_stream();
        stream.accept_waveform(MODEL_RATE as i32, pcm);
        self.rec.decode(&stream);
        stream.get_result().map(|r| r.text).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------------------------
// VAD: тишину в модель не подаём — это основная экономия CPU.

const FRAME_MS: u32 = 10;
/// Сколько тишины после речи ещё держим фразу открытой.
const HANGOVER_MS: u32 = 500;
/// Сколько звука до срабатывания VAD подклеиваем в начало, чтобы не обрезать первый слог.
const PREROLL_MS: u32 = 300;
/// Максимальная длина фразы: при непрерывной речи без пауз иначе растут задержка и разовый всплеск CPU.
const MAX_PHRASE_MS: u32 = 10_000;
/// В каком хвосте длинной фразы искать самое тихое место для разреза (чтобы не разрубить слово).
const CUT_SEARCH_MS: u32 = 1_500;

struct Vad {
    frame_len: usize,
    min_level: f32,
    noise: f32,
    hang_left: u32,
    hang_frames: u32,
    preroll: VecDeque<f32>,
    preroll_len: usize,
}

impl Vad {
    fn new(rate: u32, min_level: f32) -> Vad {
        let frame_len = (rate * FRAME_MS / 1000).max(1) as usize;
        let preroll_len = (rate * PREROLL_MS / 1000) as usize;
        Vad {
            frame_len,
            min_level,
            noise: min_level,
            hang_left: 0,
            hang_frames: HANGOVER_MS / FRAME_MS,
            preroll: VecDeque::with_capacity(preroll_len + frame_len),
            preroll_len,
        }
    }

    fn speaking(&self) -> bool {
        self.hang_left > 0
    }

    /// Обрабатывает кадр; речь (с предзаписью) дописывает в `out`.
    /// Возвращает `true`, когда фраза закончилась.
    fn push(&mut self, frame: &[f32], out: &mut Vec<f32>) -> bool {
        let rms = rms(frame);
        let threshold = self.min_level.max(self.noise * 3.0);
        if rms > threshold {
            if !self.speaking() {
                out.extend(self.preroll.drain(..));
            }
            self.hang_left = self.hang_frames;
            out.extend_from_slice(frame);
            return false;
        }
        if self.speaking() {
            out.extend_from_slice(frame);
            self.hang_left -= 1;
            return self.hang_left == 0;
        }
        // Тишина: подстраиваем уровень шума (вниз быстро, вверх медленно) и копим предзапись.
        let k = if rms < self.noise { 0.1 } else { 0.005 };
        self.noise += (rms - self.noise) * k;
        self.preroll.extend(frame);
        let extra = self.preroll.len().saturating_sub(self.preroll_len);
        self.preroll.drain(..extra);
        false
    }
}

fn rms(s: &[f32]) -> f32 {
    (s.iter().map(|x| x * x).sum::<f32>() / s.len().max(1) as f32).sqrt()
}

// ---------------------------------------------------------------------------------------------

/// Обработка одного источника: VAD собирает фразу, на её конце — ресемплинг и распознавание.
struct Pipeline<'a> {
    asr: &'a Asr,
    vad: Vad,
    resampler: Option<LinearResampler>,
    phrase: Vec<f32>,
    max_len: usize,
    cut_search: usize,
}

impl<'a> Pipeline<'a> {
    fn new(asr: &'a Asr, rate: u32, vad_level: f32) -> Pipeline<'a> {
        let resampler = (rate != MODEL_RATE)
            .then(|| LinearResampler::create(rate as i32, MODEL_RATE as i32))
            .flatten();
        Pipeline {
            asr,
            vad: Vad::new(rate, vad_level),
            resampler,
            phrase: Vec::new(),
            max_len: (rate * MAX_PHRASE_MS / 1000) as usize,
            cut_search: (rate * CUT_SEARCH_MS / 1000) as usize,
        }
    }

    fn frame_len(&self) -> usize {
        self.vad.frame_len
    }

    /// Один кадр. Возвращает текст, когда фраза закончилась или стала слишком длинной.
    fn push_frame(&mut self, frame: &[f32]) -> Option<String> {
        if self.vad.push(frame, &mut self.phrase) {
            return Some(self.finish());
        }
        if self.phrase.len() >= self.max_len {
            let rest = self.phrase.split_off(self.quietest_point());
            let text = self.finish();
            self.phrase = rest;
            return Some(text);
        }
        None
    }

    /// Индекс разреза: начало самого тихого 10-мс окна в хвосте фразы.
    fn quietest_point(&self) -> usize {
        let step = self.vad.frame_len;
        let start = self.phrase.len().saturating_sub(self.cut_search);
        (start..self.phrase.len().saturating_sub(step))
            .step_by(step)
            .min_by(|&a, &b| {
                rms(&self.phrase[a..a + step]).total_cmp(&rms(&self.phrase[b..b + step]))
            })
            .unwrap_or(self.phrase.len())
    }

    /// Распознать накопленную фразу и очистить буфер.
    fn finish(&mut self) -> String {
        if self.phrase.is_empty() {
            return String::new();
        }
        let text = match &self.resampler {
            Some(r) => {
                let pcm = r.resample(&self.phrase, true);
                r.reset();
                self.asr.decode(&pcm)
            }
            None => self.asr.decode(&self.phrase),
        };
        self.phrase.clear();
        text
    }
}

struct Worker {
    matcher: Matcher,
    vad_level: f32,
    counter: SharedCounter,
}

impl Worker {
    fn run(mut self, asr: &Asr, rx: Receiver<Ctrl>) {
        let mut source: Option<(rtrb::Consumer<f32>, Pipeline)> = None;
        let mut frame = Vec::new();
        let mut last_day_check = Instant::now();

        loop {
            loop {
                match rx.try_recv() {
                    Ok(Ctrl::Source { consumer, rate }) => {
                        if let Some((_, mut p)) = source.take() {
                            self.count(&p.finish());
                        }
                        source = Some((consumer, Pipeline::new(asr, rate, self.vad_level)));
                    }
                    Ok(Ctrl::Stop) => {
                        if let Some((_, mut p)) = source.take() {
                            self.count(&p.finish());
                        }
                    }
                    Ok(Ctrl::SetMatcher(m)) => self.matcher = m,
                    Ok(Ctrl::SetVadLevel(v)) => {
                        self.vad_level = v;
                        if let Some((_, p)) = &mut source {
                            p.vad.min_level = v;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }

            if let Some((consumer, p)) = &mut source {
                while let Ok(chunk) = consumer.read_chunk(p.frame_len()) {
                    let (a, b) = chunk.as_slices();
                    frame.clear();
                    frame.extend_from_slice(a);
                    frame.extend_from_slice(b);
                    chunk.commit_all();
                    if let Some(text) = p.push_frame(&frame) {
                        self.count(&text);
                    }
                }
            }

            if last_day_check.elapsed() >= Duration::from_secs(30) {
                last_day_check = Instant::now();
                self.counter.lock().unwrap().roll_day();
            }
            // Аудио будит поток ~10 раз/с во время записи; без звука — раз в 30 с для смены дня.
            thread::park_timeout(Duration::from_secs(30));
        }
    }

    fn count(&self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let n = self.matcher.count(text);
        if cfg!(debug_assertions) {
            eprintln!("[asr] {text}  → +{n}");
        }
        if n > 0 {
            self.counter.lock().unwrap().add(n);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Отладка без микрофона: `solonka --wav файл.wav` (вывод виден в debug-сборке).

pub fn run_wav(
    path: &Path,
    model_dir: &Path,
    matcher: &Matcher,
    vad_level: f32,
) -> Result<(), String> {
    let (samples, rate) = read_wav(path)?;
    let t0 = Instant::now();
    let asr = Asr::load(model_dir)?;
    println!("модель загружена за {:.1} с", t0.elapsed().as_secs_f32());

    let mut p = Pipeline::new(&asr, rate, vad_level);
    let mut total = 0;
    let mut report = |text: &str, fed: usize| {
        let text = text.trim();
        if !text.is_empty() {
            let n = matcher.count(text);
            total += n;
            println!("[{:6.1} с] {text}  → +{n}", fed as f32 / rate as f32);
        }
    };
    let t1 = Instant::now();
    let mut fed = 0;
    for frame in samples.chunks_exact(p.frame_len()) {
        fed += frame.len();
        if let Some(t) = p.push_frame(frame) {
            report(&t, fed);
        }
    }
    report(&p.finish(), fed);
    let audio = samples.len() as f32 / rate as f32;
    let spent = t1.elapsed().as_secs_f32();
    println!(
        "итого матов: {total}; {audio:.1} с аудио обработано за {spent:.2} с ({:.0}% от реального времени)",
        spent / audio * 100.0
    );
    Ok(())
}

/// Минимальный разбор WAV: PCM 16 бит или float 32 бит, любое число каналов → моно f32.
fn read_wav(path: &Path) -> Result<(Vec<f32>, u32), String> {
    let d = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if d.len() < 12 || &d[0..4] != b"RIFF" || &d[8..12] != b"WAVE" {
        return Err("это не WAV".into());
    }
    let u16_at = |i: usize| u16::from_le_bytes([d[i], d[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]);
    let (mut fmt, mut data) = (None, None);
    let mut i = 12;
    while i + 8 <= d.len() {
        let size = u32_at(i + 4) as usize;
        let body = i + 8;
        match &d[i..i + 4] {
            b"fmt " if body + 16 <= d.len() => {
                fmt = Some((
                    u16_at(body),
                    u16_at(body + 2),
                    u32_at(body + 4),
                    u16_at(body + 14),
                ))
            }
            b"data" => data = Some(&d[body..(body + size).min(d.len())]),
            _ => {}
        }
        i = body + size + (size & 1);
    }
    let (format, channels, rate, bits) = fmt.ok_or("нет fmt-чанка")?;
    let data = data.ok_or("нет data-чанка")?;
    let ch = channels.max(1) as usize;
    let mix = |f: &[u8], width: usize, conv: &dyn Fn(&[u8]) -> f32| {
        f.chunks_exact(width).map(conv).sum::<f32>() / ch as f32
    };
    let mono: Vec<f32> = match (format, bits) {
        (1, 16) => data
            .chunks_exact(2 * ch)
            .map(|f| mix(f, 2, &|s| i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0))
            .collect(),
        (3, 32) => data
            .chunks_exact(4 * ch)
            .map(|f| mix(f, 4, &|s| f32::from_le_bytes([s[0], s[1], s[2], s[3]])))
            .collect(),
        _ => return Err(format!("неподдерживаемый WAV: формат {format}, {bits} бит")),
    };
    Ok((mono, rate))
}
