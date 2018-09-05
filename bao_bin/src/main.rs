#[macro_use]
extern crate arrayref;
extern crate bao;
extern crate docopt;
extern crate failure;
extern crate hex;
extern crate os_pipe;
#[macro_use]
extern crate serde_derive;
extern crate memmap;

use failure::{err_msg, Error};
use std::fs::{File, OpenOptions};
use std::io;
use std::io::prelude::*;
use std::path::{Path, PathBuf};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "
Usage: bao hash [<input>] [--encoded]
       bao encode [<input>] [<output>]
       bao decode <hash> [<input>] [<output>] [--start=<offset>]
       bao slice <start> <len> [<input>] [<output>]
       bao decode-slice <hash> <start> <len> [<input>] [<output>]
       bao (--help | --version)
";

#[derive(Debug, Deserialize)]
struct Args {
    cmd_decode: bool,
    cmd_encode: bool,
    cmd_hash: bool,
    cmd_slice: bool,
    cmd_decode_slice: bool,
    arg_input: Option<PathBuf>,
    arg_output: Option<PathBuf>,
    arg_hash: String,
    arg_start: u64,
    arg_len: u64,
    flag_encoded: bool,
    flag_help: bool,
    flag_start: Option<u64>,
    flag_version: bool,
}

fn main() -> Result<(), Error> {
    // TODO: Error reporting.
    let args: Args = docopt::Docopt::new(USAGE)
        .and_then(|d| d.deserialize())
        .unwrap_or_else(|e| e.exit());

    let (in_file, out_file) = in_out_files(&args)?;

    if args.flag_help {
        print!("{}", USAGE);
    } else if args.flag_version {
        println!("{}", VERSION);
    } else if args.cmd_hash {
        if args.flag_encoded {
            hash_encoded(&args, in_file)?;
        } else {
            hash(&args, in_file)?;
        }
    } else if args.cmd_encode {
        encode(&args, in_file, out_file)?;
    } else if args.cmd_decode {
        decode(&args, in_file, out_file)?;
    } else if args.cmd_slice {
        slice(&args, in_file, out_file)?;
    } else if args.cmd_decode_slice {
        decode_slice(&args, in_file, out_file)?;
    } else {
        unreachable!();
    }

    Ok(())
}

fn hash(_args: &Args, mut in_file: File) -> Result<(), Error> {
    let hash;
    if let Some(map) = maybe_memmap_input(&in_file)? {
        hash = bao::hash::hash(&map);
    } else {
        let mut writer = bao::hash::Writer::new();
        io::copy(&mut in_file, &mut writer)?;
        hash = writer.finish();
    }
    println!("{}", hex::encode(hash));
    Ok(())
}

fn hash_encoded(_args: &Args, mut in_file: File) -> Result<(), Error> {
    let hash = bao::decode::hash_from_encoded(&mut in_file)?;
    println!("{}", hex::encode(hash));
    Ok(())
}

fn encode(_args: &Args, mut in_file: File, out_file: File) -> Result<(), Error> {
    if let Some(in_map) = maybe_memmap_input(&in_file)? {
        let target_len = bao::encode::encoded_size(in_map.len() as u64);
        if let Some(mut out_map) = maybe_memmap_output(&out_file, target_len)? {
            bao::encode::encode(&in_map, &mut out_map);
            return Ok(());
        }
    }
    // If one or both of the files weren't mappable, fall back to the writer. First check that we
    // have an actual file and not a pipe, because the writer requires seek.
    confirm_real_file(&out_file, "encode output")?;
    let mut writer = bao::encode::Writer::new(out_file);
    io::copy(&mut in_file, &mut writer)?;
    writer.finish()?;
    Ok(())
}

fn decode(args: &Args, in_file: File, mut out_file: File) -> Result<(), Error> {
    let hash = parse_hash(args)?;
    // If we're not seeking, try to memmap the files.
    if args.flag_start.is_none() {
        if let Some(in_map) = maybe_memmap_input(&in_file)? {
            let content_len = bao::decode::parse_and_check_content_len(&in_map)?;
            if let Some(mut out_map) = maybe_memmap_output(&out_file, content_len as u128)? {
                bao::decode::decode(&in_map, &mut out_map, &hash)?;
                return Ok(());
            }
        }
    }
    // If one or both of the files weren't mappable, or if we're seeking, fall back to the reader.
    let mut reader = bao::decode::Reader::new(&in_file, &hash);
    if let Some(offset) = args.flag_start {
        confirm_real_file(&in_file, "when seeking, decode input")?;
        reader.seek(io::SeekFrom::Start(offset))?;
    }
    allow_broken_pipe(io::copy(&mut reader, &mut out_file))?;
    Ok(())
}

