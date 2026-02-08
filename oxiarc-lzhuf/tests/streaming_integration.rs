//! Integration tests for streaming LZH decompression.
//!
//! These tests verify the streaming decoder works correctly with various
//! input patterns and chunk sizes.

use oxiarc_core::traits::{DecompressStatus, Decompressor};
use oxiarc_lzhuf::{
    DecoderPhase, LzhMethod, StreamingBitReader, StreamingLzhDecoder, create_streaming_decoder,
    decode_lzh_streaming,
};

// ============================================================================
// Basic Functionality Tests
// ============================================================================

#[test]
fn test_streaming_decoder_stored_full_buffer() {
    let data = b"Hello, World! This is a test of the streaming LZH decoder.";
    let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh0, data.len() as u64);
    let mut output = vec![0u8; data.len()];

    let (consumed, produced, status) = decoder
        .decompress(data, &mut output)
        .expect("Decompress failed");

    assert_eq!(consumed, data.len());
    assert_eq!(produced, data.len());
    assert_eq!(status, DecompressStatus::Done);
    assert_eq!(&output, data);
    assert!(decoder.is_finished());
}

#[test]
fn test_streaming_decoder_stored_small_input_chunks() {
    let data = b"The quick brown fox jumps over the lazy dog.";
    let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh0, data.len() as u64);
    let mut output = Vec::new();
    let mut input_pos = 0;

    // Process with very small input chunks (3 bytes)
    while input_pos < data.len() {
        let chunk_end = (input_pos + 3).min(data.len());
        let input_chunk = &data[input_pos..chunk_end];
        let mut chunk_output = vec![0u8; 10];

        let (consumed, produced, status) = decoder
            .decompress(input_chunk, &mut chunk_output)
            .expect("Decompress failed");

        input_pos += consumed;
        output.extend_from_slice(&chunk_output[..produced]);

        if status == DecompressStatus::Done {
            break;
        }
    }

    assert_eq!(output, data);
    assert!(decoder.is_finished());
}

#[test]
fn test_streaming_decoder_stored_small_output_buffer() {
    let data = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit.";
    let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh0, data.len() as u64);
    let mut output = Vec::new();
    let mut input_pos = 0;

    // Process with small output buffer (5 bytes)
    loop {
        let mut chunk_output = vec![0u8; 5];
        let input_slice = &data[input_pos..];

        let (consumed, produced, status) = decoder
            .decompress(input_slice, &mut chunk_output)
            .expect("Decompress failed");

        input_pos += consumed;
        output.extend_from_slice(&chunk_output[..produced]);

        match status {
            DecompressStatus::Done => break,
            DecompressStatus::NeedsInput => {
                if input_pos >= data.len() {
                    break;
                }
            }
            DecompressStatus::NeedsOutput => {
                // Continue with more output space
            }
            DecompressStatus::BlockEnd => {}
        }
    }

    assert_eq!(output, data);
}

#[test]
fn test_streaming_decoder_stored_single_byte_chunks() {
    let data = b"Single byte test";
    let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh0, data.len() as u64);
    let mut output = Vec::new();

    // Process one byte at a time
    for byte in data.iter() {
        let input = [*byte];
        let mut chunk_output = vec![0u8; 1];

        let (consumed, produced, status) = decoder
            .decompress(&input, &mut chunk_output)
            .expect("Decompress failed");

        assert_eq!(consumed, 1);
        assert_eq!(produced, 1);
        output.push(chunk_output[0]);

        if status == DecompressStatus::Done {
            break;
        }
    }

    assert_eq!(output, data);
}

// ============================================================================
// Convenience Function Tests
// ============================================================================

#[test]
fn test_decode_lzh_streaming_stored() {
    let data = b"Convenience function test data with some extra content.";
    let result =
        decode_lzh_streaming(data, LzhMethod::Lh0, data.len() as u64).expect("Decode failed");
    assert_eq!(result, data);
}

#[test]
fn test_create_streaming_decoder() {
    let decoder = create_streaming_decoder(LzhMethod::Lh5, 1000);
    assert_eq!(decoder.uncompressed_size(), 1000);
    assert_eq!(decoder.bytes_decoded(), 0);
    assert!(!decoder.is_finished());
    assert_eq!(decoder.phase(), DecoderPhase::ReadBlockSize);
}

