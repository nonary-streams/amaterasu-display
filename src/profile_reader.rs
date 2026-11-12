//! Bounded loading for catalog-owned HDR profile files.
use std::io::{self, Read};

#[derive(Debug)]
pub(crate) enum ProfileReadError {
    Io(io::Error),
    TooLarge,
    TooSmall,
}

pub(crate) fn read_bounded_profile(
    reader: impl Read,
    max_bytes: usize,
    min_bytes: usize,
) -> Result<Vec<u8>, ProfileReadError> {
    let read_limit = max_bytes.saturating_add(1) as u64;
    let mut bytes = Vec::new();
    reader
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(ProfileReadError::Io)?;
    if bytes.len() > max_bytes {
        return Err(ProfileReadError::TooLarge);
    }
    if bytes.len() < min_bytes {
        return Err(ProfileReadError::TooSmall);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::{ProfileReadError, read_bounded_profile};
    use std::io::Cursor;

    #[test]
    fn rejects_one_byte_beyond_the_limit_without_reading_the_rest() {
        let input = Cursor::new(vec![0_u8; 10_000]);
        assert!(matches!(
            read_bounded_profile(input, 8, 4),
            Err(ProfileReadError::TooLarge)
        ));
    }

    #[test]
    fn rejects_truncated_profile_headers() {
        let input = Cursor::new(vec![0_u8; 3]);
        assert!(matches!(
            read_bounded_profile(input, 8, 4),
            Err(ProfileReadError::TooSmall)
        ));
    }

    #[test]
    fn accepts_a_complete_profile_within_the_limit() {
        let input = Cursor::new(vec![7_u8; 8]);
        assert_eq!(read_bounded_profile(input, 8, 4).unwrap(), vec![7; 8]);
    }
    #[test]
    fn preserves_reader_failures_instead_of_reporting_a_complete_profile() {
        struct Denied;
        impl std::io::Read for Denied {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::PermissionDenied.into())
            }
        }
        match read_bounded_profile(Denied, 8, 4) {
            Err(ProfileReadError::Io(error)) => {
                assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied)
            }
            other => panic!("expected the original reader error: {other:?}"),
        }
    }
}
