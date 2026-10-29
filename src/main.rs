use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process;

use rayon::prelude::*;
use snap::read::FrameDecoder;
use snap::write::FrameEncoder;
use tar::{Archive, Builder};

fn usage() {
    eprintln!(
        "Usage:
  snaper -c <input_path1> [input_path2 ...]   # compress
  snaper -d <input_file1.sz> [input_file2.sz ...]   # decompress"
    );
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.2} {}", size, UNITS[unit])
    }
}

fn compressed_path(input: &Path) -> PathBuf {
    if input.is_dir() {
        let mut p = input.as_os_str().to_owned();
        p.push(".tar.sz");
        PathBuf::from(p)
    } else {
        let mut p = input.as_os_str().to_owned();
        p.push(".sz");
        PathBuf::from(p)
    }
}

fn decompressed_path(input: &Path) -> io::Result<PathBuf> {
    let s = input
        .to_str()
        .ok_or_else(|| io::Error::other("invalid UTF-8 in path"))?;
    if !s.ends_with(".sz") {
        return Err(io::Error::other("input must end with .sz"));
    }
    Ok(PathBuf::from(&s[..s.len() - 3]))
}

fn get_dir_size(path: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() {
            total += get_dir_size(&entry.path())?;
        } else {
            total += meta.len();
        }
    }
    Ok(total)
}

fn remove_path(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn compress(input: &Path, output: &Path, is_dir: bool) -> io::Result<()> {
    if !input.exists() {
        return Err(io::Error::other(format!(
            "input does not exist: {}",
            input.display()
        )));
    }
    if input == output {
        return Err(io::Error::other("input and output paths must differ"));
    }

    let original_size = if is_dir {
        get_dir_size(input)?
    } else {
        fs::metadata(input)?.len()
    };

    let out_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|e| {
            let _ = fs::remove_file(output);
            e
        })?;

    let result = (|| {
        let mut writer = BufWriter::new(out_file);
        {
            let mut encoder = FrameEncoder::new(&mut writer);
            if is_dir {
                let mut builder = Builder::new(&mut encoder);
                builder.append_dir_all(
                    input
                        .file_name()
                        .unwrap_or_else(|| std::ffi::OsStr::new(".")),
                    input,
                )?;
                builder.finish()?;
            } else {
                let mut in_file = File::open(input)?;
                io::copy(&mut in_file, &mut encoder)?;
            }
            let _inner = encoder.into_inner().map_err(|e| e.into_error())?;
        }
        writer.flush()?;
        Ok(())
    })();

    if let Err(e) = result {
        let _ = fs::remove_file(output);
        return Err(e);
    }

    let compressed_size = fs::metadata(output)?.len();
    let ratio = if original_size == 0 {
        0.0
    } else {
        (compressed_size as f64 / original_size as f64) * 100.0
    };

    // Remove original only after successful compression
    remove_path(input)?;

    println!(
        "Compressed {} -> {} ({} → {}, {:.1}% of original)",
        input.display(),
        output.display(),
        human_size(original_size),
        human_size(compressed_size),
        ratio
    );
    Ok(())
}

fn decompress(input: &Path, output: &Path) -> io::Result<()> {
    if !input.exists() {
        return Err(io::Error::other(format!(
            "input does not exist: {}",
            input.display()
        )));
    }
    if input == output {
        return Err(io::Error::other("input and output paths must differ"));
    }

    let is_tar = output
        .extension()
        .map(|e| e == "tar")
        .unwrap_or(false);

    if is_tar {
        // Directory archive: mydir.tar.sz → mydir/
        let dir_name = output.with_extension("");
        if dir_name.exists() {
            return Err(io::Error::other(format!(
                "target directory already exists: {}",
                dir_name.display()
            )));
        }

        let file = File::open(input)?;
        let decoder = FrameDecoder::new(BufReader::new(file));
        let mut archive = Archive::new(decoder);

        let result = archive.unpack(&dir_name);
        if let Err(e) = result {
            let _ = fs::remove_dir_all(&dir_name);
            return Err(e);
        }

        // Remove original .sz only after successful extraction
        remove_path(input)?;

        println!("Extracted folder: {}", dir_name.display());
    } else {
        // Regular file
        let out_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output)?;

        let result = (|| {
            let mut writer = BufWriter::new(out_file);
            let mut decoder = FrameDecoder::new(BufReader::new(File::open(input)?));
            io::copy(&mut decoder, &mut writer)?;
            writer.flush()?;
            Ok(())
        })();

        if let Err(e) = result {
            let _ = fs::remove_file(output);
            return Err(e);
        }

        // Remove original .sz only after successful decompression
        remove_path(input)?;

        println!("Created: {}", output.display());
    }
    Ok(())
}

fn process_path(input: &Path, do_compress: bool) -> io::Result<()> {
    if do_compress {
        let is_dir = input.is_dir();
        let output = compressed_path(input);
        compress(input, &output, is_dir)
    } else {
        let output = decompressed_path(input)?;
        decompress(input, &output)
    }
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        usage();
        return 2;
    }

    let flag = args[1].as_str();
    let do_compress = match flag {
        "-c" => true,
        "-d" => false,
        _ => {
            usage();
            return 2;
        }
    };

    let paths: Vec<PathBuf> = args[2..].iter().map(PathBuf::from).collect();

    let results: Vec<io::Result<()>> = paths
        .par_iter()
        .map(|p| process_path(p, do_compress))
        .collect();

    let mut any_error = false;
    for (path, result) in paths.iter().zip(results) {
        if let Err(e) = result {
            eprintln!("{}: {}", path.display(), e);
            any_error = true;
        }
    }

    if any_error {
        1
    } else {
        0
    }
}

fn main() {
    let code = run();
    process::exit(code);
}