#[test]
fn test_create_streaming_decoder_stored() {
    let decoder = create_streaming_decoder(LzhMethod::Lh0, 500);
    assert_eq!(decoder.uncompressed_size(), 500);
    assert_eq!(decoder.phase(), DecoderPhase::DecodeBlock);
}

// ============================================================================
// Bit Reader Tests
// ============================================================================

#[test]
fn test_streaming_bit_reader_multiple_reads() {
    let data = [0xFF, 0x00, 0xAA, 0x55];
    let mut reader = StreamingBitReader::new();

    // Read 8 bits (0xFF)
    assert_eq!(reader.read_bits(&data, 8), Some(0xFF));

    // Read 8 bits (0x00)
    assert_eq!(reader.read_bits(&data, 8), Some(0x00));

    // Read 4 bits (low nibble of 0xAA = 0xA)
    assert_eq!(reader.read_bits(&data, 4), Some(0xA));

    // Read 4 bits (high nibble of 0xAA = 0xA)
    assert_eq!(reader.read_bits(&data, 4), Some(0xA));

    // Read 8 bits (0x55)
    assert_eq!(reader.read_bits(&data, 8), Some(0x55));

    assert_eq!(reader.bytes_consumed(), 4);
}

#[test]
fn test_streaming_bit_reader_peek_without_consume() {
    let data = [0xAB];
    let mut reader = StreamingBitReader::new();

    // Peek at 4 bits
    assert_eq!(reader.peek_bits(&data, 4), Some(0xB));

    // Peek again - should be the same
    assert_eq!(reader.peek_bits(&data, 4), Some(0xB));

    // Now read - should consume
    assert_eq!(reader.read_bits(&data, 4), Some(0xB));

    // Peek next 4 bits
    assert_eq!(reader.peek_bits(&data, 4), Some(0xA));
}

#[test]
fn test_streaming_bit_reader_read_single_bits() {
    let data = [0b10101010]; // 0xAA
    let mut reader = StreamingBitReader::new();

    // Read LSB first: 0, 1, 0, 1, 0, 1, 0, 1
    assert_eq!(reader.read_bit(&data), Some(false)); // bit 0 = 0
    assert_eq!(reader.read_bit(&data), Some(true)); // bit 1 = 1
    assert_eq!(reader.read_bit(&data), Some(false)); // bit 2 = 0
    assert_eq!(reader.read_bit(&data), Some(true)); // bit 3 = 1
    assert_eq!(reader.read_bit(&data), Some(false)); // bit 4 = 0
    assert_eq!(reader.read_bit(&data), Some(true)); // bit 5 = 1
    assert_eq!(reader.read_bit(&data), Some(false)); // bit 6 = 0
    assert_eq!(reader.read_bit(&data), Some(true)); // bit 7 = 1
}

#[test]
fn test_streaming_bit_reader_state_save_restore_complex() {
    let data = [0x12, 0x34, 0x56, 0x78];
    let mut reader = StreamingBitReader::new();

    // Read some bits
    reader.read_bits(&data, 12);
    let state = reader.save_state();

    // Read more bits
    reader.read_bits(&data, 8);
    let after_read = reader.bytes_consumed();

    // Restore and verify
    reader.restore_state(state);
    assert!(reader.bytes_consumed() < after_read);

    // Read same bits again
    let result = reader.read_bits(&data, 8);
    assert!(result.is_some());
}

// ============================================================================
// Decoder State Tests
// ============================================================================

#[test]
fn test_decoder_reset() {
    let data = b"Test data";
    let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh0, data.len() as u64);

    // Partially decompress
    let mut output = vec![0u8; 5];
    let _ = decoder.decompress(&data[..5], &mut output);

    assert!(decoder.bytes_decoded() > 0);

    // Reset
    decoder.reset();

    assert_eq!(decoder.bytes_decoded(), 0);
    assert!(!decoder.is_finished());
    assert_eq!(decoder.phase(), DecoderPhase::DecodeBlock);
}

