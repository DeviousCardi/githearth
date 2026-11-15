//! Verification helper for `scripts/verify-offhost-backup.py`.
//!
//! Decrypts a `githearth-server backup` archive and prints its manifest and
//! header as JSON. It is not part of the installed binary or its behaviour: it
//! exists so a verification drill can assert the manifest that `restore`
//! authenticates without adding an operator-facing inspect command.
//!
//! Usage: `backup-inspect <archive.ghbackup> <key-file>`

use std::{env, fs, path::Path, process::Command};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() != 2 {
        return Err("usage: backup-inspect <archive.ghbackup> <key-file>".into());
    }
    let key = githearth_server::backup::read_key_file(Path::new(&args[1]))?;
    let directory = env::temp_dir().join(format!(
        "githearth-backup-inspect-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    fs::create_dir_all(&directory)?;
    let payload = directory.join("payload.tar");
    let header = githearth_server::backup::decrypt_file(Path::new(&args[0]), &payload, &key)?;
    let root = directory.join("root");
    fs::create_dir_all(&root)?;
    let status = Command::new("tar")
        .arg("-xf")
        .arg(&payload)
        .arg("-C")
        .arg(&root)
        .status()?;
    if !status.success() {
        return Err("cannot extract the decrypted payload tar".into());
    }
    let manifest = githearth_server::backup::read_manifest(&root)?;
    println!("MANIFEST {}", serde_json::to_string(&manifest)?);
    println!("HEADER {}", serde_json::to_string(&header)?);
    let _ = fs::remove_dir_all(&directory);
    Ok(())
}
