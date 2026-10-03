package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"
	"time"
)

// retryEndpoint adds an independently stoppable endpoint to the two-host
// ledger. Its handler can lose replies without losing the server's writes.
func retryEndpoint(t *testing.T, r *twoHost, wrap func(http.ResponseWriter, *http.Request)) (string, net.Listener, *http.Server) {
	t.Helper()
	ln, err := net.Listen("tcp", "[::1]:0")
	if err != nil {
		t.Fatal(err)
	}
	r.d.hub.Load().hosts[ln.Addr().String()] = true
	srv := &http.Server{Handler: http.HandlerFunc(wrap)}
	t.Cleanup(func() { srv.Close(); ln.Close() })
	return "http://" + ln.Addr().String(), ln, srv
}

func cutRetryReply(w http.ResponseWriter, status ...int) {
	conn, buf, _ := w.(http.Hijacker).Hijack()
	defer conn.Close()
	code := http.StatusOK
	if len(status) > 0 {
		code = status[0]
	}
	fmt.Fprintf(buf, "HTTP/1.1 %d %s\r\nContent-Length: 1000\r\n\r\n{", code, http.StatusText(code))
	buf.Flush()
}

func retryCLI(home string, args ...string) (int, string, string) {
	var out, errb bytes.Buffer
	code := cliMain(args, clientEnv(home, nil), &out, &errb)
	return code, out.String(), errb.String()
}

func checkRetryAnnounce(t *testing.T, stderr, reason, retry string) {
	t.Helper()
	line, _, _ := strings.Cut(stderr, "\n")
	prefix := "taskr: " + reason + "; retrying until "
	suffix := ""
	if retry != "" {
		suffix = "; if interrupted, retry with: " + retry
	}
	stamp := strings.TrimSuffix(strings.TrimPrefix(line, prefix), suffix)
	deadline, err := time.Parse(time.RFC3339, stamp)
	if err != nil || line != prefix+deadline.UTC().Format(time.RFC3339)+suffix {
		t.Fatalf("announce = %q, want %q + UTC deadline + %q", line, prefix, suffix)
	}
}

