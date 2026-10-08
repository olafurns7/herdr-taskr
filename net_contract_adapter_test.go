package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// Forward only synthetic test knobs to the selected executable. The ordinary
// Go runner keeps its original injected functions; product binaries ignore them.
func contractNetEnv(args []string, getenv func(string) string) func(string) string {
	return func(k string) string {
		switch k {
		case "TASKR_CONTRACT_TAILNET":
			return os.Getenv(k)
		case "TASKR_CONTRACT_RETRY_MS":
			return fmt.Sprint(rpcRetryWindow(args).Milliseconds())
		case "TASKR_FROZEN_NOW":
			// Only forward an injected spool clock, never freeze ordinary retry deadlines.
			if name, _ := rpcCommand(args); name == "spool" {
				if delta := spoolNow().Sub(clockNow()); delta > time.Second || delta < -time.Second {
					return spoolNow().UTC().Format(time.RFC3339Nano)
				}
			}
		}
		return getenv(k)
	}
}

// Go constructs synthetic records; sending always belongs to the target client.
func contractNetSendSpool(t *testing.T, dir, raw string, log *daemonLog) (int, error) {
	t.Helper()
	if os.Getenv("TASKR_BIN") == "" {
		return sendSpool(dir, raw, log)
	}
	home := t.TempDir()
	parent := filepath.Join(home, ".local", "state")
	if err := os.MkdirAll(parent, 0700); err != nil {
		return 0, err
	}
	if err := os.Symlink(dir, filepath.Join(parent, "taskr")); err != nil {
		return 0, err
	}
	if err := os.WriteFile(filepath.Join(dir, serverURLFile), []byte(raw+"\n"), 0600); err != nil {
		return 0, err
	}
	var out, stderr bytes.Buffer
	code := contractCLIMain(t, []string{"--json", "spool", "send"}, clientEnv(home, nil), &out, &stderr)
	if code != 0 {
		var fields struct{ Error, Kind string }
		_ = json.Unmarshal([]byte(strings.TrimSpace(out.String())), &fields)
		return 0, &exitErr{code, fields.Kind, fields.Error}
	}
	var rep struct{ Sent int }
	if err := json.Unmarshal(out.Bytes(), &rep); err != nil {
		return 0, err
	}
	return rep.Sent, nil
}

// These tests invoke the executable for client behavior; direct RPC calls test
// the Go hub, and direct queue helpers only seed on-disk synthetic fixtures.
var contractNetConverted = map[string]bool{
	"TestSpoolConcurrentProcessesAllocateOrderedSequences": true,
	"TestHarnessRetry":                            true,
	"TestGlanceWatchFlagsAndRPCRefusal":           true,
	"TestRetryHookDeadline":                       true,
	"TestRPCModes":                                true,
	"TestHarnessRPCDuplicateFlags":                true,
	"TestSpoolHookTimeoutWritesBeforeProcessExit": true,
	"TestSpoolHookLargeQueueWritesBeforeExit":     true,

	"TestCloseOrphanWarningRPC":                         true,
	"TestGlanceRPCFresh":                                true,
	"TestHookClientRPCNormalizedFieldsAndHostCheck":     true,
	"TestOwnerNoteClientRejectsBeforeSpoolOrRPC":        true,
	"TestSpoolOwnerNoteAndFreshRPCNotes":                true,
	"TestSearchRPC":                                     true,
	"TestSpoolAttemptHonorsRetryWindow":                 true,
	"TestSpoolNonRecordAndServerRefusalDoNotQueue":      true,
	"TestSpoolOutcomeUnknownKeepsQueue":                 true,
	"TestSpoolOutcomeUnknownStaysStuckThenRefuses":      true,
	"TestSpoolOutcomeUnknownStoredResultClearsQueue":    true,
	"TestSpoolForbiddenHeadStaysQueuedAndNotifiesOnce":  true,
	"TestSpoolTransportFailureNeverStartsStuckClock":    true,
	"TestSpoolStallAgeUsesClientClock":                  true,
	"TestSpoolOldStallExpiresAndOldSessionStartRecords": true,
	"TestSpoolConcurrentSendersUseSendLock":             true,
	"TestSpoolBadFilesQuarantineAndNotify":              true,

	"TestHarnessHostIdentity":                          true,
	"TestHarnessWaitAfterAdopt":                        true,
	"TestHarnessRPCPaths":                              true,
	"TestHarnessRPCWaits":                              true,
	"TestHarnessRPCDroppedConnection":                  true,
	"TestRetryWaitServerReturns":                       true,
	"TestRetryWriteLostReply":                          true,
	"TestRetryDeadline":                                true,
	"TestRetryWaitLostReplyTimeout":                    true,
	"TestRetryNonRetryable":                            true,
	"TestRetryRefusalCutOff":                           true,
	"TestRetryHealthyWaitTimeout":                      true,
	"TestSpoolRecordCommandsQueueAfterTransportWindow": true,
	"TestSpoolQueuedBehindSaysWhy":                     true,
	"TestSpoolRecordQueuesWhileSenderOwnsSendLock":     true,
	"TestSpoolQueuedOrderAndManualSend":                true,
	"TestSpoolAppliedRequestIsNotAppliedTwice":         true,
	"TestSpoolTransportErrorStopsInSequence":           true,
	"TestSpoolRefusalMovesOnAndNotifiesOnce":           true,
	"TestSpoolReadyReportCapturedAtQueueTime":          true,
	"TestSpoolDoneUploadsServerRequestedReport":        true,
	"TestSpoolUploadFailureKeepsReady":                 true,
	"TestSpoolUploadRefusalDropsAndContinues":          true,
	"TestSpoolUpload429KeepsThenSendsInOrder":          true,
	"TestSpoolQueueWriteFailurePrintsRetry":            true,
	"TestSpoolHTTPProxyErrorsKeepOrRefuse":             true,
	"TestSpoolHTTPErrorUsesJSONErrorAndLimitsBody":     true,
	"TestSpoolRemoveBadFileByName":                     true,
	"TestSpoolProxyFiveXXQueuesRecordOnly":             true,
	"TestSpoolOlderServerIgnoresQueuedAt":              true,
	"TestSpoolQueuedAtCoversEveryRecordEventPath":      true,
	"TestSpoolQueuedDoneTimeAndOrdinaryDone":           true,
	"TestSpoolListRemoveAndStatusStayLocal":            true,
	"TestSpoolFileBound":                               true,
}

func contractNetR2(t *testing.T, name string, check func(*testing.T)) {
	t.Helper()
	t.Run(name, func(t *testing.T) {
		if os.Getenv("TASKR_BIN") != "" {
			t.Skip("R2: daemon observation or logging; client CLI behavior is checked separately")
		}
		check(t)
	})
}

// Hub-only conversions require the external server seam; retain the R1 client registry.
func init() {
	if os.Getenv("TASKR_HUB_BIN") != "" {
		contractNetConverted["TestHarnessRPCAdmission"] = true
	}
}
