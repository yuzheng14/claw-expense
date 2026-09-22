use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use crate::error::AppError;

/// An exact CNY amount, represented in integer minor units (fen).
///
/// Parsing accepts unsigned decimal input only. Signed amounts can be created
/// from minor units for derived values, such as a negative net expense.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub struct Money(i64);

impl Money {
    pub fn from_minor(value: i64) -> Self {
        Self(value)
    }

    pub fn minor(self) -> i64 {
        self.0
    }
}

impl FromStr for Money {
    type Err = AppError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let (whole, fraction) = match input.split_once('.') {
            Some((whole, fraction)) => (whole, Some(fraction)),
            None => (input, None),
        };

        if whole.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.is_some_and(|value| {
                !(1..=2).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return Err(AppError::invalid(
                "amount must be an unsigned decimal string with at most two decimal places",
            ));
        }

        let overflow = || AppError::invalid("amount exceeds the maximum of 92233720368547758.07");
        let whole = whole.bytes().try_fold(0_i64, |value, byte| {
            value
                .checked_mul(10)
                .and_then(|value| value.checked_add(i64::from(byte - b'0')))
                .ok_or_else(overflow)
        })?;

        // Both fractional bytes have already been validated above. A single
        // digit denotes tenths of a yuan, so it contributes ten fen.
        let fraction = fraction.map_or(0_i64, |value| {
            let bytes = value.as_bytes();
            i64::from(bytes[0] - b'0') * 10 + bytes.get(1).map_or(0, |byte| i64::from(*byte - b'0'))
        });
        let minor = whole
            .checked_mul(100)
            .and_then(|value| value.checked_add(fraction))
            .ok_or_else(overflow)?;

        Ok(Self(minor))
    }
}

impl fmt::Display for Money {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&format_minor(i128::from(self.0)))
    }
}

impl Serialize for Money {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Money {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let input = String::deserialize(deserializer)?;
        input.parse().map_err(D::Error::custom)
    }
}

/// Format integer minor units without converting to floating point.
///
/// `unsigned_abs` handles the full signed range, including `i128::MIN`, whose
/// positive magnitude cannot be represented by an `i128`.
pub fn format_minor(value: i128) -> String {
    let magnitude = value.unsigned_abs();
    let sign = if value < 0 { "-" } else { "" };
    format!("{sign}{}.{:02}", magnitude / 100, magnitude % 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_decimal_strings_exactly() {
        for (input, expected) in [
            ("0", 0),
            ("0.00", 0),
            ("0.01", 1),
            ("0.1", 10),
            ("12", 1200),
            ("12.3", 1230),
            ("12.30", 1230),
            ("00012.30", 1230),
            ("90071992547409.93", 9_007_199_254_740_993),
            ("92233720368547758.07", i64::MAX),
        ] {
            assert_eq!(input.parse::<Money>().unwrap().minor(), expected, "{input}");
        }
    }

    #[test]
    fn decimal_addition_is_exact_in_minor_units() {
        let first: Money = "0.10".parse().unwrap();
        let second: Money = "0.20".parse().unwrap();
        let result = Money::from_minor(first.minor().checked_add(second.minor()).unwrap());
        assert_eq!(result.minor(), 30);
        assert_eq!(result.to_string(), "0.30");
    }

    #[test]
    fn rejects_invalid_syntax_instead_of_rounding_or_coercing() {
        for input in [
            "",
            " ",
            " 1",
            "1 ",
            "1\n",
            "-1",
            "-0.00",
            "+1",
            ".1",
            "1.",
            "1.234",
            "1.000",
            "1.2.3",
            "1e2",
            "1E2",
            "NaN",
            "inf",
            "1,000",
            "1_000",
            "１２.３４",
            "١٢.٣٤",
            "1\0",
        ] {
            assert!(input.parse::<Money>().is_err(), "accepted {input:?}");
        }
    }

    #[test]
    fn rejects_out_of_range_amounts() {
        for input in [
            "92233720368547758.08",
            "92233720368547759",
            "9223372036854775807",
            "9223372036854775808",
            "999999999999999999999999999999999999999999999999999999",
        ] {
            assert!(input.parse::<Money>().is_err(), "accepted {input}");
        }
    }

    #[test]
    fn formats_signed_amounts_and_extreme_values() {
        for (value, expected) in [
            (0, "0.00"),
            (1, "0.01"),
            (-1, "-0.01"),
            (-10, "-0.10"),
            (-100, "-1.00"),
            (-200, "-2.00"),
            (i64::MAX, "92233720368547758.07"),
            (i64::MIN, "-92233720368547758.08"),
        ] {
            assert_eq!(Money::from_minor(value).to_string(), expected);
        }
        assert_eq!(
            format_minor(i128::MAX),
            "1701411834604692317316873037158841057.27"
        );
        assert_eq!(
            format_minor(i128::MIN),
            "-1701411834604692317316873037158841057.28"
        );
    }

    #[test]
    fn nonnegative_values_round_trip_through_decimal_strings() {
        for minor in (0..=10_000).chain([9_007_199_254_740_993, i64::MAX - 1, i64::MAX]) {
            let amount = Money::from_minor(minor);
            assert_eq!(amount.to_string().parse::<Money>().unwrap(), amount);
        }
    }

    #[test]
    fn json_preserves_precision_using_strings() {
        let amount = Money::from_minor(9_007_199_254_740_993);
        let json = serde_json::to_string(&amount).unwrap();
        assert_eq!(json, "\"90071992547409.93\"");
        assert_eq!(serde_json::from_str::<Money>(&json).unwrap(), amount);
        assert_eq!(
            serde_json::to_string(&Money::from_minor(-200)).unwrap(),
            "\"-2.00\""
        );
    }

    #[test]
    fn json_rejects_numbers_and_invalid_decimal_strings() {
        for json in [
            "12",
            "12.30",
            "0",
            "null",
            "true",
            "[]",
            "{}",
            "\"12.345\"",
            "\"-1.00\"",
            "\"+1\"",
            "\"1e2\"",
        ] {
            assert!(
                serde_json::from_str::<Money>(json).is_err(),
                "accepted {json}"
            );
        }
    }
}
