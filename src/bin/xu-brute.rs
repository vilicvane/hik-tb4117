// Brute-force probe of the vendor Extension Unit (unit 10) on 2bdf:0101.
// The device lies about GET_LEN/GET_INFO for most selectors, so just try
// GET_CUR with a large buffer on every selector, and also listen to the
// interrupt status endpoint 0x83 for a while.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusb::UsbContext;

const VID: u16 = 0x2bdf;
const PID: u16 = 0x0101;
const IF_CONTROL: u8 = 0;
const XU_UNIT_ID: u8 = 10;
const EP_STATUS_IN: u8 = 0x83;

const GET_CUR: u8 = 0x81;
const GET_MIN: u8 = 0x82;
const GET_MAX: u8 = 0x83;
const GET_DEF: u8 = 0x87;

const REQ_IN: u8 = 0xa1;
const TIMEOUT: Duration = Duration::from_millis(800);

fn main() -> Result<()> {
    let ctx = rusb::Context::new()?;
    let handle = ctx
        .open_device_with_vid_pid(VID, PID)
        .context("device 2bdf:0101 not found")?;

    if handle.kernel_driver_active(IF_CONTROL)? {
        handle.detach_kernel_driver(IF_CONTROL)?;
    }
    handle.claim_interface(IF_CONTROL)?;

    let result = run(&handle);

    let _ = handle.release_interface(IF_CONTROL);
    let _ = handle.attach_kernel_driver(IF_CONTROL);

    result
}

fn run(handle: &rusb::DeviceHandle<rusb::Context>) -> Result<()> {
    let windex = ((XU_UNIT_ID as u16) << 8) | IF_CONTROL as u16;

    for sel in 1u8..=15 {
        let wvalue = (sel as u16) << 8;
        let mut line = format!("sel {sel:>2}:");
        for (name, req) in [("CUR", GET_CUR), ("MIN", GET_MIN), ("MAX", GET_MAX), ("DEF", GET_DEF)] {
            let mut buf = vec![0u8; 512];
            match handle.read_control(REQ_IN, req, wvalue, windex, &mut buf, TIMEOUT) {
                Ok(n) if n > 0 => {
                    let data = &buf[..n];
                    let ascii: String = data
                        .iter()
                        .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
                        .collect();
                    line.push_str(&format!("  {name}={n}B [{}] «{}»", hex(data), ascii.trim_end_matches('.')));
                }
                _ => {}
            }
        }
        println!("{line}");
    }

    // Listen to the interrupt status endpoint for a few seconds.
    println!("--- listening on interrupt EP 0x83 for 5s ---");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buf = [0u8; 64];
    while Instant::now() < deadline {
        match handle.read_interrupt(EP_STATUS_IN, &mut buf, Duration::from_millis(500)) {
            Ok(n) => println!("int {}B: {}", n, hex(&buf[..n])),
            Err(rusb::Error::Timeout) => {}
            Err(e) => {
                println!("int error: {e}");
                break;
            }
        }
    }
    Ok(())
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}
