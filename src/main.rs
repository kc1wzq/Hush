use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use ringbuf::RingBuffer;
use serde::Deserialize;
use serde_json::json;
use std::collections::VecDeque;
use std::f32::consts::PI;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};
use std::collections::HashMap;

const CONTROL_PORT: u16 = 53421;
const DEFAULT_SR: u32 = 48000;
const RING_CAP: usize = 1 << 17; // 131k

// ---------- Mode config types ----------
#[derive(Clone, Debug)]
pub struct MfskParams {
    pub m: usize,
    pub center: f32,
    pub spacing: f32,
    pub symbol_len_ms: u32,
    pub preamble_repeats: u32,
}

#[derive(Clone, Debug)]
pub struct DpskParams {
    pub center: f32,
    pub symbol_len_ms: u32,
    pub preamble_repeats: u32,
}

#[derive(Clone, Debug)]
pub enum ModeConfig {
    Mfsk(MfskParams),
    Dpsk(DpskParams),
    // future: Qam(QamParams), Ofdm(OfdmParams), Msk(MskParams), etc.
}

// ---------- Shared state ----------
struct DevicesInfo {
    names: Vec<String>,
}

struct Shared {
    running: Arc<AtomicBool>,
    tx_enabled: Arc<AtomicBool>,
    tx_freq_hz: Arc<Mutex<f32>>,
    last_rms: Arc<Mutex<f32>>,
    sample_rate: Arc<Mutex<u32>>, // current opened sample rate (set on open)
    // RX ring
    rb_producer: Arc<Mutex<ringbuf::Producer<f32>>>,
    rb_consumer: Arc<Mutex<ringbuf::Consumer<f32>>>,
    // TX ring
    tx_producer: Arc<Mutex<ringbuf::Producer<f32>>>,
    tx_consumer: Arc<Mutex<ringbuf::Consumer<f32>>>,
    // preamble template for matched filter (monophonic samples)
    preamble_template: Arc<Mutex<Option<Vec<f32>>>>,
    // active mode (set when send_frame or explicit set_mode is called)
    current_mode: Arc<Mutex<Option<ModeConfig>>>,
    // detection state: hold last detected time to avoid spam
    last_detection: Arc<Mutex<Option<Instant>>>,
}

impl Shared {
    fn new() -> Self {
        let rx_rb = RingBuffer::<f32>::new(RING_CAP);
        let (rx_p, rx_c) = rx_rb.split();
        let tx_rb = RingBuffer::<f32>::new(RING_CAP);
        let (tx_p, tx_c) = tx_rb.split();
        Shared {
            running: Arc::new(AtomicBool::new(false)),
            tx_enabled: Arc::new(AtomicBool::new(false)),
            tx_freq_hz: Arc::new(Mutex::new(600.0)),
            last_rms: Arc::new(Mutex::new(0.0)),
            sample_rate: Arc::new(Mutex::new(DEFAULT_SR)),
            rb_producer: Arc::new(Mutex::new(rx_p)),
            rb_consumer: Arc::new(Mutex::new(rx_c)),
            tx_producer: Arc::new(Mutex::new(tx_p)),
            tx_consumer: Arc::new(Mutex::new(tx_c)),
            preamble_template: Arc::new(Mutex::new(None)),
            current_mode: Arc::new(Mutex::new(None)),
            last_detection: Arc::new(Mutex::new(None)),
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
    path: Option<String>,
    // send_frame params / mode choice
    mode: Option<String>,             // "mfsk" | "dpsk"
    speed: Option<String>,            // optional preset name, e.g., "MEDIUM"
    mfsk_m: Option<usize>,
    mfsk_center: Option<f32>,
    mfsk_spacing: Option<f32>,
    symbol_len_ms: Option<u32>,
    preamble_repeats: Option<u32>,
    payload_bits: Option<String>,
}

// mode setups
// TODO: work on adding more
fn presets_map() -> HashMap<&'static str, MfskParams> {
    let mut m = HashMap::new();
    m.insert("QRP_ULTRA", MfskParams { m: 4,  center: 1000.0, spacing: 5.0,  symbol_len_ms: 300, preamble_repeats: 8 });
    m.insert("QRP",       MfskParams { m: 4,  center: 1000.0, spacing: 10.0, symbol_len_ms: 150, preamble_repeats: 6 });
    m.insert("SLOW",      MfskParams { m: 8,  center: 1000.0, spacing: 20.0, symbol_len_ms: 100, preamble_repeats: 5 });
    m.insert("MEDIUM",    MfskParams { m: 16, center: 1000.0, spacing: 20.0, symbol_len_ms: 60,  preamble_repeats: 4 });
    m.insert("FAST",      MfskParams { m: 32, center: 1000.0, spacing: 30.0, symbol_len_ms: 30,  preamble_repeats: 3 });
    m.insert("TURBO",     MfskParams { m: 64, center: 1000.0, spacing: 200.0, symbol_len_ms: 20,  preamble_repeats: 3 });
    m
}


// ---------- modems module (generators) ----------
mod modems {
    use super::*;
    // simple tone generator
    pub fn generate_tone(freq: f32, dur_s: f32, sr: u32) -> Vec<f32> {
        let n = (dur_s * sr as f32).round() as usize;
        let mut out = Vec::with_capacity(n);
        let dt = 1.0f32 / sr as f32;
        let mut phi = 0.0f32;
        let two_pi = PI * 2.0;
        for _ in 0..n {
            out.push(phi.sin());
            phi += two_pi * freq * dt;
            if phi > two_pi { phi -= two_pi; }
        }
        out
    }

