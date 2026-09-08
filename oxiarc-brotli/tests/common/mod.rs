//! Fixtures and drivers shared by the `oxiarc-brotli` test binaries.
//!
//! The `*_BR` constants are the exact output of the reference `brotli` CLI
//! (version 1.1.0) for the corresponding `*_ORIG` input at the quality (`q`)
//! and window (`w`) noted per fixture. They run without any external tooling
//! and permanently pin decode-direction interoperability — both for the
//! one-shot decoder and for the incremental `oxiarc_brotli::BrotliStream`.
//!
//! Regenerate with the `brotli` CLI if fixtures ever need to change:
//! `brotli -c -q <Q> -w <W> < input > output.br`.
//!
//! The fixture set intentionally spans the format surface: an empty stream,
//! plain ASCII, all-zero input at the minimum window (w10), dictionary-rich
//! English text at q11 (static dictionary references + word transforms),
//! UTF-8 multibyte text (UTF8 context mode), structured binary at q11
//! (multiple block types / context maps), incompressible random bytes
//! (uncompressed meta-blocks inside a compressed stream), and mixed content.

#![allow(dead_code)]

pub fn unhex(s: &str) -> Vec<u8> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..clean.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).expect("valid hex"))
        .collect()
}

/// `empty_q1_w22`: reference brotli 1.1.0, q=1 w=22; original 0 bytes, 1 compressed.
pub const EMPTY_Q1_W22_ORIG: &str = "";
pub const EMPTY_Q1_W22_BR: &str = "3b";

/// `hello_q11_w22`: reference brotli 1.1.0, q=11 w=22; original 44 bytes, 34 compressed.
pub const HELLO_Q11_W22_ORIG: &str =
    "48656c6c6f2c2042726f746c69212048656c6c6f2c2042726f746c69212048656c6c6f2c2042726f746c6921";
pub const HELLO_Q11_W22_BR: &str =
    "1b2b00f89dc9e3de3b8d9adaa9a8d9de90d216885095c965080b5d301502f4a1dc0e";

/// `zeros256_q5_w10`: reference brotli 1.1.0, q=5 w=10; original 256 bytes, 11 compressed.
pub const ZEROS256_Q5_W10_ORIG: &str = "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000\
         000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000\
         000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000\
         000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000\
         000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000\
         00000000000000000000000000000000";
pub const ZEROS256_Q5_W10_BR: &str = "a1f807002001108b059203";

/// `dicttext_q11_w22`: reference brotli 1.1.0, q=11 w=22; original 600 bytes, 74 compressed.
pub const DICTTEXT_Q11_W22_ORIG: &str = "5468652074696d65206f66207468652070656f706c652069732074686520776f726b206f6620746865207075626c6963\
         20746861742073686f756c642068617665206265656e206d6164652066726f6d20616c6c20746865736520776f726473\
         2077686963682077657265207573656420666f72207468652066697273742074696d6520647572696e67207468652064\
         6576656c6f706d656e74206f662074686520776f726c6420776964652077656220616e64206f7468657220696e666f72\
         6d6174696f6e2e205468652074696d65206f66207468652070656f706c652069732074686520776f726b206f66207468\
         65207075626c696320746861742073686f756c642068617665206265656e206d6164652066726f6d20616c6c20746865\
         736520776f7264732077686963682077657265207573656420666f72207468652066697273742074696d652064757269\
         6e672074686520646576656c6f706d656e74206f662074686520776f726c6420776964652077656220616e64206f7468\
         657220696e666f726d6174696f6e2e205468652074696d65206f66207468652070656f706c652069732074686520776f\
         726b206f6620746865207075626c696320746861742073686f756c642068617665206265656e206d6164652066726f6d\
         20616c6c20746865736520776f7264732077686963682077657265207573656420666f72207468652066697273742074\
         696d6520647572696e672074686520646576656c6f706d656e74206f662074686520776f726c64207769646520776562\
         20616e64206f7468657220696e666f726d6174696f6e2e20";
pub const DICTTEXT_Q11_W22_BR: &str = "1b5702008c54b5bf162b9bbb5320643a39607f45a9bc2e2da00421bde1f36a1b45046eb2341b663caaa797af2ed9e262\
         57383133304e8b015484240bab381852d46977f164c984afc405";

/// `dicttext_q5_w22`: reference brotli 1.1.0, q=5 w=22; original 600 bytes, 118 compressed.
pub const DICTTEXT_Q5_W22_ORIG: &str = "5468652074696d65206f66207468652070656f706c652069732074686520776f726b206f6620746865207075626c6963\
         20746861742073686f756c642068617665206265656e206d6164652066726f6d20616c6c20746865736520776f726473\
         2077686963682077657265207573656420666f72207468652066697273742074696d6520647572696e67207468652064\
         6576656c6f706d656e74206f662074686520776f726c6420776964652077656220616e64206f7468657220696e666f72\
         6d6174696f6e2e205468652074696d65206f66207468652070656f706c652069732074686520776f726b206f66207468\
         65207075626c696320746861742073686f756c642068617665206265656e206d6164652066726f6d20616c6c20746865\
         736520776f7264732077686963682077657265207573656420666f72207468652066697273742074696d652064757269\
         6e672074686520646576656c6f706d656e74206f662074686520776f726c6420776964652077656220616e64206f7468\
         657220696e666f726d6174696f6e2e205468652074696d65206f66207468652070656f706c652069732074686520776f\
         726b206f6620746865207075626c696320746861742073686f756c642068617665206265656e206d6164652066726f6d\
         20616c6c20746865736520776f7264732077686963682077657265207573656420666f72207468652066697273742074\
         696d6520647572696e672074686520646576656c6f706d656e74206f662074686520776f726c64207769646520776562\
         20616e64206f7468657220696e666f726d6174696f6e2e20";
