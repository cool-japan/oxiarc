//! One macro that stamps out flate2's `bufread` / `read` / `write` encoder
//! and decoder types for a container; `zlib` and `deflate` differ only in
//! whether [`crate::Compress`] / [`crate::Decompress`] carry a zlib header.

macro_rules! flate_streams {
    ($zlib:expr, $Enc:ident, $Dec:ident, $what:literal) => {
        /// Types that read from a `BufRead`.
        pub mod bufread {
            use std::io::{self, BufRead, Read, Write};

            use crate::zio;
            use crate::{Compress, Compression, Decompress};

            #[doc = concat!("A ", $what, " encoder, or compressor.")]
            ///
            /// This structure implements a [`Read`] interface. When read
            /// from, it reads uncompressed data from the underlying
            /// [`BufRead`] and provides the compressed data.
            #[derive(Debug)]
            pub struct $Enc<R> {
                obj: R,
                data: Compress,
            }

            impl<R: BufRead> $Enc<R> {
                /// Creates a new encoder which will read uncompressed data
                /// from the given stream and emit the compressed stream.
                pub fn new(r: R, level: Compression) -> $Enc<R> {
                    $Enc {
                        obj: r,
                        data: Compress::new(level, $zlib),
                    }
                }

                /// Creates a new encoder with the given compression
                /// settings.
                pub fn new_with_compress(r: R, compression: Compress) -> $Enc<R> {
                    $Enc {
                        obj: r,
                        data: compression,
                    }
                }
            }

            impl<R> $Enc<R> {
                /// Resets the state of this encoder entirely, swapping out
                /// the input stream for another. Returns the old stream.
                pub fn reset(&mut self, r: R) -> R {
                    self.data.reset();
                    std::mem::replace(&mut self.obj, r)
                }

                /// Acquires a reference to the underlying reader.
                pub fn get_ref(&self) -> &R {
                    &self.obj
                }

                /// Acquires a mutable reference to the underlying stream.
                pub fn get_mut(&mut self) -> &mut R {
                    &mut self.obj
                }

                /// Consumes this encoder, returning the underlying reader.
                pub fn into_inner(self) -> R {
                    self.obj
                }

                /// Returns the number of bytes that have been read into this
                /// compressor.
                pub fn total_in(&self) -> u64 {
                    self.data.total_in()
                }

                /// Returns the number of bytes that the compressor has
                /// produced.
                pub fn total_out(&self) -> u64 {
                    self.data.total_out()
                }
            }

            impl<R: BufRead> Read for $Enc<R> {
                fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                    zio::read(&mut self.obj, &mut self.data, buf)
                }
            }

            impl<R: BufRead + Write> Write for $Enc<R> {
                fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                    self.get_mut().write(buf)
                }

                fn flush(&mut self) -> io::Result<()> {
                    self.get_mut().flush()
                }
            }

            #[doc = concat!("A ", $what, " decoder, or decompressor.")]
            ///
            /// This structure implements a [`Read`] interface. When read
            /// from, it reads compressed data from the underlying
            /// [`BufRead`] and provides the uncompressed data. It never
            /// consumes bytes past the end of the compressed stream.
            #[derive(Debug)]
            pub struct $Dec<R> {
                obj: R,
                data: Decompress,
            }

            impl<R: BufRead> $Dec<R> {
                /// Creates a new decoder which will decompress data read
                /// from the given stream.
                pub fn new(r: R) -> $Dec<R> {
                    $Dec {
                        obj: r,
                        data: Decompress::new($zlib),
                    }
                }

                /// Creates a new decoder with the given decompression
                /// settings.
                pub fn new_with_decompress(r: R, decompression: Decompress) -> $Dec<R> {
                    $Dec {
                        obj: r,
                        data: decompression,
                    }
                }
            }

            impl<R> $Dec<R> {
                /// Resets the state of this decoder entirely, swapping out
                /// the input stream for another. Returns the old stream.
                pub fn reset(&mut self, r: R) -> R {
                    self.data.reset($zlib);
                    std::mem::replace(&mut self.obj, r)
                }

                /// Resets the decompression state only, keeping the input
                /// stream (used by the gzip decoders between members).
                #[allow(dead_code)]
                pub(crate) fn reset_data(&mut self) {
                    self.data.reset($zlib);
                }

                /// Acquires a reference to the underlying stream.
                pub fn get_ref(&self) -> &R {
                    &self.obj
                }

                /// Acquires a mutable reference to the underlying stream.
                pub fn get_mut(&mut self) -> &mut R {
                    &mut self.obj
                }

                /// Consumes this decoder, returning the underlying reader.
                pub fn into_inner(self) -> R {
                    self.obj
                }

                /// Returns the number of bytes that the decompressor has
                /// consumed.
                pub fn total_in(&self) -> u64 {
                    self.data.total_in()
                }

                /// Returns the number of bytes that the decompressor has
                /// produced.
                pub fn total_out(&self) -> u64 {
                    self.data.total_out()
                }
            }

            impl<R: BufRead> Read for $Dec<R> {
                fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
                    zio::read(&mut self.obj, &mut self.data, into)
                }
            }

            impl<R: BufRead + Write> Write for $Dec<R> {
                fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                    self.get_mut().write(buf)
                }

                fn flush(&mut self) -> io::Result<()> {
                    self.get_mut().flush()
                }
            }
        }

        /// Types that read from a `Read` (buffered internally).
        pub mod read {
            use std::io::{self, Read, Write};

            use crate::bufreader::BufReader;

            use super::bufread;
            use crate::{Compress, Compression, Decompress};

            #[doc = concat!("A ", $what, " encoder, or compressor.")]
            ///
            /// This structure implements a [`Read`] interface. When read
            /// from, it reads uncompressed data from the underlying
            /// [`Read`] and provides the compressed data.
            #[derive(Debug)]
            pub struct $Enc<R> {
                inner: bufread::$Enc<BufReader<R>>,
            }

            impl<R: Read> $Enc<R> {
                /// Creates a new encoder which will read uncompressed data
                /// from the given stream and emit the compressed stream.
                pub fn new(r: R, level: Compression) -> $Enc<R> {
                    $Enc {
                        inner: bufread::$Enc::new(BufReader::new(r), level),
                    }
                }

                /// Creates a new encoder with the given compression
                /// settings.
                pub fn new_with_compress(r: R, compression: Compress) -> $Enc<R> {
                    $Enc {
                        inner: bufread::$Enc::new_with_compress(BufReader::new(r), compression),
                    }
                }
            }

            impl<R> $Enc<R> {
                /// Resets the state of this encoder entirely, swapping out
                /// the input stream for another. Returns the old stream;
                /// any data it had buffered is discarded.
                pub fn reset(&mut self, r: R) -> R {
                    self.inner.reset(BufReader::new(r)).into_inner()
                }

                /// Acquires a reference to the underlying stream.
                pub fn get_ref(&self) -> &R {
                    self.inner.get_ref().get_ref()
                }

                /// Acquires a mutable reference to the underlying stream.
                pub fn get_mut(&mut self) -> &mut R {
                    self.inner.get_mut().get_mut()
                }

                /// Consumes this encoder, returning the underlying reader.
                pub fn into_inner(self) -> R {
                    self.inner.into_inner().into_inner()
                }

                /// Returns the number of bytes that have been read into this
                /// compressor.
                pub fn total_in(&self) -> u64 {
                    self.inner.total_in()
                }

                /// Returns the number of bytes that the compressor has
                /// produced.
                pub fn total_out(&self) -> u64 {
                    self.inner.total_out()
                }
            }

            impl<R: Read> Read for $Enc<R> {
                fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                    self.inner.read(buf)
                }
            }

            impl<W: Read + Write> Write for $Enc<W> {
                fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                    self.get_mut().write(buf)
                }

                fn flush(&mut self) -> io::Result<()> {
                    self.get_mut().flush()
                }
            }

            #[doc = concat!("A ", $what, " decoder, or decompressor.")]
            ///
            /// This structure implements a [`Read`] interface. When read
            /// from, it reads compressed data from the underlying [`Read`]
            /// and provides the uncompressed data.
            #[derive(Debug)]
            pub struct $Dec<R> {
                inner: bufread::$Dec<BufReader<R>>,
            }

            impl<R: Read> $Dec<R> {
                /// Creates a new decoder which will decompress data read
                /// from the given stream.
                pub fn new(r: R) -> $Dec<R> {
                    $Dec::new_with_buf(r, vec![0; 32 * 1024])
                }

                /// Same as `new`, but the intermediate buffer for data is
                /// sized like `buf`.
                pub fn new_with_buf(r: R, buf: Vec<u8>) -> $Dec<R> {
                    $Dec {
                        inner: bufread::$Dec::new(BufReader::with_buf(buf, r)),
                    }
                }

                /// Creates a new decoder with the given decompression
                /// settings.
                pub fn new_with_decompress(r: R, decompression: Decompress) -> $Dec<R> {
                    $Dec::new_with_decompress_and_buf(r, vec![0; 32 * 1024], decompression)
                }

                /// Creates a new decoder with the given decompression
                /// settings and a buffer sized like `buf`.
                pub fn new_with_decompress_and_buf(
                    r: R,
                    buf: Vec<u8>,
                    decompression: Decompress,
                ) -> $Dec<R> {
                    $Dec {
                        inner: bufread::$Dec::new_with_decompress(
                            BufReader::with_buf(buf, r),
                            decompression,
                        ),
                    }
                }
            }

            impl<R> $Dec<R> {
                /// Resets the state of this decoder entirely, swapping out
                /// the input stream for another. Returns the old stream;
                /// any data it had buffered is discarded.
                pub fn reset(&mut self, r: R) -> R {
                    self.inner.reset(BufReader::new(r)).into_inner()
                }

                /// Acquires a reference to the underlying stream.
                pub fn get_ref(&self) -> &R {
                    self.inner.get_ref().get_ref()
                }

                /// Acquires a mutable reference to the underlying stream.
                pub fn get_mut(&mut self) -> &mut R {
                    self.inner.get_mut().get_mut()
                }

                /// Consumes this decoder, returning the underlying reader.
                /// Buffered, not yet decompressed bytes are lost.
                pub fn into_inner(self) -> R {
                    self.inner.into_inner().into_inner()
                }

                /// Returns the number of bytes that the decompressor has
                /// consumed.
                pub fn total_in(&self) -> u64 {
                    self.inner.total_in()
                }

                /// Returns the number of bytes that the decompressor has
                /// produced.
                pub fn total_out(&self) -> u64 {
                    self.inner.total_out()
                }
            }

            impl<R: Read> Read for $Dec<R> {
                fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
                    self.inner.read(into)
                }
            }

            impl<R: Read + Write> Write for $Dec<R> {
                fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                    self.get_mut().write(buf)
                }

                fn flush(&mut self) -> io::Result<()> {
                    self.get_mut().flush()
                }
            }
        }

        /// Types that write to a `Write`.
        pub mod write {
            use std::io::{self, Read, Write};

            use crate::zio;
            use crate::{Compress, Compression, Decompress};

            #[doc = concat!("A ", $what, " encoder, or compressor.")]
            ///
            /// This structure implements a [`Write`] interface and takes a
            /// stream of uncompressed data, writing the compressed data to
            /// the wrapped writer. The stream is finished when the encoder
            /// is dropped (errors are then ignored; call `finish` to see
            /// them).
            #[derive(Debug)]
            pub struct $Enc<W: Write> {
                inner: zio::Writer<W, Compress>,
            }

            impl<W: Write> $Enc<W> {
                /// Creates a new encoder which will write compressed data to
                /// the stream given at the given compression level.
                pub fn new(w: W, level: Compression) -> $Enc<W> {
                    $Enc {
                        inner: zio::Writer::new(w, Compress::new(level, $zlib)),
                    }
                }

                /// Creates a new encoder with the given compression
                /// settings.
                pub fn new_with_compress(w: W, compression: Compress) -> $Enc<W> {
                    $Enc {
                        inner: zio::Writer::new(w, compression),
                    }
                }

                /// Acquires a reference to the underlying writer.
                pub fn get_ref(&self) -> &W {
                    self.inner.get_ref()
                }

                /// Acquires a mutable reference to the underlying writer.
                pub fn get_mut(&mut self) -> &mut W {
                    self.inner.get_mut()
                }

                /// Resets the state of this encoder entirely, swapping out
                /// the output stream for another. The current stream is
                /// finished first; the old writer is returned.
                ///
                /// # Errors
                ///
                /// Any error finishing the current stream.
                pub fn reset(&mut self, w: W) -> io::Result<W> {
                    self.inner.finish()?;
                    self.inner.data.reset();
                    Ok(self.inner.replace(w))
                }

                /// Attempt to finish this output stream, writing out final
                /// chunks of data.
                ///
                /// # Errors
                ///
                /// Any I/O error from the underlying writer.
                pub fn try_finish(&mut self) -> io::Result<()> {
                    self.inner.finish()
                }

                /// Consumes this encoder, flushing the output stream.
                ///
                /// # Errors
                ///
                /// Any I/O error from the underlying writer.
                pub fn finish(mut self) -> io::Result<W> {
                    self.inner.finish()?;
                    self.inner.take_inner()
                }

                /// Consumes this encoder, flushing (sync flush) the output
                /// stream without terminating it.
                ///
                /// # Errors
                ///
                /// Any I/O error from the underlying writer.
                pub fn flush_finish(mut self) -> io::Result<W> {
                    self.inner.flush()?;
                    self.inner.take_inner()
                }

                /// Returns the number of bytes that have been written to
                /// this compressor.
                pub fn total_in(&self) -> u64 {
                    self.inner.data.total_in()
                }

                /// Returns the number of bytes that the compressor has
                /// produced.
                pub fn total_out(&self) -> u64 {
                    self.inner.data.total_out()
                }
            }

            impl<W: Write> Write for $Enc<W> {
                fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                    self.inner.write(buf)
                }

                fn flush(&mut self) -> io::Result<()> {
                    self.inner.flush()
                }
            }

            impl<W: Read + Write> Read for $Enc<W> {
                fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                    self.get_mut().read(buf)
                }
            }

            #[doc = concat!("A ", $what, " decoder, or decompressor.")]
            ///
            /// This structure implements a [`Write`] and will emit a stream
            /// of decompressed data when fed a stream of compressed data.
            #[derive(Debug)]
            pub struct $Dec<W: Write> {
                inner: zio::Writer<W, Decompress>,
            }

            impl<W: Write> $Dec<W> {
                /// Creates a new decoder which will write uncompressed data
                /// to the stream.
                pub fn new(w: W) -> $Dec<W> {
                    $Dec {
                        inner: zio::Writer::new(w, Decompress::new($zlib)),
                    }
                }

                /// Creates a new decoder with the given decompression
                /// settings.
                pub fn new_with_decompress(w: W, decompression: Decompress) -> $Dec<W> {
                    $Dec {
                        inner: zio::Writer::new(w, decompression),
                    }
                }

                /// Acquires a reference to the underlying writer.
                pub fn get_ref(&self) -> &W {
                    self.inner.get_ref()
                }

                /// Acquires a mutable reference to the underlying writer.
                pub fn get_mut(&mut self) -> &mut W {
                    self.inner.get_mut()
                }

                /// Resets the state of this decoder entirely, swapping out
                /// the output stream for another. The current stream is
                /// flushed first; the old writer is returned.
                ///
                /// # Errors
                ///
                /// Any error flushing the current stream.
                pub fn reset(&mut self, w: W) -> io::Result<W> {
                    self.inner.finish()?;
                    self.inner.data = Decompress::new($zlib);
                    Ok(self.inner.replace(w))
                }

                /// Attempt to finish this output stream, writing out final
                /// chunks of data.
                ///
                /// # Errors
                ///
                /// Any I/O error from the underlying writer.
                pub fn try_finish(&mut self) -> io::Result<()> {
                    self.inner.finish()
                }

                /// Consumes this decoder, flushing the output stream.
                ///
                /// # Errors
                ///
                /// Any I/O error from the underlying writer.
                pub fn finish(mut self) -> io::Result<W> {
                    self.inner.finish()?;
                    self.inner.take_inner()
                }

                /// Returns the number of bytes that the decompressor has
                /// consumed for decompression.
                pub fn total_in(&self) -> u64 {
                    self.inner.data.total_in()
                }

                /// Returns the number of bytes that the decompressor has
                /// written to its output stream.
                pub fn total_out(&self) -> u64 {
                    self.inner.data.total_out()
                }
            }

            impl<W: Write> Write for $Dec<W> {
                fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                    self.inner.write(buf)
                }

                fn flush(&mut self) -> io::Result<()> {
                    self.inner.flush()
                }
            }

            impl<W: Read + Write> Read for $Dec<W> {
                fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                    self.get_mut().read(buf)
                }
            }
        }
    };
}

pub(crate) use flate_streams;
