package main

import (
	"flag"
	"strings"
	"unicode"
)

func cmdSearch(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("search", flag.ContinueOnError)
	root := fs.Int64("root", 0, "campaign root id")
	kind := fs.String("kind", "", "document kind or decision, ask, answer, note")
	limit := fs.Int("limit", 20, "maximum hits, 1 to 100 (default 20)")
	raw := fs.Bool("raw", false, "use FTS5 query syntax")
	pos, err := parseArgs(c, fs, args, 1, 1)
	if err != nil {
		return nil, 0, err
	}
	if *root < 0 || flagWasSet(fs, "root") && *root == 0 {
		return nil, 0, usageErr("--root must be a positive campaign id")
	}
	if *limit < 1 || *limit > 100 {
		return nil, 0, usageErr("--limit must be between 1 and 100")
	}
	if *kind != "" && !documentKinds[*kind] && *kind != "decision" && *kind != "ask" && *kind != "answer" && *kind != "note" {
		return nil, 0, usageErr("unknown search kind %q", *kind)
	}
	query := pos[0]
	if strings.ContainsRune(query, '\x00') {
		return nil, 0, usageErr("search query must not contain a NUL byte")
	}
	if !*raw {
		terms := strings.Fields(query)
		for i, term := range terms {
			prefix := strings.HasSuffix(term, "*")
			if prefix {
				term = strings.TrimSuffix(term, "*")
			}
			terms[i] = `"` + strings.ReplaceAll(term, `"`, `""`) + `"`
			if prefix {
				terms[i] += "*"
			}
		}
		query = strings.Join(terms, " AND ")
		if query == "" {
			c.lines = true
			return nil, exitOK, nil
		}
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	where, qargs := `search_fts match ?`, []any{query}
	if *root != 0 {
		where += ` and root_id = ?`
		qargs = append(qargs, *root)
	}
	if *kind == "answer" {
		where += ` and kind in ('answer', 'owner_answer')`
	} else if *kind != "" {
		where += ` and kind = ?`
		qargs = append(qargs, *kind)
	}
	qargs = append(qargs, *limit)
	rows, err := db.Query(`select src, ref, root_id, task_id, kind, name, at,
		snippet(search_fts, 0, '[', ']', '...', 12) from search_fts where `+where+`
		order by bm25(search_fts), at desc, ref desc limit ?`, qargs...)
	queryError := func(err error) error {
		if *raw && (strings.Contains(err.Error(), "fts5:") || strings.Contains(err.Error(), "unterminated string") || strings.Contains(err.Error(), "no such column:")) {
			return usageErr("invalid FTS5 query: %v", err)
		}
		return dbErr(err)
	}
	if err != nil {
		return nil, 0, queryError(err)
	}
	defer rows.Close()
	var hits []map[string]any
	for rows.Next() {
		var src, kind, name, at, snippet string
		var ref, root, task int64
		if err := rows.Scan(&src, &ref, &root, &task, &kind, &name, &at, &snippet); err != nil {
			return nil, 0, dbErr(err)
		}
		snippet = strings.Map(func(r rune) rune {
			if unicode.IsControl(r) || r == '\u2028' || r == '\u2029' {
				return ' '
			}
			return r
		}, snippet)
		runes := []rune(snippet)
		if len(runes) > 200 {
			snippet = string(runes[:200])
		}
		hit := map[string]any{"src": src, "ref": ref, "root": root, "task": task, "kind": kind, "at": at, "snippet": snippet}
		if src == "doc" {
			hit["name"] = name
		}
		hits = append(hits, hit)
	}
	if err := rows.Err(); err != nil {
		return nil, 0, queryError(err)
	}
	c.lines = true
	for _, hit := range hits {
		c.emit(hit)
	}
	return nil, exitOK, nil
}
