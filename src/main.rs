use std::{
    env::args,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use cpal::{
    self, Sample, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

use crossbeam_channel as channel;

fn build_stream<T>(
    device: &cpal::Device,
    cfg: &cpal::StreamConfig,
    tx: mpsc::SyncSender<Vec<i16>>,
) -> cpal::Stream
where
    T: SizedSample + Sample + 'static,
    <T as Sample>::Float: Into<f32>,
{
    let channels = cfg.channels;

    let data_callback: Box<dyn FnMut(&[T], &cpal::InputCallbackInfo) + Send + 'static> =
        Box::new(move |data: &[T], _info: &cpal::InputCallbackInfo| {
            let mut buf = Vec::with_capacity(data.len());
            for &s in data {
                let f = s.to_float_sample().into();
                let clamped = f.clamp(-1.0, 1.0);
                let i = (clamped * i16::MAX as f32) as i16;
                buf.push(i);
            }
            let _ = tx.send(buf);
        });

    let error_callback: Box<dyn FnMut(cpal::StreamError) + Send + 'static> =
        Box::new(move |_err| {
            eprintln!("Stream error");
        });

    device
        .build_input_stream(cfg, data_callback, error_callback, None)
        .expect("Failed to build input stream")
}

fn main() -> () {
    println!("Hello, world!");
    let default_output_path = "output.wav";
    let out_path = args().nth(1).unwrap_or(default_output_path.into());
    eprintln!("output path: {}", out_path);
    let host = cpal::default_host();
    let device = host.default_input_device().expect("No input device found");
    eprintln!("input device: {:?}", device.name());
    let config = device
        .default_input_config()
        .expect("No default input config");
    eprintln!(
        "input config: {:?}, {}Hz, {}ch",
        config.sample_format(),
        config.sample_rate().0,
        config.channels()
    );
    let wav_spec = hound::WavSpec {
        channels: config.channels(),
        sample_rate: config.sample_rate().0,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let (tx, rx) = mpsc::sync_channel::<Vec<i16>>(8);
    let writer_handle = thread::spawn({
        let out_path = out_path.clone();
        move || {
            let mut writer = hound::WavWriter::create(out_path, wav_spec)
                .expect("Cannot open output file for write out");
            while let Ok(buf) = rx.recv() {
                for s in buf {
                    writer.write_sample(s).expect("wav write failed");
                }
            }
        }
    });
    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        ctrlc::set_handler(move || {
            eprintln!("Stopping...");
            running.store(false, Ordering::SeqCst);
        })
        .expect("cannot set ctrl+c handler");
    }
    let cfg = cpal::StreamConfig {
        channels: config.channels(),
        sample_rate: config.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => build_stream::<f32>(&device, &cfg, tx.clone()),
        cpal::SampleFormat::I32 => build_stream::<i32>(&device, &cfg, tx.clone()),
        cpal::SampleFormat::U16 => build_stream::<u16>(&device, &cfg, tx.clone()),
        cpal::SampleFormat::I16 => build_stream::<i16>(&device, &cfg, tx.clone()),
        _ => todo!(),
    };
    stream.play().expect("Failed to play stream");
    eprintln!("Recording... (press Ctrl+C to stop)");
    while running.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(100));
    }
    drop(stream);
    drop(tx);
    writer_handle.join().ok();
    eprintln!("Done");
}
