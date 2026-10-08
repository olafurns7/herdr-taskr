package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"testing"
)

const contractNotImplemented = 125

type contractClass struct{ Family, Ready, Group, Reason string }

var contractMissing sync.Map
var contractFiltered sync.Map
var contractClasses = map[string]contractClass{}
var contractStats = struct {
	sync.Mutex
	Counts map[string]map[string]int
}{Counts: map[string]map[string]int{}}

func contractInit() {
	if bin := os.Getenv("TASKR_BIN"); bin != "" {
		abs, err := filepath.Abs(bin)
		if err != nil {
			panic(err)
		}
		os.Setenv("TASKR_BIN", abs)
		data, err := os.ReadFile("testdata/contract/tests.tsv")
		if err != nil {
			panic(err)
		}
		for _, line := range strings.Split(strings.TrimSpace(string(data)), "\n")[1:] {
			fields := strings.SplitN(line, "\t", 5)
			if len(fields) != 5 {
				panic("invalid tests.tsv row: " + line)
			}
			contractClasses[fields[0]] = contractClass{fields[1], fields[2], fields[3], fields[4]}
		}
	}
}

func contractOutcome(family, outcome string) {
	contractStats.Lock()
	defer contractStats.Unlock()
	if contractStats.Counts[family] == nil {
		contractStats.Counts[family] = map[string]int{}
	}
	contractStats.Counts[family][outcome]++
}

// Every top-level test gets this guard so Go internals never masquerade as Rust coverage.
func contractGuard(t *testing.T) {
	t.Helper()
	if os.Getenv("TASKR_BIN") == "" {
		return
	}
	root := strings.SplitN(t.Name(), "/", 2)[0]
	class, ok := contractClasses[root]
	if !ok {
		t.Fatal("missing contract classification: " + root)
	}
	t.Cleanup(func() {
		outcome := "pass"
		if _, missing := contractMissing.Load(root); missing {
			outcome = "not-implemented"
		}
		if _, filtered := contractFiltered.Load(root); filtered {
			outcome = "skipped-family"
		}
		if t.Failed() {
			outcome = "mismatch"
		} else if t.Skipped() && outcome == "pass" {
			if class.Ready == "no" {
				outcome = "skipped-internals"
			} else {
				outcome = "skipped-other"
			}
		}
		contractOutcome(class.Family, outcome)
	})
	if filter := os.Getenv("TASKR_CONTRACT_FAMILY"); filter != "" && class.Family != filter && !strings.HasPrefix(class.Family, filter+":") {
		contractFiltered.Store(root, true)
		t.Skip("adapter: outside selected family " + filter)
	}
	if class.Ready != "yes" {
		t.Skip("adapter: " + class.Reason)
	}
}

func contractSummary() {
	if os.Getenv("TASKR_BIN") == "" {
		return
	}
	contractStats.Lock()
	defer contractStats.Unlock()
	var families []string
	for family := range contractStats.Counts {
		families = append(families, family)
	}
	sort.Strings(families)
	for _, family := range families {
		counts := contractStats.Counts[family]
		fmt.Fprintf(os.Stderr, "contract family=%s pass=%d not-implemented=%d skipped-internals=%d mismatch=%d skipped-family=%d skipped-other=%d\n", family, counts["pass"], counts["not-implemented"], counts["skipped-internals"], counts["mismatch"], counts["skipped-family"], counts["skipped-other"])
	}
	if path := os.Getenv("TASKR_CONTRACT_SUMMARY"); path != "" {
		data, err := json.MarshalIndent(contractStats.Counts, "", "  ")
		if err != nil {
			panic(err)
		}
		if err = os.WriteFile(path, append(data, '\n'), 0600); err != nil {
			panic(err)
		}
	}
}

// The target gets a deliberate environment, never the worker's task identity or HOME.
func contractCommand(args []string, getenv func(string) string, stdout, stderr io.Writer) *exec.Cmd {
	cmd := exec.Command(os.Getenv("TASKR_BIN"), args...)
	cmd.Stdout, cmd.Stderr = stdout, stderr
	for _, key := range []string{"HOME", "TASKR_DB", "PATH", "HERDR_SOCKET_PATH", "TASKR_FORMAT", "TASKR_TASK", "TASKR_LAUNCH", "HERDR_ENV", "HERDR_PANE_ID", "HERDR_TAB_ID", "HERDR_WORKSPACE_ID", "INVOCATION_ID", "SYSTEMD_EXEC_PID", "CLAUDE_CONFIG_DIR", "CODEX_HOME", "CODEX_THREAD_ID"} {
		value := getenv(key)
		if key == "PATH" && value == "" {
			value = os.Getenv("PATH")
		}
		if value != "" {
			cmd.Env = append(cmd.Env, key+"="+value)
		}
	}
	if raw := os.Getenv("TASKR_FROZEN_NOW"); raw != "" {
		cmd.Env = append(cmd.Env, "TASKR_FROZEN_NOW="+raw)
	}
	cmd.Env = append(cmd.Env, "LANG=C.UTF-8", "TZ=UTC", "TASKR_CONTRACT_ORACLE=1")
	return cmd
}

func contractInvoke(args []string, getenv func(string) string, stdout, stderr io.Writer) int {
	err := contractCommand(args, getenv, stdout, stderr).Run()
	if err == nil {
		return 0
	}
	if e, ok := err.(*exec.ExitError); ok {
		return e.ExitCode()
	}
	fmt.Fprintf(stderr, "adapter exec: %v\n", err)
	return 127
}

func contractRun(t *testing.T, args []string, getenv func(string) string, stdout, stderr io.Writer) int {
	t.Helper()
	if os.Getenv("TASKR_BIN") == "" {
		return run(args, getenv, stdout, stderr)
	}
	code := contractInvoke(args, getenv, stdout, stderr)
	if code == contractNotImplemented {
		contractMissing.Store(strings.SplitN(t.Name(), "/", 2)[0], true)
		t.Skipf("adapter: not implemented: %v", args)
	}
	return code
}

// Direct cliMain tests also cross the binary boundary; local Go behavior stays unchanged.
func contractCLIMain(t *testing.T, args []string, getenv func(string) string, stdout, stderr io.Writer) int {
	if os.Getenv("TASKR_BIN") == "" {
		return cliMain(args, getenv, stdout, stderr)
	}
	code := contractInvoke(args, getenv, stdout, stderr)
	if code == contractNotImplemented {
		contractMissing.Store(strings.SplitN(t.Name(), "/", 2)[0], true)
		t.Skipf("adapter: not implemented: %v", args)
	}
	return code
}

func TestContractVersionAdapter(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	data, err := os.ReadFile("testdata/contract/cases.json")
	if err != nil {
		t.Fatal(err)
	}
	var cases []struct {
		Family string
		Argv   []string
		Env    map[string]string
	}
	if err := json.Unmarshal(data, &cases); err != nil {
		t.Fatal(err)
	}
	for _, sample := range cases {
		if sample.Family != "read:version" {
			continue
		}
		args := sample.Argv
		var out, errb bytes.Buffer
		code := contractRun(t, args, h.getenv(sample.Env), &out, &errb)
		var wantOut, wantErr bytes.Buffer
		wantCode := run(args, h.getenv(sample.Env), &wantOut, &wantErr)
		if code != wantCode || out.String() != wantOut.String() || errb.String() != wantErr.String() {
			t.Fatalf("version %v: (%d,%q,%q), want (%d,%q,%q)", args, code, out.String(), errb.String(), wantCode, wantOut.String(), wantErr.String())
		}
	}
}
