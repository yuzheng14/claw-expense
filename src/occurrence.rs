//! Optional user-supplied occurrence times, preserving local date and precision.
//! No inferred timezone, fabricated midnight, or conversion to UTC is performed.
use crate::{
    error::{AppError, Result},
    store::validate_date,
};

fn invalid() -> AppError {
    AppError::invalid(
        "occurred-at 必须为带明确时区的 ISO 8601 时间，如 2026-09-30T23:30+08:00 或 2026-09-30T23:30:12.123Z；不接受未知时区 -00:00",
    )
}

/// Normalize explicit UTC offsets to Z, without changing minute/second precision,
/// fractional precision, the represented local date, or nonzero offsets.
pub fn normalize_occurred_at(value: &str) -> Result<String> {
    if !value.is_ascii() || !(17..=35).contains(&value.len()) {
        return Err(invalid());
    }
    let (local, offset) = if let Some(local) = value.strip_suffix('Z') {
        (local, "Z")
    } else {
        let (local, offset) = value.split_at(value.len() - 6);
        let bytes = offset.as_bytes();
        if !matches!(bytes[0], b'+' | b'-')
            || bytes[3] != b':'
            || ![bytes[1], bytes[2], bytes[4], bytes[5]]
                .iter()
                .all(u8::is_ascii_digit)
            || number(&bytes[1..3]) > 23
            || number(&bytes[4..6]) > 59
            || offset == "-00:00"
        {
            return Err(invalid());
        }
        (local, offset)
    };
    let bytes = local.as_bytes();
    if local.len() < 16
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || ![bytes[11], bytes[12], bytes[14], bytes[15]]
            .iter()
            .all(u8::is_ascii_digit)
        || number(&bytes[11..13]) > 23
        || number(&bytes[14..16]) > 59
    {
        return Err(invalid());
    }
    validate_date(&local[..10])?;
    if local.len() != 16 {
        if local.len() < 19
            || bytes[16] != b':'
            || !bytes[17..19].iter().all(u8::is_ascii_digit)
            || number(&bytes[17..19]) > 59
        {
            return Err(invalid());
        }
        if local.len() != 19
            && (bytes[19] != b'.'
                || !(1..=9).contains(&(local.len() - 20))
                || !bytes[20..].iter().all(u8::is_ascii_digit))
        {
            return Err(invalid());
        }
    }
    Ok(format!(
        "{local}{}",
        if offset == "+00:00" { "Z" } else { offset }
    ))
}

fn number(bytes: &[u8]) -> u8 {
    (bytes[0] - b'0') * 10 + bytes[1] - b'0'
}

/// Use the date at the supplied offset, never the date after UTC conversion.
pub fn date_from_occurred_at(value: &str) -> Result<String> {
    Ok(normalize_occurred_at(value)?[..10].to_owned())
}

pub fn validate_occurrence(date: &str, occurred_at: Option<&str>) -> Result<Option<String>> {
    validate_date(date)?;
    let normalized = occurred_at.map(normalize_occurred_at).transpose()?;
    if normalized
        .as_ref()
        .is_some_and(|value| &value[..10] != date)
    {
        return Err(AppError::invalid(
            "date 必须与 occurred-at 自带时区中的本地日期一致",
        ));
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_precision_and_offset_local_date() {
        for value in [
            "2026-09-30T23:30-07:00",
            "2026-09-30T23:30:12-07:00",
            "2026-09-30T23:30:12.1-07:00",
            "2026-09-30T23:30:12.123456789-07:00",
            "2026-09-30T23:30Z",
            "2026-09-30T23:30:00.000Z",
            "0001-01-01T00:00+08:00",
            "9999-12-31T23:59:59-12:00",
            "2024-02-29T12:34+05:30",
        ] {
            assert_eq!(normalize_occurred_at(value).unwrap(), value);
            assert_eq!(date_from_occurred_at(value).unwrap(), &value[..10]);
        }
        assert_eq!(
            normalize_occurred_at("2026-09-30T23:30+00:00").unwrap(),
            "2026-09-30T23:30Z"
        );
        assert_eq!(
            normalize_occurred_at("2026-09-30T23:30:12.123+00:00").unwrap(),
            "2026-09-30T23:30:12.123Z"
        );
        assert!(validate_occurrence("2026-10-01", Some("2026-09-30T23:30:12-07:00")).is_err());
        assert_eq!(validate_occurrence("2026-09-30", None).unwrap(), None);
    }

    #[test]
    fn rejects_ambiguous_invalid_or_overprecise_times() {
        for value in [
            "",
            "2026-09-30",
            "2026-09-30T23:30",
            "2026-09-30T23:30:12",
            "2026-09-30T23:30-00:00",
            "2026-09-30T23:30:12-00:00",
            "2026-09-30T24:00Z",
            "2026-09-30T23:60Z",
            "2026-09-30T23:59:60Z",
            "2026-09-30T23:30+24:00",
            "2026-09-30T23:30+08:60",
            "2026-09-30T23:30+0800",
            "2026-09-30T23:30:12.1234567890Z",
            "2026-09-30T23:30:12.Z",
            "2026-09-30T23:30.1Z",
            "2026-09-31T23:30Z",
            "2026-02-29T12:30Z",
            "0000-01-01T12:30Z",
            "10000-01-01T12:30Z",
            "2026-09-30 23:30Z",
            "2026-09-30t23:30Z",
            "2026-09-30T23:30z",
            "2026-09-30T23:30Z ",
            "２０２６-09-30T23:30Z",
            "2026-09-30T1:30Z",
            "2026-09-30T23:30:1Z",
            "2026-09-30T23:30:12..1Z",
            "2026-09-30T23:30:12.1aZ",
            "2026-09-30T23:30+0a:00",
        ] {
            assert!(normalize_occurred_at(value).is_err(), "accepted {value}");
        }
    }
}
