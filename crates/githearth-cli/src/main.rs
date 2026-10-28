use githearth_core::{ObjectId, RepositoryId, RepositoryReader};
use githearth_git::layout::{ObjectFormat, Storage};
use std::{
    error::Error,
    io::{self, Write},
};
use uuid::Uuid;

fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        return Err(
            "usage: githearth <init|refs|blob> <storage-root> <repository-id> [object-id]".into(),
        );
    }
    let expected = if args[0] == "blob" { 4 } else { 3 };
    if args.len() != expected {
        return Err("incorrect number of arguments".into());
    }
    let id: RepositoryId = args[2].parse()?;
    let storage = Storage::open(&args[1])?;
    match args[0].as_str() {
        "init" => {
            let uuid = Uuid::parse_str(&id.to_string())
                .map_err(|e| format!("invalid repository id: {e}"))?;
            println!(
                "{}",
                storage
                    .create(uuid, Uuid::new_v4(), ObjectFormat::Sha1)?
                    .display()
            );
        }
        "refs" => {
            for reference in storage.list_refs(&id)? {
                println!("{} {}", reference.target, reference.name);
            }
        }
        "blob" => {
            let oid: ObjectId = args[3].parse()?;
            io::stdout()
                .lock()
                .write_all(&storage.read_blob(&id, &oid, 8 * 1024 * 1024)?)?;
        }
        _ => return Err("unknown command; expected init, refs, or blob".into()),
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("githearth: {error}");
        std::process::exit(1);
    }
}
