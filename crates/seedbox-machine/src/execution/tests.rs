use super::*;

#[test]
fn rejects_speedups_and_out_of_range_slowdowns_without_rounding() {
    for factor in [0, 1, 999, 1_000_001, u32::MAX] {
        let error = ExecutionConfig::new(factor).unwrap_err();
        assert_eq!(error.factor_permille, factor);
        assert!(error.to_string().contains(&factor.to_string()));
    }
    // The endpoints and a sub-percent slowdown retain their exact factors.
    for factor in [1000, 1001, 1_000_000] {
        assert_eq!(
            ExecutionConfig::new(factor).unwrap().factor_permille(),
            factor
        );
    }
}
