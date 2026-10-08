package main

import (
	"bytes"
	"database/sql"
	"errors"
	"path/filepath"
	"strings"
	"testing"
	"unicode"
	"unicode/utf8"
)

func TestSearchFTS5Available(t *testing.T) {
	contractGuard(t)
	db, err := sql.Open("sqlite", ":memory:")
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	if _, err := db.Exec(`CREATE VIRTUAL TABLE search_probe USING fts5(body, tokenize = 'unicode61 remove_diacritics 2')`); err != nil {
		t.Fatalf("FTS5 unavailable: %v", err)
	}
}

func searchDocument(t *testing.T, db *sql.DB, task int64, kind, name, body string) document {
	t.Helper()
	var d document
	err := withTx(db, func(tx *sql.Tx) error {
		var err error
		d, _, err = storeDocument(tx, task, kind, name, bodyDocument([]byte(body), "search.md"), nil, 0)
		return err
	})
	if err != nil {
		t.Fatal(err)
	}
	return d
}

func searchHits(t *testing.T, h *harness, query string, flags ...string) []map[string]any {
	t.Helper()
	code, hits := h.run(nil, append([]string{"search", query}, flags...)...)
	if code != exitOK {
		t.Fatalf("search %q: exit %d, %v", query, code, hits)
	}
	return hits
}

func TestSearchSources(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	lane := h.newTask("lane", "implementer", root)
	db := h.openDB()
	d := searchDocument(t, db, lane, "report", "", "sharedtoken document")
	for _, kind := range []string{"decision", "ask", "answer", "note", "ready", "owner_answer"} {
		// Direct inserts prove the trigger covers paths outside insertEvent.
		docExec(t, db, `insert into events(task_id, kind, summary, created_at) values (?, ?, 'sharedtoken event', ?)`, lane, kind, now())
	}
	hits := searchHits(t, h, "sharedtoken")
	if len(hits) != 6 {
		t.Fatalf("hits = %v", hits)
	}
	seen := map[string]bool{}
	for _, hit := range hits {
		seen[hit["kind"].(string)] = true
		if num(hit, "root") != root || num(hit, "task") != lane {
			t.Fatalf("identity = %v", hit)
		}
		if hit["src"] == "doc" {
			if num(hit, "ref") != d.ID || hit["name"] != "" {
				t.Fatalf("document = %v", hit)
			}
		} else if _, ok := hit["name"]; ok {
			t.Fatalf("event has document name: %v", hit)
		}
	}
	for _, kind := range []string{"report", "decision", "ask", "answer", "owner_answer", "note"} {
		if !seen[kind] {
			t.Fatalf("missing %s", kind)
		}
	}
	if got := searchHits(t, h, "absenttoken"); len(got) != 0 {
		t.Fatal(got)
	}
	code, out, _ := h.runText(nil, "search", "absenttoken")
	if code != 0 || out != "" {
		t.Fatalf("no hits = %d %q", code, out)
	}
}