#[test]
fn test_decoder_phase_transitions_lh5() {
    let decoder = StreamingLzhDecoder::new(LzhMethod::Lh5, 100);
    assert_eq!(decoder.phase(), DecoderPhase::ReadBlockSize);

    let decoder_lh4 = StreamingLzhDecoder::new(LzhMethod::Lh4, 100);
    assert_eq!(decoder_lh4.phase(), DecoderPhase::ReadBlockSize);

    let decoder_lh6 = StreamingLzhDecoder::new(LzhMethod::Lh6, 100);
    assert_eq!(decoder_lh6.phase(), DecoderPhase::ReadBlockSize);

    let decoder_lh7 = StreamingLzhDecoder::new(LzhMethod::Lh7, 100);
    assert_eq!(decoder_lh7.phase(), DecoderPhase::ReadBlockSize);
}

#[test]
fn test_decoder_decompressor_trait() {
    let data = b"Trait test data";
    let mut decoder: Box<dyn Decompressor> =
        Box::new(StreamingLzhDecoder::new(LzhMethod::Lh0, data.len() as u64));

    let mut output = vec![0u8; data.len()];
    let (consumed, produced, status) = decoder
        .decompress(data, &mut output)
        .expect("Decompress failed");

    assert_eq!(consumed, data.len());
    assert_eq!(produced, data.len());
    assert_eq!(status, DecompressStatus::Done);
    assert!(decoder.is_finished());
}

// ============================================================================
// Edge Case Tests
// ============================================================================

#[test]
fn test_empty_input() {
    let data: &[u8] = b"";
    let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh0, 0);
    let mut output = vec![0u8; 0];

    let (consumed, produced, status) = decoder
        .decompress(data, &mut output)
        .expect("Decompress failed");

    assert_eq!(consumed, 0);
    assert_eq!(produced, 0);
    assert_eq!(status, DecompressStatus::Done);
}

#[test]
fn test_large_stored_data() {
    // Create a larger test data set
    let data: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();
    let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh0, data.len() as u64);
    let mut output = Vec::new();
    let mut input_pos = 0;

    // Process in medium-sized chunks
    while input_pos < data.len() {
        let chunk_size = 1000.min(data.len() - input_pos);
        let mut chunk_output = vec![0u8; chunk_size];

        let (consumed, produced, status) = decoder
            .decompress(&data[input_pos..], &mut chunk_output)
            .expect("Decompress failed");

        input_pos += consumed;
        output.extend_from_slice(&chunk_output[..produced]);

        if status == DecompressStatus::Done {
            break;
        }
    }

    assert_eq!(output.len(), data.len());
    assert_eq!(output, data);
}

#[test]
fn test_zero_length_read() {
    let data = [0xAB, 0xCD];
    let mut reader = StreamingBitReader::new();

    // Reading 0 bits should return 0
    assert_eq!(reader.read_bits(&data, 0), Some(0));
    assert_eq!(reader.bytes_consumed(), 0);
}

#[test]
fn test_decoder_progress_tracking() {
    let data = b"Progress tracking test";
    let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh0, data.len() as u64);

    let mut output = vec![0u8; 10];
    let (consumed, _, _) = decoder
        .decompress(&data[..10], &mut output)
        .expect("Decompress failed");

    assert_eq!(decoder.bytes_decoded(), consumed as u64);
    assert_eq!(decoder.uncompressed_size(), data.len() as u64);
}

// ============================================================================
// Method-specific Tests
// ============================================================================

#[test]
fn test_all_lzh_methods_initial_phase() {
    let methods = [
        (LzhMethod::Lh0, DecoderPhase::DecodeBlock),
        (LzhMethod::Lh4, DecoderPhase::ReadBlockSize),
        (LzhMethod::Lh5, DecoderPhase::ReadBlockSize),
        (LzhMethod::Lh6, DecoderPhase::ReadBlockSize),
        (LzhMethod::Lh7, DecoderPhase::ReadBlockSize),
    ];

    for (method, expected_phase) in methods {
        let decoder = StreamingLzhDecoder::new(method, 100);
        assert_eq!(
            decoder.phase(),
            expected_phase,
            "Method {:?} should start in {:?}",
            method,
            expected_phase
        );
    }
}

#[test]
fn test_decode_lzh_streaming_empty() {
    let data: &[u8] = b"";
    let result = decode_lzh_streaming(data, LzhMethod::Lh0, 0).expect("Decode failed");
    assert!(result.is_empty());
}
