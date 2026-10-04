package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"syscall"
	"time"
)

const (
	spoolDirName  = "spool"
	spoolQueueDir = "queue"
	spoolFailDir  = "refused"
	spoolFileMax  = 500
	spoolByteMax  = 20 << 20
)

type spoolRecord struct {
	Version    int            `json:"v"`
	Seq        int64          `json:"seq"`
	RequestKey string         `json:"request_key"`
	Request    rpcRequest     `json:"request"`
	QueuedAt   string         `json:"queued_at"`
	Document   *rpcDocPayload `json:"document,omitempty"`
	RefusedAt  string         `json:"refused_at,omitempty"`
	Exit       int            `json:"exit,omitempty"`
	Error      string         `json:"error,omitempty"`
	Shown      bool           `json:"shown,omitempty"`
}

type spoolFile struct {
	path   string
	record spoolRecord
}

type spoolListItem struct {
	Seq     int64  `json:"seq"`
	Age     string `json:"age"`
	Command string `json:"command"`
	Task    int64  `json:"task,omitempty"`
	Error   string `json:"error,omitempty"`
}

func spoolPath(dir string) string { return filepath.Join(dir, spoolDirName) }

func spoolCounts(dir string) (int, int) {
	return countSpoolFiles(filepath.Join(spoolPath(dir), spoolQueueDir)),
		countSpoolFiles(filepath.Join(spoolPath(dir), spoolFailDir))
}

func countSpoolFiles(dir string) int {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return 0
	}
	n := 0
	for _, entry := range entries {
		if !entry.IsDir() && strings.HasSuffix(entry.Name(), ".json") {
			n++
		}
	}
	return n
}

func ensureSpoolDirs(dir string) error {
	root := spoolPath(dir)
	for _, path := range []string{root, filepath.Join(root, spoolQueueDir), filepath.Join(root, spoolFailDir)} {
		if err := os.MkdirAll(path, 0o700); err != nil {
			return err
		}
	}
	return nil
}

func lockSpool(dir string, wait bool) (*os.File, bool, error) {
	root := spoolPath(dir)
	if wait {
		if err := os.MkdirAll(root, 0o700); err != nil {
			return nil, false, err
		}
	} else if _, err := os.Stat(root); errors.Is(err, os.ErrNotExist) {
		return nil, false, nil
	} else if err != nil {
		return nil, false, err
	}
	f, err := os.OpenFile(filepath.Join(root, "lock"), os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return nil, false, err
	}
	mode := syscall.LOCK_EX
	if !wait {
		mode |= syscall.LOCK_NB
	}
	if err := syscall.Flock(int(f.Fd()), mode); err != nil {
		f.Close()
		if errors.Is(err, syscall.EWOULDBLOCK) || errors.Is(err, syscall.EAGAIN) {
			return nil, false, nil
		}
		return nil, false, err
	}
	return f, true, nil
}

func unlockSpool(f *os.File) {
	if f == nil {
		return
	}
	_ = syscall.Flock(int(f.Fd()), syscall.LOCK_UN)
	_ = f.Close()
}

func readSpoolFiles(dir string) ([]spoolFile, error) {
	entries, err := os.ReadDir(dir)
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var files []spoolFile
	for _, entry := range entries {
		if entry.IsDir() || !strings.HasSuffix(entry.Name(), ".json") {
			continue
		}
		path := filepath.Join(dir, entry.Name())
		b, err := os.ReadFile(path)
		if err != nil {
			return nil, err
		}
		var record spoolRecord
		if err := json.Unmarshal(b, &record); err != nil {
			return nil, fmt.Errorf("read spool record %s: %w", entry.Name(), err)
		}
		if record.Version != 1 || record.Seq <= 0 || !requestKeyRe.MatchString(record.RequestKey) ||
			record.Request.RequestKey != record.RequestKey || !validSpoolTime(record.QueuedAt) {
			return nil, fmt.Errorf("invalid spool record %s", entry.Name())
		}
		files = append(files, spoolFile{path: path, record: record})
	}
	sort.Slice(files, func(i, j int) bool { return files[i].record.Seq < files[j].record.Seq })
	return files, nil
}