pub const DICTTEXT_Q5_W22_BR: &str = "1b570200047ab7f1a95e48b66428531f280e84efcc763a50de6f83755d5ed1a44b031a26982864372b2f6fa51d07fbad\
         16b90ba3723909eacd1f5c57ac3dbdfb151f55a65d4a46f42f06a7b14081b91f290760b403d383d0168962b61912d125\
         7c2eacae31de1e992253a7512fa1d2c94ce22b25b800";

/// `utf8_q9_w22`: reference brotli 1.1.0, q=9 w=22; original 560 bytes, 92 compressed.
pub const UTF8_Q9_W22_ORIG: &str = "e38193e38293e381abe381a1e381afe4b896e7958ce38082d09ad0bed0bcd0bfd180d0b5d181d181d0b8d18f20d0b4d0\
         b0d0bdd0bdd18bd185207472c3a873206269656e2e20e38193e38293e381abe381a1e381afe4b896e7958ce38082d09a\
         d0bed0bcd0bfd180d0b5d181d181d0b8d18f20d0b4d0b0d0bdd0bdd18bd185207472c3a873206269656e2e20e38193e3\
         8293e381abe381a1e381afe4b896e7958ce38082d09ad0bed0bcd0bfd180d0b5d181d181d0b8d18f20d0b4d0b0d0bdd0\
         bdd18bd185207472c3a873206269656e2e20e38193e38293e381abe381a1e381afe4b896e7958ce38082d09ad0bed0bc\
         d0bfd180d0b5d181d181d0b8d18f20d0b4d0b0d0bdd0bdd18bd185207472c3a873206269656e2e20e38193e38293e381\
         abe381a1e381afe4b896e7958ce38082d09ad0bed0bcd0bfd180d0b5d181d181d0b8d18f20d0b4d0b0d0bdd0bdd18bd1\
         85207472c3a873206269656e2e20e38193e38293e381abe381a1e381afe4b896e7958ce38082d09ad0bed0bcd0bfd180\
         d0b5d181d181d0b8d18f20d0b4d0b0d0bdd0bdd18bd185207472c3a873206269656e2e20e38193e38293e381abe381a1\
         e381afe4b896e7958ce38082d09ad0bed0bcd0bfd180d0b5d181d181d0b8d18f20d0b4d0b0d0bdd0bdd18bd185207472\
         c3a873206269656e2e20e38193e38293e381abe381a1e381afe4b896e7958ce38082d09ad0bed0bcd0bfd180d0b5d181\
         d181d0b8d18f20d0b4d0b0d0bdd0bdd18bd185207472c3a873206269656e2e20";
pub const UTF8_Q9_W22_BR: &str = "1b2f02001ca9515fccee6022471a500eed007650e54807e0039215b84f0ee2b690eb7778fac170b4db0483493078d880\
         139028b75177dae1c6be7435935453b64c39f7c6f75074d39f5fd4890b830cb6f8df562b1ccbfa2a129f5448";

