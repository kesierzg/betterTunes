//! Pair records: the certificates a host and a device exchange once, so that
//! later sessions can be encrypted.
//!
//! For devices the system usbmuxd knows, it keeps the records and hands them
//! out. For devices betterTunes drives directly it keeps its own, in the same
//! plist format, under the user's data folder.

use crate::{Error, Result};
use openssl::asn1::{Asn1Integer, Asn1Time};
use openssl::bn::BigNum;
use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Private};
use openssl::rsa::Rsa;
use openssl::x509::extension::{BasicConstraints, KeyUsage, SubjectKeyIdentifier};
use openssl::x509::{X509Builder, X509Name, X509};
use plist::{Dictionary, Value};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct PairRecord {
    /// PEM certificates and keys.
    pub device_certificate: Vec<u8>,
    pub host_certificate: Vec<u8>,
    pub host_private_key: Vec<u8>,
    pub root_certificate: Vec<u8>,
    pub root_private_key: Vec<u8>,
    pub host_id: String,
    pub system_buid: String,
}

impl PairRecord {
    pub fn from_plist(bytes: &[u8]) -> Result<PairRecord> {
        let dict: Dictionary = plist::from_bytes(bytes)?;
        let data = |key: &str| {
            dict.get(key)
                .and_then(Value::as_data)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| Error::protocol(format!("pair record has no {key}")))
        };
        let string = |key: &str| {
            dict.get(key)
                .and_then(Value::as_string)
                .map(str::to_owned)
                .ok_or_else(|| Error::protocol(format!("pair record has no {key}")))
        };
        Ok(PairRecord {
            device_certificate: data("DeviceCertificate")?,
            host_certificate: data("HostCertificate")?,
            host_private_key: data("HostPrivateKey")?,
            root_certificate: data("RootCertificate")?,
            root_private_key: data("RootPrivateKey")?,
            host_id: string("HostID")?,
            system_buid: string("SystemBUID").unwrap_or_default(),
        })
    }

    pub fn to_plist(&self) -> Result<Vec<u8>> {
        let mut dict = Dictionary::new();
        for (key, value) in [
            ("DeviceCertificate", &self.device_certificate),
            ("HostCertificate", &self.host_certificate),
            ("HostPrivateKey", &self.host_private_key),
            ("RootCertificate", &self.root_certificate),
            ("RootPrivateKey", &self.root_private_key),
        ] {
            dict.insert(key.into(), Value::Data(value.clone()));
        }
        dict.insert("HostID".into(), self.host_id.clone().into());
        dict.insert("SystemBUID".into(), self.system_buid.clone().into());
        let mut out = Vec::new();
        plist::to_writer_xml(&mut out, &dict)?;
        Ok(out)
    }

    /// Makes a new record for a device from its `DevicePublicKey`, as iTunes
    /// does: a root CA, and host and device certificates signed by it. SHA-1
    /// signatures, because that's what the oldest firmware understands.
    pub fn generate(device_public_key: &[u8], system_buid: &str) -> Result<PairRecord> {
        let device_key = Rsa::public_key_from_pem_pkcs1(device_public_key)
            .or_else(|_| Rsa::public_key_from_pem(device_public_key))
            .map_err(|e| Error::protocol(format!("bad device public key: {e}")))?;
        let device_key = PKey::from_rsa(device_key).map_err(ssl_error)?;
        let root_key =
            PKey::from_rsa(Rsa::generate(2048).map_err(ssl_error)?).map_err(ssl_error)?;
        let host_key =
            PKey::from_rsa(Rsa::generate(2048).map_err(ssl_error)?).map_err(ssl_error)?;

        let root = certificate(&root_key, &root_key, true)?;
        let host = certificate(&host_key, &root_key, false)?;
        let device = certificate(&device_key, &root_key, false)?;
        let pem = |c: &X509| c.to_pem().map_err(ssl_error);
        let key_pem = |k: &PKey<Private>| k.private_key_to_pem_pkcs8().map_err(ssl_error);
        Ok(PairRecord {
            device_certificate: pem(&device)?,
            host_certificate: pem(&host)?,
            host_private_key: key_pem(&host_key)?,
            root_certificate: pem(&root)?,
            root_private_key: key_pem(&root_key)?,
            host_id: new_uuid()?,
            system_buid: system_buid.to_owned(),
        })
    }

    /// The certificates sent with Pair and ValidatePair.
    pub(crate) fn request_dict(&self) -> Dictionary {
        let mut dict = Dictionary::new();
        dict.insert(
            "DeviceCertificate".into(),
            Value::Data(self.device_certificate.clone()),
        );
        dict.insert(
            "HostCertificate".into(),
            Value::Data(self.host_certificate.clone()),
        );
        dict.insert(
            "RootCertificate".into(),
            Value::Data(self.root_certificate.clone()),
        );
        dict.insert("HostID".into(), self.host_id.clone().into());
        dict.insert("SystemBUID".into(), self.system_buid.clone().into());
        dict
    }
}