    pub fn generate_tone_with_phase(freq: f32, dur_s: f32, sr: u32, phi: &mut f32) -> Vec<f32> {
        let n = (dur_s * sr as f32).round() as usize;
        let mut out = Vec::with_capacity(n);
        let dt = 1.0f32 / sr as f32;
        let two_pi = PI * 2.0;
        for _ in 0..n {
            out.push(phi.sin());
            *phi += two_pi * freq * dt;
            if *phi > two_pi { *phi -= two_pi; }
        }
        out
    }


    pub fn generate_mfsk_frame(sr: u32, params: &MfskParams, payload_bits: &str) -> Vec<f32> {
        let mut phi = 0.0f32;
        let sym_s = params.symbol_len_ms as f32 / 1000.0;
        let mut frame: Vec<f32> = Vec::new();
        // tone0 is lowest tone
        let tone0 = params.center - (params.spacing * ((params.m - 1) as f32) / 2.0);
        // preamble: repeated tone0
        let pre_tone = generate_tone(tone0, sym_s, sr);
        for _ in 0..params.preamble_repeats {
            frame.extend_from_slice(&pre_tone);
        }
        // payload: group log2(m) bits per symbol
        let bits: Vec<char> = payload_bits.chars().filter(|c| *c=='0' || *c=='1').collect();
        let bits_per_symbol = ((params.m as f32).log2().round()) as usize;
        let mut i = 0usize;
        while i < bits.len() {
            let mut val = 0usize;
            for b in 0..bits_per_symbol {
                val <<= 1;
                if i + b < bits.len() && bits[i + b] == '1' { val |= 1; }
            }
            i += bits_per_symbol;
            let tone_index = val % params.m;
            let tone_freq = tone0 + (tone_index as f32) * params.spacing;
            let t = generate_tone_with_phase(tone_freq, sym_s, sr, &mut phi);
            frame.extend_from_slice(&t);
        }
        // postamble: one symbol silence
        let n = (sym_s * sr as f32).round() as usize;
        let mut post: Vec<f32> = vec![0.0; n];
        for i in 0..n {
            let w = 0.5 * (1.0 - ((i as f32) / (n as f32) * PI).cos());
            post[i] *= w;
        }
        

        frame.extend_from_slice(&post);
        frame
    }