/// `binary_q11_w18`: reference brotli 1.1.0, q=11 w=18; original 2048 bytes, 244 compressed.
pub const BINARY_Q11_W18_ORIG: &str = "00070e151c232a31383f464d545b626970777e858c939aa1a8afb6bdc4cbd2d9e0e7eef5fc030a11181f262d343b4249\
         50575e656c737a81888f969da4abb2b9c0c7ced5dce3eaf1f8ff060d141b222930373e454c535a61686f767d848b9299\
         a0a7aeb5bcc3cad1d8dfe6edf4fb020910171e252c333a41484f565d646b727980878e959ca3aab1b8bfc6cdd4dbe2e9\
         f0f7fe050c131a21282f363d444b525960676e757c838a91989fa6adb4bbc2c9d0d7dee5ecf3fa01080f161d242b3239\
         40474e555c636a71787f868d949ba2a9b0b7bec5ccd3dae1e8eff6fd040b121920272e353c434a51585f666d747b8289\
         90979ea5acb3bac1c8cfd6dde4ebf2f900070e151c232a31383f464d545b626970777e858c939aa1a8afb6bdc4cbd2d9\
         e0e7eef5fc030a11181f262d343b424950575e656c737a81888f969da4abb2b9c0c7ced5dce3eaf1f8ff060d141b2229\
         30373e454c535a61686f767d848b9299a0a7aeb5bcc3cad1d8dfe6edf4fb020910171e252c333a41484f565d646b7279\
         80878e959ca3aab1b8bfc6cdd4dbe2e9f0f7fe050c131a21282f363d444b525960676e757c838a91989fa6adb4bbc2c9\
         d0d7dee5ecf3fa01080f161d242b323940474e555c636a71787f868d949ba2a9b0b7bec5ccd3dae1e8eff6fd040b1219\
         20272e353c434a51585f666d747b828990979ea5acb3bac1c8cfd6dde4ebf2f900070e151c232a31383f464d545b6269\
         70777e858c939aa1a8afb6bdc4cbd2d9e0e7eef5fc030a11181f262d343b424950575e656c737a81888f969da4abb2b9\
         c0c7ced5dce3eaf1f8ff060d141b222930373e454c535a61686f767d848b9299a0a7aeb5bcc3cad1d8dfe6edf4fb0209\
         10171e252c333a41484f565d646b727980878e959ca3aab1b8bfc6cdd4dbe2e9f0f7fe050c131a21282f363d444b5259\
         60676e757c838a91989fa6adb4bbc2c9d0d7dee5ecf3fa01080f161d242b323940474e555c636a71787f868d949ba2a9\
         b0b7bec5ccd3dae1e8eff6fd040b121920272e353c434a51585f666d747b828990979ea5acb3bac1c8cfd6dde4ebf2f9\
         00070e151c232a31383f464d545b626970777e858c939aa1a8afb6bdc4cbd2d9e0e7eef5fc030a11181f262d343b4249\
         50575e656c737a81888f969da4abb2b9c0c7ced5dce3eaf1f8ff060d141b222930373e454c535a61686f767d848b9299\
         a0a7aeb5bcc3cad1d8dfe6edf4fb020910171e252c333a41484f565d646b727980878e959ca3aab1b8bfc6cdd4dbe2e9\
         f0f7fe050c131a21282f363d444b525960676e757c838a91989fa6adb4bbc2c9d0d7dee5ecf3fa01080f161d242b3239\
         40474e555c636a71787f868d949ba2a9b0b7bec5ccd3dae1e8eff6fd040b121920272e353c434a51585f666d747b8289\
         90979ea5acb3bac1c8cfd6dde4ebf2f900070e151c232a31383f464d545b626970777e858c939aa1a8afb6bdc4cbd2d9\
         e0e7eef5fc030a11181f262d343b424950575e656c737a81888f969da4abb2b9c0c7ced5dce3eaf1f8ff060d141b2229\
         30373e454c535a61686f767d848b9299a0a7aeb5bcc3cad1d8dfe6edf4fb020910171e252c333a41484f565d646b7279\
         80878e959ca3aab1b8bfc6cdd4dbe2e9f0f7fe050c131a21282f363d444b525960676e757c838a91989fa6adb4bbc2c9\
         d0d7dee5ecf3fa01080f161d242b323940474e555c636a71787f868d949ba2a9b0b7bec5ccd3dae1e8eff6fd040b1219\
         20272e353c434a51585f666d747b828990979ea5acb3bac1c8cfd6dde4ebf2f900070e151c232a31383f464d545b6269\
         70777e858c939aa1a8afb6bdc4cbd2d9e0e7eef5fc030a11181f262d343b424950575e656c737a81888f969da4abb2b9\
         c0c7ced5dce3eaf1f8ff060d141b222930373e454c535a61686f767d848b9299a0a7aeb5bcc3cad1d8dfe6edf4fb0209\
         10171e252c333a41484f565d646b727980878e959ca3aab1b8bfc6cdd4dbe2e9f0f7fe050c131a21282f363d444b5259\
         60676e757c838a91989fa6adb4bbc2c9d0d7dee5ecf3fa01080f161d242b323940474e555c636a71787f868d949ba2a9\
         b0b7bec5ccd3dae1e8eff6fd040b121920272e353c434a51585f666d747b828990979ea5acb3bac1c8cfd6dde4ebf2f9\
         00070e151c232a31383f464d545b626970777e858c939aa1a8afb6bdc4cbd2d9e0e7eef5fc030a11181f262d343b4249\
         50575e656c737a81888f969da4abb2b9c0c7ced5dce3eaf1f8ff060d141b222930373e454c535a61686f767d848b9299\
         a0a7aeb5bcc3cad1d8dfe6edf4fb020910171e252c333a41484f565d646b727980878e959ca3aab1b8bfc6cdd4dbe2e9\
         f0f7fe050c131a21282f363d444b525960676e757c838a91989fa6adb4bbc2c9d0d7dee5ecf3fa01080f161d242b3239\
         40474e555c636a71787f868d949ba2a9b0b7bec5ccd3dae1e8eff6fd040b121920272e353c434a51585f666d747b8289\
         90979ea5acb3bac1c8cfd6dde4ebf2f900070e151c232a31383f464d545b626970777e858c939aa1a8afb6bdc4cbd2d9\
         e0e7eef5fc030a11181f262d343b424950575e656c737a81888f969da4abb2b9c0c7ced5dce3eaf1f8ff060d141b2229\
         30373e454c535a61686f767d848b9299a0a7aeb5bcc3cad1d8dfe6edf4fb020910171e252c333a41484f565d646b7279\
         80878e959ca3aab1b8bfc6cdd4dbe2e9f0f7fe050c131a21282f363d444b525960676e757c838a91989fa6adb4bbc2c9\
         d0d7dee5ecf3fa01080f161d242b323940474e555c636a71787f868d949ba2a9b0b7bec5ccd3dae1e8eff6fd040b1219\
         20272e353c434a51585f666d747b828990979ea5acb3bac1c8cfd6dde4ebf2f9";
pub const BINARY_Q11_W18_BR: &str = "13ff07f8af0a6c470781647378963df6d9e0abc173ab54ee2bbf95dfc6918f8d3d66b50ece4b6df6c9c17921b4f92207\
         e795d0e68b1c9c97429b7d321c8d577cba028ef1a8c9e3f1174cb1a372d8bf8eca5658f4dbbb109d86fd7bb09286c2b7\
         e50e48d0bdb499af0af29a9bd4d33900eeea7c1d3f583fa2d3b07f99ca5658f4dbdb12c90cf26e7d66ac62bfbd37925c\
         80bb3a5f1ef29a9bd4d359024e75e26a3cfe8dd9329dfb478d2d56b1dfde442433c8bbf569b068f859bbdc1392a15b9f\
         0b0acacf6c994e0738d589abf1f87ba6d85139ecdf4a4d1e8fff255a07c9d0ad4fc4a2e167ed72d748d0bdb499af110a\
         df96fb24";

