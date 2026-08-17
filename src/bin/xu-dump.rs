// Dump the vendor Extension Unit (unit 10) of the HIK UVC thermal camera.
// For each selector: GET_INFO, GET_LEN, then GET_CUR. Read-only.

use std::time::Duration;

use anyhow::{Context, Result};
use rusb::UsbContext;

const VID: u16 = 0x2bdf;
const PID: u16 = 0x0101;
const IF_CONTROL: u8 = 0;
const XU_UNIT_ID: u8 = 10;

const GET_CUR: u8 = 0x81;
const GET_MIN: u8 = 0x82;
const GET_MAX: u8 = 0x83;
const GET_RES: u8 = 0x84;
const GET_LEN: u8 = 0x85;
const GET_INFO: u8 = 0x86;
const GET_DEF: u8 = 0x87;

const REQ_IN: u8 = 0xa1;
const TIMEOUT: Duration = Duration::from_millis(1000);

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

fn get(handle: &rusb::DeviceHandle<rusb::Context>, req: u8, sel: u8, buf: &mut [u8]) -> Result<usize, rusb::Error> {
    let wvalue = (sel as u16) << 8;
    let windex = ((XU_UNIT_ID as u16) << 8) | IF_CONTROL as u16;
    handle.read_control(REQ_IN, req, wvalue, windex, buf, TIMEOUT)
}

fn run(handle: &rusb::DeviceHandle<rusb::Context>) -> Result<()> {
    for sel in 1u8..=15 {
        let wvalue = (sel as u16) << 8;
        let windex = ((XU_UNIT_ID as u16) << 8) | IF_CONTROL as u16;
        println!("--- selector {sel} ---");

        let mut info = [0u8; 1];
        match get(handle, GET_INFO, sel, &mut info) {
            Ok(_) => println!("GET_INFO: {:#04x} (get={} set={})", info[0], info[0] & 1 != 0, info[0] & 2 != 0),
            Err(e) => {
                println!("GET_INFO: {e}");
                continue;
            }
        }

        let mut lenbuf = [0u8; 2];
        let len = match get(handle, GET_LEN, sel, &mut lenbuf) {
            Ok(_) => u16::from_le_bytes(lenbuf),
            Err(e) => {
                println!("GET_LEN: {e}");
                continue;
            }
        };
        println!("GET_LEN: {len}");
        if len == 0 || len > 4096 {
            continue;
        }

        for (name, req) in [
            ("CUR", GET_CUR),
            ("MIN", GET_MIN),
            ("MAX", GET_MAX),
            ("RES", GET_RES),
            ("DEF", GET_DEF),
        ] {
            let mut buf = vec![0u8; len as usize];
            match handle.read_control(REQ_IN, req, wvalue, windex, &mut buf, TIMEOUT) {
                Ok(n) => {
                    let data = &buf[..n];
                    let ascii: String = data
                        .iter()
                        .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
                        .collect();
                    println!("GET_{name}: {n}B hex={} ascii={ascii}", hex(data));
                }
                Err(_) => {} // unsupported op, skip silently
            }
        }
    }
    Ok(())
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}