func TestRetryWaitServerReturns(t *testing.T) {
	r := newTwoHost(t)
	top := num(r.want(0, "host-a", nil, "new", "top", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	w := num(r.want(0, "host-a", nil, "new", "worker", "--role", "implementer", "--parent", id(top), "--cwd", t.TempDir()), "task_id")
	l := num(r.want(0, "host-a", nil, "launch", id(w), "--provider", "codex", "--model", "m", "--effort", "high"), "launch_id")
	url, ln, srv := retryEndpoint(t, r, r.d.ServeHTTP)
	addr := ln.Addr().String()
	ln.Close() // The client really dials a closed port first.
	home := r.clientHome(url)
	started := make(chan struct{})
	finished := make(chan struct{})
	go func() {
		defer close(finished)
		<-started
		l, err := net.Listen("tcp", addr)
		if err != nil {
			t.Error(err)
			return
		}
		srv.Serve(l)
	}()
	// The event is written while the retry endpoint is down.
	e := num(r.want(0, "host-a", as(w, l), "ready", "during outage"), "event_id")
	time.AfterFunc(200*time.Millisecond, func() { close(started) })
	code, out, stderr := retryCLI(home, "--json", "wait", "--as", id(top), "--timeout", "20000")
	if code != 0 || num(lastJSON(out)["event"].(map[string]any), "id") != e ||
		strings.Count(stderr, "retrying until") != 1 {
		t.Fatalf("wait after outage = %d %s %s", code, out, stderr)
	}
	checkRetryAnnounce(t, stderr, "server unreachable", "")
	srv.Close()
	<-finished
}

func TestRetryWriteLostReply(t *testing.T) {
	r := newTwoHost(t)
	top := num(r.want(0, "host-a", nil, "new", "top", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	var attempts atomic.Int32
	var mu sync.Mutex
	var keys []string
	var stored string
	url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
		body, _ := io.ReadAll(q.Body)
		var req rpcRequest
		json.Unmarshal(body, &req)
		q.Body = io.NopCloser(bytes.NewReader(body))
		mu.Lock()
		keys = append(keys, req.RequestKey)
		mu.Unlock()
		if attempts.Add(1) == 1 {
			rec := httptest.NewRecorder()
			r.d.ServeHTTP(rec, q)
			var rep rpcReply
			json.Unmarshal(rec.Body.Bytes(), &rep)
			mu.Lock()
			stored = rep.Stdout
			mu.Unlock()
			cutRetryReply(w)
			return
		}
		r.d.ServeHTTP(w, q)
	})
	go srv.Serve(ln)
	code, out, stderr := retryCLI(r.clientHome(url), "--json", "note", "once", "--as", id(top))
	mu.Lock()
	defer mu.Unlock()
	if code != 0 || len(keys) != 2 || keys[0] != keys[1] || out != stored ||
		r.count(`select count(*) from events where kind = 'note' and task_id = ?`, top) != 1 ||
		strings.Count(stderr, "retrying until") != 1 {
		t.Fatalf("lost reply = %d %s %s; keys=%v, stored=%q", code, out, stderr, keys, stored)
	}
	checkRetryAnnounce(t, stderr, "server unreachable", "taskr --request-key "+keys[0]+" --json note once --as "+id(top))
}

func TestRetryStillRunning(t *testing.T) {
	r := newTwoHost(t)
	w := r.promptTarget()
	started, release := make(chan struct{}), make(chan struct{})
	var once sync.Once
	setVar(t, &receiptPolled, func(int64) { once.Do(func() { close(started); <-release }) })
	argv := []string{"--json", "prompt", id(w), "--text", "once", "--confirm", "--confirm-timeout", "100"}
	done := make(chan rpcReply, 1)
	go func() {
		done <- r.d.rpcStored("host-a", rpcRequest{Argv: argv, Cwd: t.TempDir(), RequestKey: "running-retry-1"})
	}()
	select {
	case <-started:
	case <-time.After(10 * time.Second):
		t.Fatal("original request never started")
	}
	// Use callRetry directly: this is the ordinary server-host prompt path,
	// without the client-host prompt relay's separate routing phase.
	cl, e := newRPCClient(r.url)
	if e != nil {
		t.Fatal(e)
	}
	var stderr bytes.Buffer
	c := &ctx{json: true, errw: &stderr, getenv: clientEnv(r.homes["host-a"], nil)}
	time.AfterFunc(200*time.Millisecond, func() { close(release) })
	retry := "taskr --request-key running-retry-1 " + shellJoin(argv)
	rep, e := cl.callRetry(c, argv, t.TempDir(), "running-retry-1", retry, nil)
	original := <-done
	if e != nil || rep != original || !strings.Contains(rep.Stdout, "no_receipt") ||
		strings.Count(stderr.String(), "retrying until") != 1 || len(r.calls("agent|prompt|")) != 1 ||
		r.count(`select count(*) from events where kind = 'prompt' and task_id = ?`, w) != 1 {
		t.Fatalf("still running = %+v %v; original %+v, stderr %q", rep, e, original, stderr.String())
	}
	checkRetryAnnounce(t, stderr.String(), "request still running", retry)
}

func TestRetryDeadline(t *testing.T) {
	r := newTwoHost(t)
	setVar(t, &rpcRetryWindow, func([]string) time.Duration { return 120 * time.Millisecond })
	dead := r.clientHome("http://[::1]:1")
	for _, args := range [][]string{
		{"--json", "wait", "--as", "1", "--timeout", "120"},
		{"--json", "--request-key", "deadline-retry-1", "note", "once", "--as", "1"},
	} {
		start := time.Now()
		code, out, stderr := retryCLI(dead, args...)
		if code != exitHerdr || time.Since(start) > 600*time.Millisecond || lastJSON(out)["kind"] != "transport" || lastJSON(out)["unreachable"] != nil ||
			!strings.Contains(stderr, "retry with:") || strings.Count(stderr, "retrying until") != 1 {
			t.Fatalf("deadline = %d %s %s after %v", code, out, stderr, time.Since(start))
		}
		retry := ""
		if args[1] != "wait" {
			retry = "taskr --request-key deadline-retry-1 --json note once --as 1"
			_, final, _ := strings.Cut(stderr, "\n")
			line, _, _ := strings.Cut(final, "\n")
			if line != "taskr: server unreachable; retry with: "+retry {
				t.Fatalf("final retry line = %q", final)
			}
		}
		checkRetryAnnounce(t, stderr, "server unreachable", retry)
	}
}

func TestRetrySignal(t *testing.T) {
	if os.Getenv("TASKR_RETRY_SIGNAL_CHILD") == "1" {
		code := exitOK
		t.Cleanup(func() { os.Exit(code) }) // Registered first, so scratch cleanup runs before exit.
		r := newTwoHost(t)
		home := r.clientHome("http://[::1]:1")
		if os.Getenv("TASKR_RETRY_SIGNAL_PHASE") == "flight" {
			url, _, _ := retryEndpoint(t, r, nil) // Connect without starting a server that waits for HTTP on cleanup.
			home = r.clientHome(url)
			setVar(t, &hubWhoisArg, func(net.IP) string {
				fmt.Fprintln(os.Stderr, "taskr-test: attempt in flight")
				time.Sleep(2 * time.Second) // Verification has its own timeout, independent of the HTTP context.
				return "hub-::1"
			})
		}
		code = cliMain([]string{"--request-key", "signal-retry-1", "note", "once", "--as", "1"},
			clientEnv(home, nil), os.Stdout, os.Stderr)
		return
	}
	for _, sig := range []os.Signal{os.Interrupt, syscall.SIGTERM} {
		for _, phase := range []string{"backoff", "flight"} {
			t.Run(sig.String()+"/"+phase, func(t *testing.T) {
				cx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
				defer cancel()
				cmd := exec.CommandContext(cx, os.Args[0], "-test.run=^TestRetrySignal$")
				cmd.Env = []string{"TASKR_RETRY_SIGNAL_CHILD=1", "TASKR_RETRY_SIGNAL_PHASE=" + phase,
					"GORACE=atexit_sleep_ms=0", "PATH=/usr/bin:/bin:/usr/sbin:/sbin"}
				stderr, err := cmd.StderrPipe()
				if err != nil {
					t.Fatal(err)
				}
				if err := cmd.Start(); err != nil {
					t.Fatal(err)
				}
				defer cmd.Process.Kill()
				reader := bufio.NewReader(stderr)
				line, err := reader.ReadString('\n')
				if err != nil {
					t.Fatalf("child announce = %q, %v", line, err)
				}
				if phase == "backoff" {
					checkRetryAnnounce(t, line, "server unreachable", "taskr --request-key signal-retry-1 note once --as 1")
				} else if line != "taskr-test: attempt in flight\n" {
					t.Fatalf("child in-flight readiness = %q", line)
				}
				start := time.Now()
				if err := cmd.Process.Signal(sig); err != nil {
					t.Fatal(err)
				}
				rest, readErr := io.ReadAll(reader)
				waitErr := cmd.Wait()
				elapsed := time.Since(start)
				exit, ok := waitErr.(*exec.ExitError)
				if !ok || exit.ExitCode() != exitHerdr || readErr != nil || elapsed >= time.Second ||
					string(rest) != "taskr: server unreachable; retry with: taskr --request-key signal-retry-1 note once --as 1\n" {
					t.Fatalf("signal %v = %v, stderr %q, read %v, elapsed %v", sig, waitErr, rest, readErr, elapsed)
				}
			})
		}
	}
}

func TestRetryAttemptSlack(t *testing.T) {
	r := newTwoHost(t)
	setVar(t, &rpcRetryWindow, func([]string) time.Duration { return 120 * time.Millisecond })
	top := num(r.want(0, "host-a", nil, "new", "top", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	var attempts atomic.Int32
	url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
		attempts.Add(1)
		time.Sleep(250 * time.Millisecond)
		r.d.ServeHTTP(w, q)
	})
	go srv.Serve(ln)
	start := time.Now()
	code, out, stderr := retryCLI(r.clientHome(url), "--json", "note", "after window", "--as", id(top))
	if code != exitOK || time.Since(start) <= 120*time.Millisecond || attempts.Load() != 1 || stderr != "" ||
		lastJSON(out)["event_id"] == nil || r.count(`select count(*) from events where kind = 'note' and task_id = ?`, top) != 1 {
		t.Fatalf("attempt slack = %d %s %s, attempts %d after %v", code, out, stderr, attempts.Load(), time.Since(start))
	}
}

func TestRetryWaitLostReplyTimeout(t *testing.T) {
	for _, format := range []string{"compact", "json", "command-json"} {
		t.Run(format, func(t *testing.T) {
			r := newTwoHost(t)
			var mu sync.Mutex
			var requests []rpcRequest
			url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
				var req rpcRequest
				json.NewDecoder(q.Body).Decode(&req)
				mu.Lock()
				requests = append(requests, req)
				mu.Unlock()
				cutRetryReply(w)
			})
			go srv.Serve(ln)
			args := []string{"wait", "--as", "1", "--timeout=1400"}
			want := exitOK
			if format != "compact" {
				if format == "json" {
					args = append([]string{"--json"}, args...)
				} else {
					args = append(args, "--json")
				}
				want = exitTimeout
			}
			code, out, stderr := retryCLI(r.clientHome(url), args...)
			mu.Lock()
			defer mu.Unlock()
			if code != want || len(requests) != 2 || requests[0].RequestKey == requests[1].RequestKey ||
				rpcBudget(requests[1].Argv) >= rpcBudget(requests[0].Argv) || strings.Contains(stderr, "retry with:") {
				t.Fatalf("connected wait timeout = %d %s %s; requests=%+v", code, out, stderr, requests)
			}
			wantOut := "x1 3 timeout unreachable\n"
			if format != "compact" {
				wantOut = "{\"as\":1,\"timeout\":true,\"unreachable\":true}\n"
			}
			if out != wantOut {
				t.Fatalf("timeout output %q, want %q", out, wantOut)
			}
		})
	}
}