func TestSearchDocumentVersionAndPurge(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	path := docFile(t, h.dir, "goal.md", "oldtoken")
	first := h.ok(nil, "doc", "set", id(root), "goal", "--file", path)
	db := h.openDB()
	if docCount(t, db, `select count(*) from search_fts_data where instr(block, cast('oldtoken' as blob)) > 0`) == 0 {
		t.Fatal("old token not present before replacement")
	}
	docFile(t, h.dir, "goal.md", "newtoken")
	second := h.ok(nil, "doc", "set", id(root), "goal", "--file", path)
	if len(searchHits(t, h, "oldtoken")) != 0 {
		t.Fatal("old version remains indexed")
	}
	if n := docCount(t, db, `select count(*) from search_fts_data where instr(block, cast('oldtoken' as blob)) > 0`); n != 0 {
		t.Fatalf("replacement left old token in %d index blocks", n)
	}
	hits := searchHits(t, h, "newtoken")
	if len(hits) != 1 || num(hits[0], "ref") != num(second, "doc_id") {
		t.Fatal(hits)
	}
	err := withTx(db, func(tx *sql.Tx) error {
		_, _, err := storeDocument(tx, root, "goal", "", documentInput{Reason: "binary"}, nil, 0)
		return err
	})
	if err != nil || len(searchHits(t, h, "newtoken")) != 1 {
		t.Fatalf("miss changed index: %v", err)
	}
	h.ok(nil, "doc", "set", id(root), "goal", "--file", path)
	if docCount(t, db, `select count(*) from search_fts where src = 'doc'`) != 1 {
		t.Fatal("duplicate search rows")
	}
	h.ok(nil, "doc", "rm", id(num(first, "doc_id")), "--purge")
	if len(searchHits(t, h, "newtoken")) != 0 || docCount(t, db, `select count(*) from search_fts where src = 'doc'`) != 0 {
		t.Fatal("purge left a search row")
	}
	if n := docCount(t, db, `select count(*) from search_fts_data where instr(block, cast('newtoken' as blob)) > 0`); n != 0 {
		t.Fatalf("purge left token in %d index blocks", n)
	}
}

func TestSearchMigration(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	h.db = filepath.Join(h.dir, "previous.db")
	db, err := sql.Open("sqlite", h.db)
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	// schemaSQL is the previous schema; the search table is only in the migration.
	docExec(t, db, schemaSQL)
	docExec(t, db, `insert into tasks(id, parent_id, name, role, created_at, updated_at) values
		(1, null, 'root', 'orchestrator', '2026-01-01', '2026-01-01'),
		(2, 1, 'lane', 'implementer', '2026-01-01', '2026-01-01'),
		(3, 2, 'child', 'implementer', '2026-01-01', '2026-01-01')`)
	for i, body := range []string{"oldtoken", "migrationtoken latest", "migrationtoken named"} {
		in := bodyDocument([]byte(body), "search.md")
		docExec(t, db, `insert into doc_blobs values (?, ?, ?)`, in.Hash, *in.Bytes, in.Body)
		name, version := "", i+1
		if i == 2 {
			name, version = "named", 1
		}
		docExec(t, db, `insert into documents(root_id, task_id, kind, name, version, sha256, captured, created_at)
			values (1, 3, 'report', ?, ?, ?, 1, '2026-01-01')`, name, version, in.Hash)
	}
	docExec(t, db, `insert into documents(root_id, task_id, kind, name, version, captured, reason, created_at)
		values (1, 3, 'report', '', 3, 0, 'binary', '2026-01-02')`)
	for _, kind := range []string{"decision", "ask", "answer", "owner_answer", "note", "ready"} {
		docExec(t, db, `insert into events(task_id, kind, summary, created_at) values (3, ?, 'migrationtoken event', '2026-01-01')`, kind)
	}
	if docCount(t, db, `select count(*) from sqlite_master where name = 'search_fts'`) != 0 {
		t.Fatal("previous schema already has search")
	}
	if err := withTx(db, migrate); err != nil {
		t.Fatal(err)
	}
	if len(searchHits(t, h, "oldtoken")) != 0 {
		t.Fatal("migration indexed old version")
	}
	hits := searchHits(t, h, "migrationtoken")
	if len(hits) != 7 {
		t.Fatal(hits)
	}
	for _, hit := range hits {
		if num(hit, "root") != 1 || num(hit, "task") != 3 {
			t.Fatal(hit)
		}
	}
	if err := withTx(db, migrate); err != nil || docCount(t, db, `select count(*) from search_fts`) != 7 {
		t.Fatalf("migration not idempotent: %v", err)
	}
	docExec(t, db, `insert into events(task_id, kind, summary, created_at) values (3, 'note', 'migrationtoken after', '2026-01-03')`)
	if len(searchHits(t, h, "migrationtoken")) != 8 {
		t.Fatal("migration trigger not active")
	}
}