func validSpoolTime(s string) bool {
	_, err := time.Parse(time.RFC3339, s)
	return err == nil
}

func spoolTime(s string) time.Time {
	t, _ := time.Parse(time.RFC3339, s)
	return t
}

func spoolMaxSeq(dir string) int64 {
	var maxSeq int64
	for _, folder := range []string{filepath.Join(spoolPath(dir), spoolQueueDir), filepath.Join(spoolPath(dir), spoolFailDir)} {
		entries, err := os.ReadDir(folder)
		if err != nil {
			continue
		}
		for _, entry := range entries {
			prefix, _, ok := strings.Cut(entry.Name(), "-")
			if !ok {
				continue
			}
			seq, err := strconv.ParseInt(prefix, 10, 64)
			if err == nil && seq > maxSeq {
				maxSeq = seq
			}
		}
	}
	return maxSeq
}

func nextSpoolSeq(lock *os.File, dir string) (int64, error) {
	if _, err := lock.Seek(0, io.SeekStart); err != nil {
		return 0, err
	}
	b, err := io.ReadAll(lock)
	if err != nil {
		return 0, err
	}
	stored, _ := strconv.ParseInt(strings.TrimSpace(string(b)), 10, 64)
	seq := max(stored, spoolMaxSeq(dir)) + 1
	if err := lock.Truncate(0); err != nil {
		return 0, err
	}
	if _, err := lock.Seek(0, io.SeekStart); err != nil {
		return 0, err
	}
	if _, err := fmt.Fprintln(lock, seq); err != nil {
		return 0, err
	}
	if err := lock.Sync(); err != nil {
		return 0, err
	}
	return seq, nil
}

func writeSpoolAtomic(dir, name string, record spoolRecord) error {
	b, err := json.Marshal(record)
	if err != nil {
		return err
	}
	b = append(b, '\n')
	tmp, err := os.CreateTemp(dir, ".spool-*.tmp")
	if err != nil {
		return err
	}
	tmpPath := tmp.Name()
	defer os.Remove(tmpPath)
	if err := tmp.Chmod(0o600); err != nil {
		tmp.Close()
		return err
	}
	if _, err := tmp.Write(b); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Sync(); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Close(); err != nil {
		return err
	}
	return os.Rename(tmpPath, filepath.Join(dir, name))
}

func queueSpoolRecord(dir string, req rpcRequest, document *rpcDocPayload) (int, error) {
	return queueSpoolRecordMode(dir, req, document, true)
}

func queueSpoolRecordMode(dir string, req rpcRequest, document *rpcDocPayload, wait bool) (int, error) {
	if !requestKeyRe.MatchString(req.RequestKey) || len(req.Argv) == 0 {
		return 0, usageErr("invalid spool request")
	}
	if err := ensureSpoolDirs(dir); err != nil {
		return 0, err
	}
	lock, ok, err := lockSpool(dir, wait)
	if err != nil {
		return 0, err
	}
	if !ok {
		return countSpoolFiles(filepath.Join(spoolPath(dir), spoolQueueDir)), errSpoolBusy
	}
	defer unlockSpool(lock)
	queueDir := filepath.Join(spoolPath(dir), spoolQueueDir)
	files, err := readSpoolFiles(queueDir)
	if err != nil {
		return 0, err
	}
	for _, file := range files {
		if file.record.RequestKey == req.RequestKey {
			if rpcRequestSHA(file.record.Request) != rpcRequestSHA(req) {
				return len(files), rejectErr("request key %s belongs to another command", req.RequestKey)
			}
			return len(files), nil
		}
	}
	count, size, err := spoolQueueUsage(queueDir)
	if err != nil {
		return 0, err
	}
	if count+1 > spoolFileMax {
		return count, errSpoolFull
	}
	record := spoolRecord{Version: 1, RequestKey: req.RequestKey, Request: req, QueuedAt: time.Now().UTC().Format(time.RFC3339), Document: document}
	record.Request.QueuedAt = ""
	if document != nil {
		copy := *document
		record.Document = &copy
	}
	seq, err := nextSpoolSeq(lock, dir)
	if err != nil {
		return count, err
	}
	record.Seq = seq
	encoded, err := json.Marshal(record)
	if err != nil {
		return 0, err
	}
	if size+int64(len(encoded)+1) > spoolByteMax {
		return count, errSpoolFull
	}
	name := fmt.Sprintf("%06d-%s.json", seq, req.RequestKey)
	if err := writeSpoolAtomic(queueDir, name, record); err != nil {
		return count, err
	}
	return count + 1, nil
}

