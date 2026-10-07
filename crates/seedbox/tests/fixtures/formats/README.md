# Frozen writer fixtures

These bytes are reader compatibility controls, not inputs to execute. Do not
regenerate them as part of a test or silently normalize paths, ordering or data.
A deliberate replacement needs its own producing-source receipt and review.

`demo41-fail-v2.json` is the original **actual guest** manifest from completed
[#41](https://github.com/gardnervickers/seedbox/issues/41), attempt
`d6b26c36e7c2ef38-Ek1JmJ`: broken server, seed1009, 96 acknowledged values lost.
Its producer was Seedbox0.1.0 debug CLI at reviewed source
`3b9102747cb0413853e35f134bf0a1a81128f828`, binary SHA-256
`7295475a38f4f025171fca9d687b9e76308f1f5501b78d9e33342f2607aa870d`.
Engine168ad and the exact remaining inputs are identified inside the original
manifest. [Independent audit](https://github.com/gardnervickers/seedbox/issues/41#issuecomment-6030374337)
accepted the actual execution/replay evidence. This small fixture contains no
input objects and cannot independently replay that guest run.

`synthetic-c84-v2.json` and `synthetic-c84-v1.seedbox.tar` were emitted once by
the **unmodified production writer** at source
`c84b57022a002ec717f26cb5f8f6c7b6f5c9e940`, Seedbox0.1.0, through the existing
`tests/bundle.rs` test-only backend. Its image/kernel/engine objects are literal
synthetic bytes, workload exit7, and `opaque.bin` is `[0,255,13,10,128,0,42]`.
No guest or builder was involved. The temporary capture harness called
`Fixture::new`, configured that exit/log, called `run`, `bundle::export` and
copied the original manifest/archive; it is absent from runtime and final tests.
The same source reader/serializer produced the two `*-inspect*.json` expected
CLI projections, including the historical guest manifest projection. These are
not captures of an older CLI's inspect stdout. Generation used the project Nix
shell and `cargo test -p seedbox --test bundle --locked -j 2
capture_c84_contract_fixture_once -- --exact` on2026-10-07. The fixture retains
temporary source/attempt paths as written; those paths need not exist to inspect
or transfer it. Writer source blobs: orchestration`b0116a9e348640113d59ef538692c805009b566e`,
bundle`98d4cdd9d49ee427c301ae3c3c1fdb67e24b01ac`.

| Original bytes | SHA-256 |
| --- | --- |
| demo41-fail-v2.json | 087758b7d9e4df25499dd26d19277ee2391a87c05c343bd8d218e4cbd61b9c38 |
| demo41-inspect-c84.json | 4fef67cc430d39b63b3c3b62810dc96fff3749cf2a5fb27b5182a6172c435724 |
| synthetic-c84-v2.json | 0a9e9ceabc534f0bacf485583f3b245984356dbc2fcc28cc856d8f1798230d70 |
| synthetic-c84-v1.seedbox.tar | 25bfef873411690df3d53c5f27e766828792d17f7053449022138a03d4ba112d |
| synthetic-c84-inspect.json | e364855b74fe12212ff90a0097fda565ba1d90ffe5e8257bad5153ac3072fb34 |

Tests cover exact current CLI projection of both frozen records, valid
rechecksummed unsupported versions, unknown fixed metadata fields, and retained
manifest/opaque-byte preservation through import/export/import. They provide
source/format evidence only. Future V0 release-reader commitments remain undecided.
