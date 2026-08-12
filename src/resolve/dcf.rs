//! Minimal Debian Control Format parsing for R metadata files.

use std::collections::BTreeMap;

/// One stanza from `PACKAGES` or `DESCRIPTION`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    fields: BTreeMap<String, String>,
}

impl Record {
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    #[must_use]
    pub fn fields(&self) -> &BTreeMap<String, String> {
        &self.fields
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid DCF at line {line}: {message}")]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

/// Parse all blank-line-delimited DCF stanzas.
pub fn parse(input: &str) -> std::result::Result<Vec<Record>, ParseError> {
    let mut records = Vec::new();
    let mut fields = BTreeMap::new();
    let mut current_key: Option<String> = None;

    for (index, raw_line) in input.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);

        if line.trim().is_empty() {
            finish_record(&mut records, &mut fields);
            current_key = None;
            continue;
        }

        if line.starts_with([' ', '\t']) {
            let key = current_key.as_ref().ok_or_else(|| ParseError {
                line: line_number,
                message: "continuation line has no preceding field".into(),
            })?;
            let value = fields.get_mut(key).expect("current DCF key must exist");
            value.push('\n');
            value.push_str(line.trim());
            continue;
        }

        let (key, value) = line.split_once(':').ok_or_else(|| ParseError {
            line: line_number,
            message: "expected `Key: value`".into(),
        })?;
        if key.is_empty() {
            return Err(ParseError {
                line: line_number,
                message: "field name cannot be empty".into(),
            });
        }
        if key
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err(ParseError {
                line: line_number,
                message: format!("invalid field name `{key}`"),
            });
        }
        if fields.contains_key(key) {
            return Err(ParseError {
                line: line_number,
                message: format!("duplicate field `{key}` in stanza"),
            });
        }

        fields.insert(key.to_owned(), value.trim().to_owned());
        current_key = Some(key.to_owned());
    }

    finish_record(&mut records, &mut fields);
    Ok(records)
}

/// Parse a `DESCRIPTION`, which must contain exactly one stanza.
pub fn parse_one(input: &str) -> std::result::Result<Record, ParseError> {
    let mut records = parse(input)?;
    if records.len() != 1 {
        return Err(ParseError {
            line: 1,
            message: format!("expected exactly one stanza, found {}", records.len()),
        });
    }
    Ok(records.remove(0))
}

fn finish_record(records: &mut Vec<Record>, fields: &mut BTreeMap<String, String>) {
    if !fields.is_empty() {
        records.push(Record {
            fields: std::mem::take(fields),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, parse_one};

    #[test]
    fn parses_description_fields_and_folded_values() {
        let input = "Package: tinyone\nVersion: 1.2.3\nDescription: First line.\n  Second line,\n\tthird line.\nAuthors@R: person('A', 'B')\n";
        let record = parse_one(input).unwrap();
        assert_eq!(record.get("Package"), Some("tinyone"));
        assert_eq!(record.get("Version"), Some("1.2.3"));
        assert_eq!(
            record.get("Description"),
            Some("First line.\nSecond line,\nthird line.")
        );
        assert_eq!(record.get("Authors@R"), Some("person('A', 'B')"));
    }

    #[test]
    fn parses_multiple_packages_stanzas_and_crlf() {
        let input = "Package: tinyone\r\nVersion: 1.0.0\r\nImports: stats,\r\n tools\r\n\r\nPackage: tinytwo\r\nVersion: 2.0.0\r\n\r\n";
        let records = parse(input).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].get("Imports"), Some("stats,\ntools"));
        assert_eq!(records[1].get("Package"), Some("tinytwo"));
    }

    #[test]
    fn ignores_repeated_blank_lines_and_empty_input() {
        assert!(parse("").unwrap().is_empty());
        assert!(parse("\n \n\t\n").unwrap().is_empty());
        assert_eq!(parse("\nPackage: x\n\n\n").unwrap().len(), 1);
    }

    #[test]
    fn exposes_fields_in_sorted_order() {
        let record = parse_one("Version: 1\nPackage: pkg\nLicense: MIT\n").unwrap();
        let keys: Vec<_> = record.fields().keys().map(String::as_str).collect();
        assert_eq!(keys, ["License", "Package", "Version"]);
    }

    #[test]
    fn rejects_malformed_lines_with_line_numbers() {
        let cases = [
            (" continuation", 1, "continuation"),
            ("Package: x\nnot a field", 2, "Key: value"),
            (": value", 1, "cannot be empty"),
            ("Bad Key: value", 1, "invalid field name"),
            ("Package: x\nPackage: y", 2, "duplicate field"),
        ];

        for (input, line, message) in cases {
            let error = parse(input).unwrap_err();
            assert_eq!(error.line, line, "{input}");
            assert!(error.message.contains(message), "{input}: {error}");
        }
    }

    #[test]
    fn description_parser_requires_one_stanza() {
        assert!(parse_one("").is_err());
        assert!(parse_one("Package: a\n\nPackage: b\n").is_err());
    }
}
