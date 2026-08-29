//! Stream Deck protocol (gen 2 devices: JPEG key images, 1024-byte reports).

use crate::hidraw::{HidDevice, HidDeviceInfo, enumerate};
use anyhow::{Result, bail};

pub const ELGATO_VID: u16 = 0x0fd9;

const REPORT_LEN: usize = 1024;
const IMAGE_HEADER_LEN: usize = 8;
const FEATURE_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kind {
    pub product_id: u16,
    pub name: &'static str,
    pub keys: u8,
    pub cols: u8,
    pub rows: u8,
    /// Edge length of the square key image, in pixels.
    pub image_size: u32,
    /// Rotate the image by 180° (how every deck except the Plus is built).
    pub rotate180: bool,
}

/// Known gen 2 devices. Gen 1 hardware (Original 0x0060, Mini 0x0063) speaks a
/// different protocol (BMP, 8192-byte reports) and is deliberately left out.
pub const KINDS: &[Kind] = &[
    Kind {
        product_id: 0x006d,
        name: "Stream Deck Original V2",
        keys: 15,
        cols: 5,
        rows: 3,
        image_size: 72,
        rotate180: true,
    },
    Kind {
        product_id: 0x0080,
        name: "Stream Deck MK.2",
        keys: 15,
        cols: 5,
        rows: 3,
        image_size: 72,
        rotate180: true,
    },
    Kind {
        product_id: 0x00ba,
        name: "Stream Deck MK.2 Scissor",
        keys: 15,
        cols: 5,
        rows: 3,
        image_size: 72,
        rotate180: true,
    },
    Kind {
        product_id: 0x006c,
        name: "Stream Deck XL",
        keys: 32,
        cols: 8,
        rows: 4,
        image_size: 96,
        rotate180: true,
    },
    Kind {
        product_id: 0x008f,
        name: "Stream Deck XL V2",
        keys: 32,
        cols: 8,
        rows: 4,
        image_size: 96,
        rotate180: true,
    },
    Kind {
        product_id: 0x0084,
        name: "Stream Deck +",
        keys: 8,
        cols: 4,
        rows: 2,
        image_size: 120,
        rotate180: false,
    },
    Kind {
        product_id: 0x009a,
        name: "Stream Deck Neo",
        keys: 8,
        cols: 4,
        rows: 2,
        image_size: 96,
        rotate180: true,
    },
];

pub fn kind_for(product_id: u16) -> Option<&'static Kind> {
    KINDS.iter().find(|k| k.product_id == product_id)
}

/// Find every attached Stream Deck. `serial` optionally narrows this down to
/// one device, which matters once more than one deck is plugged in.
pub fn find_devices(serial: Option<&str>) -> Result<Vec<(HidDeviceInfo, &'static Kind)>> {
    let mut found = Vec::new();
    for info in enumerate()? {
        if info.vendor_id != ELGATO_VID {
            continue;
        }
        let Some(kind) = kind_for(info.product_id) else {
            log::warn!(
                "Elgato device {:04x}:{:04x} ({}) is not supported",
                info.vendor_id,
                info.product_id,
                info.name
            );
            continue;
        };
        if let Some(want) = serial
            && info.serial != want
        {
            continue;
        }
        found.push((info, kind));
    }
    Ok(found)
}

pub struct StreamDeck {
    dev: HidDevice,
    pub kind: &'static Kind,
    /// Last reported state of each key, used for edge detection.
    key_states: Vec<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    Down(u8),
    Up(u8),
}

impl StreamDeck {
    pub fn open(info: &HidDeviceInfo, kind: &'static Kind) -> Result<Self> {
        let dev = HidDevice::open(info)?;
        Ok(Self {
            dev,
            kind,
            key_states: vec![false; kind.keys as usize],
        })
    }

    pub fn serial(&self) -> &str {
        &self.dev.info.serial
    }

    /// Firmware version (feature report 0x05).
    pub fn firmware_version(&self) -> Result<String> {
        let mut buf = [0u8; FEATURE_LEN];
        buf[0] = 0x05;
        let n = self.dev.get_feature(&mut buf)?;
        Ok(cstr(&buf[6.min(n)..n]))
    }

    /// Clear all keys and put the device back into its default state.
    pub fn reset(&self) -> Result<()> {
        let mut buf = [0u8; FEATURE_LEN];
        buf[0] = 0x03;
        buf[1] = 0x02;
        self.dev.send_feature(&buf)
    }

    /// Brightness in percent (0–100).
    pub fn set_brightness(&self, percent: u8) -> Result<()> {
        let mut buf = [0u8; FEATURE_LEN];
        buf[0] = 0x03;
        buf[1] = 0x08;
        buf[2] = percent.min(100);
        self.dev.send_feature(&buf)
    }

    /// Write a JPEG image to one key, in 1016-byte chunks.
    pub fn set_key_image(&self, key: u8, jpeg: &[u8]) -> Result<()> {
        if key >= self.kind.keys {
            bail!("no such key {key} (device has {})", self.kind.keys);
        }
        let max_chunk = REPORT_LEN - IMAGE_HEADER_LEN;
        let mut sent = 0usize;
        let mut page: u16 = 0;

        loop {
            let chunk = (jpeg.len() - sent).min(max_chunk);
            let last = sent + chunk == jpeg.len();

            let mut report = [0u8; REPORT_LEN];
            report[0] = 0x02;
            report[1] = 0x07;
            report[2] = key;
            report[3] = u8::from(last);
            report[4..6].copy_from_slice(&(chunk as u16).to_le_bytes());
            report[6..8].copy_from_slice(&page.to_le_bytes());
            report[IMAGE_HEADER_LEN..IMAGE_HEADER_LEN + chunk]
                .copy_from_slice(&jpeg[sent..sent + chunk]);

            self.dev.write_report(&report)?;

            sent += chunk;
            page += 1;
            if last {
                break;
            }
        }
        Ok(())
    }

    /// Wait for key events. An empty vector means the poll timed out.
    pub fn poll_events(&mut self, timeout_ms: i32) -> Result<Vec<KeyEvent>> {
        let mut buf = [0u8; REPORT_LEN];
        let n = self.dev.read_timeout(&mut buf, timeout_ms)?;
        let keys = self.kind.keys as usize;

        // Gen 2 key report: 01 00 <count:u16le> <state per key>
        if n < 4 + keys || buf[0] != 0x01 || buf[1] != 0x00 {
            return Ok(Vec::new());
        }

        let mut events = Vec::new();
        for i in 0..keys {
            let pressed = buf[4 + i] != 0;
            if pressed != self.key_states[i] {
                self.key_states[i] = pressed;
                events.push(if pressed {
                    KeyEvent::Down(i as u8)
                } else {
                    KeyEvent::Up(i as u8)
                });
            }
        }
        Ok(events)
    }
}

fn cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}
