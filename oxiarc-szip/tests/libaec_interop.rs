//! Differential interop regression fixtures against libaec 1.1.4
//! (CCSDS-121.0-B-2 reference implementation).
//!
//! These fixtures were produced by the real `libaec` encoder/decoder at
//! fixture-generation time and are embedded so the interop gate always runs:
//!
//! - `libaec_stream`: bytes emitted by libaec's encoder for `raw`. The
//!   oxiarc decoder must reproduce `raw` byte-identically (direction A).
//! - `oxiarc_stream`: bytes emitted by `oxiarc_szip::encode_bytes` that were
//!   verified byte-identical through libaec's *decoder* when generated
//!   (direction B); embedded to lock the encoder's wire format.
//!
//! A live differential gate against an installed libaec is available behind
//! the `libaec-oracle` cargo feature (`tests/libaec_oracle.rs`).

use oxiarc_szip::{SzipParams, decode, encode_bytes};

struct Fixture {
    name: &'static str,
    bits_per_pixel: u8,
    pixels_per_block: u32,
    reference_sample_interval: u32,
    nn_preprocess: bool,
    rsi_byte_align: bool,
    /// Raw big-endian sample bytes (ground truth).
    raw: &'static str,
    /// Stream produced by libaec's encoder (None for PAD_RSI fixtures,
    /// which libaec only decodes).
    libaec_stream: Option<&'static str>,
    /// Stream produced by oxiarc-szip, accepted by libaec's decoder.
    oxiarc_stream: &'static str,
}

fn from_hex(hex: &str) -> Vec<u8> {
    assert!(hex.len() % 2 == 0, "odd hex fixture length");
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("bad hex fixture"))
        .collect()
}

fn params_for(fx: &Fixture, samples: usize) -> SzipParams {
    SzipParams {
        bits_per_pixel: fx.bits_per_pixel,
        pixels_per_block: fx.pixels_per_block,
        samples,
        reference_sample_interval: fx.reference_sample_interval,
        msb: true,
        nn_preprocess: fx.nn_preprocess,
        rsi_byte_align: fx.rsi_byte_align,
    }
}

