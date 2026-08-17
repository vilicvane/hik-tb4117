// thermal-camera: capture frames and query temperatures from the HIK
// TB-4117-3/S module (2bdf:0101).
//
// usage: thermal-camera [frames=N] [out=DIR] [point=X,Y]... [roi=X,Y,W,H]...
//                        [temps=FILE]
//   frames/out: save N streaming JPEG frames (240x320, with OSD) into DIR
//               (default: 3 frames into ./captures; frames=0 to skip)
//   point:      temperature of a pixel, in 240x320 display coordinates
//   roi:        max temperature of a rectangle, in 240x320 display coordinates
//   temps:      also dump the full 120x160 temperature matrix as CSV
//
// Point/roi/temps use one 2046 radiometric capture (120x160 f32 °C matrix,
// values identical to the device OSD). Display coords map by dividing by 2.
//
// Example: thermal-camera frames=1 out=/tmp point=120,160 roi=0,0,240,320

use std::fs::File;
use std::io::Write;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use thermal_camera::Device;

fn parse_nums(arg: &str, prefix: &str, n: usize) -> Result<Option<Vec<u32>>> {
    let Some(v) = arg.strip_prefix(prefix) else {
        return Ok(None);
    };
    let nums: std::result::Result<Vec<u32>, _> = v.split(',').map(|s| s.parse()).collect();
    let nums = nums.with_context(|| format!("bad {prefix} value {v:?}"))?;
    if nums.len() != n {
        bail!("{prefix} expects {n} comma-separated numbers, got {v:?}");
    }
    Ok(Some(nums))
}

fn main() -> Result<()> {
    let mut frames_wanted = 3usize;
    let mut out_dir = "captures".to_string();
    let mut points: Vec<(u32, u32)> = Vec::new();
    let mut rois: Vec<(u32, u32, u32, u32)> = Vec::new();
    let mut temps_file: Option<String> = None;
    for arg in std::env::args().skip(1) {
        if let Some(v) = arg.strip_prefix("frames=") {
            frames_wanted = v.parse()?;
        } else if let Some(v) = arg.strip_prefix("out=") {
            out_dir = v.to_string();
        } else if let Some(v) = arg.strip_prefix("temps=") {
            temps_file = Some(v.to_string());
        } else if let Some(n) = parse_nums(&arg, "point=", 2)? {
            points.push((n[0], n[1]));
        } else if let Some(n) = parse_nums(&arg, "roi=", 4)? {
            rois.push((n[0], n[1], n[2], n[3]));
        } else {
            bail!("unknown argument {arg:?}");
        }
    }
    let need_radiometric = !points.is_empty() || !rois.is_empty() || temps_file.is_some();
    if frames_wanted > 0 {
        std::fs::create_dir_all(&out_dir)?;
    }

    let dev = Arc::new(Device::open()?);
    dev.start_stream()?;

    // XU commands only answer while streaming, so keep the stream pumped in
    // the background.
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));
    let saved = Arc::new(Mutex::new(0usize));
    let pump = {
        let dev = dev.clone();
        let stop = stop.clone();
        let ready = ready.clone();
        let saved = saved.clone();
        let out_dir = out_dir.clone();
        thread::spawn(move || {
            let start = Instant::now();
            for frame in dev.frames() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(jpeg) = frame else { continue };
                ready.store(true, Ordering::Relaxed);
                let i = *saved.lock().unwrap();
                if i < frames_wanted {
                    let path = format!("{out_dir}/frame_{i:03}.jpg");
                    if File::create(&path).and_then(|mut f| f.write_all(&jpeg)).is_ok() {
                        let fps = (i + 1) as f64 / start.elapsed().as_secs_f64();
                        println!("{path}: {} bytes, {fps:.1} fps avg", jpeg.len());
                        *saved.lock().unwrap() = i + 1;
                    }
                }
            }
        })
    };

    // XU transactions fail before the stream is flowing; wait (up to 5s) for
    // the first frame instead of a fixed delay.
    let t_wait = Instant::now();
    while !ready.load(Ordering::Relaxed) {
        if t_wait.elapsed() > Duration::from_secs(5) {
            bail!("stream did not produce frames within 5s");
        }
        thread::sleep(Duration::from_millis(10));
    }

    let mut result = Ok(());
    if need_radiometric {
        result = run_queries(&dev, &points, &rois, temps_file.as_deref());
    }

    // Wait until the requested frames are saved (they may lag the queries).
    for _ in 0..50 {
        if *saved.lock().unwrap() >= frames_wanted {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    stop.store(true, Ordering::Relaxed);
    let _ = pump.join();
    result
}

fn run_queries(
    dev: &Device,
    points: &[(u32, u32)],
    rois: &[(u32, u32, u32, u32)],
    temps_file: Option<&str>,
) -> Result<()> {
    let cap = dev.capture_radiometric()?;
    println!("radiometric capture: {}x{} matrix", cap.width, cap.height);

    // Query coordinates arrive in 240x320 display space; the matrix is half
    // that size.
    let to_map = |dx: u32, dy: u32| ((dx / 2) as usize, (dy / 2) as usize);

    if let Some((t, x, y)) = cap.max_temp() {
        println!("frame max: {t:.1} C at display ({}, {})", x * 2, y * 2);
    }
    for &(dx, dy) in points {
        let (x, y) = to_map(dx, dy);
        match cap.temp_at(x, y) {
            Some(t) => println!("point ({dx}, {dy}): {t:.1} C"),
            None => println!("point ({dx}, {dy}): out of range"),
        }
    }
    for &(dx, dy, dw, dh) in rois {
        let (x0, y0) = to_map(dx, dy);
        let (x1, y1) = to_map(dx + dw, dy + dh);
        let mut best: Option<(f32, usize, usize)> = None;
        for y in y0..y1.min(cap.height) {
            for x in x0..x1.min(cap.width) {
                let t = cap.temps[y * cap.width + x];
                if best.map_or(true, |(bt, _, _)| t > bt) {
                    best = Some((t, x, y));
                }
            }
        }
        match best {
            Some((t, x, y)) => {
                println!("roi ({dx}, {dy}, {dw}x{dh}): max {t:.1} C at display ({}, {})", x * 2, y * 2)
            }
            None => println!("roi ({dx}, {dy}, {dw}x{dh}): empty/out of range"),
        }
    }

    if let Some(path) = temps_file {
        let mut s = String::new();
        for row in cap.temps.chunks(cap.width) {
            let line: Vec<String> = row.iter().map(|t| format!("{t:.2}")).collect();
            s.push_str(&line.join(","));
            s.push('\n');
        }
        std::fs::write(path, s)?;
        println!("temperature matrix saved to {path}");
    }
    Ok(())
}
