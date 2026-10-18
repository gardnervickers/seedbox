//! Application progress budget shared by authoring, retained inputs and PV launch.

use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;

/// Translated application blocks between progress calls. Zero is invalid on every wire boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProgressBudget(NonZeroU32);

impl ProgressBudget {
    /// Small standalone Machine default; service projects use [`Default`].
    pub const MACHINE_DEFAULT: Self = Self(NonZeroU32::new(32).expect("nonzero budget"));

    pub const fn get(self) -> u32 {
        self.0.get()
    }

    /// Translator arguments are built here, never patched by positional indexes.
    pub fn translator_arguments(self) -> [String; 3] {
        [
            "--blocks-per-progress-call".into(),
            self.get().to_string(),
            "--".into(),
        ]
    }
}

impl Default for ProgressBudget {
    fn default() -> Self {
        Self(NonZeroU32::new(10_000).expect("nonzero budget"))
    }
}

impl From<NonZeroU32> for ProgressBudget {
    fn from(value: NonZeroU32) -> Self {
        Self(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_have_one_numeric_wire_format_and_checked_launch_arguments() {
        for value in [1, 32, 10_000, u32::MAX] {
            let budget: ProgressBudget = serde_json::from_value(value.into()).unwrap();
            assert_eq!(budget.get(), value);
            assert_eq!(serde_json::to_value(budget).unwrap(), value);
            assert_eq!(
                budget.translator_arguments(),
                [
                    "--blocks-per-progress-call".into(),
                    value.to_string(),
                    "--".into()
                ]
            );
        }
        assert!(serde_json::from_value::<ProgressBudget>(0.into()).is_err());
        assert_eq!(ProgressBudget::default().get(), 10_000);
        assert_eq!(ProgressBudget::MACHINE_DEFAULT.get(), 32);
    }
}
