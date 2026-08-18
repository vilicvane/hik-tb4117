// record: sample once per minute — stream frame (240x320 with OSD),
// radiometric JPEG (120x160) and the full temperature matrix (CSV) —
// into captures/record/. Body-temp compensation is disabled while running
// and restored on clean exit (Ctrl+C). Ctrl+C stops the recording.

use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use thermal_camera::Device;

fn timestamp() -> String {
    let out = Command::new("date").arg("+%Y%m%d-%H%M%S").output().expect("date");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn main() -> Result<()> {
    let out_dir = "captures/record";
    std::fs::create_dir_all(out_dir)?;

    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        ctrlc::set_handler(move || running.store(false, Ordering::SeqCst))?;
    }

    let dev = Arc::new(Device::open()?);
    dev.start_stream()?;

    // Stream pump: keep the latest 240x320 frame for snapshotting.
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
                if let Ok(jpeg) = frame {
                    *latest.lock().unwrap() = jpeg;
                }
            }
        })
    };
    thread::sleep(Duration::from_secs(2)); // drain the stale FIFO burst

    // Compensation off while recording.
    let comp_orig = match dev.body_temp_compensation() {
        Ok(orig) if orig.get(1) == Some(&1) => {
            dev.set_body_temp_compensation(false)?;
            println!("body-temp compensation off (restored on exit)");
            Some(orig)
        }
        Ok(_) => {
            println!("body-temp compensation already off");
            None
        }
        Err(e) => {
            eprintln!("compensation query failed, leaving as-is: {e:#}");
            None
        }
    };

    println!("recording to {out_dir}/, one sample per minute; Ctrl+C to stop");
    let mut failures = 0u32;
    while running.load(Ordering::Relaxed) {
        let ts = timestamp();
        match dev.capture_radiometric() {
            Ok(cap) => {
                std::fs::write(format!("{out_dir}/{ts}-rad.jpg"), &cap.jpeg)?;
                let mut s = String::new();
                for row in cap.temps.chunks(cap.width) {
                    let line: Vec<String> = row.iter().map(|t| format!("{t:.2}")).collect();
                    s.push_str(&line.join(","));
                    s.push('\n');
                }
                std::fs::write(format!("{out_dir}/{ts}.csv"), &s)?;
                let osd = latest.lock().unwrap().clone();
                if !osd.is_empty() {
                    std::fs::write(format!("{out_dir}/{ts}.jpg"), &osd)?;
                }
                let (t, x, y) = cap.max_temp().unwrap();
                println!("{ts}: max {t:.1} C at display ({}, {})", x * 2, y * 2);
                failures = 0;
            }
            Err(e) => {
                failures += 1;
                eprintln!("{ts}: capture failed ({failures}): {e:#}");
                if failures > 5 {
                    eprintln!("too many consecutive failures, giving up");
                    break;
                }
            }
        }

        // Sleep until the next minute boundary (in 0.2s ticks so Ctrl+C is
        // responsive).
        let next = Instant::now() + Duration::from_secs(60);
        while running.load(Ordering::Relaxed) && Instant::now() < next {
            thread::sleep(Duration::from_millis(200));
        }
    }

    if let Some(orig) = comp_orig {
        dev.restore_body_temp_compensation(&orig)?;
        println!("compensation config restored");
    }
    stop.store(true, Ordering::Relaxed);
    let _ = pump.join();
    Ok(())
}