/// `random_q9_w22`: reference brotli 1.1.0, q=9 w=22; original 512 bytes, 516 compressed.
pub const RANDOM_Q9_W22_ORIG: &str = "38b4e652e44da7f2370d9e260e27136550a4a3a6d07f5c0c332f8b1224083fd22b902f8911e81818f8c99d5d5d983195\
         7504d90e945de2e8f54ee781cc75f636d85099095aa300165a67036f9b540d6b8f0be21124179c3dd9f73817ce6e118d\
         264aad6cb6dd210faf94acd3cf92c190237cb11f5d108cf25930263938b370a1b5769fa0f1483f95a90d9df2f130d60f\
         cf04bd93f50ae69514da8c659ce2b10cccdaebf990d19838b0d7ec0b3e97818ecb96c4dbadbe172296d5234a42b24c6b\
         a4e6ed24ec636a8ac0a1271e5866279238aaf84e58056d8f2fa8edd094ba97ae8b15442ee2db611a91bfe39469733a92\
         47d58fa3c55018300372555fd235f11829fb388c22e44cb637f01210c3707a90b405420fb169779edfb5b9342405157f\
         54b12eae62d11e887eb0766d1877f8c6eff26b5010af3177d161e79587a766ec30e4037458a9905cad87bd4c77e2983f\
         27745ccb9a31052e944cf1b220eaa2c7fb1b7d3e3f73f414af6e0d935520dd4c2147738606f2bf7ec70209e0cd05ee57\
         20edbcba3acce672084ab649fcbce49b38bdecface4abd12108f391ebc070e83e8180a6bd4f43a2afffcd3c12ef89057\
         5575e826e2cbeaee82af2c7d696cf46b977c090af4e146f6d03110ab86efde139eeabac37a0dde8ef2d3b1925e1302ca\
         57501fe0ca9a7fd1ccc15150424212578fe0feb17b4aa559cd9f28984b14267f";
pub const RANDOM_Q9_W22_BR: &str = "8bff8038b4e652e44da7f2370d9e260e27136550a4a3a6d07f5c0c332f8b1224083fd22b902f8911e81818f8c99d5d5d\
         9831957504d90e945de2e8f54ee781cc75f636d85099095aa300165a67036f9b540d6b8f0be21124179c3dd9f73817ce\
         6e118d264aad6cb6dd210faf94acd3cf92c190237cb11f5d108cf25930263938b370a1b5769fa0f1483f95a90d9df2f1\
         30d60fcf04bd93f50ae69514da8c659ce2b10cccdaebf990d19838b0d7ec0b3e97818ecb96c4dbadbe172296d5234a42\
         b24c6ba4e6ed24ec636a8ac0a1271e5866279238aaf84e58056d8f2fa8edd094ba97ae8b15442ee2db611a91bfe39469\
         733a9247d58fa3c55018300372555fd235f11829fb388c22e44cb637f01210c3707a90b405420fb169779edfb5b93424\
         05157f54b12eae62d11e887eb0766d1877f8c6eff26b5010af3177d161e79587a766ec30e4037458a9905cad87bd4c77\
         e2983f27745ccb9a31052e944cf1b220eaa2c7fb1b7d3e3f73f414af6e0d935520dd4c2147738606f2bf7ec70209e0cd\
         05ee5720edbcba3acce672084ab649fcbce49b38bdecface4abd12108f391ebc070e83e8180a6bd4f43a2afffcd3c12e\
         f890575575e826e2cbeaee82af2c7d696cf46b977c090af4e146f6d03110ab86efde139eeabac37a0dde8ef2d3b1925e\
         1302ca57501fe0ca9a7fd1ccc15150424212578fe0feb17b4aa559cd9f28984b14267f03";