var errSpoolFull = errors.New("spool is full")
var errSpoolBusy = errors.New("spool lock is held")

func spoolQueueUsage(dir string) (int, int64, error) {
	entries, err := os.ReadDir(dir)
	if errors.Is(err, os.ErrNotExist) {
		return 0, 0, nil
	}
	if err != nil {
		return 0, 0, err
	}
	var count int
	var size int64
	for _, entry := range entries {
		if entry.IsDir() || !strings.HasSuffix(entry.Name(), ".json") {
			continue
		}
		info, err := entry.Info()
		if err != nil {
			return 0, 0, err
		}
		count++
		size += info.Size()
	}
	return count, size, nil
}

func queueSpoolIfWaiting(dir string, req rpcRequest, document *rpcDocPayload) (int, bool, error) {
	return queueSpoolIfWaitingMode(dir, req, document, true)
}

func queueSpoolIfWaitingMode(dir string, req rpcRequest, document *rpcDocPayload, wait bool) (int, bool, error) {
	queueDir := filepath.Join(spoolPath(dir), spoolQueueDir)
	if _, err := os.Stat(queueDir); errors.Is(err, os.ErrNotExist) {
		return 0, false, nil
	} else if err != nil {
		return 0, false, err
	}
	lock, ok, err := lockSpool(dir, wait)
	if err != nil {
		return 0, false, err
	}
	if !ok {
		return countSpoolFiles(queueDir), true, nil
	}
	defer unlockSpool(lock)
	files, err := readSpoolFiles(queueDir)
	if err != nil {
		return 0, false, err
	}
	if len(files) == 0 {
		return 0, false, nil
	}
	count, err := queueSpoolRecordLocked(dir, lock, req, document, files)
	return count, true, err
}

func queueSpoolRecordLocked(dir string, lock *os.File, req rpcRequest, document *rpcDocPayload, files []spoolFile) (int, error) {
	queueDir := filepath.Join(spoolPath(dir), spoolQueueDir)
	for _, file := range files {
		if file.record.RequestKey == req.RequestKey {
			if rpcRequestSHA(file.record.Request) != rpcRequestSHA(req) {
				return len(files), rejectErr("request key %s belongs to another command", req.RequestKey)
			}
			return len(files), nil
		}
	}
	count, size, err := spoolQueueUsage(queueDir)
	if err != nil {
		return 0, err
	}
	if count+1 > spoolFileMax {
		return count, errSpoolFull
	}
	record := spoolRecord{Version: 1, RequestKey: req.RequestKey, Request: req, QueuedAt: time.Now().UTC().Format(time.RFC3339)}
	record.Request.QueuedAt = ""
	if document != nil {
		copy := *document
		record.Document = &copy
	}
	seq, err := nextSpoolSeq(lock, dir)
	if err != nil {
		return count, err
	}
	record.Seq = seq
	encoded, err := json.Marshal(record)
	if err != nil {
		return 0, err
	}
	if size+int64(len(encoded)+1) > spoolByteMax {
		return count, errSpoolFull
	}
	name := fmt.Sprintf("%06d-%s.json", seq, req.RequestKey)
	if err := writeSpoolAtomic(queueDir, name, record); err != nil {
		return count, err
	}
	return count + 1, nil
}

