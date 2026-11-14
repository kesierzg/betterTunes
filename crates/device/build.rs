// openssl-src is a build dependency only for its features (see Cargo.toml);
// openssl-sys's build script does the actual building.
use openssl_src as _;

fn main() {}