    pub fn generate_dpsk_frame(sr: u32, params: &DpskParams, payload_bits: &str) -> Vec<f32> {
        let sym_s = params.symbol_len_ms as f32 / 1000.0;
        let mut frame: Vec<f32> = Vec::new();
        // preamble: repeated carrier
        let pre_tone = generate_tone(params.center, sym_s, sr);
        for _ in 0..params.preamble_repeats {
            frame.extend_from_slice(&pre_tone);
        }
        // payload: phase flips per symbol (differential)
        let samples_per_sym = (sym_s * sr as f32).round() as usize;
        let dt = 1.0f32 / sr as f32;
        let two_pi = PI * 2.0;
        let mut prev_flip = false;
        for c in payload_bits.chars() {
            if c != '0' && c != '1' { continue; }
            let bit = c == '1';
            let flip = bit ^ prev_flip;
            prev_flip = flip;
            // phase offset of PI when flip true
            let phase_offset = if flip { PI } else { 0.0 };
            for n in 0..samples_per_sym {
                let t = n as f32 * dt;
                let sample = ((two_pi * params.center * t) + phase_offset).sin();
                frame.push(sample);
            }
        }
        // postamble silence
        frame.extend(std::iter::repeat(0.0f32).take(samples_per_sym));
        frame
    }

    pub fn generate_frame(sr: u32, mode: &ModeConfig, payload_bits: &str) -> Vec<f32> {
        match mode {
            ModeConfig::Mfsk(p) => generate_mfsk_frame(sr, p, payload_bits),
            ModeConfig::Dpsk(p) => generate_dpsk_frame(sr, p, payload_bits),
        }
    }

