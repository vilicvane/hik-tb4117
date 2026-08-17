// Probe XU selectors while the video stream is running.
// Some devices only answer extension-unit commands during streaming.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusb::UsbContext;

const VID: u16 = 0x2bdf;
const PID: u16 = 0x0101;
const IF_CONTROL: u8 = 0;
const IF_STREAM: u8 = 1;
const EP_VIDEO_IN: u8 = 0x81;
const EP_STATUS_IN: u8 = 0x83;
const XU_UNIT_ID: u8 = 10;

const SET_CUR: u8 = 0x01;
const GET_CUR: u8 = 0x81;
const GET_LEN: u8 = 0x85;
const GET_INFO: u8 = 0x86;
const VS_PROBE_CONTROL: u16 = 0x0100;
const VS_COMMIT_CONTROL: u16 = 0x0200;
const REQ_OUT: u8 = 0x21;
const REQ_IN: u8 = 0xa1;
const TIMEOUT: Duration = Duration::from_millis(800);

fn main() -> Result<()> {
    let ctx = rusb::Context::new()?;
    let handle = Arc::new(ctx.open_device_with_vid_pid(VID, PID).context("not found")?);

    for iface in [IF_CONTROL, IF_STREAM] {
        if handle.kernel_driver_active(iface)? {
            handle.detach_kernel_driver(iface)?;
        }
        handle.claim_interface(iface)?;
    }

    let result = run(handle.clone());

    for iface in [IF_STREAM, IF_CONTROL] {
        let _ = handle.release_interface(iface);
        let _ = handle.attach_kernel_driver(iface);
    }
    result
}

fn run(handle: Arc<rusb::DeviceHandle<rusb::Context>>) -> Result<()> {
    // Negotiate MJPEG 240x320 @ 30fps.
    let mut probe = [0u8; 26];
    probe[2] = 2;
    probe[3] = 2;
    probe[4..8].copy_from_slice(&333_333u32.to_le_bytes());
    handle.write_control(REQ_OUT, SET_CUR, VS_PROBE_CONTROL, IF_STREAM as u16, &probe, TIMEOUT)?;
    let mut cur = [0u8; 26];
    handle.read_control(REQ_IN, GET_CUR, VS_PROBE_CONTROL, IF_STREAM as u16, &mut cur, TIMEOUT)?;
    handle.write_control(REQ_OUT, SET_CUR, VS_COMMIT_CONTROL, IF_STREAM as u16, &cur, TIMEOUT)?;
    println!("stream committed");

    // Stream-pump thread: read and discard bulk video.
    let pump = {
        let handle = handle.clone();
        thread::spawn(move || {
            let mut buf = vec![0u8; 1 << 20];
            let mut total = 0u64;
            let end = Instant::now() + Duration::from_secs(12);
            while Instant::now() < end {
                if let Ok(n) = handle.read_bulk(EP_VIDEO_IN, &mut buf, Duration::from_millis(500)) {
                    total += n as u64;
                }
            }
            println!("pump: read {total} bytes total");
        })
    };

    // Let streaming warm up.
    thread::sleep(Duration::from_secs(2));

    let windex = ((XU_UNIT_ID as u16) << 8) | IF_CONTROL as u16;
    for sel in 1u8..=15 {
        let wvalue = (sel as u16) << 8;
        let mut info = [0u8; 1];
        let info_str = match handle.read_control(REQ_IN, GET_INFO, wvalue, windex, &mut info, TIMEOUT) {
            Ok(_) => format!("info={:#04x}", info[0]),
            Err(_) => "info=ERR".to_string(),
        };
        let mut lenbuf = [0u8; 2];
        let len_str = match handle.read_control(REQ_IN, GET_LEN, wvalue, windex, &mut lenbuf, TIMEOUT) {
            Ok(_) => format!("len={}", u16::from_le_bytes(lenbuf)),
            Err(_) => "len=ERR".to_string(),
        };
        let mut buf = vec![0u8; 512];
        let cur_str = match handle.read_control(REQ_IN, GET_CUR, wvalue, windex, &mut buf, TIMEOUT) {
            Ok(n) if n > 0 => format!("cur={}B {}", n, hex(&buf[..n.min(64)])),
            _ => "cur=none".to_string(),
        };
        println!("sel {sel:>2}: {info_str} {len_str} {cur_str}");
    }

    // Listen to interrupt status EP while streaming.
    println!("--- interrupt EP 0x83, 5s ---");
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

    let _ = pump.join();
    Ok(())
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}