const FIXTURES: &[Fixture] = &[
    // options observed in the libaec stream: zero-ros
    Fixture {
        name: "zeros_ros_8bpp",
        bits_per_pixel: 8,
        pixels_per_block: 8,
        reference_sample_interval: 1024,
        nn_preprocess: true,
        rsi_byte_align: false,
        raw: "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        libaec_stream: Some("000080"),
        oxiarc_stream: "e0000000000000001c0000000000000003800000000000000070000000000000000e0000000000000001c0000000000000003800000000000000070000000000000000e0000000000000001c0000000000000003800000000000000070000000000000000e0000000000000001c0000000000000003800000000000000070000000000000000e0000000000000001c0000000000000003800000000000000070000000000000000e0000000000000001c0000000000000003800000000000000070000000000000000e0000000000000001c0000000000000003800000000000000070000000000000000e0000000000000001c0000000000000003800000000000000070000000000000000e0000000000000001c0000000000000003800000000000000070000000000000000e0000000000000001c00000000000000000",
    },
    // options observed in the libaec stream: zero-short
    Fixture {
        name: "zeros_multirsi_8bpp",
        bits_per_pixel: 8,
        pixels_per_block: 8,
        reference_sample_interval: 16,
        nn_preprocess: true,
        rsi_byte_align: false,
        raw: "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        libaec_stream: Some("000400100040"),
        oxiarc_stream: "e0000000000000001c0000000000000003800000000000000070000000000000000e0000000000000001c00000000000000000",
    },
    // options observed in the libaec stream: zero-short
    Fixture {
        name: "zeros_nopp_8bpp",
        bits_per_pixel: 8,
        pixels_per_block: 8,
        reference_sample_interval: 16,
        nn_preprocess: false,
        rsi_byte_align: false,
        raw: "0000000000000000000000000000000000",
        libaec_stream: Some("0420"),
        oxiarc_stream: "e0000000000000001c0000000000000003800000000000000000",
    },
    // options observed in the libaec stream: split-k0, split-k1, split-k2
    Fixture {
        name: "smooth_split_8bpp",
        bits_per_pixel: 8,
        pixels_per_block: 8,
        reference_sample_interval: 32,
        nn_preprocess: true,
        rsi_byte_align: false,
        raw: "7e7b797a7878787a7d7d7a787a787b7d7b7977757576787a7c7978797c7f808383817e7d7d7d807f7f7d7e7b78797b7a7c7f817e8081818082817f817e8081848385828483807f7d80838585828083807d7c7f7c7e7b7d7c7b787b7b7c7a7d7b78787b7e",
        libaec_stream: Some(
            "4fc55ce86db587384ab49f06bad16aa5069e3e2aa4a6b288925c451a4945a1419266af6adad03ca31249eaa91a8b8a5e2040f8",
        ),
        oxiarc_stream: "efc0a060406000009c1800140c100c1813818181818001020270405010206060206f06060a0200000c03c000c0814140810078203020282010000f0401030405040206f06080a08020a0207c18181000140c18178280830282028200f0105060002030603ef0000c0c00000000",
    },
    // options observed in the libaec stream: split-k0
    Fixture {
        name: "ramp_16bpp",
        bits_per_pixel: 16,
        pixels_per_block: 16,
        reference_sample_interval: 32,
        nn_preprocess: true,
        rsi_byte_align: false,
        raw: "0000000100020003000400050006000700080009000a000b000c000d000e000f0010001100120013001400150016001700180019001a001b001c001d001e001f0020002100220023002400250026002700280029002a002b002c002d002e002f0030003100320033003400350036003700380039003a003b003c003d003e003f004000410042004300440045",
        libaec_stream: Some(
            "1000049249249249124924924924910020249249249248924924924924880201249ffc",
        ),
        oxiarc_stream: "f0000000100020002000200020002000200020002000200020002000200020002f0002000200020002000200020002000200020002000200020002000200020002f0020000200020002000200020002000200020002000200020002000200020002f0002000200020002000200020002000200020002000200020002000200020002f00400002000200020002000200000000000000000000000000000000000000000",
    },
    // options observed in the libaec stream: uncomp
    Fixture {
        name: "random_nopp_8bpp",
        bits_per_pixel: 8,
        pixels_per_block: 8,
        reference_sample_interval: 16,
        nn_preprocess: false,
        rsi_byte_align: false,
        raw: "1c3a1c52aabb2e547a285ce26f659b10123eb2135fadb07d8346f64143158f1628a2e962aaa11856ce43a1722498a1d6",
        libaec_stream: Some(
            "e387438a555765ca9de8a17389bd966c43891f5909afd6d83ef8346f64143158f16e5145d2c5554230adf390e85c8926287580",
        ),
        oxiarc_stream: "e387438a555765ca9de8a17389bd966c43891f5909afd6d83ef8346f64143158f16e5145d2c5554230adf390e85c8926287580",
    },
    // options observed in the libaec stream: se, split-k0
    Fixture {
        name: "tiny_se_8bpp",
        bits_per_pixel: 8,
        pixels_per_block: 8,
        reference_sample_interval: 16,
        nn_preprocess: true,
        rsi_byte_align: false,
        raw: "00000001010001000100010001000100010001000100000000010100010000010101010100010100000000010100000000000001000101010001000000010100",
        libaec_stream: Some("201b54aaaa90155736ad203d68c9900d5cabb4"),
        oxiarc_stream: "e0000000200020203c0404040404040407808080808080800070001000101010001e0200000002020003c0000000400040003800000008080800070101010000010001",
    },
    // options observed in the libaec stream: uncomp
    Fixture {
        name: "random_1bpp",
        bits_per_pixel: 1,
        pixels_per_block: 8,
        reference_sample_interval: 16,
        nn_preprocess: true,
        rsi_byte_align: false,
        raw: "00010000010000010001010000000000010100000101000000010001000001000101010101000101",
        libaec_stream: Some("edbf43d577bf0c"),
        oxiarc_stream: "edbf43d577bf0c",
    },
    // options observed in the libaec stream: split-k0
    Fixture {
        name: "ramp_4bpp",
        bits_per_pixel: 4,
        pixels_per_block: 8,
        reference_sample_interval: 16,
        nn_preprocess: true,
        rsi_byte_align: false,
        raw: "000102030405060708090a0b0c0d0e0f000102030405060708090a0b0c0d0e0f0001020304050607",
        libaec_stream: Some("2092492492492482492492492492092492"),
        oxiarc_stream: "e02444445c8888888b80911111722222222e02444444",
    },
    // options observed in the libaec stream: split-k7, uncomp, zero-short
    Fixture {
        name: "mixed_13bpp",
        bits_per_pixel: 13,
        pixels_per_block: 16,
        reference_sample_interval: 32,
        nn_preprocess: true,
        rsi_byte_align: false,
        raw: "00000000000000000000000000000000000000000000000000000000000000000ffe100010021004100510041006100410041003100210031001100310041005072016ae1c4313fb1fdc1da30f580a86169317fe0ad105840c4f0517041d06950000000000000000000000000000000000000000000000000000000000000000",
        libaec_stream: Some(
            "00003000000003fffff040810101080c0010208184040bce416ae5953012f8425c853a68ed262d6a972a6589ee6f0f993c20000000ffff5200000000000000000000000000",
        ),
        oxiarc_stream: "f0000000000000000000000000000000000000000000000000000f7ff0010008004001000400800300000040020020018010004002f3905ab9654c04be1097214e9a3b498b5aa5ca99627b9bc3e64f0f6948000000000000000000000000000000000000000000000000",
    },
    // options observed in the libaec stream: uncomp
    Fixture {
        name: "random_nopp_32bpp",
        bits_per_pixel: 32,
        pixels_per_block: 8,
        reference_sample_interval: 8,
        nn_preprocess: false,
        rsi_byte_align: false,
        raw: "57d92a9915771e8cd629d3b1752b4c561404cf4eac56d550cde52cfa3eec9823ad1529d36cbf1740c797b96eede30d9c16462c87a64d611e66c27c5cc2536fa4b80ea07b552fc3fbb37202bc67b1c9b9",
        libaec_stream: Some(
            "fabec954c8abb8f466b14e9d8ba95a62b0a0267a7562b6aa866f2967d1f764c11feb454a74db2fc5d031e5ee5bbb78c36705918b21e993584799b09f173094dbe93f701d40f6aa5f87f766e40578cf639372cf639372cf639372cf639372cf639372",
        ),
        oxiarc_stream: "fabec954c8abb8f466b14e9d8ba95a62b0a0267a7562b6aa866f2967d1f764c11feb454a74db2fc5d031e5ee5bbb78c36705918b21e993584799b09f173094dbe93f701d40f6aa5f87f766e40578cf639372cf639372cf639372cf639372cf639372",
    },
    // options observed in the libaec stream: split-k0, split-k1, split-k2
    Fixture {
        name: "smooth_32bpp",
        bits_per_pixel: 32,
        pixels_per_block: 16,
        reference_sample_interval: 64,
        nn_preprocess: true,
        rsi_byte_align: false,
        raw: "7fffffff7fffffff7ffffffe7ffffffb7ffffffd7ffffffc7ffffffc7ffffffc7ffffffe7ffffffe8000000180000004800000048000000280000002800000017fffffff7ffffffe7ffffffd7fffffff7ffffffe8000000180000002800000058000000780000006800000048000000580000004800000048000000480000007800000078000000a8000000a8000000780000006800000098000000780000004800000078000000580000007800000088000000680000004800000018000000180000004800000068000000980000008800000078000000680000008800000058000000580000005800000048000000380000006800000088000000b8000000880000008800000078000000a8000000b8000000d800000108000000d80000010800000138000001180000011800000108000001380000012800000158000001780000019800000198000001880000016800000198000001a8000001a8000001780000019800000188000001b8000001b8000001a80000017800000178000001880000015800000128000000f80000011",
        libaec_stream: Some(
            "13fffffffe4f311bb4051398a26bc7a1a05198a452a98da70848f27c4891d311c00000045aabdab44d633023a2c98e698a60410410fff8",
        ),
        oxiarc_stream: "fbfffffff800000000000000080000002800000020000000080000000000000000000000200000000000000030000000300000000000000018000000000000000fc0000000c0000000400000004000000100000000400000018000000080000001800000010000000040000000c000000080000000400000000000000000000001be000000000000000c000000000000000a000000020000000c000000060000000a0000000c00000006000000080000000400000006000000060000000a00000001f00000006000000040000000600000001000000010000000100000004000000050000000000000000000000010000000100000006000000040000000600000005fc00000040000000080000003000000010000000200000003000000028000000300000003000000018000000000000000800000030000000080000003000000027c0000001000000000000000040000000c00000018000000080000000000000014000000100000000400000018000000000000000400000014000000000000000be0000000a0000000a0000000a000000080000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
    },
    // options observed in the libaec stream: split-k0, split-k1
    Fixture {
        name: "smooth_j64_8bpp",
        bits_per_pixel: 8,
        pixels_per_block: 64,
        reference_sample_interval: 128,
        nn_preprocess: true,
        rsi_byte_align: false,
        raw: "7d7d7a78757876747374716e71707171707272706e7073706f6d6b6c6a6d6c6d6f727274777a7877787673767574747371747376747477797a7b787b7c7e817f828080808080818285827f7f81808383848484858484827f7d7a7c79797a7a7774767976787b7a7d7d7d7b7a7774757475747273757676797976747775757674757876747273726f706d6d6b6e6b6b6a696865686a6b",
        libaec_stream: Some(
            "4fb2915a48dcd489aa8d23222d48fa3162548a45776933d406db504285f512663774a49b2489231d92da96328b540640bd1923d60692ea04444a09062041aa08109ffffffffffc",
        ),
        oxiarc_stream: "efa000a060a0c060602040a0a0c0204000208000606080c0a02060604060c0204080c00080c0c060204060a0c02020002060c020c06000c0804040a0c04080c07c180c00000000080818141400100418000800000804000c140c14101400080014141018141018041800000c041414080408040c081008001800140c180c00080fba83018181810082810280018302800080808283020100000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
    },
    // options observed in the libaec stream: uncomp
    Fixture {
        name: "edges_16bpp",
        bits_per_pixel: 16,
        pixels_per_block: 8,
        reference_sample_interval: 8,
        nn_preprocess: true,
        rsi_byte_align: false,
        raw: "0000ffff0000ffff0000ffff0000ffff0000ffff0000ffff0000ffff0000ffff0000ffff0000ffff0000ffff0000ffff0000ffff0000ffff0000ffff0000ffff",
        libaec_stream: Some(
            "f0000fffffffffffffffffffffffffffff0000fffffffffffffffffffffffffffff0000fffffffffffffffffffffffffffff0000ffffffffffffffffffffffffffff",
        ),
        oxiarc_stream: "f0000fffffffffffffffffffffffffffff0000fffffffffffffffffffffffffffff0000fffffffffffffffffffffffffffff0000ffffffffffffffffffffffffffff",
    },
    // options observed in the libaec stream: zero-short
    Fixture {
        name: "const_tail_8bpp",
        bits_per_pixel: 8,
        pixels_per_block: 8,
        reference_sample_interval: 32,
        nn_preprocess: true,
        rsi_byte_align: false,
        raw: "2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a",
        libaec_stream: Some("02a102a8"),
        oxiarc_stream: "e5400000000000001c0000000000000003800000000000000070000000000000000e5400000000000000",
    },
    // options observed in the libaec stream: split-k0, split-k1, split-k2, split-k3, split-k5, uncomp, zero-long, zero-ros
    Fixture {
        name: "mixed_zerolong_8bpp",
        bits_per_pixel: 8,
        pixels_per_block: 8,
        reference_sample_interval: 1024,
        nn_preprocess: true,
        rsi_byte_align: false,
        raw: "0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000007d7b7d7f7e7c7e7c7c7b7c7a7a7c7b7a7877797a7a7b7d7d7d7c7b7b7c7a7b7c7d7f7e7e7e7e7c7c7b7a79787674737372747573737373747573747270706f7172727472717273737375776d691d9b76e8bb305876ba6767a3a2c5c08d12404ed67d61212da19c59ff541eb2636de9cc6b518d5cec6d2b1f1175ab3a62e4f178b21b551189fac908005e6e96f51e24fc6b9ff4a3dc8d000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        libaec_stream: Some(
            "00000670001f0057214975d523b2de090eb244921731aa8a795e92d5542d659686fd42d007f979b49e444cf503cf113000f0028c12cbfb500735a08ddfc63d084c2ffd5b5d94e714e93394337861ecf25062e36ead98aa1f906a1dd391561e27f11b7b87af10285f7e10cfc9468aa5c72c203fcbe00000000040",
        ),
        oxiarc_stream: "e0000000000000001c0000000000000003800000000000000070000000000000000e0000000000000001c0000000000000003800000000000000070000000000000000e0000000000000001c000001f40c101007818201800081018070401010301040200e0408000002020005c0c080808100400038001800080808081f0301000104020300e0000040406040607c0004100800100c078101000002020983f979b49e444cf503cf113000f0028c12cbfb500735a08ddfc63d084c2ffd5b5d94e714e93394337861ecf25062e36ead98aa1f906a1dd391561e27f11b7b87af10285f7e10cfc9468aa5c72ee5fe000000000001c0000000000000003800000000000000070000000000000000e0000000000000001c0000000000000003800000000000000070000000000000000e0000000000000001c00000000000000000",
    },
    // options observed in the libaec stream: n/a (PAD_RSI, direction B only)
    Fixture {
        name: "pad_rsi_8bpp",
        bits_per_pixel: 8,
        pixels_per_block: 8,
        reference_sample_interval: 16,
        nn_preprocess: true,
        rsi_byte_align: true,
        raw: "81848585858789888684858383868684817f8083848584817e81848687878487858685878786898a",
        libaec_stream: None,
        oxiarc_stream: "f020c040000080803c0c0c080c0018000cf0206040c0404020bc1418181008001418f0a04020800020c040",
    },
];