    // small helper to create template for matched-filter from mode:
    pub fn preamble_template_for(mode: &ModeConfig, sr: u32) -> Vec<f32> {
        match mode {
            ModeConfig::Mfsk(p) => {
                let tone0 = p.center - (p.spacing * ((p.m - 1) as f32) / 2.0);
                generate_tone(tone0, p.symbol_len_ms as f32 / 1000.0, sr)
            }
            ModeConfig::Dpsk(p) => {
                generate_tone(p.center, p.symbol_len_ms as f32 / 1000.0, sr)
            }
        }
    }
}

// ---------- worker: matched-filter + rms (unchanged logic) ----------
// ---- Goertzel helper (improved) ----
// Compute Goertzel energy for frequency `f` (Hz) over `samples` sampled at `sr` (Hz).
fn goertzel_energy(samples: &[f32], f: f32, sr: f32) -> f32 {
    let n = samples.len();
    if n == 0 { return 0.0; }
    // floating k approximation
    let kf = (n as f32 * f / sr);
    let omega = 2.0 * PI * kf / (n as f32);
    let coeff = 2.0 * omega.cos();
    let mut s_prev = 0.0f32;
    let mut s_prev2 = 0.0f32;
    for &x in samples {
        let s = x + coeff * s_prev - s_prev2;
        s_prev2 = s_prev;
        s_prev = s;
    }
    let power = s_prev2*s_prev2 + s_prev*s_prev - coeff*s_prev*s_prev2;
    if power.is_sign_negative() { 0.0 } else { power }
}

// ---- Windowing helper (Hann) ----
fn apply_hann_inplace(buf: &mut [f32]) {
    let n = buf.len();
    if n < 2 { return; }
    for i in 0..n {
        let w = 0.5 * (1.0 - (2.0 * PI * i as f32 / (n as f32)).cos());
        buf[i] *= w as f32;
    }
}

// ---- Worker: matched-filter + symbol sync + MFSK decode ----
fn worker_thread(shared: Arc<Shared>) {
    let mut sliding: VecDeque<f32> = VecDeque::new();
    let mut template_len = 0usize;
    let mut template: Vec<f32> = Vec::new();

    loop {
        if !shared.running.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
            continue;
        }

        // pull samples from RX ring
        let mut got_any = false;
        let mut buff = [0f32; 2048];
        let popped: usize;
        {
            let mut cons = shared.rb_consumer.lock();
            popped = cons.pop_slice(&mut buff);
        }

        if popped > 0 {
            got_any = true;
            let sumsq: f64 = buff[..popped].iter().map(|v| (*v as f64) * (*v as f64)).sum();
            let rms = ((sumsq / (popped as f64)).sqrt()) as f32;
            *shared.last_rms.lock() = rms;
            for &s in &buff[..popped] {
                sliding.push_back(s);
            }
        }

        // update preamble template if changed
        if let Some(t) = shared.preamble_template.lock().as_ref() {
            if t.len() != template_len {
                template_len = t.len();
                template = t.clone();
            }
        }

        // detection + decode
        if template_len > 0 && sliding.len() >= template_len {
            // correlate on the most recent template_len samples
            let start = sliding.len().saturating_sub(template_len);
            let mut dot = 0f32;
            let mut energy_s = 0f32;
            let mut energy_t = 0f32;
            for i in 0..template_len {
                let s_val = sliding[start + i];
                let t_val = template[i];
                dot += s_val * t_val;
                energy_s += s_val * s_val;
                energy_t += t_val * t_val;
            }
            let denom = (energy_s.sqrt() * energy_t.sqrt()).max(1e-12);
            let corr = dot / denom;

            // threshold
            if corr.abs() > 0.50 {
                let mut last_det = shared.last_detection.lock();
                let now = Instant::now();
                if last_det.map(|t| now.duration_since(t)).unwrap_or_else(|| Duration::from_secs(3600)) > Duration::from_millis(800) {
                    println!("[DETECT] matched preamble at {:?} corr={:.3}", now, corr);
                    *last_det = Some(now);

                    if let Some(mode) = shared.current_mode.lock().clone() {
                        println!("[INFO] decoding with mode: {:?}", mode);
                    } else {
                        println!("[INFO] no current_mode set (can't decode mode-specific)");
                    }

                    // Attempt MFSK decode if mode set
                    if let Some(mode_cfg) = shared.current_mode.lock().clone() {
                        match mode_cfg {
                            ModeConfig::Mfsk(params) => {
                                let sr = *shared.sample_rate.lock();
                                let sym_len = ((params.symbol_len_ms as f32 / 1000.0) * sr as f32).round() as usize;
                                if sym_len == 0 {
                                    println!("[DECODE] symbol length too small ({})", sym_len);
                                } else {
                                    let preamble_start = sliding.len().saturating_sub(template_len);
                                    let assumed_payload_start = preamble_start + template_len;

                                    // alignment search over ±half-symbol
                                    let half = sym_len / 2;
                                    let mut best_offset = 0isize;
                                    let mut best_conf = -1f32;
                                    let max_check_symbols = 8usize;
                                    let m = params.m;
                                    let center = params.center;
                                    let spacing = params.spacing;
                                    let tone0 = center - (spacing * ((m - 1) as f32) / 2.0);
                                    let sr_f = sr as f32;

                                    for off in -(half as isize)..=(half as isize) {
                                        let mut energies_sum = 0.0f32;
                                        let mut valid_blocks = 0usize;
                                        let mut payload_idx = if off < 0 {
                                            assumed_payload_start.saturating_sub((-off) as usize)
                                        } else {
                                            assumed_payload_start + (off as usize)
                                        };
                                        for _ in 0..max_check_symbols {
                                            if payload_idx + sym_len > sliding.len() { break; }
                                            let mut block = Vec::with_capacity(sym_len);
                                            for j in 0..sym_len {
                                                block.push(sliding[payload_idx + j]);
                                            }
                                            apply_hann_inplace(&mut block);
                                            let mut best_e = 0.0f32;
                                            let mut sum_e = 0.0f32;
                                            for tone_idx in 0..m {
                                                let f = tone0 + (tone_idx as f32) * spacing;
                                                let e = goertzel_energy(&block, f, sr_f);
                                                sum_e += e;
                                                if e > best_e { best_e = e; }
                                            }
                                            if sum_e > 0.0 {
                                                energies_sum += best_e / (sum_e + 1e-12);
                                                valid_blocks += 1;
                                            }
                                            payload_idx += sym_len;
                                        }
                                        if valid_blocks > 0 {
                                            let conf = energies_sum / (valid_blocks as f32);
                                            if conf > best_conf {
                                                best_conf = conf;
                                                best_offset = off;
                                            }
                                        }
                                    }

                                    if best_conf < 0.12 {
                                        println!("[DECODE] alignment search low confidence: {:.3} (increase preamble / symbol duration)", best_conf);
                                    } else {
                                        let mut payload_idx = if best_offset < 0 {
                                            assumed_payload_start.saturating_sub((-best_offset) as usize)
                                        } else {
                                            assumed_payload_start + (best_offset as usize)
                                        };

                                        let mut symbols: Vec<usize> = Vec::new();
                                        let mut confidences: Vec<f32> = Vec::new();

                                        while payload_idx + sym_len <= sliding.len() {
                                            let mut block = Vec::with_capacity(sym_len);
                                            for j in 0..sym_len {
                                                block.push(sliding[payload_idx + j]);
                                            }
                                            apply_hann_inplace(&mut block);

                                            let mut best_idx = 0usize;
                                            let mut best_e = 0.0f32;
                                            let mut sum_e = 0.0f32;
                                            for tone_idx in 0..m {
                                                let f = tone0 + (tone_idx as f32) * spacing;
                                                let e = goertzel_energy(&block, f, sr_f);
                                                sum_e += e;
                                                if e > best_e { best_e = e; best_idx = tone_idx; }
                                            }
                                            let conf = if sum_e > 0.0 { best_e / (sum_e + 1e-12) } else { 0.0 };
                                            symbols.push(best_idx);
                                            confidences.push(conf);
                                            payload_idx += sym_len;
                                            if symbols.len() > 1024 { break; }
                                        }

                                        if !symbols.is_empty() {
                                            let bits_per_symbol = ((m as f32).log2().round()) as usize;
                                            let mut bitstr = String::new();
                                            for &sym in &symbols {
                                                for b in (0..bits_per_symbol).rev() {
                                                    let bit = ((sym >> b) & 1) != 0;
                                                    bitstr.push(if bit { '1' } else { '0' });
                                                }
                                            }
                                            let avg_conf: f32 = confidences.iter().copied().sum::<f32>() / (confidences.len() as f32);
                                            println!("[DECODE] symbols(len={}): {:?}\n         bits: {}\n         avg_conf: {:.3}  best_offset: {}", symbols.len(), symbols, bitstr, avg_conf, best_offset);
                                        } else {
                                            println!("[DECODE] no full symbols available after alignment.");
                                        }
                                    }
                                }
                            }
                            _ => {
                                println!("[DECODE] current_mode not MFSK — skipping detailed decode.");
                            }
                        }
                    } else {
                        println!("[DECODE] no current_mode set — skipping decode.");
                    }
                }
            }

            // trim sliding buffer
            while sliding.len() > template_len * 6 {
                sliding.pop_front();
            }
        }

        if !got_any {
            thread::sleep(Duration::from_millis(3));
        }
    }
}


