// Raw bulk streaming probe for the HIK UVC thermal camera (2bdf:0101).
//
// Bypasses the kernel uvcvideo driver: detaches it, negotiates the stream with
// UVC VS PROBE/COMMIT control transfers, then reads the bulk IN endpoint 0x81
// directly and logs transfer sizes + payload headers so we can see exactly what
// the device puts on the wire.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rusb::UsbContext;

const VID: u16 = 0x2bdf;
const PID: u16 = 0x0101;

const IF_CONTROL: u8 = 0;
const IF_STREAM: u8 = 1;
const EP_VIDEO_IN: u8 = 0x81;

// UVC request / selector constants.
const SET_CUR: u8 = 0x01;
const GET_CUR: u8 = 0x81;
const VS_PROBE_CONTROL: u16 = 0x0100;
const VS_COMMIT_CONTROL: u16 = 0x0200;

// USB control transfer type: class-specific, to interface.
const REQ_OUT: u8 = 0x21;
const REQ_IN: u8 = 0xa1;

const TIMEOUT: Duration = Duration::from_millis(2000);

fn main() -> Result<()> {
    let ctx = rusb::Context::new()?;
    for dev in ctx.devices()?.iter() {
        match dev.device_descriptor() {
            Ok(d) => println!(
                "found {:04x}:{:04x} bus={} addr={}",
                d.vendor_id(),
                d.product_id(),
                dev.bus_number(),
                dev.address()
            ),
            Err(e) => eprintln!(
                "descriptor error bus={} addr={}: {e}",
                dev.bus_number(),
                dev.address()
            ),
        }
    }
    let handle = ctx
        .open_device_with_vid_pid(VID, PID)
        .context("device 2bdf:0101 not found (is it attached to WSL?)")?;

    for iface in [IF_CONTROL, IF_STREAM] {
        if handle.kernel_driver_active(iface)? {
            handle.detach_kernel_driver(iface)?;
            println!("detached kernel driver from interface {iface}");
        }
        handle.claim_interface(iface)?;
    }

    let result = run(&handle);

    for iface in [IF_STREAM, IF_CONTROL] {
        let _ = handle.release_interface(iface);
        let _ = handle.attach_kernel_driver(iface);
    }

    result
}

fn run(handle: &rusb::DeviceHandle<rusb::Context>) -> Result<()> {
    // UVC 1.1 video probe/commit structure, 26 bytes, little-endian.
    let mut probe = [0u8; 26];
    probe[0..2].copy_from_slice(&0u16.to_le_bytes()); // bmHint
    probe[2] = 1; // bFormatIndex: 1 = YUYV
    probe[3] = 1; // bFrameIndex: 1 = 384x288
    probe[4..8].copy_from_slice(&200_000u32.to_le_bytes()); // 50 fps, in 100 ns units

    let n = handle.write_control(REQ_OUT, SET_CUR, VS_PROBE_CONTROL, IF_STREAM as u16, &probe, TIMEOUT)?;
    println!("PROBE SET_CUR -> {n} bytes");

    let mut cur = [0u8; 26];
    let n = handle.read_control(REQ_IN, GET_CUR, VS_PROBE_CONTROL, IF_STREAM as u16, &mut cur, TIMEOUT)?;
    println!("PROBE GET_CUR <- {n} bytes");
    if n >= 26 {
        let interval = u32::from_le_bytes(cur[4..8].try_into()?);
        let max_frame = u32::from_le_bytes(cur[18..22].try_into()?);
        let max_payload = u32::from_le_bytes(cur[22..26].try_into()?);
        println!(
            "  negotiated: interval={interval} ({} fps) max_frame_size={max_frame} max_payload={max_payload}",
            10_000_000 / interval.max(1)
        );
    }

    let n = handle.write_control(REQ_OUT, SET_CUR, VS_COMMIT_CONTROL, IF_STREAM as u16, &cur, TIMEOUT)?;
    println!("COMMIT SET_CUR -> {n} bytes");

    // Read the raw bulk stream for a few seconds.
    let mut buf = vec![0u8; 1 << 20]; // 1 MiB per read
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut total = 0u64;
    let mut reads = 0u64;
    let mut frame_marks = 0u64;

    println!("streaming bulk reads...");
    while Instant::now() < deadline {
        match handle.read_bulk(EP_VIDEO_IN, &mut buf, TIMEOUT) {
            Ok(n) => {
                reads += 1;
                total += n as u64;
                // UVC payload header: buf[0] = header length, buf[1] = bmInfo
                // (bit0 FID, bit7 EOH...). FID toggle marks a new frame.
                if n >= 2 {
                    let hlen = buf[0];
                    let info = buf[1];
                    if reads <= 10 || info & 0x80 != 0 {
                        println!(
                            "read {n:>8} bytes  hlen={hlen} bmInfo={info:#04x} head={:02x?}",
                            &buf[..n.min(16)]
                        );
                    }
                    if info & 0x02 != 0 {
                        frame_marks += 1;
                    }
                }
            }
            Err(rusb::Error::Timeout) => {
                println!("read timeout");
            }
            Err(e) => bail!("bulk read error: {e}"),
        }
    }

    println!("total: {total} bytes in {reads} reads, eos-marks={frame_marks}");
    println!("expected per frame: {} bytes", 384 * 288 * 2);
    Ok(())
}
