package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net/http"
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
	spoolBadDir   = "bad"
	spoolFileMax  = 500
	spoolByteMax  = 20 << 20
)

var spoolNow = time.Now

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
	path     string
	record   spoolRecord
	parseErr string
}

type spoolListItem struct {
	Seq     int64  `json:"seq"`
	Age     string `json:"age"`
	Command string `json:"command"`
	Task    int64  `json:"task,omitempty"`
	Kind    string `json:"kind,omitempty"`
	Error   string `json:"error,omitempty"`
}

func spoolPath(dir string) string { return filepath.Join(dir, spoolDirName) }

func spoolCounts(dir string) (int, int, int) {
	return countSpoolFiles(filepath.Join(spoolPath(dir), spoolQueueDir)),
		countSpoolFiles(filepath.Join(spoolPath(dir), spoolFailDir)),
		countSpoolFiles(filepath.Join(spoolPath(dir), spoolBadDir))
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
	for _, path := range []string{root, filepath.Join(root, spoolQueueDir), filepath.Join(root, spoolFailDir), filepath.Join(root, spoolBadDir)} {
		if err := os.MkdirAll(path, 0o700); err != nil {
			return err
		}
	}
	return nil
}

func lockSpoolFile(dir, name string, wait bool) (*os.File, bool, error) {
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
	f, err := os.OpenFile(filepath.Join(root, name), os.O_CREATE|os.O_RDWR, 0o600)
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

func lockSpool(dir string, wait bool) (*os.File, bool, error) {
	return lockSpoolFile(dir, "lock", wait)
}

func lockSpoolSender(dir string) (*os.File, bool, error) {
	return lockSpoolFile(dir, "send.lock", false)
}

func unlockSpool(f *os.File) {
	if f == nil {
		return
	}
	_ = syscall.Flock(int(f.Fd()), syscall.LOCK_UN)
	_ = f.Close()
}

func spoolSeqFromName(name string) int64 {
	prefix, _, ok := strings.Cut(name, "-")
	if !ok {
		return 0
	}
	seq, _ := strconv.ParseInt(prefix, 10, 64)
	return seq
}

func readSpoolEntries(dir string) ([]spoolFile, error) {
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
		file := spoolFile{path: path, record: spoolRecord{Seq: spoolSeqFromName(entry.Name())}}
		b, readErr := os.ReadFile(path)
		if readErr != nil {
			file.parseErr = readErr.Error()
		} else {
			var record spoolRecord
			if parseErr := json.Unmarshal(b, &record); parseErr != nil {
				file.parseErr = parseErr.Error()
			} else if record.Version != 1 {
				file.parseErr = fmt.Sprintf("unsupported spool version %d", record.Version)
			} else if record.Seq <= 0 || !requestKeyRe.MatchString(record.RequestKey) ||
				record.Request.RequestKey != record.RequestKey || !validSpoolTime(record.QueuedAt) {
				file.parseErr = "invalid spool record fields"
			} else {
				file.record = record
			}
		}
		files = append(files, file)
	}
	sort.Slice(files, func(i, j int) bool {
		if files[i].record.Seq == files[j].record.Seq {
			return files[i].path < files[j].path
		}
		return files[i].record.Seq < files[j].record.Seq
	})
	return files, nil
}

func readSpoolFiles(dir string) ([]spoolFile, error) {
	entries, err := readSpoolEntries(dir)
	if err != nil {
		return nil, err
	}
	files := make([]spoolFile, 0, len(entries))
	for _, file := range entries {
		if file.parseErr != "" {
			return nil, fmt.Errorf("read spool record %s: %s", filepath.Base(file.path), file.parseErr)
		}
		files = append(files, file)
	}
	return files, nil
}

func syncSpoolDir(dir string) error {
	f, err := os.Open(dir)
	if err != nil {
		return err
	}
	defer f.Close()
	return f.Sync()
}

func renameSpoolFile(from, to string) error {
	if err := os.Rename(from, to); err != nil {
		return err
	}
	if err := syncSpoolDir(filepath.Dir(to)); err != nil {
		return err
	}
	if filepath.Dir(from) != filepath.Dir(to) {
		return syncSpoolDir(filepath.Dir(from))
	}
	return nil
}

