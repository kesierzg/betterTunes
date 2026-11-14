//! The notification proxy, for posting system notifications on the device,
//! such as the ones that bracket a sync.

use crate::lockdown::{read_message, write_message};
use crate::{Device, Result};
use plist::Dictionary;

pub const SERVICE: &str = "com.apple.mobile.notification_proxy";

pub const SYNC_WILL_START: &str = "com.apple.itunes-mobdev.syncWillStart";
pub const SYNC_LOCK_REQUEST: &str = "com.apple.itunes-mobdev.syncLockRequest";
pub const SYNC_DID_START: &str = "com.apple.itunes-mobdev.syncDidStart";
pub const SYNC_DID_FINISH: &str = "com.apple.itunes-mobdev.syncDidFinish";
pub const SYNC_FAILED_TO_START: &str = "com.apple.itunes-mobdev.syncFailedToStart";

/// Posts one notification. Each goes over its own short connection, as
/// libgpod does.
pub fn post(device: &Device, name: &str) -> Result<()> {
    let mut stream = device.start_service(SERVICE)?;
    let mut msg = Dictionary::new();
    msg.insert("Command".into(), "PostNotification".into());
    msg.insert("Name".into(), name.into());
    write_message(&mut stream, &msg)?;
    let mut bye = Dictionary::new();
    bye.insert("Command".into(), "Shutdown".into());
    write_message(&mut stream, &bye)?;
    // The proxy answers the shutdown with "ProxyDeath" before closing.
    let _ = read_message(&mut stream);
    Ok(())
}
