//! A register service with read, write and compare-and-set. The planted bug
//! releases the register's lock between comparing and setting, so two
//! concurrent compare-and-sets from the same value can both succeed. The final
//! value looks plausible; only the history of operations shows the violation.
pub const PORT: u16 = 7003;
