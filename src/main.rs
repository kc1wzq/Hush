use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use ringbuf::RingBuffer;
use serde::{Deserialize, Serialize};
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
const RING_CAP: usize = 1 << 17; // ~131k samples (mono)

#[derive(Clone)]
struct Shared {
    running: Arc<AtomicBool>,
    tx_enabled: Arc<AtomicBool>,
    tx_freq_hz: Arc<Mutex<f32>>,
    last_rms: Arc<Mutex<f32>>,
    // the ringbuf: producer (audio callback) -> consumer (worker)
    rb_producer: ringbuf::Producer<f32>,
    rb_consumer: ringbuf::Consumer<f32>,
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
            rb_producer: p,
            rb_consumer: c,
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
    // Set up host and device lists
    let host = cpal::default_host();
    let devices: Vec<_> = host
        .devices()
        .context("Failed to enumerate devices")?
        .collect();

    println!("Detected {} audio devices.", devices.len());
    for (i, d) in devices.iter().enumerate() {
        let name = d.name().unwrap_or_else(|_| "<name error>".to_string());
        println!("{}: {}", i, name);
    }

    let shared = Arc::new(Shared::new());

    // Start worker thread to consume audio from ring buffer and compute RMS
    {
        let s = shared.clone();
        thread::spawn(move || worker_thread(s));
    }

    // Start control server thread
    {
        let s = shared.clone();
        thread::spawn(move || {
            if let Err(e) = control_server(s) {
                eprintln!("control server error: {:?}", e);
            }
        });
    }

    println!("Hush Rust core skeleton running. Control port: {}", CONTROL_PORT);
    println!("Press Ctrl+C to quit.");
    // keep main alive: sleep loop
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

// Simple worker: read from ring buffer, compute RMS and store in shared.last_rms
fn worker_thread(shared: Arc<Shared>) {
    let mut buf = Vec::with_capacity(1024);
    loop {
        // if not running, sleep
        if !shared.running.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
            continue;
        }

        buf.clear();
        // read up to 1024 samples
        for _ in 0..1024 {
            if let Some(s) = shared.rb_consumer.pop() {
                buf.push(s);
            } else {
                break;
            }
        }

        if !buf.is_empty() {
            let sumsq: f64 = buf.iter().map(|v| (*v as f64) * (*v as f64)).sum();
            let rms = ((sumsq / (buf.len() as f64)).sqrt()) as f32;
            *shared.last_rms.lock() = rms;
            // you could also produce waterfall rows here
        } else {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

// Control server: newline-delimited JSON protocol
fn control_server(shared: Arc<Shared>) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", CONTROL_PORT))
        .context("Failed to bind control socket")?;
    println!("Control server listening on 127.0.0.1:{}", CONTROL_PORT);

    // Keep a handle to current streams so we can close them on "close"
    let mut current_input_stream: Option<cpal::Stream> = None;
    let mut current_output_stream: Option<cpal::Stream> = None;
    let host = cpal::default_host();
    let mut devices_cache: Vec<cpal::Device> = host
        .devices()
        .context("Failed to enumerate devices")?
        .collect();

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let shared = shared.clone();
                let mut cur_in = current_input_stream.take();
                let mut cur_out = current_output_stream.take();
                let devices = devices_cache.clone();
                thread::spawn(move || {
                    if let Err(e) =
                        handle_client(s, shared, devices, &mut cur_in, &mut cur_out)
                    {
                        eprintln!("client handler error: {:?}", e);
                    }
                });
                // detach (we don't reassign cur_in/out to outer scope; each client thread manages streams)
            }
            Err(e) => eprintln!("accept error: {:?}", e),
        }
    }
    Ok(())
}

fn handle_client(
    stream: TcpStream,
    shared: Arc<Shared>,
    devices: Vec<cpal::Device>,
    cur_in: &mut Option<cpal::Stream>,
    cur_out: &mut Option<cpal::Stream>,
) -> Result<()> {
    let peer = stream.peer_addr().ok();
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            // closed
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
                for (i, d) in devices.iter().enumerate() {
                    let name = d.name().unwrap_or_else(|_| "<err>".to_string());
                    let in_ch = d.supported_input_configs().map(|c| c.next().is_some()).unwrap_or(false);
                    let out_ch = d.supported_output_configs().map(|c| c.next().is_some()).unwrap_or(false);
                    arr.push(json!({"id": i, "name": name, "has_input": in_ch, "has_output": out_ch}));
                }
                let resp = json!({"ok": true, "devices": arr});
                writeln!(writer, "{}", resp.to_string())?;
            }
            "open" => {
                let input_idx = req.input.unwrap_or(usize::MAX);
                let output_idx = req.output.unwrap_or(usize::MAX);
                let sr = req.sr.unwrap_or(DEFAULT_SR);

                // close old streams first
                *cur_in = None;
                *cur_out = None;
                shared.running.store(false, Ordering::SeqCst);
                // Create streams if indexes valid
                // Input
                if input_idx < devices.len() {
                    let device = &devices[input_idx];
                    // pick input config with f32 sample format and desired sample rate if possible
                    let config = device
                        .supported_input_configs()
                        .context("no input configs")?
                        .filter(|c| c.sample_format() == cpal::SampleFormat::F32)
                        .next()
                        .context("no suitable input config with f32")?
                        .with_sample_rate(cpal::SampleRate(sr));
                    let stream = build_input_stream(device, &config.into(), &shared)?;
                    *cur_in = Some(stream);
                }

                // Output
                if output_idx < devices.len() {
                    let device = &devices[output_idx];
                    let config = device
                        .supported_output_configs()
                        .context("no output configs")?
                        .filter(|c| c.sample_format() == cpal::SampleFormat::F32)
                        .next()
                        .context("no suitable output config with f32")?
                        .with_sample_rate(cpal::SampleRate(sr));
                    let stream = build_output_stream(device, &config.into(), &shared)?;
                    *cur_out = Some(stream);
                }

                // start streams
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
    let sr = config.sample_rate.0 as f32;
    let producer = shared.rb_producer.clone();

    let err_fn = |err| eprintln!("input stream error: {:?}", err);
    let stream = device.build_input_stream(
        config,
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            // data is interleaved; for mono we take first channel, otherwise average channels
            if channels == 1 {
                let pushed = producer.push_slice(data);
                if pushed < data.len() {
                    // buffer overflow: drop oldest? ringbuf crate's SPSC just drops remaining
                }
            } else {
                // downmix to mono
                let mut tmp = Vec::with_capacity(data.len() / channels);
                for frame in data.chunks(channels) {
                    let sum: f32 = frame.iter().sum();
                    tmp.push(sum / (channels as f32));
                }
                let _ = producer.push_slice(&tmp);
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
                // generate mono tone, then duplicate to channels
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
                // output silence
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
