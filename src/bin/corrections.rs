// Dump thermal correction-related configs: 2040 TEMPERATURE_CORRECT (3/5),
// 2044 BODYTEMP_COMPENSATION (3/8), 2038 STREAM_PARAM (3/5? no: 3/5 is 2040).

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

    for (cmd, group, sub, name) in [
        (2040u16, 3u8, 5u8, "TEMPERATURE_CORRECT"),
        (2042, 3, 6, "BLACK_BODY"),
        (2044, 3, 8, "BODYTEMP_COMPENSATION"),
        (2038, 3, 5, "STREAM_PARAM?"),
    ] {
        match dev.simple_get(group, sub) {
            Ok(p) => println!("[{cmd} {name}] {} bytes: {}", p.len(), hex(&p)),
            Err(e) => println!("[{cmd} {name}] failed: {e:#} (last_err={:?})", dev.xu_last_error()),
        }
    }

    stop.store(true, Ordering::Relaxed);
    let _ = pump.join();
    Ok(())
}
