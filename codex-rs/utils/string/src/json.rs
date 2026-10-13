//! JSON serialization helpers for ASCII-only transports and bounded output.

use std::io;

use serde::Serialize;

struct AsciiJsonFormatter;

impl serde_json::ser::Formatter for AsciiJsonFormatter {
    // serde_json has no ensure_ascii flag; this formatter keeps its serializer
    // in charge and only escapes non-ASCII string fragments.
    fn write_string_fragment<W>(&mut self, writer: &mut W, fragment: &str) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        let mut start = 0;
        for (index, ch) in fragment.char_indices() {
            if ch.is_ascii() {
                continue;
            }

            if start < index {
                writer.write_all(&fragment.as_bytes()[start..index])?;
            }

            let mut utf16 = [0; 2];
            for code_unit in ch.encode_utf16(&mut utf16) {
                write!(writer, "\\u{code_unit:04x}")?;
            }
            start = index + ch.len_utf8();
        }

        if start < fragment.len() {
            writer.write_all(&fragment.as_bytes()[start..])?;
        }

        Ok(())
    }
}

/// Serialize JSON while escaping non-ASCII string content as `\uXXXX`.
///
/// This is useful when JSON needs to remain parseable as JSON but must be
/// carried through ASCII-safe transports such as HTTP headers.
pub fn to_ascii_json_string<T>(value: &T) -> serde_json::Result<String>
where
    T: Serialize + ?Sized,
{
    let mut bytes = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut bytes, AsciiJsonFormatter);
    value.serialize(&mut serializer)?;
    String::from_utf8(bytes)
        .map_err(|err| serde_json::Error::io(io::Error::new(io::ErrorKind::InvalidData, err)))
}

/// Serialize UTF-8 JSON while retaining at most `max_bytes` of output.
pub fn to_json_string_bounded<T>(value: &T, max_bytes: usize) -> serde_json::Result<String>
where
    T: Serialize + ?Sized,
{
    struct BoundedBuffer {
        bytes: Vec<u8>,
        max_bytes: usize,
    }

    impl io::Write for BoundedBuffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.max_bytes.saturating_sub(self.bytes.len()) {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "JSON byte limit exceeded",
                ));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut buffer = BoundedBuffer {
        bytes: Vec::new(),
        max_bytes,
    };
    let mut serializer = serde_json::Serializer::new(&mut buffer);
    value.serialize(&mut serializer)?;
    String::from_utf8(buffer.bytes)
        .map_err(|err| serde_json::Error::io(io::Error::new(io::ErrorKind::InvalidData, err)))
}