func moveSpoolBad(dir string, file spoolFile) error {
	badDir := filepath.Join(spoolPath(dir), spoolBadDir)
	if err := os.MkdirAll(badDir, 0o700); err != nil {
		return err
	}
	name := filepath.Base(file.path)
	to := filepath.Join(badDir, name)
	for suffix := 1; ; suffix++ {
		if _, err := os.Lstat(to); errors.Is(err, os.ErrNotExist) {
			break
		} else if err != nil {
			return err
		}
		name = strings.TrimSuffix(filepath.Base(file.path), ".json") + "-bad-" + strconv.Itoa(suffix) + ".json"
		to = filepath.Join(badDir, name)
	}
	return renameSpoolFile(file.path, to)
}

func quarantineBadSpoolEntries(dir string, files []spoolFile) ([]spoolFile, error) {
	valid := make([]spoolFile, 0, len(files))
	for _, file := range files {
		if file.parseErr == "" {
			valid = append(valid, file)
			continue
		}
		if err := moveSpoolBad(dir, file); err != nil {
			return nil, fmt.Errorf("move bad spool record %s: %w", filepath.Base(file.path), err)
		}
	}
	return valid, nil
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
	for _, folder := range []string{filepath.Join(spoolPath(dir), spoolQueueDir), filepath.Join(spoolPath(dir), spoolFailDir), filepath.Join(spoolPath(dir), spoolBadDir)} {
		entries, err := os.ReadDir(folder)
		if err != nil {
			continue
		}
		for _, entry := range entries {
			seq := spoolSeqFromName(entry.Name())
			if seq > maxSeq {
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
	if err := os.Rename(tmpPath, filepath.Join(dir, name)); err != nil {
		return err
	}
	return syncSpoolDir(dir)
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
	entries, err := readSpoolEntries(queueDir)
	if err != nil {
		return 0, err
	}
	files, err := quarantineBadSpoolEntries(dir, entries)
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
	record := spoolRecord{Version: 1, RequestKey: req.RequestKey, Request: req, QueuedAt: spoolNow().UTC().Format(time.RFC3339), Document: document}
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
		return countSpoolFiles(queueDir), false, errSpoolBusy
	}
	defer unlockSpool(lock)
	entries, err := readSpoolEntries(queueDir)
	if err != nil {
		return 0, false, err
	}
	files, err := quarantineBadSpoolEntries(dir, entries)
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
	record := spoolRecord{Version: 1, RequestKey: req.RequestKey, Request: req, QueuedAt: spoolNow().UTC().Format(time.RFC3339)}
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
	root := spoolPath(dir)
	empty := map[string]any{"queued": []spoolListItem{}, "refused": []spoolListItem{}, "bad": []spoolListItem{}}
	if _, err := os.Stat(root); errors.Is(err, os.ErrNotExist) {
		return empty, nil
	} else if err != nil {
		return nil, err
	}
	lock, ok, err := lockSpool(dir, true)
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, errSpoolBusy
	}
	queue, err := readSpoolEntries(filepath.Join(root, spoolQueueDir))
	if err == nil {
		queue, err = quarantineBadSpoolEntries(dir, queue)
	}
	refused, refusedErr := readSpoolEntries(filepath.Join(root, spoolFailDir))
	if refusedErr == nil {
		refused, refusedErr = quarantineBadSpoolEntries(dir, refused)
	}
	unlockSpool(lock)
	if err != nil {
		return nil, err
	}
	if refusedErr != nil {
		return nil, refusedErr
	}
	bad, err := readSpoolEntries(filepath.Join(root, spoolBadDir))
	if err != nil {
		return nil, err
	}
	toItems := func(files []spoolFile, kind string) []spoolListItem {
		items := make([]spoolListItem, 0, len(files))
		for _, file := range files {
			r := file.record
			item := spoolListItem{Seq: r.Seq}
			if kind == "bad" {
				item.Kind = "bad"
				if item.Seq <= 0 {
					item.Seq = spoolSeqFromName(filepath.Base(file.path))
				}
				item.Error = file.parseErr
				items = append(items, item)
				continue
			}
			age := time.Since(spoolTime(r.QueuedAt))
			if age < 0 {
				age = 0
			}
			command, _ := rpcCommand(r.Request.Argv)
			item.Age, item.Command, item.Task = age.Round(time.Second).String(), command, spoolTaskID(r.Request)
			if kind == "refused" {
				item.Error = r.Error
			}
			items = append(items, item)
		}
		return items
	}
	return map[string]any{"queued": toItems(queue, ""), "refused": toItems(refused, "refused"), "bad": toItems(bad, "bad")}, nil
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
	for _, folder := range []string{spoolQueueDir, spoolFailDir, spoolBadDir} {
		folderPath := filepath.Join(spoolPath(dir), folder)
		entries, err := os.ReadDir(folderPath)
		if err != nil {
			if errors.Is(err, os.ErrNotExist) {
				continue
			}
			return err
		}
		for _, entry := range entries {
			if entry.IsDir() || !strings.HasSuffix(entry.Name(), ".json") || spoolSeqFromName(entry.Name()) != seq {
				continue
			}
			path := filepath.Join(folderPath, entry.Name())
			if err := os.Remove(path); err != nil {
				return err
			}
			if folder == spoolBadDir {
				_ = os.Remove(path + ".shown")
			}
			return syncSpoolDir(folderPath)
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
	return renameSpoolFile(file.path, filepath.Join(spoolPath(dir), spoolFailDir, name))
}

func spoolQueueHead(dir string) (spoolFile, bool, error) {
	queueDir := filepath.Join(spoolPath(dir), spoolQueueDir)
	lock, ok, err := lockSpool(dir, true)
	if err != nil || !ok {
		return spoolFile{}, false, err
	}
	defer unlockSpool(lock)
	entries, err := readSpoolEntries(queueDir)
	if err != nil {
		return spoolFile{}, false, err
	}
	files, err := quarantineBadSpoolEntries(dir, entries)
	if err != nil {
		return spoolFile{}, false, err
	}
	if len(files) == 0 {
		return spoolFile{}, false, nil
	}
	return files[0], true, nil
}

func removeQueuedSpoolFile(dir string, file spoolFile) error {
	lock, ok, err := lockSpool(dir, true)
	if err != nil || !ok {
		return err
	}
	defer unlockSpool(lock)
	if err := os.Remove(file.path); err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}
	return syncSpoolDir(filepath.Dir(file.path))
}

func moveQueuedSpoolRefused(dir string, file spoolFile, exit int, message string) error {
	lock, ok, err := lockSpool(dir, true)
	if err != nil || !ok {
		return err
	}
	defer unlockSpool(lock)
	if _, err := os.Stat(file.path); errors.Is(err, os.ErrNotExist) {
		return nil
	} else if err != nil {
		return err
	}
	return moveSpoolRefused(dir, file, exit, message)
}

func spoolTransportError(err *exitErr) bool {
	if err == nil || err.kind == "transport" {
		return err != nil
	}
	const prefix = "server answered "
	if !strings.HasPrefix(err.msg, prefix) {
		return false
	}
	status, _, _ := strings.Cut(strings.TrimPrefix(err.msg, prefix), ":")
	code, parseErr := strconv.Atoi(status)
	return parseErr == nil && (code == http.StatusUnauthorized || code == http.StatusForbidden ||
		code == http.StatusRequestTimeout || code == http.StatusTooManyRequests || code >= 500)
}

func sendSpool(dir, raw string, log *daemonLog) (int, error) {
	if countSpoolFiles(filepath.Join(spoolPath(dir), spoolQueueDir)) == 0 {
		return 0, nil
	}
	sendLock, ok, err := lockSpoolSender(dir)
	if err != nil || !ok {
		return 0, err
	}
	defer unlockSpool(sendLock)
	file, hasFile, err := spoolQueueHead(dir)
	if err != nil || !hasFile {
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
	for hasFile {
		req := file.record.Request
		req.QueuedAt = file.record.QueuedAt
		age := spoolNow().Sub(spoolTime(file.record.QueuedAt))
		if age < 0 {
			age = 0
		}
		req.QueuedAgeMS = age.Milliseconds()
		rep, callErr, noReply := cl.callStored(context.Background(), req)
		if callErr != nil {
			if noReply || spoolTransportError(callErr) {
				return sent, nil
			}
			if err := moveQueuedSpoolRefused(dir, file, callErr.code, callErr.msg); err != nil {
				return sent, err
			}
			file, hasFile, err = spoolQueueHead(dir)
			if err != nil {
				return sent, err
			}
			continue
		}
		if rep.Exit == exitHerdr && strings.Contains(rep.Stdout, "outcome unknown (still running") {
			return sent, nil
		}
		if reqName, _ := rpcCommand(req.Argv); reqName == "_hook" && rep.Exit == exitOK && strings.TrimSpace(rep.Stdout) == "expired" {
			if err := removeQueuedSpoolFile(dir, file); err != nil {
				return sent, err
			}
			file, hasFile, err = spoolQueueHead(dir)
			if err != nil {
				return sent, err
			}
			continue
		}
		if rep.Exit != exitOK {
			if err := moveQueuedSpoolRefused(dir, file, rep.Exit, rpcReplyError(rep)); err != nil {
				return sent, err
			}
			file, hasFile, err = spoolQueueHead(dir)
			if err != nil {
				return sent, err
			}
			continue
		}
		wants := []rpcDocWant{}
		if rep.Upload != nil {
			wants = *rep.Upload
		}
		if err := clientUploadSpoolDocs(cl, wants, req.Cwd, req.Env, file.record.Document); err != nil {
			return sent, err
		}
		if err := removeQueuedSpoolFile(dir, file); err != nil {
			return sent, err
		}
		sent++
		file, hasFile, err = spoolQueueHead(dir)
		if err != nil {
			return sent, err
		}
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
	lock, ok, err := lockSpool(dir, true)
	if err != nil || !ok {
		if log != nil && err != nil {
			log.logf("spool refused lock failed: %v", err)
		}
		return
	}
	entries, err := readSpoolEntries(filepath.Join(spoolPath(dir), spoolFailDir))
	if err == nil {
		entries, err = quarantineBadSpoolEntries(dir, entries)
	}
	unlockSpool(lock)
	if err != nil {
		if log != nil {
			log.logf("spool refused read failed: %v", err)
		}
		return
	}
	for _, file := range entries {
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

func notifySpoolBad(dir, sock string, log *daemonLog) {
	lock, ok, err := lockSpool(dir, true)
	if err != nil || !ok {
		if log != nil && err != nil {
			log.logf("spool bad lock failed: %v", err)
		}
		return
	}
	files, err := readSpoolEntries(filepath.Join(spoolPath(dir), spoolBadDir))
	unlockSpool(lock)
	if err != nil {
		if log != nil {
			log.logf("spool bad read failed: %v", err)
		}
		return
	}
	for _, file := range files {
		marker := file.path + ".shown"
		if _, err := os.Stat(marker); err == nil {
			continue
		}
		body := fmt.Sprintf("taskr: queued record %s is bad: %s", filepath.Base(file.path), file.parseErr)
		if err := herdrRun(sock, "notification", "show", "taskr: queued record is bad",
			"--body", truncate(body, notifyBodyMax), "--sound", "request"); err != nil {
			if log != nil {
				log.logf("notify bad spool record %s failed: %v", filepath.Base(file.path), err)
			}
			continue
		}
		lock, ok, err := lockSpool(dir, true)
		if err != nil || !ok {
			if log != nil && err != nil {
				log.logf("spool bad marker lock failed: %v", err)
			}
			continue
		}
		if _, err := os.Stat(file.path); err == nil {
			if markerFile, err := os.OpenFile(marker, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600); err == nil {
				_ = markerFile.Close()
				if err := syncSpoolDir(filepath.Dir(file.path)); err != nil && log != nil {
					log.logf("spool bad marker sync failed: %v", err)
				}
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
		queued, refused, _ := spoolCounts(dir)
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
