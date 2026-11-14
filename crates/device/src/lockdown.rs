//! Client for lockdownd, the device service on port 62078 that reports device
//! information and starts other services.
//!
//! Messages are XML plists, each preceded by its length as a big-endian u32.
//! A few values can be read straight away; the rest, and starting services,
//! need a session, which switches the connection to TLS using the pair
//! record's host certificate.

use crate::pair::{ssl_error, PairRecord};
use crate::{Device, Error, Result, Stream};
use openssl::error::ErrorStack;
use openssl::pkey::PKey;
use openssl::ssl::{HandshakeError, Ssl, SslContext, SslMethod, SslOptions, SslVerifyMode};
use openssl::x509::X509;
use plist::{Dictionary, Value};
use std::io::{self, Read, Write};

pub const PORT: u16 = 62078;

pub struct Lockdown {
    stream: Box<dyn Stream>,
}

impl Lockdown {
    pub fn connect(device: &Device) -> Result<Lockdown> {
        let mut lockdown = Lockdown {
            stream: device.connect(PORT)?,
        };
        let reply = lockdown.request("QueryType", Dictionary::new())?;
        match reply.get("Type").and_then(Value::as_string) {
            Some("com.apple.mobile.lockdown") => Ok(lockdown),
            other => Err(Error::protocol(format!("not lockdownd: {other:?}"))),
        }
    }

    /// Reads a device value such as "ProductVersion". Returns `None` when the
    /// device doesn't have the key or won't reveal it without a session.
    pub fn get_value(&mut self, key: &str) -> Result<Option<Value>> {
        self.get_domain_value(None, Some(key))
    }

    /// Reads a value from a domain such as "com.apple.mobile.iTunes"; with
    /// no key, the whole domain.
    pub fn get_domain_value(
        &mut self,
        domain: Option<&str>,
        key: Option<&str>,
    ) -> Result<Option<Value>> {
        let mut args = Dictionary::new();
        if let Some(domain) = domain {
            args.insert("Domain".into(), domain.into());
        }
        if let Some(key) = key {
            args.insert("Key".into(), key.into());
        }
        let reply = self.request("GetValue", args)?;
        Ok(reply.get("Value").cloned())
    }

    /// Pairs with the device, which stores the record's certificates. Old
    /// firmware does this silently; iOS 7 and later ask the user to trust.
    pub fn pair(&mut self, record: &PairRecord) -> Result<()> {
        self.request("Pair", pair_args(record)).map(drop)
    }

    /// Starts a session, encrypted if the device asks for it. Firmware before
    /// iOS 7 wants the pairing validated first.
    pub fn start_session(&mut self, record: &PairRecord) -> Result<()> {
        let major = self
            .get_value("ProductVersion")
            .ok()
            .flatten()
            .and_then(|v| v.as_string()?.split('.').next()?.parse::<u32>().ok());
        if major.is_none_or(|m| m < 7) {
            self.request("ValidatePair", pair_args(record))?;
        }
        let mut args = Dictionary::new();
        args.insert("HostID".into(), record.host_id.clone().into());
        args.insert("SystemBUID".into(), record.system_buid.clone().into());
        let reply = self.request("StartSession", args)?;
        if reply.get("EnableSessionSSL").and_then(Value::as_boolean) == Some(true) {
            let plain = std::mem::replace(&mut self.stream, Box::new(io::Cursor::new(Vec::new())));
            self.stream = tls(plain, record)?;
        }
        Ok(())
    }

    /// Starts a service on the device. Returns its port and whether it wants
    /// TLS.
    pub fn start_service(&mut self, service: &str) -> Result<(u16, bool)> {
        let mut args = Dictionary::new();
        args.insert("Service".into(), service.into());
        let reply = self.request("StartService", args)?;
        let port = reply
            .get("Port")
            .and_then(Value::as_unsigned_integer)
            .and_then(|p| u16::try_from(p).ok())
            .ok_or_else(|| Error::protocol(format!("StartService {service}: no port")))?;
        let ssl = reply.get("EnableServiceSSL").and_then(Value::as_boolean) == Some(true);
        Ok((port, ssl))
    }

    fn request(&mut self, request: &str, mut args: Dictionary) -> Result<Dictionary> {
        args.insert("Label".into(), "bettertunes".into());
        args.insert("Request".into(), request.into());
        write_message(&mut self.stream, &args)?;
        let reply = read_message(&mut self.stream)?;
        if let Some(err) = reply.get("Error").and_then(Value::as_string) {
            // Values hidden before a session are reported as missing.
            if request == "GetValue" && err == "MissingValue" {
                return Ok(Dictionary::new());
            }
            return Err(Error::Refused(format!("lockdownd {request}: {err}")));
        }
        Ok(reply)
    }
}

fn pair_args(record: &PairRecord) -> Dictionary {
    let mut args = Dictionary::new();
    args.insert(
        "PairRecord".into(),
        Value::Dictionary(record.request_dict()),
    );
    args.insert("ProtocolVersion".into(), "2".into());
    args
}

/// Wraps a connection in TLS as the client, presenting the host certificate.
/// Anything goes, down to SSL 3, because the oldest firmware knows nothing
/// newer than TLS 1.0. The device's certificate isn't checked: the record
/// pins it, but the device presents it only for the session's sake.
pub(crate) fn tls(stream: Box<dyn Stream>, record: &PairRecord) -> Result<Box<dyn Stream>> {
    let build = || -> std::result::Result<Ssl, ErrorStack> {
        let cert = X509::from_pem(&record.host_certificate)?;
        let key = PKey::private_key_from_pem(&record.host_private_key)?;
        let mut ctx = SslContext::builder(SslMethod::tls_client())?;
        ctx.set_security_level(0);
        ctx.set_min_proto_version(None)?;
        ctx.set_cipher_list("ALL:@SECLEVEL=0")?;
        ctx.set_verify(SslVerifyMode::NONE);
        // Firmware from before RFC 5746 (2010) can't do secure renegotiation.
        ctx.set_options(SslOptions::ALLOW_UNSAFE_LEGACY_RENEGOTIATION);
        ctx.set_certificate(&cert)?;
        ctx.set_private_key(&key)?;
        Ssl::new(&ctx.build())
    };
    let ssl = build().map_err(ssl_error)?;
    match ssl.connect(stream) {
        Ok(stream) => Ok(Box::new(stream)),
        Err(HandshakeError::SetupFailure(e)) => Err(ssl_error(e)),
        Err(HandshakeError::Failure(mid) | HandshakeError::WouldBlock(mid)) => {
            Err(ssl_error(mid.error()))
        }
    }
}

pub(crate) fn write_message(w: &mut impl Write, msg: &Dictionary) -> Result<()> {
    let mut body = Vec::new();
    plist::to_writer_xml(&mut body, msg)?;
    w.write_all(&(body.len() as u32).to_be_bytes())?;
    w.write_all(&body)?;
    Ok(())
}

pub(crate) fn read_message(r: &mut impl Read) -> Result<Dictionary> {
    let mut len = [0; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > 1 << 24 {
        return Err(Error::protocol(format!("bad message length {len}")));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    Ok(plist::from_bytes(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_messages() {
        let mut msg = Dictionary::new();
        msg.insert("Request".into(), "QueryType".into());
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        assert_eq!(
            u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize,
            buf.len() - 4
        );
        assert_eq!(read_message(&mut &buf[..]).unwrap(), msg);
    }
}