func TestSearchSecureDeleteExisting(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	db := h.openDB()
	docExec(t, db, `insert into search_fts(search_fts, rank) values('secure-delete', 0)`)
	db = h.openDB()
	if docCount(t, db, `select count(*) from search_fts_config where k = 'secure-delete' and v = 1`) != 1 {
		t.Fatal("existing table did not enable secure-delete on start")
	}
}

func TestSearchRepair(t *testing.T) {
	contractGuard(t)
	for _, missing := range []string{"table", "trigger", "old-trigger"} {
		t.Run(missing, func(t *testing.T) {
			h := newHarness(t)
			root := h.newTask("root", "orchestrator", 0)
			db := h.openDB()
			searchDocument(t, db, root, "goal", "", "repairtoken document")
			h.ok(nil, "note", "repairtoken old note", "--as", id(root))
			if missing == "old-trigger" {
				var oldSQL string
				if err := db.QueryRow(`select sql from sqlite_master where name = 'search_events_insert'`).Scan(&oldSQL); err != nil {
					t.Fatal(err)
				}
				// Reproduce the definition committed before owner answers were indexed.
				oldSQL = strings.ReplaceAll(oldSQL, ", 'owner_answer'", "")
				docExec(t, db, `drop trigger search_events_insert`)
				docExec(t, db, oldSQL)
			}
			docExec(t, db, `insert into events(task_id, kind, summary, created_at) values (?, 'owner_answer', 'repairtoken old owner answer', ?)`, root, now())
			switch missing {
			case "table":
				docExec(t, db, `drop table search_fts`)
			case "trigger":
				docExec(t, db, `drop trigger search_events_insert`)
			}
			h.ok(nil, "note", "repairtoken new note", "--as", id(root))
			if hits := searchHits(t, h, "repairtoken"); len(hits) != 4 {
				t.Fatalf("repair hits = %v", hits)
			}
			// Preserve distinctive rowids to detect an unnecessary second rebuild.
			docExec(t, db, `update search_fts set rowid = rowid + 1000`)
			if code, _ := h.run(nil, "status", "--tree", id(root)); code != exitOK {
				t.Fatal("selected CLI migration failed", code, h.lastStderr())
			} // selected CLI performs the second migration
			if docCount(t, db, `select count(*) from search_fts`) != 4 || docCount(t, db, `select count(*) from search_fts where rowid > 1000`) != 4 {
				var storedSQL string
				if err := db.QueryRow(`select sql from sqlite_master where name = 'search_events_insert'`).Scan(&storedSQL); err != nil {
					t.Fatal(err)
				}
				t.Fatalf("second start changed or duplicated rows; stored trigger = %q", storedSQL)
			}
			if len(searchHits(t, h, "repairtoken")) != 4 {
				t.Fatal("second start changed search results")
			}
		})
	}
}

func TestSearchNULQuery(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	for _, flags := range [][]string{nil, {"--raw"}} {
		h.one(exitUsage, nil, append([]string{"search", "a\x00b"}, flags...)...)
	}
	r := newTwoHost(t)
	for _, flags := range [][]string{nil, {"--raw"}} {
		_, rep, _ := r.post("host-a", rpcBody(r.dir, nil, "search-nul-01", append([]string{"search", "a\x00b"}, flags...)...))
		if rep.Exit != exitUsage {
			t.Fatalf("RPC NUL query = %+v", rep)
		}
	}
}

func TestSearchControlSnippet(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	docExec(t, h.openDB(), `insert into events(task_id, kind, summary, created_at) values (?, 'note', ?, ?)`, root, "controltoken\tESC\x1bBEL\aDEL\x7fend", now())
	hits := searchHits(t, h, "controltoken")
	if len(hits) != 1 {
		t.Fatal(hits)
	}
	snippet := hits[0]["snippet"].(string)
	if strings.IndexFunc(snippet, unicode.IsControl) >= 0 || snippet != "[controltoken] ESC BEL DEL end" {
		t.Fatalf("control snippet = %q", snippet)
	}
}

