//! `cargo xtask brotli <input> <output>`: Brotli-compress one file at quality 11.
//!
//! `packaging/web/package.sh` uses this to emit `<name>_bg.wasm.br` sidecars: serving the
//! pre-compressed bytes lets hosts skip on-the-fly compression, and only a sidecar is written —
//! the `.wasm` itself is never touched, so the SHA-384 SRI digests baked into `index.html`
//! still hold. Compressing in Rust (instead of shelling out to a `brotli` CLI, which distros
//! may not ship) keeps the pipeline on the `brotli` crate that `crates/fonts/build.rs` already
//! uses for the minimal font set.

use std::fs::{self, File, Metadata};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

/// Brotli's maximum quality. Sidecars are built once per release, so the slowest (and densest)
/// setting is the right trade here. `lgwin: 24` is the largest window, which also suits the
/// multi-megabyte wasm better than the default 22.
const QUALITY: i32 = 11;
const LGWIN: i32 = 24;

/// Compress everything `input` yields into `output` at [`QUALITY`]. Exposed separately from
/// [`run`] so tests can round-trip in-memory buffers without touching the filesystem.
pub fn compress<R: Read, W: Write>(input: &mut R, output: &mut W) -> Result<(), String> {
    let params = brotli::enc::BrotliEncoderParams { quality: QUALITY, lgwin: LGWIN, ..brotli::enc::BrotliEncoderParams::default() };
    brotli::BrotliCompress(input, output, &params).map(|_| ()).map_err(|e| format!("brotli compress: {e}"))?;
    output.flush().map_err(|e| format!("brotli output flush: {e}"))
}

/// Human-readable `output/input` share, e.g. `26.6% of input`; `n/a` for an empty input.
fn ratio_str(input_bytes: usize, output_bytes: usize) -> String {
    if input_bytes == 0 {
        return "n/a (empty input)".to_string();
    }
    let pct = output_bytes as f64 / input_bytes as f64 * 100.0;
    format!("{pct:.1}% of input")
}

/// `path` resolved to its canonical location even when it doesn't exist yet: canonicalize the
/// file itself, else canonicalize the parent and re-append the name (resolving `.`/`..` and
/// symlinked directories). Falls back to the raw path when nothing resolves.
fn canonicalish(path: &Path) -> PathBuf {
    if let Ok(c) = fs::canonicalize(path) {
        return c;
    }
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => fs::canonicalize(dir).map(|d| d.join(name)).unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

/// Whether two file metadata records describe the same file (hard links included): device and
/// inode on Unix, volume serial and file index on Windows.
fn same_file(a: &Metadata, b: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        a.volume_serial_number().is_some()
            && a.volume_serial_number() == b.volume_serial_number()
            && a.file_index().is_some()
            && a.file_index() == b.file_index()
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (a, b);
        false
    }
}

pub fn run(args: &[&str]) -> Result<(), String> {
    let [input, output] = args else {
        return Err(
            "usage: cargo xtask brotli <input> <output>\n\n  Brotli-compress <input> at quality 11 into <output> (packaging/web wasm .br sidecars)"
                .into(),
        );
    };
    let input_path = Path::new(input);
    let output_path = Path::new(output);
    let input_meta = fs::metadata(input_path).map_err(|e| format!("{}: {e}", input_path.display()))?;
    let input_bytes = usize::try_from(input_meta.len()).map_err(|_| format!("{}: file too large", input_path.display()))?;

    let in_file = File::open(input_path).map_err(|e| format!("{}: {e}", input_path.display()))?;
    // Refuse to truncate the input: identical or aliased paths (symlinks, `..`, hard links)
    // would otherwise destroy the file being compressed.
    let output_is_input =
        canonicalish(input_path) == canonicalish(output_path) || fs::metadata(output_path).is_ok_and(|m| same_file(&input_meta, &m));
    if output_is_input {
        return Err(format!("{}: output refers to the input file", output_path.display()));
    }
    // Write to a sibling temp file and rename only after compression succeeds, so a failed or
    // interrupted run never leaves a truncated sidecar at `output_path`.
    let mut tmp_os = output_path.as_os_str().to_os_string();
    tmp_os.push(format!(".tmp-{}", std::process::id()));
    let tmp_path = PathBuf::from(tmp_os);
    let out_file = File::create(&tmp_path).map_err(|e| format!("{}: {e}", tmp_path.display()))?;
    if let Err(e) = compress(&mut BufReader::new(in_file), &mut BufWriter::new(out_file)) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp_path, output_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(format!("{}: {e}", output_path.display()));
    }

    let output_bytes = usize::try_from(std::fs::metadata(output_path).map_err(|e| format!("{}: {e}", output_path.display()))?.len())
        .map_err(|_| format!("{}: file too large", output_path.display()))?;
    println!("brotli (quality {QUALITY}): {} -> {}", input_path.display(), output_path.display());
    println!("  input:  {input_bytes} bytes");
    println!("  output: {output_bytes} bytes ({})", ratio_str(input_bytes, output_bytes));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Round-trip `data` through [`compress`] and the `brotli-decompressor` crate (the same
    /// decoder wasm-hosted fonts use), asserting byte equality.
    fn round_trip(data: &[u8]) {
        let mut compressed = Vec::new();
        compress(&mut Cursor::new(data), &mut compressed).unwrap();
        let mut decompressed = Vec::new();
        brotli_decompressor::Decompressor::new(&compressed[..], 4096)
            .read_to_end(&mut decompressed)
            .unwrap_or_else(|e| panic!("decompress failed: {e}"));
        assert_eq!(decompressed, data, "round-trip mismatch for {} input bytes", data.len());
    }

    #[test]
    fn round_trips_small_buffer() {
        let text = b"WordCraft brotli round-trip sample: \0\0\xFF the quick brown fox\n".repeat(16);
        round_trip(&text);
    }

    #[test]
    fn empty_input_round_trips() {
        round_trip(b"");
    }

    #[test]
    fn compresses_repetitive_input_smaller() {
        let repetitive = vec![0x42u8; 64 * 1024];
        let mut compressed = Vec::new();
        compress(&mut Cursor::new(&repetitive), &mut compressed).unwrap();
        assert!(compressed.len() < 1024, "64 KiB of one byte compressed to {}", compressed.len());
    }

    #[test]
    fn ratio_str_guards_empty_input() {
        assert_eq!(ratio_str(0, 0), "n/a (empty input)");
        assert_eq!(ratio_str(200, 50), "25.0% of input");
    }

    #[test]
    fn run_refuses_to_truncate_its_input() {
        let dir = std::env::temp_dir().join(format!("wordcraft-xtask-brotli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.bin");
        std::fs::write(&input, b"payload").unwrap();
        let input_s = input.to_str().unwrap().to_string();

        // The identical path string.
        assert!(run(&[input_s.as_str(), input_s.as_str()]).is_err());
        // A hard link is a different path to the same file.
        let link = dir.join("link.bin");
        if std::fs::hard_link(&input, &link).is_ok() {
            assert!(run(&[input_s.as_str(), link.to_str().unwrap()]).is_err());
        }
        assert_eq!(std::fs::read(&input).unwrap(), b"payload", "the input must survive a rejected run");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
