// A/B/A test of 2044/2045 USB_BODYTEMP_COMPENSATION (group 3 sub 8):
// capture ON -> disable -> capture OFF (with OSD frame) -> restore -> capture ON.

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

fn face_stats(dev: &Device, label: &str) -> Result<()> {
    let cap = dev.capture_radiometric()?;
    let region = |x0: usize, y0: usize, x1: usize, y1: usize| {
        let mut best = f32::MIN;
        let (mut bx, mut by) = (0, 0);
        let mut sum = 0.0f64;
        let mut n = 0usize;
        for y in y0..y1.min(cap.height) {
            for x in x0..x1.min(cap.width) {
                let t = cap.temps[y * cap.width + x];
                sum += t as f64;
                n += 1;
                if t > best {
                    best = t;
                    bx = x;
                    by = y;
                }
            }
        }
        (best, bx, by, sum / n as f64)
    };
    // face: display (85..175, 0..75); cup: display (30..90, 180..240)
    let (fm, fx, fy, fmean) = region(42, 0, 88, 38);
    let (cm, cx, cy, _) = region(15, 90, 45, 120);
    println!(
        "[{label}] face max {fm:.1} C at display ({}, {}), face mean {fmean:.1} C | cup max {cm:.1} C at ({}, {})",
        fx * 2,
        fy * 2,
        cx * 2,
        cy * 2
    );
    Ok(())
}

fn main() -> Result<()> {
    let dev = Arc::new(Device::open()?);
    dev.start_stream()?;
    let stop = Arc::new(AtomicBool::new(false));
    let snap = Arc::new(AtomicBool::new(false));
    let pump = {
        let dev = dev.clone();
        let stop = stop.clone();
        let snap = snap.clone();
        thread::spawn(move || {
            let mut seen = 0usize;
            for frame in dev.frames() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(jpeg) = frame else { continue };
                seen += 1;
                if seen < 10 {
                    continue; // skip stale FIFO burst
                }
                if snap.swap(false, Ordering::Relaxed) {
                    let _ = std::fs::write("/tmp/comp-off-osd.jpg", &jpeg);
                }
            }
        })
    };
    thread::sleep(Duration::from_secs(2));

    let orig = dev.simple_get(3, 8)?;
    println!("original: {}", hex(&orig));

    face_stats(&dev, "ON baseline")?;

    // Disable.
    let mut off = orig.clone();
    off[0] = 1;
    off[1] = 0;
    dev.select_command(3, 8)?;
    dev.xu_set(3, &off)?;
    thread::sleep(Duration::from_millis(500));
    snap.store(true, Ordering::Relaxed); // grab an OSD frame in OFF mode
    face_stats(&dev, "OFF")?;

    // Restore.
    dev.select_command(3, 8)?;
    dev.xu_set(3, &orig)?;
    thread::sleep(Duration::from_millis(500));
    let back = dev.simple_get(3, 8)?;
    println!("restored byEnabled={} (live fields drift: {})", back[1], back[3] != orig[3]);
    face_stats(&dev, "ON restored")?;

    stop.store(true, Ordering::Relaxed);
    let _ = pump.join();
    Ok(())
}
