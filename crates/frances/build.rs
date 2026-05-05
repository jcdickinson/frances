use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

fn main() {
    let path = env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();

    let mut rand = [0u8; 32];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut rand))
        .expect("read /dev/urandom");

    let combined = format!("{path}\n{unix}\n{}", hex(&rand));

    let mut hasher = Sha256::new();
    hasher.update(combined.as_bytes());
    let digest = hasher.finalize();

    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    let id = u64::from_be_bytes(bytes);

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR not set"));
    let out_path = out_dir.join("protocol_id.rs");
    fs::write(
        &out_path,
        format!("pub const PROTOCOL_VERSION: u64 = 0x{id:016x};\n"),
    )
    .expect("write protocol_id.rs");
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
