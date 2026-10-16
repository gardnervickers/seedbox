//! Host-only canonical framing vectors shared with production QEMU C tests.
//! These do not execute or prove architectural delivery.
use seedbox_machine::machine::execution_evidence::chain;

#[test]
fn x86_special_delivery_and_reset_rows_match_c_goldens() {
    let initial = chain::initial();
    for (kind, a, b, expected) in [
        (
            b'J',
            1,
            0,
            "fe97db2f78e759c8620b6c5a1387b55496ec625469582822e40ea8d3c0b2bf31",
        ),
        (
            b'J',
            2,
            0,
            "fde5621fa9c29a6f59401df00f07b99f01f13519644ead788571df2936df6a14",
        ),
        (
            b'J',
            3,
            0x7a,
            "a1e2123708f463f39b59e1b6e1e833faba4258bccd3388581c67f94bcfd19908",
        ),
        (
            b'G',
            1,
            0,
            "db3d671534c4512ab6266a14732869d7b9d352251e8289255e795efc7b33731d",
        ),
        (
            b'G',
            2,
            0,
            "9e02f171353fca073065a378c49ef1704a1926e1cee77d481d938dbdeeb03095",
        ),
    ] {
        let h = chain::record(&initial, kind, 0, 123, a, b);
        let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, expected);
        assert_ne!(h, chain::record(&initial, kind, 1, 123, a, b));
        assert_ne!(h, chain::record(&initial, kind, 0, 124, a, b));
        assert_ne!(h, chain::record(&initial, kind, 0, 123, a + 1, b));
        assert_ne!(h, chain::record(&initial, kind, 0, 123, a, b + 1));
    }
}
