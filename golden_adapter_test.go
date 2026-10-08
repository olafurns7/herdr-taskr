package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestContractGoldenVectors(t *testing.T) {
	contractGuard(t)
	type compactVector struct {
		Name  string          `json:"name"`
		Input json.RawMessage `json:"input"`
		JSON  string          `json:"json"`
	}
	type floatVector struct {
		Bits string `json:"bits"`
		JSON string `json:"json"`
	}
	type requestVector struct {
		Name     string          `json:"name"`
		Input    json.RawMessage `json:"input"`
		Argv     []string        `json:"argv"`
		Document *rpcDocPayload  `json:"document"`
		JSON     string          `json:"json"`
		SHA256   string          `json:"sha256"`
	}
	vectors := struct {
		Compact  []compactVector `json:"compact"`
		Integers []compactVector `json:"integers"`
		Floats   []floatVector   `json:"floats"`
		Requests []requestVector `json:"requests"`
	}{}
	for _, raw := range []string{
		`null`, `[]`, `{}`, `{"z":"<>&\u2028\u2029","a":"Þ😀\"\\\n\t"}`,
		`{"ids":[9007199254740993,1.0,-0.0,0.000001,0.0000001,1e20,1e21,1.2345678901234567]}`,
		`{"<key>":{"b":false,"a":[null,"<&>"]},"é":"é"}`,
	} {
		var value any
		if err := json.Unmarshal([]byte(raw), &value); err != nil {
			t.Fatal(err)
		}
		vectors.Compact = append(vectors.Compact, compactVector{fmt.Sprint(len(vectors.Compact)), json.RawMessage(raw), jsonText(value)})
	}
	for _, value := range []any{int64(9007199254740993), int64(-9223372036854775808), uint64(18446744073709551615), map[string]any{"id": int64(9007199254740993), "nil": nil}} {
		text := jsonText(value)
		vectors.Integers = append(vectors.Integers, compactVector{fmt.Sprint(len(vectors.Integers)), json.RawMessage(text), text})
	}
	// Bits retain negative zero and integral floats that JSON round-tripping would erase.
	bits := []uint64{0, 1, 0x8000000000000000, 0x7fefffffffffffff, math.Float64bits(1e-6), math.Float64bits(1e-7), math.Float64bits(1e20), math.Float64bits(1e21), math.Float64bits(9007199254740993), math.Float64bits(1.2345678901234567)}
	state := uint64(0x9e3779b97f4a7c15)
	for range 2048 {
		state ^= state << 13
		state ^= state >> 7
		state ^= state << 17
		if !math.IsNaN(math.Float64frombits(state)) && !math.IsInf(math.Float64frombits(state), 0) {
			bits = append(bits, state)
		}
	}
	for _, b := range bits {
		vectors.Floats = append(vectors.Floats, floatVector{fmt.Sprintf("%016x", b), jsonText(math.Float64frombits(b))})
	}
	for _, raw := range []string{
		`{}`, `{"argv":null}`, `{"argv":[]}`, `{"argv":[],"document":null}`,
		`{"argv":["note","<>&\u2028\u2029","Þ😀"],"document":null}`,
		`{"argv":["set","--ref","a=1","--ref","a=2","--as","1","--as","1"]}`,
		`{"argv":["doc","set"],"document":{"task":1,"kind":"goal","name":"","path":"<>&\u2028\u2029"}}`,
		`{"argv":null,"document":{"task":9007199254740993,"kind":"report","name":"x","path":"p","event_id":null,"body":"","bytes":0}}`,
		`{"argv":[],"document":{"task":2,"kind":"report","name":"Þ😀","path":"p","event_id":3,"body":"PGFiYz4=","sha256":"abc","bytes":5,"reason":"<&>","backfill":true,"dry_run":true}}`,
	} {
		var req rpcRequest
		if err := json.Unmarshal([]byte(raw), &req); err != nil {
			t.Fatal(err)
		}
		encoded := jsonText(req.Argv)
		if req.Document != nil {
			encoded = jsonText(struct {
				Argv     []string       `json:"argv"`
				Document *rpcDocPayload `json:"document"`
			}{req.Argv, req.Document})
		}
		vectors.Requests = append(vectors.Requests, requestVector{fmt.Sprint(len(vectors.Requests)), json.RawMessage(raw), req.Argv, req.Document, encoded, rpcRequestSHA(req)})
	}
	data, err := json.MarshalIndent(vectors, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	contractGolden(t, "hash-vectors.json", append(data, '\n'))
}

func TestContractGoldenSchema(t *testing.T) {
	contractGuard(t)
	source, err := os.ReadFile("db.go")
	if err != nil {
		t.Fatal(err)
	}
	text := string(source)
	trigger := strings.SplitN(strings.SplitN(text, "const searchEventTriggerSQL = `", 2)[1], "`", 2)[0]
	rebuild := strings.SplitN(text, "_, err := tx.Exec(`\ndrop trigger", 2)[1]
	before, after, _ := strings.Cut(rebuild, "` + searchEventTriggerSQL + `;")
	rebuild = "\ndrop trigger" + before + trigger + ";" + strings.SplitN(after, "`)", 2)[0]
	for name, want := range map[string]string{"search-trigger.sql": trigger, "search-rebuild.sql": rebuild} {
		p := filepath.Join("crates", "taskr-core", "src", name)
		if os.Getenv("TASKR_GENERATE_CONTRACT") == "1" {
			if err := os.WriteFile(p, []byte(want), 0644); err != nil {
				t.Fatal(err)
			}
		}
		got, err := os.ReadFile(p)
		if err != nil {
			t.Fatal(err)
		}
		if string(got) != want {
			t.Fatalf("%s differs from Go source; regenerate with TASKR_GENERATE_CONTRACT=1", p)
		}
	}
}

func contractGolden(t *testing.T, name string, want []byte) {
	t.Helper()
	p := filepath.Join("testdata", "contract", name)
	if os.Getenv("TASKR_GENERATE_CONTRACT") == "1" {
		if err := os.WriteFile(p, want, 0644); err != nil {
			t.Fatal(err)
		}
	}
	got, err := os.ReadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(got, want) {
		t.Fatalf("%s differs from Go vectors; regenerate with TASKR_GENERATE_CONTRACT=1", p)
	}
}
