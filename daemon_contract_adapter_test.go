package main

// These cases now run the selected daemon process, including its PID and signals.
func init() {
	for _, name := range []string{
		"TestDaemonSubscriptionShapeAndWriteSilence", "TestDaemonCoalescesBurst",
		"TestDaemonReconnects", "TestDaemonExitsWhenSocketRemoved", "TestDaemonLockRefusesSecondInstance",
		"TestDaemonPerPaneSubscriptions", "TestDaemonEmptyPaneSetSubscribes", "TestDaemonStreamErrorResubscribes",
		"TestDaemonStreamErrorNoSpin", "TestDaemonSetupErrorWithID",
		"TestDaemonStatusStale", "TestDashboardBindConflictDaemonCarriesOn", "TestDashboardOffAndRefusedInDaemon",
	} {
		contractNetConverted[name] = true
	}
}
