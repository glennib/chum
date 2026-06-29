//! Splitting a multi-statement migration file into individual statements.
//!
//! ClickHouse's HTTP interface (and the native protocol) execute exactly one
//! statement per request, so a migration file holding several `CREATE`s must
//! be split client-side. Naively splitting on `;` is wrong — semicolons appear
//! inside string literals and comments.
//!
//! We use [`sqlparser`]'s *tokenizer* (not its parser). The parser's ClickHouse
//! dialect cannot model real ClickHouse DDL (`CREATE DICTIONARY`, the
//! `ENGINE … ORDER BY` tail, `AggregateFunction`, the `JSON` type, …), so a
//! full parse fails. But we don't need a validated AST — only statement
//! *boundaries*. The tokenizer gives those robustly: it classifies `;` inside
//! strings and comments as part of those tokens, never as a separator. We then
//! slice the *original* source at the byte offsets of top-level semicolons, so
//! each statement is preserved verbatim (comments, formatting, and all).

use sqlparser::dialect::ClickHouseDialect;
use sqlparser::tokenizer::Token;
use sqlparser::tokenizer::Tokenizer;

/// Split a migration's SQL into individual statements.
///
/// Returns the statements with surrounding whitespace and trailing `;`
/// trimmed. Comment-only and empty chunks are dropped.
///
/// # Errors
///
/// Returns the tokenizer's error message if the SQL cannot be lexed.
pub fn split_statements(sql: &str) -> Result<Vec<String>, String> {
    let tokens = Tokenizer::new(&ClickHouseDialect {}, sql)
        .tokenize_with_location()
        .map_err(|e| e.to_string())?;

    let line_starts = line_start_offsets(sql);

    // Byte offsets that bound each statement: start of file, just after every
    // top-level semicolon, and end of file.
    let mut cuts = vec![0usize];
    for tws in &tokens {
        if matches!(tws.token, Token::SemiColon) {
            let off = loc_to_byte(
                &line_starts,
                sql,
                tws.span.start.line,
                tws.span.start.column,
            );
            cuts.push(off + 1);
        }
    }
    cuts.push(sql.len());

    let mut statements = Vec::new();
    for window in cuts.windows(2) {
        let chunk = sql[window[0]..window[1]].trim().trim_end_matches(';');
        let stmt = strip_leading_noise(chunk);
        if !stmt.is_empty() {
            statements.push(stmt.to_owned());
        }
    }
    Ok(statements)
}

/// Drop leading blank and single-line-comment lines so the statement starts at
/// its first SQL keyword. Internal and inline comments are preserved.
fn strip_leading_noise(chunk: &str) -> &str {
    let mut offset = 0;
    for line in chunk.split_inclusive('\n') {
        let t = line.trim();
        if t.is_empty() || t.starts_with("--") {
            offset += line.len();
        } else {
            break;
        }
    }
    chunk[offset..].trim()
}

/// Byte offset of the start of each line (index 0 == line 1).
fn line_start_offsets(src: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in src.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// Map sqlparser's 1-based (line, column) [`Location`] to a byte offset.
///
/// sqlparser advances `column` once per *character*, so for lines containing
/// multi-byte characters (e.g. the box-drawing glyphs in our migration
/// comments) we must walk char boundaries rather than assume 1 column == 1
/// byte.
///
/// [`Location`]: sqlparser::tokenizer::Location
fn loc_to_byte(line_starts: &[usize], src: &str, line: u64, column: u64) -> usize {
    let (Ok(line_idx), Ok(char_offset)) = (usize::try_from(line - 1), usize::try_from(column - 1))
    else {
        return src.len();
    };
    let Some(&line_start) = line_starts.get(line_idx) else {
        return src.len();
    };
    src[line_start..]
        .char_indices()
        .nth(char_offset)
        .map_or(src.len(), |(i, _)| line_start + i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_statement() {
        let stmts = split_statements("CREATE TABLE t (a UInt8) ENGINE = Memory").unwrap();
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].starts_with("CREATE TABLE t"));
    }

    #[test]
    fn trailing_semicolon_is_trimmed() {
        let stmts = split_statements("CREATE TABLE t (a UInt8) ENGINE = Memory;").unwrap();
        assert_eq!(stmts.len(), 1);
        assert!(!stmts[0].ends_with(';'));
    }

    #[test]
    fn multiple_statements() {
        let sql =
            "CREATE TABLE a (x UInt8) ENGINE = Memory;\nCREATE TABLE b (y UInt8) ENGINE = Memory;";
        let stmts = split_statements(sql).unwrap();
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("TABLE a"));
        assert!(stmts[1].contains("TABLE b"));
    }

    #[test]
    fn semicolon_inside_string_is_not_a_separator() {
        let sql = "INSERT INTO t VALUES ('a;b'); CREATE TABLE u (z UInt8) ENGINE = Memory";
        let stmts = split_statements(sql).unwrap();
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("'a;b'"));
    }

    #[test]
    fn comment_only_chunks_are_dropped() {
        let sql = "-- a leading comment\nCREATE TABLE t (a UInt8) ENGINE = Memory;\n-- a trailing \
                   comment\n";
        let stmts = split_statements(sql).unwrap();
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].starts_with("CREATE TABLE t"));
    }

    #[test]
    fn semicolon_inside_block_comment_is_not_a_separator() {
        let sql = "CREATE TABLE t /* drop ; me */ (a UInt8) ENGINE = Memory;\nCREATE TABLE u (z \
                   UInt8) ENGINE = Memory";
        let stmts = split_statements(sql).unwrap();
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("/* drop ; me */"));
        assert!(stmts[1].contains("TABLE u"));
    }

    #[test]
    fn semicolon_inside_quoted_identifier_is_not_a_separator() {
        let sql = "CREATE TABLE `weird;name` (a UInt8) ENGINE = Memory;\nCREATE TABLE u (z UInt8) \
                   ENGINE = Memory";
        let stmts = split_statements(sql).unwrap();
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("weird;name"));
        assert!(stmts[1].contains("TABLE u"));
    }

    #[test]
    fn inline_comment_within_statement_is_preserved() {
        let sql = "CREATE TABLE t\n(\n    a UInt8 -- the a column\n)\nENGINE = Memory";
        let stmts = split_statements(sql).unwrap();
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].contains("-- the a column"));
    }

    #[test]
    fn multibyte_comment_offsets_are_correct() {
        // Box-drawing glyphs make columns != bytes; the slice must still land
        // on a char boundary and capture the whole statement.
        let sql = "-- ── header ──\nCREATE TABLE café (x UInt8) ENGINE = Memory;\nCREATE TABLE b \
                   (y UInt8) ENGINE = Memory";
        let stmts = split_statements(sql).unwrap();
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("café"));
        assert!(stmts[1].contains("TABLE b"));
    }
}
