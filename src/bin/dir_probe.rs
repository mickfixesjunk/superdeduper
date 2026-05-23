//! Diagnostic probe for the D:\Studio enumeration bug.
//!
//! Calls `std::fs::read_dir` on both the supplied path AND its
//! verbatim-prefixed form, printing what each returns. If the two
//! agree, the walker is at fault. If they disagree, the bug is in
//! Rust's stdlib read_dir on verbatim Windows paths.

use std::path::PathBuf;

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() < 2 {
        eprintln!("usage: dir_probe <path>");
        std::process::exit(1);
    }
    let raw = PathBuf::from(&argv[1]);

    println!("==== probe: {} ====", raw.display());

    // Form 1: as supplied.
    probe("supplied", &raw);

    // Form 2: verbatim-prefixed (Windows only). Mirrors what walk.rs's
    // to_verbatim() does.
    #[cfg(windows)]
    {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStrExt;
        let wide_first: Vec<u16> = raw.as_os_str().encode_wide().take(4).collect();
        if !wide_first.as_slice().starts_with(&[0x5C, 0x5C, 0x3F, 0x5C]) && raw.is_absolute() {
            let mut s = OsString::from(r"\\?\");
            s.push(raw.as_os_str());
            let verbatim = PathBuf::from(s);
            probe("verbatim", &verbatim);
        } else {
            println!("(input is already verbatim or non-absolute; skipping verbatim probe)");
        }
    }
}

fn probe(label: &str, path: &PathBuf) {
    println!("\n[{label}] read_dir({})", path.display());
    match std::fs::read_dir(path) {
        Ok(rd) => {
            let mut count = 0u32;
            for entry in rd {
                match entry {
                    Ok(e) => {
                        count += 1;
                        let ft = e
                            .file_type()
                            .map(|t| {
                                if t.is_dir() {
                                    "DIR"
                                } else if t.is_symlink() {
                                    "LNK"
                                } else if t.is_file() {
                                    "FILE"
                                } else {
                                    "???"
                                }
                            })
                            .unwrap_or("ERR");
                        println!("  [{ft}] {}", e.file_name().to_string_lossy());
                    }
                    Err(e) => println!("  ENTRY ERR: {e}"),
                }
            }
            println!("  TOTAL: {count} entries");
        }
        Err(e) => {
            println!("  read_dir FAILED: {} (kind = {:?})", e, e.kind());
        }
    }
}
