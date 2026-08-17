// Live protocol test: query device info, thermometry params, and ROI max
// temperature from the TB-4117-3/S, following docs/hcusb-uvc-protocol.md.

use std::fs::File;
use std::io::Write;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use thermal_camera::Device;

fn main() -> Result<()> {
    let dev = Arc::new(Device::open()?);
    dev.start_stream()?;
    println!("stream started");

    // Keep draining the bulk stream in the background; the device only answers
    // XU commands while streaming. Save the first JPEG frame for OSD
    // cross-checking.
    let stop = Arc::new(AtomicBool::new(false));
    let pump = {
        let dev = dev.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            let mut saved = false;
            for frame in dev.frames() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(jpeg) = frame {
                    if !saved {
                        let _ = File::create("captures/therm-check.jpg").map(|mut f| f.write_all(&jpeg));
                        println!("saved captures/therm-check.jpg ({} bytes)", jpeg.len());
                        saved = true;
                    }
                }
            }
        })
    };

    // Let the stream warm up.
    thread::sleep(Duration::from_secs(2));

    let result = queries(&dev);

    stop.store(true, Ordering::Relaxed);
    let _ = pump.join();
    result
}

fn queries(dev: &Device) -> Result<()> {
    // 2011 USB_SYSTEM_DEVICE_INFO: group 1, sub 1.
    match dev.simple_get(1, 1) {
        Ok(info) => {
            println!("[2011 DEVICE_INFO] {} bytes", info.len());
            for (i, name) in ["serial?", "type?", "id?", "hw?", "fw?", "hw2?"].iter().enumerate() {
                let start = 1 + i * 0x40; // GET responses start at payload[1]
                if start + 0x40 <= info.len() {
                    let s: String = info[start..start + 0x40]
                        .iter()
                        .take_while(|&&b| b != 0)
                        .map(|&b| b as char)
                        .collect();
                    println!("  slot{i} ({name}): {s:?}");
                }
            }
        }
        Err(e) => println!("[2011] failed: {e:#} (last_err={:?})", dev.xu_last_error()),
    }

    // 2030 USB_THERMOMETRY_BASIC_PARAM: group 3, sub 1.
    match dev.simple_get(3, 1) {
        Ok(p) => {
            println!("[2030 BASIC_PARAM] {} bytes", p.len());
            if p.len() >= 47 {
                let u32at = |o: usize| u32::from_le_bytes(p[o..o + 4].try_into().unwrap());
                println!("  enabled={} unit={} range={}", p[1], p[5], p[6]);
                println!("  emissivity={} (x100) distance={} unit={}", u32at(16), u32at(21), p[20]);
                println!("  alert={} alarm={} (x100?)", u32at(32), u32at(36));
            } else {
                println!("  raw: {}", hex(&p));
            }
        }
        Err(e) => println!("[2030] failed: {e:#} (last_err={:?})", dev.xu_last_error()),
    }

    // 2032 USB_THERMOMETRY_MODE: group 3, sub 2.
    match dev.simple_get(3, 2) {
        Ok(p) => println!("[2032 MODE] {} bytes: {}", p.len(), hex(&p)),
        Err(e) => println!("[2032] failed: {e:#} (last_err={:?})", dev.xu_last_error()),
    }

    // 2047 ROI_MAX_TEMPERATURE_SEARCH: group 3, sub 10, DoubleGet.
    let mut req = vec![0u8; 234];
    req[0] = 2; // byChannelID: thermal
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    // Fill the timestamp fields (approximate local time; content likely irrelevant).
    let secs = now % 60;
    req[1..3].copy_from_slice(&0u16.to_le_bytes()); // ms
    req[3] = secs as u8;
    req[4] = ((now / 60) % 60) as u8;
    req[5] = ((now / 3600) % 24) as u8;
    req[6] = 17;
    req[7] = 8;
    req[8..10].copy_from_slice(&2026u16.to_le_bytes());
    req[10] = 0; // no JPEG back
    req[11] = 0; // no max-temp overlay
    req[12] = 0; // no regions overlay
    req[13] = 1; // one ROI region
    // Region 1: full frame 240x320, distance 100cm.
    req[14] = 1; // region id
    req[15] = 1; // enabled
    req[16..20].copy_from_slice(&0u32.to_le_bytes()); // X
    req[20..24].copy_from_slice(&0u32.to_le_bytes()); // Y
    req[24..28].copy_from_slice(&240u32.to_le_bytes()); // W
    req[28..32].copy_from_slice(&320u32.to_le_bytes()); // H
    req[32..36].copy_from_slice(&100u32.to_le_bytes()); // distance

    match dev.double_get(3, 10, &req) {
        Ok(resp) => {
            println!("[2047 ROI] {} bytes", resp.len());
            println!("  raw head: {}", hex(&resp[..resp.len().min(64)]));
            if resp.len() >= 26 {
                let u32at = |o: usize| u32::from_le_bytes(resp[o..o + 4].try_into().unwrap());
                let max_t = u32at(1);
                println!("  dwMaxP2PTemperature raw={max_t}  -> x100: {:.2} C, x10: {:.1} C", max_t as f64 / 100.0, max_t as f64 / 10.0);
                println!("  thermal max point: ({}, {}), visible: ({}, {})", u32at(13), u32at(17), u32at(5), u32at(9));
                println!("  roi regions: {}, jpeg len: {}", resp[21], u32at(22));
                if resp.len() >= 26 + 21 {
                    let r = &resp[26..26 + 21];
                    let ru32 = |o: usize| u32::from_le_bytes(r[o..o + 4].try_into().unwrap());
                    println!("  region {}: max temp raw={} ({:.2} C @x100) at thermal ({}, {})", r[0], ru32(1), ru32(1) as f64 / 100.0, ru32(9), ru32(13));
                }
            }
        }
        Err(e) => println!("[2047] failed: {e:#} (last_err={:?})", dev.xu_last_error()),
    }

    Ok(())
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}