type retryRoundTripper func(*http.Request) (*http.Response, error)

func (f retryRoundTripper) RoundTrip(q *http.Request) (*http.Response, error) { return f(q) }

func TestRetryNonRetryable(t *testing.T) {
	for _, tc := range []struct {
		name   string
		status int
		body   string
	}{
		{"HTTP refusal", 403, `{"error":"refused"}`},
		{"HTTP server error", 503, `{"error":"down"}`},
		{"bad JSON", 200, `not JSON`},
		{"reply too large", 200, strings.Repeat("x", rpcReplyMax+1)},
	} {
		t.Run(tc.name, func(t *testing.T) {
			r := newTwoHost(t)
			var attempts atomic.Int32
			url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
				attempts.Add(1)
				w.WriteHeader(tc.status)
				io.WriteString(w, tc.body)
			})
			go srv.Serve(ln)
			code, _, stderr := retryCLI(r.clientHome(url), "status")
			if code == exitOK || attempts.Load() != 1 || strings.Contains(stderr, "retrying until") {
				t.Fatalf("non-retryable = %d, attempts %d, %s", code, attempts.Load(), stderr)
			}
		})
	}
	t.Run("unverified connection", func(t *testing.T) {
		r := newTwoHost(t)
		setVar(t, &rpcHTTPTransport, func() http.RoundTripper {
			return retryRoundTripper(func(*http.Request) (*http.Response, error) {
				return &http.Response{StatusCode: 200, Body: io.NopCloser(strings.NewReader(`{}`)), Header: http.Header{}}, nil
			})
		})
		code, out, stderr := retryCLI(r.homes["host-a"], "status")
		if code != exitHerdr || !strings.Contains(out, "verified server connection") || strings.Contains(stderr, "retrying until") {
			t.Fatalf("unverified = %d %s %s", code, out, stderr)
		}
	})
	t.Run("usage", func(t *testing.T) {
		r := newTwoHost(t)
		code, _, stderr := retryCLI(r.homes["host-a"], "wait", "--timeout", "-1")
		if code != exitUsage || strings.Contains(stderr, "retrying until") {
			t.Fatalf("usage = %d %s", code, stderr)
		}
	})
}