func readSpoolListing(dir string) (map[string]any, error) {
	queue, err := readSpoolFiles(filepath.Join(spoolPath(dir), spoolQueueDir))
	if err != nil {
		return nil, err
	}
	refused, err := readSpoolFiles(filepath.Join(spoolPath(dir), spoolFailDir))
	if err != nil {
		return nil, err
	}
	toItems := func(files []spoolFile, failed bool) []spoolListItem {
		items := make([]spoolListItem, 0, len(files))
		for _, file := range files {
			r := file.record
			age := time.Since(spoolTime(r.QueuedAt))
			if age < 0 {
				age = 0
			}
			command, _ := rpcCommand(r.Request.Argv)
			item := spoolListItem{Seq: r.Seq, Age: age.Round(time.Second).String(), Command: command, Task: spoolTaskID(r.Request)}
			if failed {
				item.Error = r.Error
			}
			items = append(items, item)
		}
		return items
	}
	return map[string]any{"queued": toItems(queue, false), "refused": toItems(refused, true)}, nil
}

func spoolTaskID(req rpcRequest) int64 {
	if n, err := strconv.ParseInt(req.Env["TASKR_TASK"], 10, 64); err == nil && n > 0 {
		return n
	}
	name, args := rpcCommand(req.Argv)
	if name == "note" || name == "decide" {
		if v, _, _, ok := flagValue(args, "as"); ok {
			n, _ := strconv.ParseInt(v, 10, 64)
			return n
		}
	}
	if name == "next" || name == "close" {
		for _, arg := range args {
			if !strings.HasPrefix(arg, "-") {
				n, _ := strconv.ParseInt(arg, 10, 64)
				return n
			}
		}
	}
	return 0
}

func rmSpoolRecord(dir string, seq int64) error {
	if _, err := os.Stat(spoolPath(dir)); errors.Is(err, os.ErrNotExist) {
		return rejectErr("spool sequence %d does not exist", seq)
	} else if err != nil {
		return err
	}
	lock, ok, err := lockSpool(dir, true)
	if err != nil {
		return err
	}
	if !ok {
		return rejectErr("spool sequence %d does not exist", seq)
	}
	defer unlockSpool(lock)
	for _, folder := range []string{spoolQueueDir, spoolFailDir} {
		files, err := readSpoolFiles(filepath.Join(spoolPath(dir), folder))
		if err != nil {
			return err
		}
		for _, file := range files {
			if file.record.Seq == seq {
				return os.Remove(file.path)
			}
		}
	}
	return rejectErr("spool sequence %d does not exist", seq)
}

func moveSpoolRefused(dir string, file spoolFile, exit int, message string) error {
	record := file.record
	record.RefusedAt, record.Exit, record.Error = time.Now().UTC().Format(time.RFC3339), exit, message
	queueDir := filepath.Dir(file.path)
	name := filepath.Base(file.path)
	if err := writeSpoolAtomic(queueDir, name, record); err != nil {
		return err
	}
	return os.Rename(file.path, filepath.Join(spoolPath(dir), spoolFailDir, name))
}

func sendSpool(dir, raw string, log *daemonLog) (int, error) {
	queueDir := filepath.Join(spoolPath(dir), spoolQueueDir)
	if countSpoolFiles(queueDir) == 0 {
		return 0, nil
	}
	lock, ok, err := lockSpool(dir, false)
	if err != nil || !ok {
		return 0, err
	}
	defer unlockSpool(lock)
	files, err := readSpoolFiles(queueDir)
	if err != nil || len(files) == 0 {
		return 0, err
	}
	cl, connectErr := newRPCClient(raw)
	if connectErr != nil {
		if log != nil {
			log.limited("spool-connect", time.Minute, "spool send unavailable: %s", connectErr.msg)
		}
		return 0, connectErr
	}
	sent := 0
	for _, file := range files {
		req := file.record.Request
		req.QueuedAt = file.record.QueuedAt
		rep, callErr, noReply := cl.callStored(context.Background(), req)
		if callErr != nil {
			if noReply {
				break
			}
			if err := moveSpoolRefused(dir, file, callErr.code, callErr.msg); err != nil {
				return sent, err
			}
			continue
		}
		if reqName, _ := rpcCommand(req.Argv); reqName == "_hook" && rep.Exit == exitOK && strings.TrimSpace(rep.Stdout) == "expired" {
			if err := os.Remove(file.path); err != nil {
				return sent, err
			}
			continue
		}
		if rep.Exit != exitOK {
			if err := moveSpoolRefused(dir, file, rep.Exit, rpcReplyError(rep)); err != nil {
				return sent, err
			}
			continue
		}
		if err := os.Remove(file.path); err != nil {
			return sent, err
		}
		wants := []rpcDocWant{}
		if rep.Upload != nil {
			wants = *rep.Upload
		}
		clientUploadSpoolDocs(cl, wants, req.Cwd, req.Env, file.record.Document)
		sent++
	}
	return sent, nil
}

