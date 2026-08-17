// Library core for talking to the HIK TB-4117-3/S thermal module (2bdf:0101).
//
// The device advertises YUY2/H264 in its UVC descriptors but always streams
// 240x320 MJPEG over the bulk endpoint, which makes the kernel uvcvideo path
// useless (every frame is flagged erroneous). So we bypass it: negotiate with
// VS PROBE/COMMIT ourselves and reassemble JPEG frames from raw bulk reads.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use rusb::{DeviceHandle, UsbContext};

pub const VID: u16 = 0x2bdf;
pub const PID: u16 = 0x0101;

pub const IF_CONTROL: u8 = 0;
pub const IF_STREAM: u8 = 1;
pub const EP_VIDEO_IN: u8 = 0x81;
pub const EP_STATUS_IN: u8 = 0x83;

pub const XU_UNIT_ID: u8 = 10;

const SET_CUR: u8 = 0x01;
const GET_CUR: u8 = 0x81;
const VS_PROBE_CONTROL: u16 = 0x0100;
const VS_COMMIT_CONTROL: u16 = 0x0200;
const REQ_CLASS_OUT: u8 = 0x21;
const REQ_CLASS_IN: u8 = 0xa1;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(2000);

/// RAII guard that owns both USB interfaces and returns them to the kernel
/// driver on drop.
pub struct Device {
    handle: DeviceHandle<rusb::Context>,
}

impl Device {
    pub fn open() -> Result<Self> {
        Self::open_with(DEFAULT_TIMEOUT)
    }

    pub fn open_with(_timeout: Duration) -> Result<Self> {
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
        Ok(Self { handle })
    }

    pub fn handle(&self) -> &DeviceHandle<rusb::Context> {
        &self.handle
    }

    /// Negotiate the video stream. The device ignores the requested
    /// format/frame and always delivers 240x320 MJPEG; we still go through the
    /// standard UVC handshake with the values it advertises.
    pub fn start_stream(&self) -> Result<()> {
        let mut probe = [0u8; 26];
        probe[2] = 2; // bFormatIndex: MJPEG
        probe[3] = 2; // bFrameIndex: 240x320
        probe[4..8].copy_from_slice(&333_333u32.to_le_bytes()); // 30 fps

        self.handle.write_control(
            REQ_CLASS_OUT,
            SET_CUR,
            VS_PROBE_CONTROL,
            IF_STREAM as u16,
            &probe,
            DEFAULT_TIMEOUT,
        )?;

        let mut cur = [0u8; 26];
        self.handle.read_control(
            REQ_CLASS_IN,
            GET_CUR,
            VS_PROBE_CONTROL,
            IF_STREAM as u16,
            &mut cur,
            DEFAULT_TIMEOUT,
        )?;

        self.handle.write_control(
            REQ_CLASS_OUT,
            SET_CUR,
            VS_COMMIT_CONTROL,
            IF_STREAM as u16,
            &cur,
            DEFAULT_TIMEOUT,
        )?;
        Ok(())
    }

    /// Read one bulk payload (UVC payload header stripped). Returns an empty
    /// vec on timeout.
    pub fn read_payload(&self, buf: &mut Vec<u8>) -> Result<usize> {
        buf.resize(1 << 16, 0);
        match self.handle.read_bulk(EP_VIDEO_IN, buf, DEFAULT_TIMEOUT) {
            Ok(n) => {
                buf.truncate(n);
                if n < 2 || (buf[0] as usize) > n {
                    buf.clear();
                    return Ok(0);
                }
                let hlen = buf[0] as usize;
                buf.drain(..hlen);
                Ok(buf.len())
            }
            Err(rusb::Error::Timeout) => Ok(0),
            Err(e) => bail!("bulk read error: {e}"),
        }
    }

