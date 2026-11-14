//! Device access for betterTunes.
//!
//! iPhones and iPod touches are reached through usbmuxd (Apple's on macOS,
//! the open-source one on Linux), which tunnels TCP connections to services
//! on the device over USB. Devices the system usbmuxd ignores, such as the
//! first iPod touch on current macOS, are driven over USB directly. Classic
//! iPods are plain USB disks.

pub mod afc;
pub mod ipod;
pub mod lockdown;
pub mod notify;
pub mod pair;
pub mod usb;
pub mod usbmux;

use std::fmt;
use std::io::{Read, Write};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// The device or usbmuxd sent something unexpected.
    Protocol(String),
    /// The device or usbmuxd refused a request.
    Refused(String),
    /// The device has to be paired with this computer first.
    NotPaired,
}

impl Error {
    pub(crate) fn protocol(msg: impl Into<String>) -> Self {
        Error::Protocol(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Protocol(msg) => write!(f, "protocol error: {msg}"),
            Error::Refused(msg) => write!(f, "refused: {msg}"),
            Error::NotPaired => write!(f, "device isn't paired with this computer"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<nusb::Error> for Error {
    fn from(e: nusb::Error) -> Self {
        Error::Io(e.into())
    }
}

impl From<plist::Error> for Error {
    fn from(e: plist::Error) -> Self {
        Error::Protocol(format!("bad plist: {e}"))
    }
}

/// A byte stream to a service on a device.
pub trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

/// An iPhone, iPod touch or iPad.
pub struct Device {
    pub udid: String,
    link: Link,
}

enum Link {
    Usbmuxd(usbmux::Device),
    Direct(usb::Mux),
}

impl Device {
    /// "USB" or "Network" through usbmuxd, or "USB (direct)".
    pub fn connection(&self) -> &str {
        match &self.link {
            Link::Usbmuxd(d) => &d.connection,
            Link::Direct(_) => "USB (direct)",
        }
    }

    /// Opens a connection to `port` on the device.
    pub fn connect(&self, port: u16) -> Result<Box<dyn Stream>> {
        Ok(match &self.link {
            Link::Usbmuxd(d) => Box::new(usbmux::Client::connect()?.connect_to(d, port)?),
            Link::Direct(mux) => Box::new(mux.connect(port)?),
        })
    }

    /// The pair record for this device: usbmuxd's, or betterTunes' own for
    /// devices driven directly.
    pub fn pair_record(&self) -> Result<Option<pair::PairRecord>> {
        match &self.link {
            Link::Usbmuxd(_) => usbmux::Client::connect()?
                .read_pair_record(&self.udid)?
                .map(|bytes| pair::PairRecord::from_plist(&bytes))
                .transpose(),
            Link::Direct(_) => pair::Store::open()?.get(&self.udid),
        }
    }

    /// Pairs a device that betterTunes drives directly and keeps the record.
    /// This stores betterTunes' certificates on the device, as iTunes does on
    /// first connection; old firmware doesn't ask the user.
    pub fn pair(&self) -> Result<()> {
        if !matches!(self.link, Link::Direct(_)) {
            return Err(Error::Refused(
                "devices on the system usbmuxd are paired by it (iTunes, Finder or a trust prompt)"
                    .into(),
            ));
        }
        let store = pair::Store::open()?;
        let mut lockdown = lockdown::Lockdown::connect(self)?;
        let key = lockdown
            .get_value("DevicePublicKey")?
            .and_then(|v| v.as_data().map(<[u8]>::to_vec))
            .ok_or_else(|| Error::protocol("device has no DevicePublicKey"))?;
        let record = pair::PairRecord::generate(&key, &store.system_buid()?)?;
        lockdown.pair(&record)?;
        store.put(&self.udid, &record)?;
        // Make sure the pairing works before reporting success.
        lockdown::Lockdown::connect(self)?.start_session(&record)
    }

    /// Connects to lockdownd and starts a session.
    pub fn session(&self) -> Result<lockdown::Lockdown> {
        let record = self.pair_record()?.ok_or(Error::NotPaired)?;
        let mut lockdown = lockdown::Lockdown::connect(self)?;
        lockdown.start_session(&record)?;
        Ok(lockdown)
    }

    /// Total and free bytes of the media partition, and bytes taken by
    /// synced music.
    pub fn storage(&self) -> Result<(u64, u64, u64)> {
        let mut afc = afc::Afc::new(self.start_service(afc::SERVICE)?);
        let info = afc.device_info()?;
        let number = |key: &str| {
            info.get(key)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
        };
        let music = afc.disk_usage("/iTunes_Control/Music").unwrap_or(0);
        Ok((number("FSTotalBytes"), number("FSFreeBytes"), music))
    }

    /// Starts a service such as "com.apple.afc" and connects to it.
    pub fn start_service(&self, service: &str) -> Result<Box<dyn Stream>> {
        let record = self.pair_record()?.ok_or(Error::NotPaired)?;
        let mut lockdown = lockdown::Lockdown::connect(self)?;
        lockdown.start_session(&record)?;
        let (port, ssl) = lockdown.start_service(service)?;
        let stream = self.connect(port)?;
        if ssl {
            lockdown::tls(stream, &record)
        } else {
            Ok(stream)
        }
    }
}

/// Finds iOS devices: those the system usbmuxd knows, over USB rather than
/// Wi-Fi when both, plus any on USB that it ignores, which are opened
/// directly. Problems that don't stop the search go to `warn`.
pub fn find_devices(mut warn: impl FnMut(Error)) -> Vec<Device> {
    let mut known = match usbmux::Client::connect().and_then(|mut c| c.list_devices()) {
        Ok(list) => list,
        Err(e) => {
            warn(e);
            Vec::new()
        }
    };
    known.sort_by_key(|d| d.connection != "USB");
    let mut devices: Vec<Device> = Vec::new();
    for d in known {
        if !devices.iter().any(|seen| seen.udid == d.udid) {
            devices.push(Device {
                udid: d.udid.clone(),
                link: Link::Usbmuxd(d),
            });
        }
    }
    // USB serial numbers are UDIDs, without the dash on newer devices.
    let plain = |s: &str| s.replace('-', "").to_ascii_lowercase();
    let usb = usb::list().unwrap_or_else(|e| {
        warn(e);
        Vec::new()
    });
    for info in usb {
        let serial = info.serial_number().unwrap_or_default().to_owned();
        if devices.iter().any(|d| plain(&d.udid) == plain(&serial)) {
            continue;
        }
        match usb::Mux::open(&info) {
            Ok(mux) => devices.push(Device {
                udid: serial,
                link: Link::Direct(mux),
            }),
            Err(e) => warn(Error::Refused(format!("USB device {serial}: {e}"))),
        }
    }
    devices
}

/// Marketing name for an Apple `ProductType` such as "iPhone7,2".
pub fn model_name(product_type: &str) -> Option<&'static str> {
    Some(match product_type {
        "iPod1,1" => "iPod touch (1st generation)",
        "iPod2,1" => "iPod touch (2nd generation)",
        "iPod3,1" => "iPod touch (3rd generation)",
        "iPod4,1" => "iPod touch (4th generation)",
        "iPod5,1" => "iPod touch (5th generation)",
        "iPhone1,1" => "iPhone",
        "iPhone1,2" => "iPhone 3G",
        "iPhone2,1" => "iPhone 3GS",
        "iPhone3,1" | "iPhone3,2" | "iPhone3,3" => "iPhone 4",
        "iPhone4,1" => "iPhone 4S",
        "iPhone5,1" | "iPhone5,2" => "iPhone 5",
        "iPhone5,3" | "iPhone5,4" => "iPhone 5c",
        "iPhone6,1" | "iPhone6,2" => "iPhone 5s",
        "iPhone7,2" => "iPhone 6",
        "iPhone7,1" => "iPhone 6 Plus",
        _ => return None,
    })
}
