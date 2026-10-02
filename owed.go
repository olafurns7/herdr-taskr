package main

import "database/sql"

// owes reports whether the launch still owes its parent a result: a prompt
// on the launch is newer than the launch's latest done or fail. A mid-brief
// ready does not end the debt, because workers write ready per slice and go
// on; a done or fail does. Shared with I1, I3 and the hook.
func owes(q queryer, launch int64, newestPrompt ...*int64) (bool, error) {
	var prompt, report sql.NullInt64
	err := q.QueryRow(`select
		(select max(id) from events where launch_id = ? and kind = 'prompt'),
		(select max(id) from events where launch_id = ? and kind in ('done', 'fail'))`,
		launch, launch).Scan(&prompt, &report)
	if err != nil {
		return false, err
	}
	if len(newestPrompt) > 0 && newestPrompt[0] != nil {
		*newestPrompt[0] = 0
		if prompt.Valid {
			*newestPrompt[0] = prompt.Int64
		}
	}
	return prompt.Valid && (!report.Valid || prompt.Int64 > report.Int64), nil
}