func rpcReplyError(rep rpcReply) string {
	line := strings.TrimSpace(rep.Stdout)
	if at := strings.LastIndex(line, "\n"); at >= 0 {
		line = line[at+1:]
	}
	if strings.HasPrefix(line, "x1 ") {
		if at := strings.IndexByte(line, ' '); at >= 0 {
			if next := strings.IndexByte(line[at+1:], ' '); next >= 0 {
				line = line[at+1+next+1:]
			}
		}
	}
	var obj map[string]any
	if json.Unmarshal([]byte(line), &obj) == nil {
		if msg, ok := obj["error"].(string); ok && msg != "" {
			return msg
		}
		if msg, ok := obj["err"].(string); ok && msg != "" {
			return msg
		}
	}
	if msg := strings.TrimSpace(rep.Stderr); msg != "" {
		return msg
	}
	return fmt.Sprintf("server exited %d", rep.Exit)
}

func notifySpoolRefused(dir, sock string, log *daemonLog) {
	files, err := readSpoolFiles(filepath.Join(spoolPath(dir), spoolFailDir))
	if err != nil {
		if log != nil {
			log.logf("spool refused read failed: %v", err)
		}
		return
	}
	for _, file := range files {
		record := file.record
		if record.Shown {
			continue
		}
		command, _ := rpcCommand(record.Request.Argv)
		body := fmt.Sprintf("taskr: queued %s for task %d was refused: %s", command, spoolTaskID(record.Request), record.Error)
		if err := herdrRun(sock, "notification", "show", "taskr: queued record refused",
			"--body", truncate(body, notifyBodyMax), "--sound", "request"); err != nil {
			if log != nil {
				log.logf("notify refused spool %d failed: %v", record.Seq, err)
			}
			continue
		}
		lock, ok, err := lockSpool(dir, true)
		if err != nil || !ok {
			if log != nil && err != nil {
				log.logf("spool notification marker lock failed: %v", err)
			}
			continue
		}
		if _, err := os.Stat(file.path); err == nil {
			record.Shown = true
			name := filepath.Base(file.path)
			if err := writeSpoolAtomic(filepath.Join(spoolPath(dir), spoolFailDir), name, record); err != nil && log != nil {
				log.logf("spool notification marker write failed: %v", err)
			}
		}
		unlockSpool(lock)
	}
}

func cmdSpool(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("spool", flag.ContinueOnError)
	pos, err := parseArgs(c, fs, args, 1, 2)
	if err != nil {
		return nil, 0, err
	}
	dir, err := stateDir(c)
	if err != nil {
		return nil, 0, err
	}
	switch pos[0] {
	case "ls":
		if len(pos) != 1 {
			return nil, 0, usageErr("spool ls takes no arguments")
		}
		listing, err := readSpoolListing(dir)
		return listing, exitOK, err
	case "send":
		if len(pos) != 1 {
			return nil, 0, usageErr("spool send takes no arguments")
		}
		if !c.client || c.server == "" {
			return nil, 0, usageErr("spool send is only available on a client host")
		}
		sent, err := sendSpool(dir, c.server, nil)
		queued, refused := spoolCounts(dir)
		return map[string]any{"ok": err == nil, "sent": sent, "queued": queued, "refused": refused}, exitOK, err
	case "rm":
		if len(pos) != 2 {
			return nil, 0, usageErr("spool rm needs SEQ")
		}
		seq, err := strconv.ParseInt(pos[1], 10, 64)
		if err != nil || seq <= 0 {
			return nil, 0, usageErr("spool rm needs a positive sequence")
		}
		if err := rmSpoolRecord(dir, seq); err != nil {
			return nil, 0, err
		}
		return map[string]any{"ok": true, "removed": seq}, exitOK, nil
	default:
		return nil, 0, usageErr("spool: expected ls, send or rm SEQ")
	}
}