func TestSearchOwnerAnswer(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root, lane, launch := docLane(t, h)
	for _, owner := range []bool{false, true} {
		args := []string{"ask", "ownertoken question"}
		if owner {
			args = append(args, "--owner")
		}
		ask := h.ok(as(lane, launch), args...)
		h.ok(nil, "answer", id(num(ask, "ask_id")), "ownertoken answer", "--as", id(root))
	}
	// Historical owner notices remain searchable alongside current CLI answers.
	docExec(t, h.openDB(), `insert into events(task_id, kind, summary, created_at) values (?, 'owner_answer', 'ownertoken legacy answer', ?)`, lane, now())
	if len(searchHits(t, h, "ownertoken")) != 5 || len(searchHits(t, h, "ownertoken", "--kind", "ask")) != 2 {
		t.Fatal("plain/owner asks or answers missing")
	}
	hits := searchHits(t, h, "ownertoken", "--kind", "answer")
	if len(hits) != 3 {
		t.Fatal(hits)
	}
	seen := false
	for _, hit := range hits {
		seen = seen || hit["kind"] == "owner_answer"
	}
	if !seen {
		t.Fatal("owner answer kind lost")
	}
}

func TestSearchQueriesAndFilters(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	other := h.newTask("other", "orchestrator", 0)
	db := h.openDB()
	searchDocument(t, db, root, "plan", "one", "alphabet beta AND a-b a:b café")
	searchDocument(t, db, root, "plan", "two", "alphabet gamma")
	searchDocument(t, db, other, "goal", "", "alphabet beta")
	docExec(t, db, `insert into events(task_id, kind, summary, created_at) values (?, 'note', 'alphabet beta', ?)`, root, now())
	for _, query := range []string{`"`, "a-b", "a:b", "(", "*", "AND", "NEAR(", "", "  "} {
		t.Run(query, func(t *testing.T) { searchHits(t, h, query) })
	}
	for _, query := range []string{"(", `"`, "unknown:beta", "alpha OR"} {
		h.one(exitUsage, nil, "search", query, "--raw")
	}
	for _, test := range []struct {
		query string
		flags []string
		want  int
	}{
		{"alpha", nil, 0}, {"alpha*", nil, 4}, {"alpha* beta", nil, 3},
		{"alpha*", []string{"--root", id(root)}, 3},
		{"alpha*", []string{"--kind", "plan"}, 2},
		{"alpha*", []string{"--kind", "note"}, 1},
		{"alpha*", []string{"--root", id(root), "--kind", "plan", "--limit", "1"}, 1},
		{"beta OR gamma", []string{"--raw"}, 4}, {"cafe", nil, 1}, {"AND", nil, 1},
	} {
		if hits := searchHits(t, h, test.query, test.flags...); len(hits) != test.want {
			t.Fatalf("%q %v = %v, want %d", test.query, test.flags, hits, test.want)
		}
	}
	for _, flags := range [][]string{{"--limit", "0"}, {"--limit", "-1"}, {"--limit", "101"}, {"--root", "0"}, {"--root", "-1"}, {"--kind", "ready"}} {
		h.one(exitUsage, nil, append([]string{"search", "alpha*"}, flags...)...)
	}
}

func TestSearchLimitAndOrder(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	db := h.openDB()
	var newest int64
	for i := 0; i < 105; i++ {
		res, err := db.Exec(`insert into events(task_id, kind, summary, created_at) values (?, 'note', 'limittoken', ?)`, root, id(int64(i+1000)))
		if err != nil {
			t.Fatal(err)
		}
		newest, _ = res.LastInsertId()
	}
	hits := searchHits(t, h, "limittoken")
	if len(hits) != 20 || num(hits[0], "ref") != newest {
		t.Fatal(hits)
	}
	if len(searchHits(t, h, "limittoken", "--limit", "100")) != 100 {
		t.Fatal("upper bound not accepted")
	}
	for i, hit := range hits {
		if num(hit, "ref") != newest-int64(i) {
			t.Fatal("ties not newest first")
		}
	}
}

