//! Minimal HID access through the Linux kernel (`/dev/hidraw*`).
//!
//! Deliberately without `hidapi`/`libusb`: that saves the C headers
//! (libudev-devel, libusb-devel), which would have to be layered on top of an
//! rpm-ostree system. The kernel exposes everything needed via hidraw + sysfs.

use anyhow::{Context, Result, bail};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct HidDeviceInfo {
    pub path: PathBuf,
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial: String,
    pub name: String,
}

/// Read every hidraw node from sysfs (VID/PID/serial from `device/uevent`).
pub fn enumerate() -> Result<Vec<HidDeviceInfo>> {
    let mut out = Vec::new();
    let dir = match std::fs::read_dir("/sys/class/hidraw") {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).context("cannot read /sys/class/hidraw"),
    };

    for entry in dir.flatten() {
        let node = entry.file_name();
        let uevent = entry.path().join("device/uevent");
        let Ok(text) = std::fs::read_to_string(&uevent) else {
            continue;
        };

        let (mut vendor_id, mut product_id) = (0u16, 0u16);
        let (mut serial, mut name) = (String::new(), String::new());

        for line in text.lines() {
            if let Some(v) = line.strip_prefix("HID_ID=") {
                // Format: bus:vendor:product, each in hex
                let parts: Vec<&str> = v.trim().split(':').collect();
                if parts.len() == 3 {
                    vendor_id = u32::from_str_radix(parts[1], 16).unwrap_or(0) as u16;
                    product_id = u32::from_str_radix(parts[2], 16).unwrap_or(0) as u16;
                }
            } else if let Some(v) = line.strip_prefix("HID_UNIQ=") {
                serial = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("HID_NAME=") {
                name = v.trim().to_string();
            }
        }

        if vendor_id == 0 && product_id == 0 {
            continue;
        }

        out.push(HidDeviceInfo {
            path: Path::new("/dev").join(node),
            vendor_id,
            product_id,
            serial,
            name,
        });
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

pub struct HidDevice {
    file: File,
    pub info: HidDeviceInfo,
}

// _IOC(dir, type, nr, size) for HIDIOCSFEATURE/HIDIOCGFEATURE ('H', 0x06/0x07).
//
// `libc::Ioctl` is the request type of `ioctl` — `c_ulong` on glibc but
// `c_int` on musl, so it has to be spelled out rather than assumed.
fn hid_iowr(nr: u32, size: usize) -> libc::Ioctl {
    const DIR_RW: u32 = 3; // _IOC_WRITE | _IOC_READ
    const MAGIC: u32 = b'H' as u32;
    (((DIR_RW) << 30) | ((size as u32) << 16) | (MAGIC << 8) | nr) as libc::Ioctl
}

impl HidDevice {
    pub fn open(info: &HidDeviceInfo) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&info.path)
            .with_context(|| {
                format!(
                    "cannot open {} — missing permissions? See `rustydeck udev-rule`",
                    info.path.display()
                )
            })?;
        Ok(Self {
            file,
            info: info.clone(),
        })
    }

    /// Send an output report. `data[0]` is the report ID.
    pub fn write_report(&self, data: &[u8]) -> Result<()> {
        let written = unsafe {
            libc::write(
                self.file.as_raw_fd(),
                data.as_ptr() as *const libc::c_void,
                data.len(),
            )
        };
        if written < 0 {
            return Err(std::io::Error::last_os_error()).context("hidraw write failed");
        }
        if written as usize != data.len() {
            bail!("short hidraw write: {written} of {} bytes", data.len());
        }
        Ok(())
    }

    /// Set a feature report (`data[0]` = report ID).
    pub fn send_feature(&self, data: &[u8]) -> Result<()> {
        let mut buf = data.to_vec();
        let rc = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                hid_iowr(0x06, buf.len()),
                buf.as_mut_ptr(),
            )
        };
        if rc < 0 {
            return Err(std::io::Error::last_os_error()).context("HIDIOCSFEATURE failed");
        }
        Ok(())
    }

    /// Read a feature report; `buf[0]` must hold the report ID.
    pub fn get_feature(&self, buf: &mut [u8]) -> Result<usize> {
        let rc = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                hid_iowr(0x07, buf.len()),
                buf.as_mut_ptr(),
            )
        };
        if rc < 0 {
            return Err(std::io::Error::last_os_error()).context("HIDIOCGFEATURE failed");
        }
        Ok(rc as usize)
    }

    /// Read an input report. Returns `Ok(0)` when nothing arrived within
    /// `timeout_ms`.
    pub fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize> {
        let mut pfd = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                return Ok(0);
            }
            return Err(err).context("poll on hidraw failed");
        }
        if rc == 0 {
            return Ok(0);
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            bail!("device was disconnected");
        }

        match (&self.file).read(buf) {
            Ok(0) => bail!("device was disconnected (EOF)"),
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(0),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(0),
            Err(e) => Err(e).context("hidraw read failed"),
        }
    }
}
