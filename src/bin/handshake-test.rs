// Isolate the XU unlock factor: argv[1] = "sel4" (version probe only) or
// "ep83" (interrupt EP read only).

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use thermal_camera::{Device, EP_STATUS_IN};

fn main() -> Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "sel4".into());
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

    match mode.as_str() {
        "sel4" => {
            println!("GET_LEN sel4 = {:?}", dev.xu_get_len(4));
            let mut ver = [0u8; 4];
            let n = dev.xu_get(4, &mut ver)?;
            println!("GET_CUR sel4 = {:?} ({n} B)", String::from_utf8_lossy(&ver));
        }
        "ep83" => {
            let mut ibuf = [0u8; 64];
            match dev
                .handle()
                .read_interrupt(EP_STATUS_IN, &mut ibuf, Duration::from_millis(300))
            {
                Ok(n) => println!("EP 0x83: {:02x?}", &ibuf[..n]),
                Err(e) => println!("EP 0x83: {e}"),
            }
        }
        _ => {}
    }

    dev.select_command(3, 8)?;
    println!("GET_LEN sel3 after sel5{{3,8}} = {:?}", dev.xu_get_len(3));

    stop.store(true, Ordering::Relaxed);
    let _ = pump.join();
    Ok(())
}
