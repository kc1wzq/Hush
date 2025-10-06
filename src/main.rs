// src/main.rs
use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use ringbuf::RingBuffer;
use serde::{Deserialize};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

const CONTROL_PORT: u16 = 53421;
const DEFAULT_SR: u32 = 48000;
const CHANNELS: u16 = 1;
const RING_CAP: usize = 1 << 17;

#[derive(Clone)]
struct DevicesInfo {
    names: Vec<String>,
}

struct Shared {
    running: Arc<AtomicBool>,
    tx_enabled: Arc<AtomicBool>,
    tx_freq_hz: Arc<Mutex<f32>>,
    last_rms: Arc<Mutex<f32>>,
    // Producer/Consumer protected by mutex so we can use them from closures/threads without Clone
    rb_producer: Arc<Mutex<ringbuf::Producer<f32>>>,
    rb_consumer: Arc<Mutex<ringbuf::Consumer<f32>>>,
}

impl Shared {
    fn new() -> Self {
        let rb = RingBuffer::<f32>::new(RING_CAP);
        let (p, c) = rb.split();
        Shared {
            running: Arc::new(AtomicBool::new(false)),
            tx_enabled: Arc::new(AtomicBool::new(false)),
            tx_freq_hz: Arc::new(Mutex::new(600.0)),
            last_rms: Arc::new(Mutex::new(0.0)),
            rb_producer: Arc::new(Mutex::new(p)),
            rb_consumer: Arc::new(Mutex::new(c)),
        }
    }
}

#[derive(Deserialize)]
struct Req {
    cmd: String,
    input: Option<usize>,
    output: Option<usize>,
    sr: Option<u32>,
    tone_freq: Option<f32>,
}

fn main() -> Result<()> {
    // enumerate devices once, but also keep device handles when we open streams
    let host = cpal::default_host();
    let devices_list: Vec<cpal::Device> = host
        .devices()
        .context("Failed to enumerate devices")?
        .collect();

    let mut names = Vec::with_capacity(devices_list.len());
    for (i, d) in devices_list.iter().enumerate() {
        let name = d.name().unwrap_or_else(|_| "<name error>".to_string());
        println!("{}: {}", i, name);
        names.push(name);
    }
    let devices_info = DevicesInfo { names };

    let shared = Arc::new(Shared::new());

    // worker thread consumes from ring buffer
    {
        let s = shared.clone();
        thread::spawn(move || worker_thread(s));
    }

    // Run the control server in the main thread (sequential client handling).
    // Important: cpal::Stream may be non-Send on some platforms; create/manipulate them here.
    control_server(shared, devices_list, devices_info)?;

    Ok(())
}

fn worker_thread(shared: Arc<Shared>) {
    let mut buf = Vec::with_capacity(1024);
    loop {
        if !shared.running.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
            continue;
        }

        buf.clear();
        // try to pop up to 1024 samples
        {
            let mut cons = shared.rb_consumer.lock();
            for _ in 0..1024 {
                if let Some(s) = cons.pop() {
                    buf.push(s);
                } else {
                    break;
                }
            }
        }

        if !buf.is_empty() {
            let sumsq: f64 = buf.iter().map(|v| (*v as f64) * (*v as f64)).sum();
            let rms = ((sumsq / (buf.len() as f64)).sqrt()) as f32;
            *shared.last_rms.lock() = rms;
        } else {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

fn control_server(
    shared: Arc<Shared>,
    devices: Vec<cpal::Device>,
    dev_info: DevicesInfo,
) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", CONTROL_PORT))
        .context("Failed to bind control socket")?;
    println!("Control server listening on 127.0.0.1:{}", CONTROL_PORT);

    // Keep Option<Stream> in this thread scope so we don't move them into other threads.
    let mut current_input_stream: Option<cpal::Stream> = None;
    let mut current_output_stream: Option<cpal::Stream> = None;

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                // handle client synchronously in this thread (no spawn)
                if let Err(e) =
                    handle_client(s, &shared, &devices, &mut current_input_stream, &mut current_output_stream, &dev_info)
                {
                    eprintln!("client handler error: {:?}", e);
                }
            }
            Err(e) => eprintln!("accept error: {:?}", e),
        }
    }
    Ok(())
}

