//! Memory-mapped file access with [`MappedFile`] (zero-copy `&[u8]` view)
//! and [`MmapReader`] (a `Read + Seek` adapter over the same mapping).
//!
//! Requires the `mmap` feature:
//! ```sh
//! cargo run -p oxiarc-core --example mmap_read --features mmap
//! ```

#[cfg(feature = "mmap")]
fn main() -> std::io::Result<()> {
    use oxiarc_core::mmap::{MappedFile, MmapOptions};
    use std::io::{Read, Seek, SeekFrom};

    // Use a temp file rather than assuming a fixture exists on disk, per
    // this workspace's testing conventions (`std::env::temp_dir()`).
    let path = std::env::temp_dir().join(format!(
        "oxiarc_core_mmap_read_example_{}.bin",
        std::process::id()
    ));
    let contents: Vec<u8> = (0..=255u8).cycle().take(64 * 1024).collect();
    std::fs::write(&path, &contents)?;

    // Ensure the scratch file is removed even if an assertion below panics.
    struct Cleanup<'a>(&'a std::path::Path);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0);
        }
    }
    let _cleanup = Cleanup(&path);

    // ---- MappedFile: zero-copy slice view ---------------------------------
    let mapped = MappedFile::open(&path)?;
    println!("MappedFile: {} bytes mapped", mapped.len());
    assert_eq!(&mapped[..4], &contents[..4]);
    assert_eq!(mapped.as_slice(), contents.as_slice());

    // ---- MmapReader: Read + Seek adapter over the same mapping ------------
    // `MmapOptions::open` returns `oxiarc_core::error::Result`, distinct
    // from this function's `std::io::Result`, so it is unwrapped explicitly
    // rather than propagated with `?`.
    let mut reader = MmapOptions::new().open(&path).expect("MmapOptions::open");
    println!(
        "MmapReader: {} bytes, position starts at {}",
        reader.len(),
        reader.position()
    );

    let mut first_16 = [0u8; 16];
    reader.read_exact(&mut first_16)?;
    assert_eq!(&first_16, &contents[..16]);
    println!("Read first 16 bytes via Read::read_exact: {first_16:?}");

    reader.seek(SeekFrom::Start(1024))?;
    let mut chunk = [0u8; 32];
    reader.read_exact(&mut chunk)?;
    assert_eq!(&chunk, &contents[1024..1024 + 32]);
    println!("Seeked to offset 1024 and read 32 bytes matching the source file.");

    println!(
        "Remaining bytes after that read: {} (of {})",
        reader.remaining(),
        reader.len()
    );

    println!("Memory-mapped read verified for both MappedFile and MmapReader.");
    Ok(())
}

#[cfg(not(feature = "mmap"))]
fn main() {
    eprintln!(
        "This example requires the `mmap` feature. Run with:\n  \
         cargo run -p oxiarc-core --example mmap_read --features mmap"
    );
}