func TestRetryHookDeadline(t *testing.T) {
	r := newTwoHost(t)
	home := r.clientHome("http://[::1]:1")
	var out, stderr bytes.Buffer
	start := time.Now()
	code := cliMain([]string{"hook", "codex", "Stop", "--session", "retry-hook-session"},
		clientEnv(home, map[string]string{"TASKR_TASK": "1", "TASKR_LAUNCH": "1"}), &out, &stderr)
	if code != exitOK || time.Since(start) > hookDeadline+100*time.Millisecond || strings.Contains(stderr.String(), "retrying until") {
		t.Fatalf("hook = %d %s %s after %v", code, out.String(), stderr.String(), time.Since(start))
	}
}

func TestRetryWindowBudget(t *testing.T) {
	if rpcRetryWindow([]string{"note", "once"}) != time.Minute ||
		rpcRetryWindow([]string{"answer", "1", "yes", "--prompt", "--confirm"}) != 90*time.Second {
		t.Fatal("retry window must cover the command budget")
	}
}

func TestRetryPromptRelayUnchanged(t *testing.T) {
	r := newTwoHost(t)
	start := time.Now()
	code, out, stderr := retryCLI(r.clientHome("http://[::1]:1"), "--json", "prompt", "1", "--text", "once")
	if code != exitHerdr || time.Since(start) > time.Second || strings.Contains(stderr, "retrying until") ||
		!strings.Contains(out, "no attempt is known") || len(r.calls("agent|prompt|")) != 0 {
		t.Fatalf("prompt relay = %d %s %s after %v", code, out, stderr, time.Since(start))
	}
}

