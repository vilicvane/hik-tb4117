// Latency benchmark: stream startup, frame cadence, 2046 radiometric
// capture, 2047 point query.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use thermal_camera::Device;

fn main() -> Result<()> {
    let t0 = Instant::now();
    let dev = Arc::new(Device::open()?);
    println!("open: {:?}", t0.elapsed());

    let t = Instant::now();
    dev.start_stream()?;
    println!("start_stream: {:?}", t.elapsed());

    // Measure frame cadence on the stream.
    let stop = Arc::new(AtomicBool::new(false));
    let pump = {
        let dev = dev.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            let mut n = 0;
            let start = Instant::now();
            let mut first = None;
            for frame in dev.frames() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                if frame.is_ok() {
                    if first.is_none() {
                        first = Some(start.elapsed());
                    }
                    n += 1;
                    if n >= 60 {
                        break;
                    }
                }
            }
            let d = start.elapsed();
            println!("stream: first frame {:?}, {} frames in {:?} ({:.1} fps)", first.unwrap_or_default(), n, d, n as f64 / d.as_secs_f64());
        })
    };
    thread::sleep(Duration::from_secs(2));

    // 2046 radiometric capture (JPEG + full 120x160 temp matrix).
    for _ in 0..3 {
        let t = Instant::now();
        let cap = dev.capture_radiometric()?;
        println!(
            "capture_radiometric (2046): {:?} total, jpeg {} B + {} temps",
            t.elapsed(),
            cap.jpeg.len(),
            cap.temps.len()
        );
    }

    // 2047 point query.
    for _ in 0..3 {
        let t = Instant::now();
        let temp = dev.pixel_temperature(120, 160)?;
        println!("pixel_temperature (2047): {:?} -> {temp:.1} C", t.elapsed());
    }

    stop.store(true, Ordering::Relaxed);
    let _ = pump.join();
    Ok(())
}