// ---------- control server & client handling ----------
fn control_server(
    shared: Arc<Shared>,
    devices: Vec<cpal::Device>,
    dev_info: DevicesInfo,
) -> Result<()> {
    let listener =
        TcpListener::bind(("127.0.0.1", CONTROL_PORT)).context("Failed to bind control socket")?;
    println!("Control server listening on 127.0.0.1:{}", CONTROL_PORT);

    // Keep streams in this thread so we don't move non-Send things around
    let mut current_input_stream: Option<cpal::Stream> = None;
    let mut current_output_stream: Option<cpal::Stream> = None;

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                if let Err(e) = handle_client(
                    s,
                    &shared,
                    &devices,
                    &mut current_input_stream,
                    &mut current_output_stream,
                    &dev_info,
                ) {
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
                    let has_in = devices.get(i).and_then(|d| d.default_input_config().ok()).is_some();
                    let has_out = devices.get(i).and_then(|d| d.default_output_config().ok()).is_some();
                    arr.push(json!({"id": i, "name": name, "has_input": has_in, "has_output": has_out}));
                }
                let resp = json!({"ok": true, "devices": arr});
                writeln!(writer, "{}", resp.to_string())?;
            }
            "list_modes" => {
                let presets = presets_map();
                let mut keys: Vec<String> = presets.keys().map(|k| k.to_string()).collect();
                keys.sort();
                let resp = json!({"ok": true, "modes": keys});
                writeln!(writer, "{}", resp.to_string())?;
            }

            "open" => {
                let input_idx = req.input.unwrap_or(usize::MAX);
                let output_idx = req.output.unwrap_or(usize::MAX);
                let sr = req.sr.unwrap_or(DEFAULT_SR);

                *cur_in = None;
                *cur_out = None;
                shared.running.store(false, Ordering::SeqCst);
                *shared.sample_rate.lock() = sr;

                // pick configs (try default, else first f32)
                fn pick_input_config(device: &cpal::Device, sr: u32) -> Option<cpal::StreamConfig> {
                    if let Ok(def) = device.default_input_config() {
                        return Some(cpal::StreamConfig {
                            channels: def.channels(),
                            sample_rate: cpal::SampleRate(sr),
                            buffer_size: cpal::BufferSize::Default,
                        });
                    }
                    if let Ok(mut iter) = device.supported_input_configs() {
                        if let Some(range) = iter.find(|c| c.sample_format() == cpal::SampleFormat::F32) {
                            return Some(cpal::StreamConfig {
                                channels: range.channels(),
                                sample_rate: cpal::SampleRate(sr),
                                buffer_size: cpal::BufferSize::Default,
                            });
                        }
                    }
                    None
                }
                fn pick_output_config(device: &cpal::Device, sr: u32) -> Option<cpal::StreamConfig> {
                    if let Ok(def) = device.default_output_config() {
                        return Some(cpal::StreamConfig {
                            channels: def.channels(),
                            sample_rate: cpal::SampleRate(sr),
                            buffer_size: cpal::BufferSize::Default,
                        });
                    }
                    if let Ok(mut iter) = device.supported_output_configs() {
                        if let Some(range) = iter.find(|c| c.sample_format() == cpal::SampleFormat::F32) {
                            return Some(cpal::StreamConfig {
                                channels: range.channels(),
                                sample_rate: cpal::SampleRate(sr),
                                buffer_size: cpal::BufferSize::Default,
                            });
                        }
                    }
                    None
                }

                // Input
                if input_idx < devices.len() {
                    let device = &devices[input_idx];
                    if let Some(cfg) = pick_input_config(device, sr) {
                        match build_input_stream(device, &cfg, shared) {
                            Ok(s) => *cur_in = Some(s),
                            Err(e) => {
                                let resp = json!({"ok":false,"error":format!("input open failed: {:?}", e)});
                                writeln!(writer, "{}", resp.to_string())?;
                                continue;
                            }
                        }
                    } else {
                        let resp = json!({"ok":false,"error":"no suitable input config for device"});
                        writeln!(writer, "{}", resp.to_string())?;
                        continue;
                    }
                }

                // Output
                if output_idx < devices.len() {
                    let device = &devices[output_idx];
                    if let Some(cfg) = pick_output_config(device, sr) {
                        match build_output_stream(device, &cfg, shared) {
                            Ok(s) => *cur_out = Some(s),
                            Err(e) => {
                                let resp = json!({"ok":false,"error":format!("output open failed: {:?}", e)});
                                writeln!(writer, "{}", resp.to_string())?;
                                continue;
                            }
                        }
                    } else {
                        let resp = json!({"ok":false,"error":"no suitable output config for device"});
                        writeln!(writer, "{}", resp.to_string())?;
                        continue;
                    }
                }

                if let Some(s) = cur_in { s.play().ok(); }
                if let Some(s) = cur_out { s.play().ok(); }

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
                    "tx_freq": *shared.tx_freq_hz.lock(),
                    "current_mode": format!("{:?}", *shared.current_mode.lock())
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
            "enqueue_wav" => {
                if let Some(path) = req.path.clone() {
                    match enqueue_wav_into_tx(&path, shared) {
                        Ok(pushed) => {
                            let resp = json!({"ok": true, "pushed": pushed});
                            writeln!(writer, "{}", resp.to_string())?;
                        }
                        Err(e) => {
                            let resp = json!({"ok": false, "error": format!("{:?}", e)});
                            writeln!(writer, "{}", resp.to_string())?;
                        }
                    }
                } else {
                    let resp = json!({"ok": false, "error": "missing path"});
                    writeln!(writer, "{}", resp.to_string())?;
                }
            }
            "send_frame" => {
                // parse mode & params from request, make ModeConfig, set shared.current_mode, generate frame
                let sr = *shared.sample_rate.lock();
                let mode_str = req.mode.as_deref().unwrap_or("mfsk");
                let payload = req.payload_bits.as_deref().unwrap_or("101100111000");

                // presets
                let presets = presets_map();

                let mode_cfg = match mode_str {
                    "mfsk" => {
                        // start from preset if provided
                        let base = if let Some(sp) = req.speed.as_deref() {
                            presets.get(sp).cloned().unwrap_or(MfskParams { m: 4, center: 1500.0, spacing: 200.0, symbol_len_ms: 80, preamble_repeats: 3 })
                        } else {
                            MfskParams { m: 4, center: 1500.0, spacing: 200.0, symbol_len_ms: 80, preamble_repeats: 3 }
                        };
                        // allow explicit overrides to replace any preset fields
                        let m = req.mfsk_m.unwrap_or(base.m);
                        let center = req.mfsk_center.unwrap_or(base.center);
                        let spacing = req.mfsk_spacing.unwrap_or(base.spacing);
                        let sym = req.symbol_len_ms.unwrap_or(base.symbol_len_ms);
                        let pre_r = req.preamble_repeats.unwrap_or(base.preamble_repeats);
                        ModeConfig::Mfsk(MfskParams { m, center, spacing, symbol_len_ms: sym, preamble_repeats: pre_r })
                    }
                    "dpsk" => {
                        // DPSK simple handling (no presets yet)
                        let center = req.mfsk_center.unwrap_or(1500.0);
                        let sym = req.symbol_len_ms.unwrap_or(80);
                        let pre_r = req.preamble_repeats.unwrap_or(3);
                        ModeConfig::Dpsk(DpskParams { center, symbol_len_ms: sym, preamble_repeats: pre_r })
                    }
                    other => {
                        // fallback to mfsk defaults if unknown
                        let base = MfskParams { m: 4, center: 1500.0, spacing: 200.0, symbol_len_ms: 80, preamble_repeats: 3 };
                        let m = req.mfsk_m.unwrap_or(base.m);
                        let center = req.mfsk_center.unwrap_or(base.center);
                        let spacing = req.mfsk_spacing.unwrap_or(base.spacing);
                        let sym = req.symbol_len_ms.unwrap_or(base.symbol_len_ms);
                        let pre_r = req.preamble_repeats.unwrap_or(base.preamble_repeats);
                        ModeConfig::Mfsk(MfskParams { m, center, spacing, symbol_len_ms: sym, preamble_repeats: pre_r })
                    }
                };

                // set current mode for worker to read later
                {
                    let mut cm = shared.current_mode.lock();
                    *cm = Some(mode_cfg.clone());
                }

                // set preamble template based on mode
                let tpl = modems::preamble_template_for(&mode_cfg, sr);
                {
                    let mut pt = shared.preamble_template.lock();
                    *pt = Some(tpl);
                }

                // generate frame and enqueue into TX FIFO
                let frame = modems::generate_frame(sr, &mode_cfg, payload);

                let mut prod = shared.tx_producer.lock();
                let mut pos = 0usize;
                let mut pushed = 0usize;
                while pos < frame.len() {
                    let n = prod.push_slice(&frame[pos..]);
                    if n == 0 {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    pos += n; pushed += n;
                }

                let resp = json!({"ok": true, "pushed": pushed, "samples": frame.len()});
                writeln!(writer, "{}", resp.to_string())?;
            }

            _ => {
                writeln!(writer, "{}", json!({"ok": false, "error": "unknown command"}).to_string())?;
            }
        }
    }

    println!("client disconnected: {:?}", peer);
    Ok(())
}

