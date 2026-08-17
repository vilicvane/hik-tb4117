// Verified 2047 ROI_MAX_TEMPERATURE_SEARCH handshake (channel=1):
//   select_command(3,10) -> SET_CUR sel3 (234B request) -> GET_LEN sel3 -> 5
//   -> GET_CUR sel3 -> [01, total_len u32] -> GET_CUR sel3 (total_len bytes)
// Phase B is a bare GET, no second SET.
//
// Usage: roi-probe [x y w h dist]...   (up to 10 regions; default: full frame)
// Saves the latest video frame to /tmp/roi-probe.jpg for OSD comparison.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use thermal_camera::Device;

fn build_req(channel: u8, regions: &[(u32, u32, u32, u32, u32)]) -> Vec<u8> {
    let mut req = vec![0u8; 234];
    req[0] = channel;
    req[8..10].copy_from_slice(&2026u16.to_le_bytes());
    req[13] = regions.len() as u8;
    for (i, &(x, y, w, h, dist)) in regions.iter().enumerate() {
        let b = 14 + i * 22;
        req[b] = (i + 1) as u8; // byROIRegionID
        req[b + 1] = 1; // enabled
        req[b + 2..b + 6].copy_from_slice(&x.to_le_bytes());
        req[b + 6..b + 10].copy_from_slice(&y.to_le_bytes());
        req[b + 10..b + 14].copy_from_slice(&w.to_le_bytes());
        req[b + 14..b + 18].copy_from_slice(&h.to_le_bytes());
        req[b + 18..b + 22].copy_from_slice(&dist.to_le_bytes());
    }
    req
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

fn dump_block(tag: &str, b: &[u8]) {
    if b.len() < 21 {
        println!("  {tag}: short block ({}B): {}", b.len(), hex(b));
        return;
    }
    let u32at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let t = u32at(1);
    println!(
        "  {tag}: id={} temp_raw={} (x10: {:.1} C, x100: {:.2} C) v1={} v2={} v3={} v4={}",
        b[0],
        t,
        t as f64 / 10.0,
        t as f64 / 100.0,
        u32at(5),
        u32at(9),
        u32at(13),
        u32at(17)
    );
}

fn main() -> Result<()> {
    let mut regions: Vec<(u32, u32, u32, u32, u32)> = Vec::new();
    let args: Vec<String> = std::env::args().skip(1).collect();
    for chunk in args.chunks(5) {
        if chunk.len() == 5 {
            let v: Vec<u32> = chunk.iter().map(|s| s.parse().unwrap()).collect();
            regions.push((v[0], v[1], v[2], v[3], v[4]));
        }
    }
    if regions.is_empty() {
        regions.push((1, 1, 240, 320, 100));
    }
    println!("regions: {regions:?}");

    let dev = Arc::new(Device::open()?);
    dev.start_stream()?;
    let stop = Arc::new(AtomicBool::new(false));
    let latest = Arc::new(Mutex::new(Vec::<u8>::new()));
    let pump = {
        let dev = dev.clone();
        let stop = stop.clone();
        let latest = latest.clone();
        thread::spawn(move || {
            for frame in dev.frames() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(frame) = frame {
                    *latest.lock().unwrap() = frame;
                }
            }
        })
    };
    thread::sleep(Duration::from_secs(2));

    let req = build_req(1, &regions);
    dev.select_command(3, 10)?;
    dev.xu_set(3, &req)?;
    thread::sleep(Duration::from_millis(50));
    let len = dev.xu_get_len(3)?;
    println!("phase A GET_LEN -> {len}");
    let mut head = vec![0u8; len as usize];
    dev.xu_get(3, &mut head)?;
    println!("phase A head: {}", hex(&head));
    let total = u32::from_le_bytes(head[1..5].try_into().unwrap()) as usize;

    let mut buf = vec![0u8; total];
    let n = dev.xu_get(3, &mut buf)?;
    buf.truncate(n);
    println!("phase B got {n} bytes:");
    for row in buf.chunks(16) {
        println!("  {}", hex(row));
    }

    // Hypothesis parse: [0]=tag, [1..5]=count, [5..26]=global block,
    // [26]=tag2, [27..31]=jpeg len, [31..]=region blocks (21B each).
    if buf.len() >= 52 {
        let count = u32::from_le_bytes(buf[1..5].try_into().unwrap());
        let jpeg_len = u32::from_le_bytes(buf[27..31].try_into().unwrap());
        println!("hdr: tag0={} count={count} tag26={} jpeg_len={jpeg_len}", buf[0], buf[26]);
        dump_block("global", &buf[5..26]);
        for i in 0..regions.len() {
            let b = 31 + i * 21;
            if b + 21 <= buf.len() {
                dump_block(&format!("region[{i}]"), &buf[b..b + 21]);
            }
        }
    }

    std::fs::write("/tmp/roi-probe.jpg", &*latest.lock().unwrap())?;
    println!("frame saved to /tmp/roi-probe.jpg");

    stop.store(true, Ordering::Relaxed);
    let _ = pump.join();
    Ok(())
}
