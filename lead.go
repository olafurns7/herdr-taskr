package main

import (
	"database/sql"
)

// Lead liveness: an open root (a campaign lead) has no launch, so its pane's
// agent status is stored on the task itself. The host that owns the pane
// records it from the listing its liveness pass already fetched. A root has
// no parent inbox: an observation writes no event and no hint.

// openRoot filters the roots that have a lead to observe or report.
const openRoot = `parent_id is null and status not in ('closed', 'planned')`

const leadListedKey = "lead_listed_at"

// leadListedFresh is how long a successful hub listing stays trusted: longer
// than the daemon's fallback pass, so a quiet hub's leads do not read
// unknown between scheduled listings.
var leadListedFresh = daemonFallback + heartbeatFresh

// lead is an open root's pane on one host and its stored observation.
type lead struct {
	TaskID  int64
	Pane    string
	Status  sql.NullString
	Present sql.NullBool
}

// leadsOn lists host's open roots that have a pane: pane ids repeat across
// hosts, so a host reads only its own.
func leadsOn(q queryer, host sql.NullString) ([]lead, error) {
	rows, err := q.Query(`select id, pane_id, lead_status, lead_present from tasks
		where `+openRoot+` and pane_id is not null and machine is ? order by id`, host)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var ls []lead
	for rows.Next() {
		var l lead
		if err := rows.Scan(&l.TaskID, &l.Pane, &l.Status, &l.Present); err != nil {
			return nil, err
		}
		ls = append(ls, l)
	}
	return ls, rows.Err()
}

// observeLeads writes the changed observations of ls against one agent
// listing of host. lead_observed_at is the time of the last change. Each
// write holds only while the root is still open at that pane on that host,
// so a pass that raced an adopt to a different pane or host cannot overwrite
// the cleared observation.
func observeLeads(db *sql.DB, host sql.NullString, ls []lead, agents map[string]agentObs) error {
	const still = ` where id = ? and pane_id = ? and machine is ? and ` + openRoot
	for _, l := range ls {
		a, ok := agents[l.Pane]
		var err error
		switch {
		case ok && (!l.Status.Valid || l.Status.String != a.Status || !l.Present.Valid || !l.Present.Bool):
			_, err = db.Exec(`update tasks set lead_status = ?, lead_present = 1, lead_observed_at = ?`+still,
				a.Status, now(), l.TaskID, l.Pane, host)
		case !ok && (!l.Present.Valid || l.Present.Bool):
			_, err = db.Exec(`update tasks set lead_present = 0, lead_observed_at = ?`+still,
				now(), l.TaskID, l.Pane, host)
		}
		if err != nil {
			return err
		}
	}
	return nil
}

// clearLead is the assignment list that forgets a root's observation; a
// rebinding to another pane applies it in its own transaction.
const clearLead = `lead_status = null, lead_present = null, lead_observed_at = null`

// leadObs is a root's lead as last observed. Status is working, idle, done,
// blocked, unknown or gone; ObservedAt is the time it last changed.
type leadObs struct {
	Status     string
	ObservedAt string
}

// leadObservations reports every open root's lead. A lead is unknown when
// the root has no pane, was never observed, or its host's daemon is not
// fresh (a client host's heartbeat; for the server host, the hub daemon's
// heartbeat and its last successful listing):
// a stored status nobody is refreshing is not a claim. It is gone when its
// host's last listing did not have the pane.
func leadObservations(q queryer) (map[int64]leadObs, error) {
	hub, _, _, err := daemonState(q)
	if err != nil {
		return nil, err
	}
	listedAt, listed, err := getMeta(q, leadListedKey)
	if err != nil {
		return nil, err
	}
	hubLive := hub == "fresh" && listed && clockNow().Sub(parseTime(listedAt)) < leadListedFresh
	type row struct {
		id            int64
		pane, machine sql.NullString
		status, at    sql.NullString
		present       sql.NullBool
	}
	rows, err := q.Query(`select id, pane_id, machine, lead_status, lead_present, lead_observed_at
		from tasks where ` + openRoot + ` order by id`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var rs []row
	for rows.Next() {
		var r row
		if err := rows.Scan(&r.id, &r.pane, &r.machine, &r.status, &r.present, &r.at); err != nil {
			return nil, err
		}
		rs = append(rs, r)
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}
	rows.Close()
	fresh := map[string]bool{}
	out := make(map[int64]leadObs, len(rs))
	for _, r := range rs {
		live := hubLive
		if r.machine.Valid {
			f, seen := fresh[r.machine.String]
			if !seen {
				if f, err = hostFresh(q, r.machine.String); err != nil {
					return nil, err
				}
				fresh[r.machine.String] = f
			}
			live = f
		}
		o := leadObs{Status: "unknown", ObservedAt: r.at.String}
		switch {
		case !r.pane.Valid || !r.present.Valid || !live:
		case !r.present.Bool:
			o.Status = "gone"
		default:
			switch r.status.String {
			case "working", "idle", "done", "blocked":
				o.Status = r.status.String
			}
		}
		out[r.id] = o
	}
	return out, nil
}
