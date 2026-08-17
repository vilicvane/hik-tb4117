// Experiment with the 2047 ROI_MAX_TEMPERATURE_SEARCH DoubleGet handshake.
// Phase A is confirmed working (channel=1): SET req, GET_LEN->5, GET->5 bytes
// [01, len32]. This file explores how Phase B (read the actual result) works.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use thermal_camera::Device;

fn build_req(channel: u8, with_region: bool) -> Vec<u8> {
    let mut req = vec![0u8; 234];
    req[0] = channel;
    req[8..10].copy_from_slice(&2026u16.to_le_bytes());
    req[13] = if with_region { 1 } else { 0 };
    if with_region {
        req[14] = 1;
        req[15] = 1;
        req[24..28].copy_from_slice(&240u32.to_le_bytes());
        req[28..32].copy_from_slice(&320u32.to_le_bytes());
        req[32..36].copy_from_slice(&100u32.to_le_bytes());
    }
    req
}

fn phase_a(dev: &Device, req: &[u8]) -> Option<usize> {
    if let Err(e) = dev.select_command(3, 10) {
        println!("sel5 failed: {e:#}");
        return None;
    }
    if let Err(e) = dev.xu_set(3, req) {
        println!("phase A SET failed: {e} (err={:?})", dev.xu_last_error());
        return None;
    }
    thread::sleep(Duration::from_millis(50));
    match dev.xu_get_len(3) {
        Ok(5) => {}
        other => {
            println!("phase A GET_LEN unexpected: {other:?} (err={:?})", dev.xu_last_error());
            return None;
        }
    }
    let mut head = [0u8; 5];
    match dev.xu_get(3, &mut head) {
        Ok(_) => Some(u32::from_le_bytes(head[1..5].try_into().unwrap()) as usize),
        Err(e) => {
            println!("phase A GET failed: {e} (err={:?})", dev.xu_last_error());
            None
        }
    }
}

fn report(out: &[u8]) {
    println!("result {} bytes: {}", out.len(), hex(&out[..out.len().min(64)]));
    if out.len() >= 26 {
        let u32at = |o: usize| u32::from_le_bytes(out[o..o + 4].try_into().unwrap());
        let max_t = u32at(1);
        println!("  max temp raw={max_t} (x100: {:.2} C, x10: {:.1} C)", max_t as f64 / 100.0, max_t as f64 / 10.0);
        println!("  thermal max point: ({}, {}), visible: ({}, {})", u32at(13), u32at(17), u32at(5), u32at(9));
        println!("  roi num: {}, jpeg len: {}", out[21], u32at(22));
        for i in 0..(out[21] as usize).min(10) {
            let base = 26 + i * 21;
            if base + 21 <= out.len() {
                let r = &out[base..base + 21];
                let ru32 = |o: usize| u32::from_le_bytes(r[o..o + 4].try_into().unwrap());
                println!("  region {}: max raw={} (x100: {:.2} C) at thermal ({}, {})", r[0], ru32(1), ru32(1) as f64 / 100.0, ru32(9), ru32(13));
            }
        }
    }
}

fn main() -> Result<()> {
    let dev = Arc::new(Device::open()?);
    dev.start_stream()?;
    let stop = Arc::new(AtomicBool::new(false));
    let pump = {
        let dev = dev.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            for frame in dev.frames() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let _ = frame;
            }
        })
    };
    thread::sleep(Duration::from_secs(2));

    let req = build_req(1, true);

    // Variant 1: no second SET, direct GET of `total` bytes.
    println!("=== V1: phase B direct GET (no SET) ===");
    if let Some(total) = phase_a(&dev, &req) {
        let mut buf = vec![0u8; total];
        match dev.xu_get(3, &mut buf) {
            Ok(n) => {
                buf.truncate(n);
                report(&buf);
            }
            Err(e) => println!("GET failed: {e} (err={:?})", dev.xu_last_error()),
        }
    }

    // Variant 2: second SET, then GET exactly `total` bytes.
    println!("=== V2: SET + GET exact ===");
    if let Some(total) = phase_a(&dev, &req) {
        if let Err(e) = dev.xu_set(3, &req) {
            println!("phase B SET failed: {e} (err={:?})", dev.xu_last_error());
        } else {
            thread::sleep(Duration::from_millis(50));
            let mut buf = vec![0u8; total];
            match dev.xu_get(3, &mut buf) {
                Ok(n) => {
                    buf.truncate(n);
                    report(&buf);
                }
                Err(e) => println!("GET failed: {e} (err={:?})", dev.xu_last_error()),
            }
        }
    }

    // Variant 3: second SET, then GET_LEN, then GET.
    println!("=== V3: SET + GET_LEN + GET ===");
    if let Some(total) = phase_a(&dev, &req) {
        if let Err(e) = dev.xu_set(3, &req) {
            println!("phase B SET failed: {e} (err={:?})", dev.xu_last_error());
        } else {
            thread::sleep(Duration::from_millis(50));
            match dev.xu_get_len(3) {
                Ok(l) => {
                    println!("GET_LEN -> {l}");
                    let mut buf = vec![0u8; l.min(4096) as usize];
                    match dev.xu_get(3, &mut buf) {
                        Ok(n) => {
                            buf.truncate(n);
                            report(&buf);
                        }
                        Err(e) => println!("GET failed: {e} (err={:?})", dev.xu_last_error()),
                    }
                }
                Err(e) => println!("GET_LEN failed: {e} (err={:?})", dev.xu_last_error()),
            }
            let _ = total;
        }
    }

    // Variant 4: phase A again but read head with GET_LEN-sized read; then
    // direct GET with 5-byte-header-tolerant accumulation.
    println!("=== V4: SET + GET with header stripping ===");
    if let Some(total) = phase_a(&dev, &req) {
        if let Err(e) = dev.xu_set(3, &req[..5].to_vec()) {
            println!("phase B short SET failed: {e} (err={:?})", dev.xu_last_error());
        }
        thread::sleep(Duration::from_millis(50));
        let mut out = Vec::new();
        while out.len() < total {
            let mut chunk = vec![0u8; total - out.len()];
            match dev.xu_get(3, &mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    chunk.truncate(n);
                    if chunk.len() > 5 && chunk[0] == 0x02 {
                        out.extend_from_slice(&chunk[5..]);
                    } else {
                        out.extend_from_slice(&chunk);
                    }
                }
                Err(e) => {
                    println!("GET failed: {e} (err={:?})", dev.xu_last_error());
                    break;
                }
            }
        }
        out.truncate(total);
        if !out.is_empty() {
            report(&out);
        }
    }

    stop.store(true, Ordering::Relaxed);
    let _ = pump.join();
    Ok(())
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}