/// `mixed_q11_w22`: reference brotli 1.1.0, q=11 w=22; original 3760 bytes, 866 compressed.
pub const MIXED_Q11_W22_ORIG: &str = "5468652074696d65206f66207468652070656f706c652069732074686520776f726b206f6620746865207075626c6963\
         20746861742073686f756c642068617665206265656e206d6164652066726f6d20616c6c20746865736520776f726473\
         2077686963682077657265207573656420666f72207468652066697273742074696d6520647572696e67207468652064\
         6576656c6f706d656e74206f662074686520776f726c6420776964652077656220616e64206f7468657220696e666f72\
         6d6174696f6e2e205468652074696d65206f66207468652070656f706c652069732074686520776f726b206f66207468\
         65207075626c696320746861742073686f756c642068617665206265656e206d6164652066726f6d20616c6c20746865\
         736520776f7264732077686963682077657265207573656420666f72207468652066697273742074696d652064757269\
         6e672074686520646576656c6f706d656e74206f662074686520776f726c6420776964652077656220616e64206f7468\
         657220696e666f726d6174696f6e2e205468652074696d65206f66207468652070656f706c652069732074686520776f\
         726b206f6620746865207075626c696320746861742073686f756c642068617665206265656e206d6164652066726f6d\
         20616c6c20746865736520776f7264732077686963682077657265207573656420666f72207468652066697273742074\
         696d6520647572696e672074686520646576656c6f706d656e74206f662074686520776f726c64207769646520776562\
         20616e64206f7468657220696e666f726d6174696f6e2e2000070e151c232a31383f464d545b626970777e858c939aa1\
         a8afb6bdc4cbd2d9e0e7eef5fc030a11181f262d343b424950575e656c737a81888f969da4abb2b9c0c7ced5dce3eaf1\
         f8ff060d141b222930373e454c535a61686f767d848b9299a0a7aeb5bcc3cad1d8dfe6edf4fb020910171e252c333a41\
         484f565d646b727980878e959ca3aab1b8bfc6cdd4dbe2e9f0f7fe050c131a21282f363d444b525960676e757c838a91\
         989fa6adb4bbc2c9d0d7dee5ecf3fa01080f161d242b323940474e555c636a71787f868d949ba2a9b0b7bec5ccd3dae1\
         e8eff6fd040b121920272e353c434a51585f666d747b828990979ea5acb3bac1c8cfd6dde4ebf2f900070e151c232a31\
         383f464d545b626970777e858c939aa1a8afb6bdc4cbd2d9e0e7eef5fc030a11181f262d343b424950575e656c737a81\
         888f969da4abb2b9c0c7ced5dce3eaf1f8ff060d141b222930373e454c535a61686f767d848b9299a0a7aeb5bcc3cad1\
         d8dfe6edf4fb020910171e252c333a41484f565d646b727980878e959ca3aab1b8bfc6cdd4dbe2e9f0f7fe050c131a21\
         282f363d444b525960676e757c838a91989fa6adb4bbc2c9d0d7dee5ecf3fa01080f161d242b323940474e555c636a71\
         787f868d949ba2a9b0b7bec5ccd3dae1e8eff6fd040b121920272e353c434a51585f666d747b828990979ea5acb3bac1\
         c8cfd6dde4ebf2f900070e151c232a31383f464d545b626970777e858c939aa1a8afb6bdc4cbd2d9e0e7eef5fc030a11\
         181f262d343b424950575e656c737a81888f969da4abb2b9c0c7ced5dce3eaf1f8ff060d141b222930373e454c535a61\
         686f767d848b9299a0a7aeb5bcc3cad1d8dfe6edf4fb020910171e252c333a41484f565d646b727980878e959ca3aab1\
         b8bfc6cdd4dbe2e9f0f7fe050c131a21282f363d444b525960676e757c838a91989fa6adb4bbc2c9d0d7dee5ecf3fa01\
         080f161d242b323940474e555c636a71787f868d949ba2a9b0b7bec5ccd3dae1e8eff6fd040b121920272e353c434a51\
         585f666d747b828990979ea5acb3bac1c8cfd6dde4ebf2f900070e151c232a31383f464d545b626970777e858c939aa1\
         a8afb6bdc4cbd2d9e0e7eef5fc030a11181f262d343b424950575e656c737a81888f969da4abb2b9c0c7ced5dce3eaf1\
         f8ff060d141b222930373e454c535a61686f767d848b9299a0a7aeb5bcc3cad1d8dfe6edf4fb020910171e252c333a41\
         484f565d646b727980878e959ca3aab1b8bfc6cdd4dbe2e9f0f7fe050c131a21282f363d444b525960676e757c838a91\
         989fa6adb4bbc2c9d0d7dee5ecf3fa01080f161d242b323940474e555c636a71787f868d949ba2a9b0b7bec5ccd3dae1\
         e8eff6fd040b121920272e353c434a51585f666d747b828990979ea5acb3bac1c8cfd6dde4ebf2f900070e151c232a31\
         383f464d545b626970777e858c939aa1a8afb6bdc4cbd2d9e0e7eef5fc030a11181f262d343b424950575e656c737a81\
         888f969da4abb2b9c0c7ced5dce3eaf1f8ff060d141b222930373e454c535a61686f767d848b9299a0a7aeb5bcc3cad1\
         d8dfe6edf4fb020910171e252c333a41484f565d646b727980878e959ca3aab1b8bfc6cdd4dbe2e9f0f7fe050c131a21\
         282f363d444b525960676e757c838a91989fa6adb4bbc2c9d0d7dee5ecf3fa01080f161d242b323940474e555c636a71\
         787f868d949ba2a9b0b7bec5ccd3dae1e8eff6fd040b121920272e353c434a51585f666d747b828990979ea5acb3bac1\
         c8cfd6dde4ebf2f900070e151c232a31383f464d545b626970777e858c939aa1a8afb6bdc4cbd2d9e0e7eef5fc030a11\
         181f262d343b424950575e656c737a81888f969da4abb2b9c0c7ced5dce3eaf1f8ff060d141b222930373e454c535a61\
         686f767d848b9299a0a7aeb5bcc3cad1d8dfe6edf4fb020910171e252c333a41484f565d646b727980878e959ca3aab1\
         b8bfc6cdd4dbe2e9f0f7fe050c131a21282f363d444b525960676e757c838a91989fa6adb4bbc2c9d0d7dee5ecf3fa01\
         080f161d242b323940474e555c636a71787f868d949ba2a9b0b7bec5ccd3dae1e8eff6fd040b121920272e353c434a51\
         585f666d747b828990979ea5acb3bac1c8cfd6dde4ebf2f900070e151c232a31383f464d545b626970777e858c939aa1\
         a8afb6bdc4cbd2d9e0e7eef5fc030a11181f262d343b424950575e656c737a81888f969da4abb2b9c0c7ced5dce3eaf1\
         f8ff060d141b222930373e454c535a61686f767d848b9299a0a7aeb5bcc3cad1d8dfe6edf4fb020910171e252c333a41\
         484f565d646b727980878e959ca3aab1b8bfc6cdd4dbe2e9f0f7fe050c131a21282f363d444b525960676e757c838a91\
         989fa6adb4bbc2c9d0d7dee5ecf3fa01080f161d242b323940474e555c636a71787f868d949ba2a9b0b7bec5ccd3dae1\
         e8eff6fd040b121920272e353c434a51585f666d747b828990979ea5acb3bac1c8cfd6dde4ebf2f900070e151c232a31\
         383f464d545b626970777e858c939aa1a8afb6bdc4cbd2d9e0e7eef5fc030a11181f262d343b424950575e656c737a81\
         888f969da4abb2b9c0c7ced5dce3eaf1f8ff060d141b222930373e454c535a61686f767d848b9299a0a7aeb5bcc3cad1\
         d8dfe6edf4fb020910171e252c333a41484f565d646b727980878e959ca3aab1b8bfc6cdd4dbe2e9f0f7fe050c131a21\
         282f363d444b525960676e757c838a91989fa6adb4bbc2c9d0d7dee5ecf3fa01080f161d242b323940474e555c636a71\
         787f868d949ba2a9b0b7bec5ccd3dae1e8eff6fd040b121920272e353c434a51585f666d747b828990979ea5acb3bac1\
         c8cfd6dde4ebf2f938b4e652e44da7f2370d9e260e27136550a4a3a6d07f5c0c332f8b1224083fd22b902f8911e81818\
         f8c99d5d5d9831957504d90e945de2e8f54ee781cc75f636d85099095aa300165a67036f9b540d6b8f0be21124179c3d\
         d9f73817ce6e118d264aad6cb6dd210faf94acd3cf92c190237cb11f5d108cf25930263938b370a1b5769fa0f1483f95\
         a90d9df2f130d60fcf04bd93f50ae69514da8c659ce2b10cccdaebf990d19838b0d7ec0b3e97818ecb96c4dbadbe1722\
         96d5234a42b24c6ba4e6ed24ec636a8ac0a1271e5866279238aaf84e58056d8f2fa8edd094ba97ae8b15442ee2db611a\
         91bfe39469733a9247d58fa3c55018300372555fd235f11829fb388c22e44cb637f01210c3707a90b405420fb169779e\
         dfb5b9342405157f54b12eae62d11e887eb0766d1877f8c6eff26b5010af3177d161e79587a766ec30e4037458a9905c\
         ad87bd4c77e2983f27745ccb9a31052e944cf1b220eaa2c7fb1b7d3e3f73f414af6e0d935520dd4c2147738606f2bf7e\
         c70209e0cd05ee5720edbcba3acce672084ab649fcbce49b38bdecface4abd12108f391ebc070e83e8180a6bd4f43a2a\
         fffcd3c12ef890575575e826e2cbeaee82af2c7d696cf46b977c090af4e146f6d03110ab86efde139eeabac37a0dde8e\
         f2d3b1925e1302ca57501fe0ca9a7fd1ccc15150424212578fe0feb17b4aa559cd9f28984b14267f5468652074696d65\
         206f66207468652070656f706c652069732074686520776f726b206f6620746865207075626c69632074686174207368\
         6f756c642068617665206265656e206d6164652066726f6d20616c6c20746865736520776f7264732077686963682077\
         657265207573656420666f72207468652066697273742074696d6520647572696e672074686520646576656c6f706d65\
         6e74206f662074686520776f726c6420776964652077656220616e64206f7468657220696e666f726d6174696f6e2e20\
         5468652074696d65206f66207468652070656f706c652069732074686520776f726b206f6620746865207075626c6963\
         20746861742073686f756c642068617665206265656e206d6164652066726f6d20616c6c20746865736520776f726473\
         2077686963682077657265207573656420666f72207468652066697273742074696d6520647572696e67207468652064\
         6576656c6f706d656e74206f662074686520776f726c6420776964652077656220616e64206f7468657220696e666f72\
         6d6174696f6e2e205468652074696d65206f66207468652070656f706c652069732074686520776f726b206f66207468\
         65207075626c696320746861742073686f756c642068617665206265656e206d6164652066726f6d20616c6c20746865\
         736520776f7264732077686963682077657265207573656420666f72207468652066697273742074696d652064757269\
         6e672074686520646576656c6f706d656e74206f662074686520776f726c6420776964652077656220616e64206f7468\
         657220696e666f726d6174696f6e2e20";
