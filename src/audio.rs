//! Захват микрофона через cpal (WASAPI). Колбэк только сводит каналы в моно и кладёт
//! сэмплы в lock-free кольцевой буфер; вся обработка — в потоке распознавания.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, SampleFormat, SizedSample, StreamConfig};

use crate::recognizer::{Ctrl, Recognizer};
use crate::win;

/// Живой поток захвата; drop останавливает запись.
pub struct Capture {
    _stream: cpal::Stream,
    pub device_name: String,
}

pub fn input_device_names() -> Vec<String> {
    let host = cpal::default_host();
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    devices
        .filter_map(|d| d.description().ok().map(|x| x.name().to_owned()))
        .collect()
}

/// Запускает захват. Если выбранного устройства нет, берётся устройство по умолчанию
/// (тогда второе значение — пояснение для статуса).
pub fn start(device: Option<&str>, rec: &Recognizer) -> Result<(Capture, Option<String>), String> {
    let host = cpal::default_host();
    let mut note = None;
    let named = device.and_then(|want| {
        host.input_devices()
            .ok()?
            .find(|d| d.description().is_ok_and(|x| x.name() == want))
    });
    let dev = match (device, named) {
        (_, Some(d)) => d,
        (wanted, None) => {
            if let Some(w) = wanted {
                note = Some(format!("Нет «{w}», взят микрофон по умолчанию"));
            }
            host.default_input_device()
                .ok_or("Не найдено ни одного микрофона")?
        }
    };
    let device_name = dev
        .description()
        .map(|x| x.name().to_owned())
        .unwrap_or_default();
    let supported = dev
        .default_input_config()
        .map_err(|e| format!("Микрофон недоступен: {e}"))?;
    let config = supported.config();
    let rate = config.sample_rate;

    // 5 секунд запаса: пока модель распознаёт длинную фразу (до ~1 с, в эко-режиме дольше),
    // звук копится здесь.
    let (producer, consumer) = rtrb::RingBuffer::<f32>::new(rate as usize * 5);
    let worker = rec.thread();
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&dev, &config, producer, worker),
        SampleFormat::I16 => build::<i16>(&dev, &config, producer, worker),
        SampleFormat::I32 => build::<i32>(&dev, &config, producer, worker),
        SampleFormat::U16 => build::<u16>(&dev, &config, producer, worker),
        f => return Err(format!("Неподдерживаемый формат звука: {f:?}")),
    }
    .map_err(|e| format!("Не удалось открыть микрофон: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("Не удалось запустить микрофон: {e}"))?;
    rec.send(Ctrl::Source { consumer, rate });
    Ok((
        Capture {
            _stream: stream,
            device_name,
        },
        note,
    ))
}

fn build<T>(
    dev: &cpal::Device,
    config: &StreamConfig,
    mut producer: rtrb::Producer<f32>,
    worker: std::thread::Thread,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels.max(1) as usize;
    let gain = 1.0 / channels as f32;
    let capacity = producer.buffer().capacity();
    // Будим поток распознавания примерно 10 раз в секунду, а не на каждый колбэк.
    let wake_at = (config.sample_rate as usize / 10).min(capacity / 2);
    dev.build_input_stream::<T, _, _>(
        *config,
        move |data: &[T], _| {
            for frame in data.chunks_exact(channels) {
                let mut sum = 0.0f32;
                for &s in frame {
                    sum += s.to_sample::<f32>();
                }
                // Буфер полон — значит, распознавание не успевает; лишнее просто теряем.
                let _ = producer.push(sum * gain);
            }
            if capacity - producer.slots() >= wake_at {
                worker.unpark();
            }
        },
        |e| {
            if matches!(
                e.kind(),
                ErrorKind::DeviceNotAvailable
                    | ErrorKind::StreamInvalidated
                    | ErrorKind::HostUnavailable
            ) {
                win::notify_main(win::EV_STREAM_ERROR);
            }
        },
        None,
    )
}