    /// Iterator over reassembled JPEG frames (SOI..EOI).
    pub fn frames(&self) -> FrameIter<'_> {
        FrameIter {
            dev: self,
            buf: Vec::new(),
            frame: Vec::with_capacity(64 * 1024),
            in_frame: false,
        }
    }

    /// Raw XU extension-unit control transfer helpers.
    pub fn xu_get(&self, selector: u8, buf: &mut [u8]) -> Result<usize, rusb::Error> {
        self.handle.read_control(
            REQ_CLASS_IN,
            GET_CUR,
            (selector as u16) << 8,
            ((XU_UNIT_ID as u16) << 8) | IF_CONTROL as u16,
            buf,
            DEFAULT_TIMEOUT,
        )
    }

    pub fn xu_set(&self, selector: u8, data: &[u8]) -> Result<usize, rusb::Error> {
        self.handle.write_control(
            REQ_CLASS_OUT,
            SET_CUR,
            (selector as u16) << 8,
            ((XU_UNIT_ID as u16) << 8) | IF_CONTROL as u16,
            data,
            DEFAULT_TIMEOUT,
        )
    }

    /// UVC GET_LEN on an XU selector (returns u32 LE).
    pub fn xu_get_len(&self, selector: u8) -> Result<u32, rusb::Error> {
        const GET_LEN: u8 = 0x85;
        let mut buf = [0u8; 4];
        self.handle.read_control(
            REQ_CLASS_IN,
            GET_LEN,
            (selector as u16) << 8,
            ((XU_UNIT_ID as u16) << 8) | IF_CONTROL as u16,
            &mut buf,
            DEFAULT_TIMEOUT,
        )?;
        Ok(u32::from_le_bytes(buf))
    }

    /// Selector 6: last command error code.
    pub fn xu_last_error(&self) -> Option<u8> {
        let mut buf = [0u8; 1];
        self.xu_get(6, &mut buf).ok().map(|_| buf[0])
    }

    // ---- HCUSBSDK ThermalV2 protocol (see docs/hcusb-uvc-protocol.md) ----

    /// Point the data channel at (group, sub) via the selector-5 hold register.
    /// The video stream must be running or this SET_CUR times out.
    pub fn select_command(&self, group: u8, sub: u8) -> Result<()> {
        self.xu_set(5, &[group, sub])
            .map_err(|e| anyhow::anyhow!("sel5 SET_CUR failed: {e} (stream running?)"))?;
        Ok(())
    }

    /// Simple GET transaction: select command, read length, read payload.
    pub fn simple_get(&self, group: u8, sub: u8) -> Result<Vec<u8>> {
        self.select_command(group, sub)?;
        let len = self
            .xu_get_len(group)
            .map_err(|e| anyhow::anyhow!("GET_LEN group {group} failed: {e}"))?;
        if len == 0 || len > 0x10000 {
            bail!("implausible GET_LEN {len} for group {group} sub {sub}");
        }
        let mut buf = vec![0u8; len as usize];
        let n = self
            .xu_get(group, &mut buf)
            .map_err(|e| anyhow::anyhow!("GET_CUR group {group} failed: {e}"))?;
        buf.truncate(n);
        Ok(buf)
    }

    /// DoubleGet transaction, verified on TB-4117-3/S (2046/2047):
    /// select_command -> SET request -> GET_LEN (5) -> GET 5-byte head
    /// ([0]=0x01, [1..5]=total length) -> chunked GETs.
    ///
    /// Phase B must NOT re-send the SET (that restarts the transaction).
    /// Data arrives in chunks of at most 512 bytes, each prefixed with a
    /// 5-byte header `{0x02, u32 LE sequence}`. A single control read may
    /// not exceed 512 bytes (larger wLength stalls the pipe). Returns the
    /// reassembled payload with chunk headers stripped.
    pub fn double_get(&self, group: u8, sub: u8, request: &[u8]) -> Result<Vec<u8>> {
        self.select_command(group, sub)?;
        // Phase A: send request, then read the 5-byte head; head[1..5] = total length.
        self.xu_set(group, request)
            .map_err(|e| anyhow::anyhow!("double_get phase A SET failed: {e}"))?;
        let head_len = self
            .xu_get_len(group)
            .map_err(|e| anyhow::anyhow!("double_get phase A GET_LEN failed: {e}"))?;
        let mut head = vec![0u8; head_len as usize];
        self.xu_get(group, &mut head)
            .map_err(|e| anyhow::anyhow!("double_get phase A GET failed: {e}"))?;
        if head.len() < 5 {
            bail!("double_get head too short: {head:?}");
        }
        let total = u32::from_le_bytes(head[1..5].try_into()?) as usize;
        if total == 0 || total > 8 << 20 {
            bail!("implausible double_get length {total}");
        }

        // Phase B: chunked reads, strip per-chunk headers.
        let mut out = Vec::with_capacity(total);
        while out.len() < total {
            let mut chunk = vec![0u8; 512];
            let n = self
                .xu_get(group, &mut chunk)
                .map_err(|e| anyhow::anyhow!("double_get phase B GET failed: {e}"))?;
            if n == 0 {
                bail!("double_get truncated: {} of {total} bytes", out.len());
            }
            chunk.truncate(n);
            if chunk.len() > 5 && chunk[0] == 0x02 {
                out.extend_from_slice(&chunk[5..]);
            } else {
                out.extend_from_slice(&chunk);
            }
        }
        out.truncate(total);
        Ok(out)
    }

    /// Query the max temperature of up to 10 rectangular regions via the 2047
    /// ROI_MAX_TEMPERATURE_SEARCH command (group 3 sub 10, DoubleGet).
    ///
    /// Coordinates are in the native 480x640 space (2x the 240x320 JPEG
    /// display resolution). Temperatures come back in 0.1 °C units and match
    /// the on-screen OSD values exactly (verified with a palm test).
    ///
    /// The stream must be running (`start_stream`) for XU commands to work.
    pub fn roi_max_temperatures(&self, regions: &[RoiRegion]) -> Result<RoiSearchResult> {
        if regions.is_empty() || regions.len() > 10 {
            bail!("roi_max_temperatures takes 1..=10 regions");
        }
        let mut req = vec![0u8; 234];
        req[0] = 1; // byChannelID: must be 1 on this firmware (2 stalls the pipe)
        req[8..10].copy_from_slice(&2026u16.to_le_bytes()); // wYear
        req[13] = regions.len() as u8;
        for (i, r) in regions.iter().enumerate() {
            let b = 14 + i * 22;
            req[b] = (i + 1) as u8;
            req[b + 1] = 1; // enabled
            req[b + 2..b + 6].copy_from_slice(&r.x.to_le_bytes());
            req[b + 6..b + 10].copy_from_slice(&r.y.to_le_bytes());
            req[b + 10..b + 14].copy_from_slice(&r.width.to_le_bytes());
            req[b + 14..b + 18].copy_from_slice(&r.height.to_le_bytes());
            req[b + 18..b + 22].copy_from_slice(&r.distance.to_le_bytes());
        }

        let buf = self.double_get(3, 10, &req)?;
        // Payload layout (after chunk headers are stripped):
        //   [0] u8 echo, [1..5] global max temp u32 (0.1 °C),
        //   [5..13] visible max point, [13..21] thermal max point,
        //   [21] u8 = 10 (region capacity), [22..26] u32 jpeg len,
        //   [26 + 21*i] region blocks {id, temp, visX, visY, thermX, thermY}.
        if buf.len() < 26 {
            bail!("ROI response too short: {} bytes", buf.len());
        }
        let block = |b: &[u8]| RoiBlock {
            id: b[0],
            // u32 LE at offset 1, units of 0.1 °C.
            max_temp_raw: u32::from_le_bytes(b[1..5].try_into().unwrap()),
            thermal_x: u32::from_le_bytes(b[13..17].try_into().unwrap()),
            thermal_y: u32::from_le_bytes(b[17..21].try_into().unwrap()),
        };
        let mut region_results = Vec::with_capacity(regions.len());
        for i in 0..regions.len() {
            let b = 26 + i * 21;
            if b + 21 > buf.len() {
                bail!("ROI response truncated at region {i}");
            }
            region_results.push(block(&buf[b..b + 21]));
        }
        Ok(RoiSearchResult {
            global: block(&buf[0..21]),
            regions: region_results,
        })
    }

    /// Convenience: temperature (°C) of a single pixel, given in 240x320
    /// display coordinates. Uses a 1x1 native-space ROI (2x scaling).
    ///
    /// Note: the 2047 search dilutes tiny hot spots (small point sources read
    /// low). For exact per-pixel values use [`Device::capture_radiometric`].
    pub fn pixel_temperature(&self, display_x: u32, display_y: u32) -> Result<f64> {
        let r = RoiRegion {
            x: display_x * 2,
            y: display_y * 2,
            width: 1,
            height: 1,
            distance: 100,
        };
        let res = self.roi_max_temperatures(&[r])?;
        Ok(res.regions[0].temperature_c())
    }

    /// Read the 2044 USB_BODYTEMP_COMPENSATION config (group 3 sub 8).
    /// Returns the raw payload (18 bytes on TB-4117-3/S); byte 1 is
    /// byEnabled, bytes 3..15 are live values the device recomputes.
    pub fn body_temp_compensation(&self) -> Result<Vec<u8>> {
        self.simple_get(3, 8)
    }

    /// Enable/disable body-temperature compensation (2045 SET, same sub).
    /// Only byte 1 (byEnabled) is toggled; the rest of the current config is
    /// written back unchanged.
    pub fn set_body_temp_compensation(&self, enabled: bool) -> Result<()> {
        let mut cfg = self.body_temp_compensation()?;
        if cfg.len() < 2 {
            bail!("unexpected 2044 payload: {cfg:?}");
        }
        cfg[0] = 1; // byChannelID
        cfg[1] = enabled as u8;
        self.select_command(3, 8)?;
        self.xu_set(3, &cfg)?;
        Ok(())
    }

    /// Restore a compensation payload previously read by
    /// [`Device::body_temp_compensation`].
    pub fn restore_body_temp_compensation(&self, original: &[u8]) -> Result<()> {
        self.select_command(3, 8)?;
        self.xu_set(3, original)?;
        Ok(())
    }

    /// Capture a JPEG plus the full-frame radiometric temperature matrix via
    /// the 2046 JPEGPIC_WITH_APPENDDATA command (group 3 sub 9, DoubleGet).
    ///
    /// Returns a 120x160 JPEG and 120x160 f32 temperatures in °C (row-major).
    /// The temperature map matches the OSD values exactly (verified); display
    /// coordinates (240x320) map to the matrix by dividing by 2.
    pub fn capture_radiometric(&self) -> Result<RadiometricCapture> {
        let mut req = vec![0u8; 13];
        req[0] = 1; // byChannelID
        req[8..10].copy_from_slice(&2026u16.to_le_bytes()); // wYear
        let buf = self.double_get(3, 9, &req)?;
        // Payload: [0] u8 tag, [1..5] u32 jpeg len, [5..9] u32 width,
        // [9..13] u32 height, [13..17] u32 temp data bytes, [17..27] reserved,
        // [27..27+jl] JPEG, then width*height f32 LE temperatures (°C).
        if buf.len() < 27 {
            bail!("2046 response too short: {} bytes", buf.len());
        }
        let u32at = |o: usize| u32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
        let jpeg_len = u32at(1) as usize;
        let width = u32at(5) as usize;
        let height = u32at(9) as usize;
        let temp_bytes = u32at(13) as usize;
        if temp_bytes != width * height * 4 {
            bail!("2046 unexpected temp data size {temp_bytes} for {width}x{height}");
        }
        let jpeg_end = 27 + jpeg_len;
        let temp_end = jpeg_end + temp_bytes;
        if buf.len() < temp_end {
            bail!("2046 response truncated: {} < {temp_end}", buf.len());
        }
        let jpeg = buf[27..jpeg_end].to_vec();
        let temps = buf[jpeg_end..temp_end]
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        Ok(RadiometricCapture { jpeg, width, height, temps })
    }
}