pub const MIXED_Q11_W22_BR: &str = "1baf0ee01f07b939f4c2faee29422b0006e00000506baddfe9703a3960af5a90ca2b91c4bcc8394a6f58be6fb3c32260\
         57255929c3d93cfc78158f949ce852d5dcf9f21053332758888099ac4033343266ddddd448ba50fa5609fcf974050087\
         43c521a662e4e0179355d136b27470f70b8d49ce2aaca86feb1d995eda3c38bf7bfd018642c4c027a365e116925450d7\
         33b571f60a8c884fcb2da96eea1c189f5bdd39be7afcf8078345c126a26460e7139551d632b4b077f30d894eca2c28af\
         6bed199e5adcd83fbb7df906824440c723a561e612949057d335b176f20c088f4bcd29ae6aece81f9b5dd93eba7c78ff\
         038541c622a4a067e3159156d23430b773f5098e4accc82fab6de91e9a5c58df3bbd79fe02848047c325a166e2141097\
         53d531b672f4f00f8b4dc92eaa6c68ef1b9d59de3abcb87ffb058146c22420a763e5119652d4d037b375f10e8a4c48cf\
         2bad69ee1a9c985fdb3db97efa24fdbe4a1c2d674a27b2e54fecb0796470e4c8a60a25c5650bfe3a30ccf4d1482410fc\
         4bd409f491881718181f93b9baba198ca9ae209b7029ba4717af72e78133ae6f6c1b0a99905ac500689803db67abc05a\
         c7431f2192a0e7f06ebe73a0cfd921c69249d5dab4ed12c2d7a7d42ccf270d2610fb34e2eb22c43c6932907172343b14\
         b6bae5173c4af0a756c2e63e3d32acc1cf83f426bf429da5a26cc598e61c35c2cc6c5d7f262c667034acdf40f3a507c6\
         4da78d6cd7f6a113a5ad124b0935c958979cdd92dc185b450d1492e36998912771547dc86980dac6d357dc2ea474a5d7\
         45a38ad01d6d1b6225f61fa7583a732589afc6178f2a6030003ba9ea2fb13e62507e73c4109dc8b4b13f20210c3b7825\
         b4800ac1375abae7edb776b29080a2faab34d2d5192de245f835b8d962b87f8cdd3f592b20d433ba2f1a9ea786979bdd\
         309c00bb685426e8d486f7cab81f65f093bbe84c673182d2a5c83c36115c158d7f63fbf2f13bbfa0d4dbc126ab12ecca\
         108a3b87813de9370e0479300b7aa74e70dbd3c53573e60421d526f9d37392cdd17bf33527d58b8410cf89d7030e177c\
         810165bdf2c245f5ffd320dd4782baaaeb05d9d1f4d55d503d8dafa5cd8b75ba0f24d4cb626f0b8c08d561f77bc87957\
         5dc35eb07b714fcb8d497ac84053ea0af8075359fe8b33838a0a424248eaf1077f8dde52a59ab3f91419d22864fe267c\
         0401";

// ─── The fixture table ──────────────────────────────────────────────────────

/// One reference fixture: a label, the original bytes, and the exact reference
/// `brotli` stream that encodes them.
pub struct ReferenceVector {
    /// Fixture name, used in assertion messages.
    pub name: &'static str,
    /// The original (decompressed) bytes.
    pub original: Vec<u8>,
    /// The reference `brotli` CLI output.
    pub compressed: Vec<u8>,
}