func TestRetryRefusalCutOff(t *testing.T) {
	for _, status := range []int{http.StatusForbidden, http.StatusServiceUnavailable} {
		t.Run(fmt.Sprint(status), func(t *testing.T) {
			r := newTwoHost(t)
			var attempts atomic.Int32
			url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
				attempts.Add(1)
				cutRetryReply(w, status)
			})
			go srv.Serve(ln)
			code, _, stderr := retryCLI(r.clientHome(url), "status")
			if code != exitHerdr || attempts.Load() != 1 || strings.Contains(stderr, "retrying until") {
				t.Fatalf("cut-off refusal = %d %s, attempts %d", code, stderr, attempts.Load())
			}
		})
	}
}

func TestRetryHealthyWaitTimeout(t *testing.T) {
	r := newTwoHost(t)
	top := num(r.want(0, "host-a", nil, "new", "top", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	for _, jsonFormat := range []bool{false, true} {
		args := []string{"wait", "--as", id(top), "--timeout", "120"}
		want := exitOK
		if jsonFormat {
			args = append([]string{"--json"}, args...)
			want = exitTimeout
		}
		code, out, stderr := retryCLI(r.homes["host-a"], args...)
		if code != want || stderr != "" {
			t.Fatalf("healthy timeout = %d %s %s", code, out, stderr)
		}
		if jsonFormat {
			if out != fmt.Sprintf("{\"as\":%d,\"due\":0,\"owed\":0,\"timeout\":true}\n", top) {
				t.Fatalf("healthy JSON timeout = %s", out)
			}
		} else if out != "w1 {\"owed\":0,\"due\":0}\nx1 3 timeout\n" {
			t.Fatalf("healthy compact timeout = %q", out)
		}
	}
}