/// One radiometric capture: JPEG plus per-pixel temperatures.
#[derive(Debug)]
pub struct RadiometricCapture {
    pub jpeg: Vec<u8>,
    pub width: usize,
    pub height: usize,
    /// Row-major, Celsius.
    pub temps: Vec<f32>,
}

impl RadiometricCapture {
    /// Temperature at matrix coordinates.
    pub fn temp_at(&self, x: usize, y: usize) -> Option<f32> {
        if x < self.width && y < self.height {
            Some(self.temps[y * self.width + x])
        } else {
            None
        }
    }

    /// Max temperature and its matrix coordinates.
    pub fn max_temp(&self) -> Option<(f32, usize, usize)> {
        let (i, &t) = self
            .temps
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))?;
        Some((t, i % self.width, i / self.width))
    }
}

/// Rectangle in native 480x640 coordinates for [`Device::roi_max_temperatures`].
#[derive(Debug, Clone, Copy)]
pub struct RoiRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Distance to target in cm (100 is the device default).
    pub distance: u32,
}

/// One 21-byte result block: the max temperature found and where.
#[derive(Debug, Clone, Copy)]
pub struct RoiBlock {
    pub id: u8,
    /// Max temperature in 0.1 °C units.
    pub max_temp_raw: u32,
    /// Location of the max, in native 480x640 coordinates.
    pub thermal_x: u32,
    pub thermal_y: u32,
}