/// Every reference fixture, decoded from hex.
pub fn reference_vectors() -> Vec<ReferenceVector> {
    let table: &[(&'static str, &'static str, &'static str)] = &[
        ("empty_q1_w22", EMPTY_Q1_W22_ORIG, EMPTY_Q1_W22_BR),
        ("hello_q11_w22", HELLO_Q11_W22_ORIG, HELLO_Q11_W22_BR),
        ("zeros256_q5_w10", ZEROS256_Q5_W10_ORIG, ZEROS256_Q5_W10_BR),
        (
            "dicttext_q11_w22",
            DICTTEXT_Q11_W22_ORIG,
            DICTTEXT_Q11_W22_BR,
        ),
        ("dicttext_q5_w22", DICTTEXT_Q5_W22_ORIG, DICTTEXT_Q5_W22_BR),
        ("utf8_q9_w22", UTF8_Q9_W22_ORIG, UTF8_Q9_W22_BR),
        ("binary_q11_w18", BINARY_Q11_W18_ORIG, BINARY_Q11_W18_BR),
        ("random_q9_w22", RANDOM_Q9_W22_ORIG, RANDOM_Q9_W22_BR),
        ("mixed_q11_w22", MIXED_Q11_W22_ORIG, MIXED_Q11_W22_BR),
    ];
    table
        .iter()
        .map(|(name, orig, br)| ReferenceVector {
            name,
            original: unhex(orig),
            compressed: unhex(br),
        })
        .collect()
}

// ─── Payload corpus ─────────────────────────────────────────────────────────

/// Deterministic pseudo-random bytes (splitmix-style), so a failure is
/// reproducible without committing a fixture.
pub fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u8
        })
        .collect()
}

/// The payload shapes the stream tests decode, chosen to exercise every
/// meta-block flavour and every resumption path:
///
/// * empty and one-byte streams (the `ISLASTEMPTY` and single-literal paths);
/// * a full window's worth and a window plus one byte;
/// * highly compressible input (long copies, distance-1 runs);
/// * incompressible input (uncompressed meta-blocks inside a compressed
///   stream);
/// * dictionary-rich English text (static dictionary words and transforms);
/// * UTF-8 text (the UTF8 context mode);
/// * structured binary (multiple block types and context maps).
pub fn payload_corpus() -> Vec<(&'static str, Vec<u8>)> {
    let mut out: Vec<(&'static str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("one_byte", vec![b'Z']),
        ("two_bytes", vec![0x00, 0xFF]),
        (
            "text",
            b"The quick brown fox jumps over the lazy dog. ".repeat(120),
        ),
        ("uniform_64k", vec![0x5Au8; 64 * 1024]),
        ("window_exact", vec![7u8; 65_520]),
        ("window_plus_one", vec![7u8; 65_521]),
        ("incompressible", pseudo_random(96 * 1024, 0x1234_5678)),
        (
            "dictionary_text",
            b"The time of the people is the work of the public that should have been \
          made from all these words which were used for the first time during the \
          development of the world wide web and other information. "
                .repeat(30),
        ),
        (
            "utf8",
            "こんにちは世界。Компрессия данных。Ελληνικά κείμενα。"
                .repeat(200)
                .into_bytes(),
        ),
    ];
    let mut structured = Vec::new();
    for i in 0..3000u32 {
        structured.extend_from_slice(&i.to_le_bytes());
        structured.extend_from_slice(format!("record-{i:06}\n").as_bytes());
    }
    out.push(("structured_binary", structured));
    let mut mixed = Vec::new();
    mixed.extend_from_slice(&vec![0u8; 20_000]);
    mixed.extend_from_slice(&pseudo_random(20_000, 99));
    mixed.extend_from_slice(
        b"tail text that should reuse dictionary words. "
            .repeat(50)
            .as_slice(),
    );
    out.push(("mixed", mixed));
    out
}

// ─── Incremental drivers ────────────────────────────────────────────────────

use oxiarc_brotli::{BrotliError, BrotliStatus, BrotliStream};
use oxiarc_core::traits::FlushMode;

/// How a driver chunks the compressed input.
#[derive(Debug, Clone, Copy)]
pub enum InputSchedule {
    /// Fixed-size chunks.
    Fixed(usize),
    /// The whole stream in one call.
    Whole,
}

/// Drive `stream` over `data`, feeding `input` in the given chunk sizes and
/// decoding into an `out_chunk`-byte output slice.
///
/// The last chunk is flagged [`FlushMode::Finish`]; every earlier one uses
/// [`FlushMode::None`]. `call_budget` bounds the number of `decode` calls so a
/// stalled decoder fails the test instead of hanging it.
///
/// Every byte of `data` is offered even after the decoder reports
/// [`BrotliStatus::StreamEnd`] — a push decoder can only reject trailing bytes
/// it is actually shown, so a driver that stopped early would silently exempt
/// the "no stream concatenation" rule from every test.
pub fn drive(
    stream: &mut BrotliStream,
    data: &[u8],
    schedule: InputSchedule,
    out_chunk: usize,
    call_budget: u64,
) -> Result<Vec<u8>, BrotliError> {
    let in_chunk = match schedule {
        InputSchedule::Fixed(n) => n.max(1),
        InputSchedule::Whole => data.len().max(1),
    };
    let mut decoded = Vec::new();
    let mut buf = vec![0u8; out_chunk.max(1)];
    let mut pos = 0usize;
    let mut calls = 0u64;
    loop {
        calls += 1;
        assert!(
            calls <= call_budget,
            "decoder exceeded {call_budget} calls without terminating"
        );
        let end = (pos + in_chunk).min(data.len());
        let flush = if end == data.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = stream.decode(&data[pos..end], &mut buf, flush)?;
        pos += progress.consumed;
        decoded.extend_from_slice(&buf[..progress.produced]);
        if progress.status == BrotliStatus::StreamEnd && pos == data.len() {
            break;
        }
        assert!(
            progress.consumed > 0 || progress.produced > 0 || end < data.len(),
            "decoder made no progress with input available"
        );
    }
    stream.finish()?;
    Ok(decoded)
}

