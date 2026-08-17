// Dump 2034 USB_THERMOMETRY_REGIONS (group 3 sub 3) — the OSD thermometry rules.

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

    match dev.simple_get(3, 3) {
        Ok(p) => {
            println!("[2034 THERMOMETRY_REGIONS] {} bytes", p.len());
            for row in p.chunks(16) {
                println!("  {}", hex(row));
            }
        }
        Err(e) => println!("[2034] failed: {e:#} (last_err={:?})", dev.xu_last_error()),
    }

    stop.store(true, Ordering::Relaxed);
    let _ = pump.join();
    Ok(())
}