fn slice(args: &Args, in_file: File, mut out_file: File) -> Result<(), Error> {
    // Slice extraction requires seek.
    confirm_real_file(&in_file, "slicing input")?;
    let mut reader = bao::decode::SliceExtractor::new(in_file, args.arg_start, args.arg_len);
    io::copy(&mut reader, &mut out_file)?;
    Ok(())
}

fn decode_slice(args: &Args, in_file: File, mut out_file: File) -> Result<(), Error> {
    let hash = parse_hash(&args)?;
    let mut reader = bao::decode::SliceReader::new(in_file, &hash, args.arg_start, args.arg_len);
    allow_broken_pipe(io::copy(&mut reader, &mut out_file))?;
    Ok(())
}

fn in_out_files(args: &Args) -> Result<(File, File), Error> {
    let in_file = if let Some(ref input_path) = args.arg_input {
        if input_path == Path::new("-") {
            os_pipe::dup_stdin()?.into()
        } else {
            File::open(input_path)?
        }
    } else {
        os_pipe::dup_stdin()?.into()
    };
    let out_file = if let Some(ref output_path) = args.arg_output {
        if output_path == Path::new("-") {
            os_pipe::dup_stdout()?.into()
        } else {
            // Both reading and writing permissions are required for MmapMut.
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(output_path)?
        }
    } else {
        os_pipe::dup_stdout()?.into()
    };
    Ok((in_file, out_file))
}

fn maybe_memmap_input(in_file: &File) -> Result<Option<memmap::Mmap>, Error> {
    let metadata = in_file.metadata()?;
    Ok(if !metadata.is_file() {
        // Not a file.
        None
    } else if metadata.len() > isize::max_value() as u64 {
        // Too long to safely map. https://github.com/danburkert/memmap-rs/issues/69
        None
    } else {
        let map = unsafe { memmap::Mmap::map(&in_file)? };
        assert!(map.len() <= isize::max_value() as usize);
        Some(map)
    })
}

fn maybe_memmap_output(
    out_file: &File,
    target_len: u128,
) -> Result<Option<memmap::MmapMut>, Error> {
    if target_len > u64::max_value() as u128 {
        panic!(format!("unreasonable target length: {}", target_len));
    }
    let metadata = out_file.metadata()?;
    Ok(if !metadata.is_file() {
        // Not a file.
        None
    } else if metadata.len() != 0 {
        // The output file hasn't been truncated. Likely opened in append mode.
        None
    } else if target_len > isize::max_value() as u128 {
        // Too long to safely map. https://github.com/danburkert/memmap-rs/issues/69
        None
    } else {
        out_file.set_len(target_len as u64)?;
        let map = unsafe { memmap::MmapMut::map_mut(&out_file)? };
        assert_eq!(map.len() as u128, target_len);
        Some(map)
    })
}

fn confirm_real_file(file: &File, name: &str) -> Result<(), Error> {
    if !file.metadata()?.is_file() {
        Err(err_msg(format!("{} must be a real file", name)))
    } else {
        Ok(())
    }
}

fn parse_hash(args: &Args) -> Result<[u8; bao::hash::HASH_SIZE], Error> {
    let hash_vec = hex::decode(&args.arg_hash).map_err(|_| err_msg("invalid hex"))?;
    if hash_vec.len() != bao::hash::HASH_SIZE {
        return Err(err_msg("wrong length hash"));
    };
    Ok(*array_ref!(hash_vec, 0, bao::hash::HASH_SIZE))
}

// When streaming out decoded content, it's acceptable for the caller to pipe us
// into e.g. `head -c 100`. We catch closed pipe errors in that case and avoid
// erroring out. When encoding, though, we let those errors stay noisy, since
// truncating an encoding is almost never correct.
fn allow_broken_pipe<T>(result: io::Result<T>) -> io::Result<()> {
    match result {
        Ok(_) => Ok(()),
        Err(e) => if e.kind() == io::ErrorKind::BrokenPipe {
            Ok(())
        } else {
            Err(e)
        },
    }
}