// enqueue a WAV onto TX FIFO (kept from earlier)
fn enqueue_wav_into_tx(path: &str, shared: &Arc<Shared>) -> Result<usize> {
    let mut reader = hound::WavReader::open(path).context("open wav")?;
    let spec = reader.spec();
    let channels = spec.channels as usize;

    let mut samples_mono: Vec<f32> = Vec::new();

    if spec.sample_format == hound::SampleFormat::Float && spec.bits_per_sample == 32 {
        let mut iter = reader.samples::<f32>();
        loop {
            let mut frame = Vec::with_capacity(channels);
            for _ in 0..channels {
                match iter.next() {
                    Some(Ok(s)) => frame.push(s),
                    Some(Err(_)) => return Err(anyhow::anyhow!("wav read error")),
                    None => break,
                }
            }
            if frame.is_empty() { break; }
            let avg = frame.iter().copied().sum::<f32>() / (frame.len() as f32);
            samples_mono.push(avg);
        }
    } else {
        let mut iter = reader.samples::<i16>();
        loop {
            let mut frame = Vec::with_capacity(channels);
            for _ in 0..channels {
                match iter.next() {
                    Some(Ok(s)) => frame.push(s),
                    Some(Err(_)) => return Err(anyhow::anyhow!("wav read error")),
                    None => break,
                }
            }
            if frame.is_empty() { break; }
            let avg_i: i32 = frame.iter().map(|&v| v as i32).sum();
            let avg_f = (avg_i as f32) / (32768.0f32 * (frame.len() as f32));
            samples_mono.push(avg_f);
        }
    }

    // push into TX ring
    let mut prod = shared.tx_producer.lock();
    let mut pushed = 0usize;
    let mut pos = 0usize;
    while pos < samples_mono.len() {
        let n = prod.push_slice(&samples_mono[pos..]);
        if n == 0 {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        pos += n;
        pushed += n;
    }
    Ok(pushed)
}

// ---------- build_input_stream & build_output_stream ----------
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
            if channels == 1 {
                let mut p = producer_arc.lock();
                let _ = p.push_slice(data);
            } else {
                let mut tmp = Vec::with_capacity(data.len() / channels);
                for frame in data.chunks(channels) {
                    let sum: f32 = frame.iter().copied().sum();
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

    let phase = Arc::new(Mutex::new(0f32));
    let tx_consumer_arc = shared.tx_consumer.clone();

    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
            let frames = data.len() / channels;
            let mut idx = 0usize;

            // fill from TX FIFO per frame, duplicating mono into all channels
            let mut cons = tx_consumer_arc.lock();
            for _frame_i in 0..frames {
                if let Some(samp) = cons.pop() {
                    for ch in 0..channels {
                        data[idx + ch] = samp;
                    }
                    idx += channels;
                } else {
                    break;
                }
            }
            drop(cons);

            if idx < data.len() {
                let enabled = tx_enabled.load(Ordering::SeqCst);
                let freq = *tx_freq.lock();
                if enabled {
                    let mut ph = phase.lock();
                    let two_pi = PI * 2.0;
                    while idx < data.len() {
                        let sample = (*ph).sin() * 0.3;
                        for ch in 0..channels {
                            data[idx + ch] = sample;
                        }
                        idx += channels;
                        *ph += two_pi * freq / sample_rate;
                        if *ph > two_pi { *ph -= two_pi; }
                    }
                } else {
                    for slot in data[idx..].iter_mut() {
                        *slot = 0.0;
                    }
                }
            }
        },
        err_fn,
        None,
    )?;
    Ok(stream)
}


fn main() {
    println!("Hush starting...");

    // enumerate audio devices
    let host = cpal::default_host();
    let devices_list: Vec<cpal::Device> = match host.devices() {
        Ok(devs) => devs.collect(),
        Err(e) => {
            eprintln!("Failed to enumerate audio devices: {:?}", e);
            return;
        }
    };

    // build device names list for UI/control
    let mut names = Vec::with_capacity(devices_list.len());
    for (i, d) in devices_list.iter().enumerate() {
        let name = d.name().unwrap_or_else(|_| "<name error>".to_string());
        println!("{}: {}", i, name);
        names.push(name);
    }
    let dev_info = DevicesInfo { names };

    // create shared state
    let shared = Arc::new(Shared::new());

    // spawn worker thread (consumes RX ring, computes RMS, runs matched filter)
    {
        let s = shared.clone();
        thread::spawn(move || worker_thread(s));
    }

    // run control server in this thread (owns streams)
    if let Err(e) = control_server(shared, devices_list, dev_info) {
        eprintln!("Control server exited with error: {:?}", e);
    }

    println!("Hush exiting.");
}