fn certificate<T: openssl::pkey::HasPublic>(
    key: &PKey<T>,
    issuer_key: &PKey<Private>,
    ca: bool,
) -> Result<X509> {
    let build = || -> std::result::Result<X509, openssl::error::ErrorStack> {
        let mut b = X509Builder::new()?;
        b.set_version(2)?;
        let serial = Asn1Integer::from_bn(BigNum::from_u32(0)?.as_ref())?;
        b.set_serial_number(&serial)?;
        let empty = X509Name::builder()?.build();
        b.set_subject_name(&empty)?;
        b.set_issuer_name(&empty)?;
        let (start, end) = (
            Asn1Time::days_from_now(0)?,
            Asn1Time::days_from_now(10 * 365)?,
        );
        b.set_not_before(&start)?;
        b.set_not_after(&end)?;
        b.set_pubkey(key)?;
        if ca {
            b.append_extension(BasicConstraints::new().critical().ca().build()?)?;
        } else {
            b.append_extension(BasicConstraints::new().critical().build()?)?;
            b.append_extension(
                KeyUsage::new()
                    .critical()
                    .digital_signature()
                    .key_encipherment()
                    .build()?,
            )?;
        }
        let ski = SubjectKeyIdentifier::new().build(&b.x509v3_context(None, None))?;
        b.append_extension(ski)?;
        b.sign(issuer_key, MessageDigest::sha1())?;
        Ok(b.build())
    };
    build().map_err(ssl_error)
}

/// A random UUID in Apple's uppercase form.
pub fn new_uuid() -> Result<String> {
    let mut b = [0u8; 16];
    openssl::rand::rand_bytes(&mut b).map_err(ssl_error)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02X}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

pub(crate) fn ssl_error(e: impl std::fmt::Display) -> Error {
    Error::Protocol(format!("TLS: {e}"))
}

/// betterTunes' own pair records, for devices it drives directly.
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// `~/Library/Application Support/betterTunes/lockdown` on macOS,
    /// `$XDG_DATA_HOME/bettertunes/lockdown` elsewhere.
    pub fn open() -> Result<Store> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| Error::protocol("HOME is not set"))?;
        let base = if cfg!(target_os = "macos") {
            home.join("Library/Application Support/betterTunes")
        } else {
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/share"))
                .join("bettertunes")
        };
        Ok(Store {
            dir: base.join("lockdown"),
        })
    }

    pub fn get(&self, udid: &str) -> Result<Option<PairRecord>> {
        match std::fs::read(self.dir.join(format!("{udid}.plist"))) {
            Ok(bytes) => PairRecord::from_plist(&bytes).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn put(&self, udid: &str, record: &PairRecord) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("{udid}.plist"));
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, record.to_plist()?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    /// This host's ID, made once and kept.
    pub fn system_buid(&self) -> Result<String> {
        let path = self.dir.join("SystemBUID");
        if let Ok(id) = std::fs::read_to_string(&path) {
            return Ok(id.trim().to_owned());
        }
        std::fs::create_dir_all(&self.dir)?;
        let id = new_uuid()?;
        std::fs::write(path, &id)?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_records_that_round_trip() {
        let device = Rsa::generate(1024).unwrap();
        let public = device.public_key_to_pem_pkcs1().unwrap();
        let record = PairRecord::generate(&public, "BUID").unwrap();
        let root = X509::from_pem(&record.root_certificate).unwrap();
        let host = X509::from_pem(&record.host_certificate).unwrap();
        let dev = X509::from_pem(&record.device_certificate).unwrap();
        let root_key = root.public_key().unwrap();
        assert!(host.verify(&root_key).unwrap());
        assert!(dev.verify(&root_key).unwrap());
        assert!(dev
            .public_key()
            .unwrap()
            .public_eq(&PKey::from_rsa(device).unwrap()));
        let again = PairRecord::from_plist(&record.to_plist().unwrap()).unwrap();
        assert_eq!(again.host_id, record.host_id);
        assert_eq!(again.host_private_key, record.host_private_key);
        assert_eq!(record.host_id.len(), 36);
    }
}