fn handle_client(
    stream: TcpStream,
    shared: &Arc<Shared>,
    devices: &Vec<cpal::Device>,
    cur_in: &mut Option<cpal::Stream>,
    cur_out: &mut Option<cpal::Stream>,
    dev_info: &DevicesInfo,
) -> Result<()> {
    let peer = stream.peer_addr().ok();
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let mut line = String::new();

    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }

        let req: Req = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(_) => {
                let resp = json!({"ok": false, "error": "invalid json"});
                writeln!(writer, "{}", resp.to_string())?;
                continue;
            }
        };

        match req.cmd.as_str() {
            "list_devices" => {
                let mut arr = Vec::new();
                for (i, name) in dev_info.names.iter().enumerate() {
                    // quick check: default_*_config presence
                    let has_in = devices.get(i).and_then(|d| d.default_input_config().ok()).is_some();
                    let has_out = devices.get(i).and_then(|d| d.default_output_config().ok()).is_some();
                    arr.push(json!({"id": i, "name": name, "has_input": has_in, "has_output": has_out}));
                }
                let resp = json!({"ok": true, "devices": arr});
                writeln!(writer, "{}", resp.to_string())?;
            }
            "open" => {
                let input_idx = req.input.unwrap_or(usize::MAX);
                let output_idx = req.output.unwrap_or(usize::MAX);
                let sr = req.sr.unwrap_or(DEFAULT_SR);

                // close old
                *cur_in = None;
                *cur_out = None;
                shared.running.store(false, Ordering::SeqCst);

                // Input
                if input_idx < devices.len() {
                    let device = &devices[input_idx];
                    let config = device
                        .default_input_config()
                        .context("no default input config")?;
                    let mut cfg = cpal::StreamConfig {
                        channels: config.channels(),
                        sample_rate: cpal::SampleRate(sr),
                        buffer_size: cpal::BufferSize::Default,
                    };
                    let stream = build_input_stream(device, &cfg, shared)?;
                    *cur_in = Some(stream);
                }

                // Output
                if output_idx < devices.len() {
                    let device = &devices[output_idx];
                    let config = device
                        .default_output_config()
                        .context("no default output config")?;
                    let mut cfg = cpal::StreamConfig {
                        channels: config.channels(),
                        sample_rate: cpal::SampleRate(sr),
                        buffer_size: cpal::BufferSize::Default,
                    };
                    let stream = build_output_stream(device, &cfg, shared)?;
                    *cur_out = Some(stream);
                }

                if let Some(s) = cur_in {
                    s.play().ok();
                }
                if let Some(s) = cur_out {
                    s.play().ok();
                }

                shared.running.store(true, Ordering::SeqCst);
                let resp = json!({"ok": true});
                writeln!(writer, "{}", resp.to_string())?;
            }
            "close" => {
                *cur_in = None;
                *cur_out = None;
                shared.running.store(false, Ordering::SeqCst);
                let resp = json!({"ok": true});
                writeln!(writer, "{}", resp.to_string())?;
            }
            "status" => {
                let resp = json!({
                    "ok": true,
                    "running": shared.running.load(Ordering::SeqCst),
                    "last_rms": *shared.last_rms.lock(),
                    "tx_enabled": shared.tx_enabled.load(Ordering::SeqCst),
                    "tx_freq": *shared.tx_freq_hz.lock()
                });
                writeln!(writer, "{}", resp.to_string())?;
            }
            "start_tx" => {
                if let Some(f) = req.tone_freq {
                    *shared.tx_freq_hz.lock() = f;
                }
                shared.tx_enabled.store(true, Ordering::SeqCst);
                writeln!(writer, "{}", json!({"ok": true}).to_string())?;
            }
            "stop_tx" => {
                shared.tx_enabled.store(false, Ordering::SeqCst);
                writeln!(writer, "{}", json!({"ok": true}).to_string())?;
            }
            _ => {
                writeln!(writer, "{}", json!({"ok": false, "error": "unknown command"}).to_string())?;
            }
        }
    }

    println!("client disconnected: {:?}", peer);
    Ok(())
}

fn build_input_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: &Arc<Shared>,
) -> Result<cpal::Stream> {
    let channels = config.channels as usize;
    let producer_arc = shared.rb_producer.clone();

    let err_fn = |err| eprintln!("input stream error: {:?}", err);
    let stream = device.build_input_stream(
        config,
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            // interleaved frames
            if channels == 1 {
                // push directly
                let mut p = producer_arc.lock();
                let _ = p.push_slice(data);
            } else {
                // downmix
                let mut tmp = Vec::with_capacity(data.len() / channels);
                for frame in data.chunks(channels) {
                    let sum: f32 = frame.iter().sum();
                    tmp.push(sum / (channels as f32));
                }
                let mut p = producer_arc.lock();
                let _ = p.push_slice(&tmp);
            }
        },
        err_fn,
        None,
    )?;
    Ok(stream)
}

fn build_output_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: &Arc<Shared>,
) -> Result<cpal::Stream> {
    let channels = config.channels as usize;
    let sample_rate = config.sample_rate.0 as f32;
    let tx_enabled = shared.tx_enabled.clone();
    let tx_freq = shared.tx_freq_hz.clone();

    let err_fn = |err| eprintln!("output stream error: {:?}", err);

    // simple tone generator state
    let phase = Arc::new(Mutex::new(0f32));

    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
            let enabled = tx_enabled.load(Ordering::SeqCst);
            let freq = *tx_freq.lock();
            if enabled {
                let mut ph = phase.lock();
                let two_pi = std::f32::consts::PI * 2.0;
                for frame in data.chunks_mut(channels) {
                    let sample = (*ph).sin() * 0.3;
                    for s in frame.iter_mut() {
                        *s = sample;
                    }
                    *ph += two_pi * freq / sample_rate;
                    if *ph > two_pi {
                        *ph -= two_pi;
                    }
                }
            } else {
                for v in data.iter_mut() {
                    *v = 0.0;
                }
            }
        },
        err_fn,
        None,
    )?;
    Ok(stream)
}
