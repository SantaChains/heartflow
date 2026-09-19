use rusqlite::{params, Connection, Row};

use crate::error::StoreError;
use crate::model::{role_from_str, SearchHit, SearchMethod};

/// Minimum number of code points for the FTS5 `trigram` path. The trigram
/// tokenizer indexes 3-character sequences, so shorter terms (including common
/// 1-2 character Chinese words) cannot match and fall back to `LIKE`.
pub const TRIGRAM_MIN: usize = 3;

/// SQL string literal wrapping for a trigram substring search.
///
/// FTS5 treats a double-quoted string as a literal phrase; embedded double
/// quotes are escaped by doubling them, so arbitrary user text (quotes, `*`,
/// `(`, `:`) is matched verbatim instead of being parsed as query syntax.
#[must_use]
pub fn fts_match_phrase(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 2);
    out.push('"');
    for ch in query.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Escape `query` for use inside a `LIKE ... ESCAPE '\'` pattern.
#[must_use]
pub fn like_escape(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 2);
    for ch in query.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            other => out.push(other),
        }
    }
    out
}

#[must_use]
pub fn choose_method(query: &str) -> SearchMethod {
    if query.chars().count() >= TRIGRAM_MIN {
        SearchMethod::Fts
    } else {
        SearchMethod::Like
    }
}

const FTS_SQL: &str = "SELECT m.id, s.session_id, m.seq, m.role, substr(m.search_text, 1, 300)
   FROM messages_fts f
   JOIN messages m ON m.id = f.rowid
   JOIN sessions s ON s.id = m.session_row
  WHERE messages_fts MATCH ?1
    AND (?2 IS NULL OR s.session_id = ?2)
  ORDER BY s.updated_at DESC, m.session_row, m.seq
  LIMIT ?3";

const LIKE_SQL: &str = "SELECT m.id, s.session_id, m.seq, m.role, substr(m.search_text, 1, 300)
   FROM messages m
   JOIN sessions s ON s.id = m.session_row
  WHERE m.search_text LIKE ?1 ESCAPE '\\'
    AND (?2 IS NULL OR s.session_id = ?2)
  ORDER BY s.updated_at DESC, m.session_row, m.seq
  LIMIT ?3";

/// Project `(session_id, seq, role, snippet)` from a result row. A named fn so
/// both query arms share one `MappedRows` closure type.
fn map_row(row: &Row<'_>) -> rusqlite::Result<(String, i64, String, Option<String>)> {
    Ok((row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
}

/// Search every stored message for `query`.
///
/// Dispatches to the FTS5 trigram index for queries of `TRIGRAM_MIN` or more
/// code points and to a `LIKE` substring scan otherwise, so short Chinese terms
/// still match. `session_id` optionally scopes the search. Results are ordered
/// by session recency then message order, capped at `limit`.
pub fn search(
    conn: &Connection,
    query: &str,
    session_id: Option<&str>,
    limit: i64,
) -> Result<Vec<SearchHit>, StoreError> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let method = choose_method(query);

    // Bind either a quoted FTS phrase (>= TRIGRAM_MIN code points) or an
    // escaped LIKE pattern; the two paths differ only in SQL and bound term.
    let (sql, term): (&str, String) = match method {
        SearchMethod::Fts => (FTS_SQL, fts_match_phrase(query)),
        SearchMethod::Like => (LIKE_SQL, format!("%{}%", like_escape(query))),
    };

    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![term, session_id, limit], map_row)?;

    let mut hits = Vec::new();
    for row in rows {
        let (session_id, seq, role_raw, snippet) = row?;
        hits.push(SearchHit {
            session_id,
            seq,
            role: role_from_str(&role_raw)?,
            snippet: snippet.unwrap_or_default(),
            method,
        });
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::{choose_method, fts_match_phrase, like_escape, SearchMethod, TRIGRAM_MIN};

    #[test]
    fn dispatches_by_code_point_length() {
        assert_eq!(choose_method("笔记"), SearchMethod::Like, "2 chars => LIKE");
        assert_eq!(choose_method("笔记本"), SearchMethod::Fts, "3 chars => FTS");
        assert_eq!(TRIGRAM_MIN, 3);
    }

    #[test]
    fn fts_phrase_wraps_and_doubles_quotes() {
        assert_eq!(fts_match_phrase("abc"), "\"abc\"");
        assert_eq!(fts_match_phrase("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn like_escape_backslashes_wildcards() {
        assert_eq!(like_escape("100%"), "100\\%");
        assert_eq!(like_escape("a_b\\c"), "a\\_b\\\\c");
        assert_eq!(like_escape("plain"), "plain");
    }
}
