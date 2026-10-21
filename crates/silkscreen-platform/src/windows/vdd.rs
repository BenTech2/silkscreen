//! Parsec Virtual Display Driver (an IddCx driver) control: plug a virtual
//! monitor in for the session and unplug it afterwards, exactly like a cable.
//!
//! Interface per <https://github.com/nomi-san/parsec-vdd> (BSD-3-Clause). The
//! driver itself is installed separately (Parsec, or the standalone
//! `parsec-vdd` installer).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED,
    FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::{DeviceIoControl, GetOverlappedResultEx, OVERLAPPED};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, RegCloseKey,
    RegCreateKeyExW, RegSetValueExW,
};
use windows::Win32::System::Threading::CreateEventW;
use windows::core::{GUID, PCWSTR, w};

const VDD_ADAPTER_GUID: GUID = GUID::from_u128(0x00b41627_04c4_429e_a26e_0265cf50c8fa);
const IOCTL_ADD: u32 = 0x0022e004;
const IOCTL_REMOVE: u32 = 0x0022a008;
const IOCTL_UPDATE: u32 = 0x0022a00c;
const IOCTL_VERSION: u32 = 0x0022e010;

struct Device(HANDLE);
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

impl Device {
    fn open() -> Result<Self> {
        unsafe {
            let set = SetupDiGetClassDevsW(
                Some(&VDD_ADAPTER_GUID),
                PCWSTR::null(),
                None,
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )?;
            let mut iface = SP_DEVICE_INTERFACE_DATA {
                cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            let mut result = None;
            let mut i = 0;
            while SetupDiEnumDeviceInterfaces(set, None, &VDD_ADAPTER_GUID, i, &mut iface).is_ok() {
                i += 1;
                let mut needed = 0;
                let _ =
                    SetupDiGetDeviceInterfaceDetailW(set, &iface, None, 0, Some(&mut needed), None);
                let mut buf = vec![0u8; needed as usize];
                let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
                (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
                if SetupDiGetDeviceInterfaceDetailW(set, &iface, Some(detail), needed, None, None)
                    .is_err()
                {
                    continue;
                }
                let path = PCWSTR((*detail).DevicePath.as_ptr());
                if let Ok(h) = CreateFileW(
                    path,
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL
                        | FILE_FLAG_NO_BUFFERING
                        | FILE_FLAG_OVERLAPPED
                        | FILE_FLAG_WRITE_THROUGH,
                    None,
                ) {
                    result = Some(Device(h));
                    break;
                }
            }
            let _ = SetupDiDestroyDeviceInfoList(set);
            result.context("Parsec Virtual Display Driver is not installed")
        }
    }

    fn ioctl(&self, code: u32, input: &[u8]) -> Result<u32> {
        let mut inbuf = [0u8; 32];
        inbuf[..input.len().min(32)].copy_from_slice(&input[..input.len().min(32)]);
        let mut out = 0u32;
        unsafe {
            let event = CreateEventW(None, true, false, PCWSTR::null())?;
            let mut ov = OVERLAPPED {
                hEvent: event,
                ..Default::default()
            };
            let _ = DeviceIoControl(
                self.0,
                code,
                Some(inbuf.as_ptr() as _),
                inbuf.len() as u32,
                Some(&mut out as *mut u32 as _),
                4,
                None,
                Some(&mut ov),
            );
            let mut transferred = 0;
            let r = GetOverlappedResultEx(self.0, &ov, &mut transferred, 5000, false);
            let _ = CloseHandle(event);
            r.with_context(|| format!("VDD ioctl {code:#x}"))?;
        }
        Ok(out)
    }
}

/// A plugged-in virtual monitor. Dropping it unplugs the monitor.
pub struct VirtualDisplay {
    device: Arc<Device>,
    index: u32,
    stop: Arc<AtomicBool>,
    keepalive: Option<std::thread::JoinHandle<()>>,
}

pub fn version() -> Option<u32> {
    Device::open()
        .ok()
        .and_then(|d| d.ioctl(IOCTL_VERSION, &[]).ok())
}

/// Registers a custom mode in the first free preset slot (the driver reads
/// `HKLM\SOFTWARE\Parsec\vdd\{0..4}` when a display is plugged in). Needs
/// administrator rights; without them the driver's built-in modes are used.
fn register_mode(width: u32, height: u32, hz: u32) {
    let key_path: Vec<u16> = "SOFTWARE\\Parsec\\vdd\\0\0".encode_utf16().collect();
    unsafe {
        let mut key = HKEY::default();
        if RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key_path.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        ) != ERROR_SUCCESS
        {
            warn!(
                "could not register {width}x{height}@{hz} with the virtual display driver (run once as administrator \
                 to enable exact panel resolutions)"
            );
            return;
        }
        for (name, v) in [(w!("width"), width), (w!("height"), height), (w!("hz"), hz)] {
            let _ = RegSetValueExW(key, name, None, REG_DWORD, Some(&v.to_le_bytes()));
        }
        let _ = RegCloseKey(key);
    }
}

impl VirtualDisplay {
    pub fn plug(width: u32, height: u32, refresh_hz: u32) -> Result<Self> {
        let device = Arc::new(Device::open()?);
        register_mode(width, height, refresh_hz);
        let index = device.ioctl(IOCTL_ADD, &[])?;
        device.ioctl(IOCTL_UPDATE, &[])?;
        info!("plugged in virtual display #{index}");

        // The driver unplugs displays that are not pinged every ~100 ms.
        let stop = Arc::new(AtomicBool::new(false));
        let keepalive = {
            let (device, stop) = (device.clone(), stop.clone());
            std::thread::Builder::new()
                .name("vdd-keepalive".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        if let Err(e) = device.ioctl(IOCTL_UPDATE, &[]) {
                            debug!("vdd keepalive: {e:#}");
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                })?
        };
        Ok(VirtualDisplay {
            device,
            index,
            stop,
            keepalive: Some(keepalive),
        })
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.keepalive.take() {
            let _ = t.join();
        }
        // The driver expects the index big-endian.
        let idx = (self.index as u16).to_be_bytes();
        match self.device.ioctl(IOCTL_REMOVE, &idx) {
            Ok(_) => info!("unplugged virtual display #{}", self.index),
            Err(e) => warn!("could not unplug virtual display: {e:#}"),
        }
        let _ = self.device.ioctl(IOCTL_UPDATE, &[]);
    }
}

pub fn ensure_installed() -> Result<()> {
    if Device::open().is_err() {
        bail!(
            "no virtual display driver found. Install the Parsec Virtual Display Driver \
             (https://github.com/nomi-san/parsec-vdd) or use a dummy HDMI/DP plug with `--source monitor`"
        );
    }
    Ok(())
}