fn sample_count(fx: &Fixture, raw_len: usize) -> usize {
    let bps = match fx.bits_per_pixel {
        1..=8 => 1,
        9..=16 => 2,
        _ => 4,
    };
    raw_len / bps
}

/// Direction A: streams produced by the real libaec encoder must decode
/// byte-identically.
#[test]
fn libaec_encoded_streams_decode_byte_identical() {
    for fx in FIXTURES {
        let Some(stream_hex) = fx.libaec_stream else {
            continue;
        };
        let raw = from_hex(fx.raw);
        let stream = from_hex(stream_hex);
        let params = params_for(fx, sample_count(fx, raw.len()));
        let decoded = decode(&stream, &params)
            .unwrap_or_else(|e| panic!("{}: decode of libaec stream failed: {e}", fx.name));
        assert_eq!(decoded, raw, "{}: libaec stream mis-decoded", fx.name);
    }
}

/// Direction B: the oxiarc encoder's wire format must stay byte-identical to
/// the libaec-verified fixture, and must self-decode.
#[test]
fn oxiarc_encoded_streams_match_libaec_verified_fixtures() {
    for fx in FIXTURES {
        let raw = from_hex(fx.raw);
        let expected = from_hex(fx.oxiarc_stream);
        let params = params_for(fx, sample_count(fx, raw.len()));
        let encoded = encode_bytes(&raw, &params)
            .unwrap_or_else(|e| panic!("{}: encode failed: {e}", fx.name));
        assert_eq!(
            encoded, expected,
            "{}: encoder wire format drifted from the libaec-verified fixture",
            fx.name
        );
        let decoded = decode(&encoded, &params)
            .unwrap_or_else(|e| panic!("{}: self-decode failed: {e}", fx.name));
        assert_eq!(decoded, raw, "{}: self round-trip mismatch", fx.name);
    }
}

/// Truncating any libaec fixture stream must never panic and must never
/// return wrong data.
#[test]
fn truncated_libaec_streams_never_return_wrong_data() {
    for fx in FIXTURES {
        let Some(stream_hex) = fx.libaec_stream else {
            continue;
        };
        let raw = from_hex(fx.raw);
        let stream = from_hex(stream_hex);
        if stream.len() < 2 {
            continue;
        }
        let params = params_for(fx, sample_count(fx, raw.len()));
        for cut in [stream.len() / 2, stream.len() - 1] {
            // Zero-block runs can legitimately terminate before the last
            // byte (they consume no trailing bits), so any Ok output must
            // still be exactly correct; everything else must be Err.
            if let Ok(bytes) = decode(&stream[..cut], &params) {
                assert_eq!(
                    bytes, raw,
                    "{}: truncated stream (cut={cut}) returned wrong Ok data",
                    fx.name
                );
            }
        }
    }
}
