// 2046 USB_JPEGPIC_WITH_APPENDDATA full capture: phase A, then chunked phase B
// (512-byte GETs, strip 5-byte chunk headers {0x02, u32 seq}), reassemble.

use std::fs::File;
use std::io::Write;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use thermal_camera::Device;

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
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

    let req = {
        let mut r = vec![0u8; 13];
        r[0] = 1;
        r[8..10].copy_from_slice(&2026u16.to_le_bytes());
        r
    };
    dev.select_command(3, 9)?;
    dev.xu_set(3, &req)?;
    thread::sleep(Duration::from_millis(50));
    let hl = dev.xu_get_len(3)?;
    let mut head = vec![0u8; hl as usize];
    dev.xu_get(3, &mut head)?;
    let total = u32::from_le_bytes(head[1..5].try_into().unwrap()) as usize;
    println!("total {total}");

    let mut out: Vec<u8> = Vec::with_capacity(total);
    let mut last_seq = 0u32;
    while out.len() < total {
        let mut chunk = vec![0u8; 512];
        match dev.xu_get(3, &mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                chunk.truncate(n);
                if chunk.len() >= 5 && chunk[0] == 0x02 {
                    let seq = u32::from_le_bytes(chunk[1..5].try_into().unwrap());
                    if seq != last_seq + 1 && seq != 1 {
                        println!("seq jump: {last_seq} -> {seq}");
                    }
                    last_seq = seq;
                    out.extend_from_slice(&chunk[5..]);
                } else {
                    out.extend_from_slice(&chunk);
                }
            }
            Err(e) => {
                println!("GET failed at {}: {e} (err={:?})", out.len(), dev.xu_last_error());
                break;
            }
        }
    }
    println!("reassembled {} / {total}", out.len());
    std::fs::write("/tmp/2046.bin", &out)?;

    if out.len() >= 13 {
        let u32at = |o: usize| u32::from_le_bytes(out[o..o + 4].try_into().unwrap());
        println!("head: {}", hex(&out[..32]));
        let jpeg_len = u32at(1) as usize;
        println!("tag={} jpeg_len={jpeg_len} f1={} f2={} f3={}", out[0], u32at(5), u32at(9), u32at(13));
        if let Some(pos) = out.windows(2).position(|w| w == [0xff, 0xd8]) {
            println!("jpeg SOI at offset {pos}");
            if pos + jpeg_len <= out.len() {
                File::create("/tmp/2046.jpg")?.write_all(&out[pos..pos + jpeg_len])?;
                println!("saved /tmp/2046.jpg ({jpeg_len} bytes)");
            }
        }
        // Assume trailing f32 array; print a few values from the end region.
        let tail_start = out.len() - 76800;
        if tail_start > 0 {
            let t = &out[tail_start..];
            let f32at = |o: usize| f32::from_le_bytes(t[o..o + 4].try_into().unwrap());
            println!(
                "tail-as-f32 samples: {:.2} {:.2} {:.2} ... {:.2} {:.2}",
                f32at(0),
                f32at(4 * 100),
                f32at(4 * 5000),
                f32at(4 * 10000),
                f32at(4 * 19199)
            );
        }
    }

    stop.store(true, Ordering::Relaxed);
    let _ = pump.join();
    Ok(())
}