/// [`drive`] with a fresh default decoder.
pub fn decode_incremental(
    data: &[u8],
    schedule: InputSchedule,
    out_chunk: usize,
) -> Result<Vec<u8>, BrotliError> {
    let mut stream = BrotliStream::new();
    drive(&mut stream, data, schedule, out_chunk, call_budget(data))
}

/// A generous call budget for a stream: enough for one-byte input and
/// one-byte output at the same time, plus slack.
pub fn call_budget(data: &[u8]) -> u64 {
    (data.len() as u64 + 1) * 64 + 4_000_000
}

/// Hand-build a one-meta-block stream whose single command copies `copy_len`
/// bytes out of the shared dictionary, starting 4 bytes before its end.
///
/// `copy_len <= 4` stops inside the dictionary; anything longer would run past
/// its end. Neither this crate's encoder nor `brotli 1.1.0 -D` emits the
/// latter — both stop a dictionary match at the dictionary's end — so the
/// decoders' path for it can only be reached by a stream written by hand. The
/// stream is otherwise legal RFC 7932: one literal block type, one
/// insert-and-copy type and one distance type, each with a single-symbol
/// *simple* prefix code (which costs zero bits per symbol), so the whole
/// meta-block is a handful of header fields plus two extra-bit fields.
///
/// Layout produced below, in bit order:
///
/// ```text
/// WBITS=16                      0
/// ISLAST=1, ISLASTEMPTY=0       1 0
/// MNIBBLES=4, MLEN-1            00, 5 + copy_len - 1 in 16 bits
/// NBLTYPES L/I/D = 1            0 0 0
/// NPOSTFIX=0, NDIRECT=0         00, 0000
/// context mode LSB6             00
/// NTREESL=1, NTREESD=1          0 0
/// literal code: simple, 'A'     01, 00, 8 bits
/// insert-and-copy code          01, 00, 10 bits
/// distance code: symbol 19      01, 00, 6 bits
/// copy extra bits               copy_len - base
/// distance extra bits (dist 9)  00
/// padding                       zeros to the byte boundary
/// ```
///
/// Returns the stream, the dictionary, and the bytes the stream means when it
/// is legal (`None` when the copy runs past the dictionary's end).
pub fn hand_built_dictionary_copy(copy_len: u32) -> (Vec<u8>, Vec<u8>, Option<Vec<u8>>) {
    use oxiarc_brotli::bit_writer::BitWriter;
    use oxiarc_brotli::huffman::alphabet_bits;
    use oxiarc_brotli::tables::{COPY_LENGTH_CODES, INSERT_LENGTH_CODES, decompose_command};

    let insert_len = 5u32;
    let distance = 9usize;
    // With `insert_len` literals produced, `max_backward` is 5, so distance 9
    // reaches 4 bytes past it: the dictionary's last 4 bytes.
    let available = 4usize;

    // The insert-and-copy symbol for (insert 5, copy `copy_len`) with an
    // explicit distance. Found by inverting the crate's own decomposition, so
    // the test cannot drift from the table it is testing against.
    let mut found = None;
    for sym in 0u16..704 {
        let (ins_code, cpy_code, implicit) = decompose_command(sym);
        if implicit {
            continue;
        }
        let (ins_base, ins_extra) = INSERT_LENGTH_CODES[ins_code as usize];
        let (cpy_base, cpy_extra) = COPY_LENGTH_CODES[cpy_code as usize];
        if ins_base == insert_len
            && ins_extra == 0
            && cpy_base <= copy_len
            && copy_len < cpy_base + (1u32 << cpy_extra)
        {
            found = Some((sym, cpy_base, cpy_extra));
            break;
        }
    }
    let (ic_symbol, cpy_base, cpy_extra) =
        found.expect("an insert-5 command symbol must exist for this copy length");

    let mut w = BitWriter::with_capacity(64);
    w.write_bit(false).expect("wbits"); // WBITS = 16
    w.write_bit(true).expect("islast");
    w.write_bit(false).expect("islastempty");
    w.write_bits(0, 2).expect("mnibbles"); // 4 nibbles
    w.write_bits(insert_len + copy_len - 1, 16).expect("mlen");
    for _ in 0..3 {
        w.write_bit(false).expect("nbltypes"); // L, I, D = 1
    }
    w.write_bits(0, 2).expect("npostfix");
    w.write_bits(0, 4).expect("ndirect");
    w.write_bits(0, 2).expect("context mode"); // LSB6
    w.write_bit(false).expect("ntreesl");
    w.write_bit(false).expect("ntreesd");
    // Three single-symbol simple prefix codes.
    for (alphabet, symbol) in [
        (256u32, u32::from(b'A')),
        (704, u32::from(ic_symbol)),
        (64, 19),
    ] {
        w.write_bits(1, 2).expect("hskip"); // hskip == 1 -> simple
        w.write_bits(0, 2).expect("nsym-1"); // one symbol
        w.write_bits(symbol, alphabet_bits(alphabet))
            .expect("symbol");
    }
    // The command: symbols cost no bits, only the extra-bit fields remain.
    w.write_bits(copy_len - cpy_base, u32::from(cpy_extra))
        .expect("copy extra");
    w.write_bits(0, 2).expect("distance extra"); // distance 9 = base 9 + 0
    w.flush();
    let stream = w.finish();

    let dict: Vec<u8> = (0..100u32).map(|i| b'a' + (i % 26) as u8).collect();
    let expected = (copy_len as usize <= available).then(|| {
        let mut out = vec![b'A'; insert_len as usize];
        out.extend_from_slice(&dict[dict.len() - copy_len as usize..]);
        assert_eq!(out.len(), (insert_len + copy_len) as usize);
        assert_eq!(distance, 9);
        out
    });
    (stream, dict, expected)
}