func TestSearchSnippet(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	db := h.openDB()
	searchDocument(t, db, root, "goal", "", "snippettoken\nnext\rline\u2028"+strings.Repeat("λ", 250)+"\nprivate tail")
	hit := searchHits(t, h, "snippettoken")[0]
	snippet := hit["snippet"].(string)
	if strings.ContainsAny(snippet, "\r\n\u2028\u2029") || utf8.RuneCountInString(snippet) > 200 || !utf8.ValidString(snippet) || !strings.Contains(snippet, "[snippettoken]") || strings.Contains(snippet, "private tail") {
		t.Fatal(snippet)
	}
	searchDocument(t, db, root, "plan", "", "snippettoken "+strings.Repeat("one ", 30))
	for _, hit := range searchHits(t, h, "snippettoken") {
		if len(strings.Fields(hit["snippet"].(string))) > 12 {
			t.Fatal("snippet exceeds 12 tokens", hit)
		}
	}
}

func TestSearchRPC(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	root := r.newTask("root", "orchestrator", 0)
	searchDocument(t, r.d.db, root, "goal", "", "rpctoken document")
	docExec(t, r.d.db, `insert into events(task_id, kind, summary, created_at) values (?, 'note', 'rpctoken event', ?)`, root, now())
	before := r.count(`select count(*) from requests`)
	for _, format := range [][]string{nil, {"--json"}} {
		args := append(append([]string{}, format...), "search", "rpctoken", "--root", id(root))
		var localOut, localErr bytes.Buffer
		code := contractRun(t, args, r.getenv(nil), &localOut, &localErr)
		local, stderr := localOut.String(), localErr.String()
		if code != 0 {
			t.Fatalf("local = %d %s", code, stderr)
		}
		for _, caller := range []string{"host-a", "host-b"} {
			r.caller.Store(caller)
			var out, errw bytes.Buffer
			code := contractCLIMain(t, args, clientEnv(r.homes[caller], nil), &out, &errw)
			if code != 0 || out.String() != local {
				t.Fatalf("RPC = %d %q %s, local %q", code, out.String(), errw.String(), local)
			}
		}
	}
	_, rep, _ := r.post("host-b", rpcBody(r.dir, nil, "search-raw-01", "--json", "search", "(", "--raw"))
	if rep.Exit != exitUsage || r.count(`select count(*) from requests`) != before {
		t.Fatalf("search read stored or raw failure = %+v", rep)
	}
}

func TestSearchTransactionRollback(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	db := h.openDB()
	old := searchDocument(t, db, root, "goal", "", "oldtoken")
	rollback := errors.New("rollback")
	err := withTx(db, func(tx *sql.Tx) error {
		if _, _, err := storeDocument(tx, root, "goal", "", bodyDocument([]byte("newtoken"), "search.md"), nil, 0); err != nil {
			return err
		}
		if _, err := tx.Exec(`insert into events(task_id, kind, summary, created_at) values (?, 'note', 'newtoken', ?)`, root, now()); err != nil {
			return err
		}
		return rollback
	})
	if err == nil || err.Error() != rollback.Error() || len(searchHits(t, h, "newtoken")) != 0 || len(searchHits(t, h, "oldtoken")) != 1 {
		t.Fatalf("capture rollback: %v", err)
	}
	docExec(t, db, `create trigger reject_purge before delete on documents begin select raise(abort, 'purge blocked'); end`)
	h.one(exitDB, nil, "doc", "rm", id(old.ID), "--purge")
	if len(searchHits(t, h, "oldtoken")) != 1 || docCount(t, db, `select count(*) from documents`) != 1 {
		t.Fatal("purge rollback lost document or index")
	}
}
