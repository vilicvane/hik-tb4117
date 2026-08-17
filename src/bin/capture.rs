// Capture streams from the HIK UVC thermal camera (2bdf:0101).
//
// Usage: capture <format_index> <frame_index> <interval_100ns> <jpeg|raw> <out>
//   jpeg mode: reassemble frames by SOI/EOI markers, write out_00.jpg ...
//   raw mode:  concatenate all payloads (UVC payload headers stripped) into out
//
// The device advertises YUY2 but actually streams MJPEG in that mode, which is
// why we bypass uvcvideo entirely and do VS PROBE/COMMIT + bulk reads ourselves.

use std::fs::File;
use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rusb::UsbContext;

const VID: u16 = 0x2bdf;
const PID: u16 = 0x0101;

const IF_CONTROL: u8 = 0;
const IF_STREAM: u8 = 1;
const EP_VIDEO_IN: u8 = 0x81;

const SET_CUR: u8 = 0x01;
const GET_CUR: u8 = 0x81;
const VS_PROBE_CONTROL: u16 = 0x0100;
const VS_COMMIT_CONTROL: u16 = 0x0200;

const REQ_OUT: u8 = 0x21;
const REQ_IN: u8 = 0xa1;

const TIMEOUT: Duration = Duration::from_millis(2000);

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 6 {
        eprintln!("usage: {} <format_index> <frame_index> <interval_100ns> <jpeg|raw> <out_prefix>", args[0]);
        std::process::exit(2);
    }
    let format_index: u8 = args[1].parse()?;
    let frame_index: u8 = args[2].parse()?;
    let interval: u32 = args[3].parse()?;
    let mode = &args[4];
    let out = &args[5];

    let ctx = rusb::Context::new()?;
    let handle = ctx
        .open_device_with_vid_pid(VID, PID)
        .context("device 2bdf:0101 not found (is it attached to WSL?)")?;

    for iface in [IF_CONTROL, IF_STREAM] {
        if handle.kernel_driver_active(iface)? {
            handle.detach_kernel_driver(iface)?;
        }
        handle.claim_interface(iface)?;
    }

    let result = run(&handle, format_index, frame_index, interval, mode, out);

    for iface in [IF_STREAM, IF_CONTROL] {
        let _ = handle.release_interface(iface);
        let _ = handle.attach_kernel_driver(iface);
    }

    result
}

fn negotiate(handle: &rusb::DeviceHandle<rusb::Context>, format_index: u8, frame_index: u8, interval: u32) -> Result<()> {
    let mut probe = [0u8; 26];
    probe[2] = format_index;
    probe[3] = frame_index;
    probe[4..8].copy_from_slice(&interval.to_le_bytes());

    handle.write_control(REQ_OUT, SET_CUR, VS_PROBE_CONTROL, IF_STREAM as u16, &probe, TIMEOUT)?;

    let mut cur = [0u8; 26];
    let n = handle.read_control(REQ_IN, GET_CUR, VS_PROBE_CONTROL, IF_STREAM as u16, &mut cur, TIMEOUT)?;
    let got_interval = u32::from_le_bytes(cur[4..8].try_into()?);
    let max_frame = u32::from_le_bytes(cur[18..22].try_into()?);
    println!(
        "negotiated {n}-byte probe: format={} frame={} interval={} ({} fps) max_frame_size={max_frame}",
        cur[2],
        cur[3],
        got_interval,
        10_000_000 / got_interval.max(1)
    );

    handle.write_control(REQ_OUT, SET_CUR, VS_COMMIT_CONTROL, IF_STREAM as u16, &cur, TIMEOUT)?;
    Ok(())
}

fn run(
    handle: &rusb::DeviceHandle<rusb::Context>,
    format_index: u8,
    frame_index: u8,
    interval: u32,
    mode: &str,
    out: &str,
) -> Result<()> {
    negotiate(handle, format_index, frame_index, interval)?;

    let mut buf = vec![0u8; 1 << 20];
    let deadline = Instant::now() + Duration::from_secs(15);

    match mode {
        "jpeg" => {
            let mut frame: Vec<u8> = Vec::with_capacity(64 * 1024);
            let mut in_frame = false;
            let mut saved = 0usize;
            while saved < 3 && Instant::now() < deadline {
                let chunk = read_payload(handle, &mut buf)?;
                if chunk.is_empty() {
                    continue;
                }
                if chunk.starts_with(&[0xff, 0xd8]) {
                    in_frame = true;
                    frame.clear();
                }
                if !in_frame {
                    continue;
                }
                frame.extend_from_slice(chunk);
                if frame.windows(2).any(|w| w == [0xff, 0xd9]) {
                    let path = format!("{out}_{saved:02}.jpg");
                    File::create(&path)?.write_all(&frame)?;
                    println!("saved {path} ({} bytes)", frame.len());
                    saved += 1;
                    in_frame = false;
                }
            }
            if saved == 0 {
                bail!("no JPEG frames seen (stream head: {:02x?})", &frame[..frame.len().min(16)]);
            }
        }
        "raw" => {
            let path = format!("{out}.bin");
            let mut file = File::create(&path)?;
            let mut total = 0u64;
            while Instant::now() < deadline && total < 8 << 20 {
                let chunk = read_payload(handle, &mut buf)?;
                if chunk.is_empty() {
                    continue;
                }
                file.write_all(chunk)?;
                total += chunk.len() as u64;
            }
            println!("saved {path} ({total} bytes)");
        }
        other => bail!("unknown mode {other}"),
    }
    Ok(())
}

fn read_payload<'a>(handle: &rusb::DeviceHandle<rusb::Context>, buf: &'a mut [u8]) -> Result<&'a [u8]> {
    match handle.read_bulk(EP_VIDEO_IN, buf, TIMEOUT) {
        Ok(n) => {
            if n < 2 {
                return Ok(&[]);
            }
            let hlen = buf[0] as usize;
            if hlen > n {
                return Ok(&[]);
            }
            Ok(&buf[hlen..n])
        }
        Err(rusb::Error::Timeout) => Ok(&[]),
        Err(e) => bail!("bulk read error: {e}"),
    }
}