impl RoiBlock {
    pub fn temperature_c(&self) -> f64 {
        self.max_temp_raw as f64 / 10.0
    }
}

#[derive(Debug)]
pub struct RoiSearchResult {
    /// Max over the whole frame.
    pub global: RoiBlock,
    /// Per-region results, in request order.
    pub regions: Vec<RoiBlock>,
}

impl Drop for Device {
    fn drop(&mut self) {
        for iface in [IF_STREAM, IF_CONTROL] {
            let _ = self.handle.release_interface(iface);
            let _ = self.handle.attach_kernel_driver(iface);
        }
    }
}

pub struct FrameIter<'a> {
    dev: &'a Device,
    buf: Vec<u8>,
    frame: Vec<u8>,
    in_frame: bool,
}

impl Iterator for FrameIter<'_> {
    type Item = Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.dev.read_payload(&mut self.buf) {
                Ok(0) => continue,
                Ok(_) => {}
                Err(e) => return Some(Err(e)),
            }
            if self.buf.starts_with(&[0xff, 0xd8]) {
                self.in_frame = true;
                self.frame.clear();
            }
            if !self.in_frame {
                continue;
            }
            self.frame.extend_from_slice(&self.buf);
            if self.frame.windows(2).any(|w| w == [0xff, 0xd9]) {
                self.in_frame = false;
                return Some(Ok(std::mem::take(&mut self.frame)));
            }
        }
    }
}
