package main

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"syscall"
	"time"
	"unicode"

	"github.com/charmbracelet/x/ansi"
	"golang.org/x/term"
)

type watchResizeKey struct{}

func glanceWatchRequested(args []string) bool {
	for _, arg := range args {
		if arg == "--" {
			return false
		}
		if arg == "--watch" || arg == "-watch" {
			return true
		}
		name, value, _ := strings.Cut(arg, "=")
		if name == "--watch" || name == "-watch" {
			watch, _ := strconv.ParseBool(value)
			return watch
		}
	}
	return false
}

func watchText(s string) string {
	return strings.TrimSpace(strings.Map(func(r rune) rune {
		if unicode.IsControl(r) {
			return ' '
		}
		return r
	}, ansi.Strip(s)))
}

func watchAge(d time.Duration) string {
	for _, u := range []struct {
		limit, unit time.Duration
		suffix      string
	}{
		{time.Minute, time.Second, "s"}, {time.Hour, time.Minute, "m"}, {48 * time.Hour, time.Hour, "h"},
	} {
		if d < u.limit {
			return fmt.Sprintf("%d%s", max(0, d/u.unit), u.suffix)
		}
	}
	return fmt.Sprintf("%dd", d/(24*time.Hour))
}

func renderGlance(v *glanceView, width, height int, age time.Duration, fetchErr string, color bool, now time.Time) []string {
	if width <= 0 || height <= 0 {
		return nil
	}
	line := func(s, tint string) string {
		s = ansi.Truncate(s, width, "…")
		if color && tint != "" {
			return "\x1b[" + tint + "m" + s + "\x1b[0m"
		}
		return s
	}
	ageOf := func(ms int64) string { return watchAge(time.Duration(ms)*time.Millisecond + age) }
	left, right, tint := "taskr · "+now.Local().Format("15:04"), "no data", "33"
	if fetchErr != "" {
		if v == nil {
			right += ": " + watchText(fetchErr)
		} else {
			right = "stale " + watchAge(age) + ": " + watchText(fetchErr)
		}
	} else if v != nil {
		switch v.Verdict {
		case "rolling":
			right, tint = "✓ all rolling", "32"
		case "needs_you":
			right, tint = fmt.Sprintf("%d need you", len(v.NeedsYou)), "31"
		case "attention":
			right = fmt.Sprintf("%d to check", len(v.Attention)+v.Quiet.WithBacklog)
		default:
			right = "? unknown"
		}
	}
	fullLeft := left
	if v != nil {
		fullLeft += " · " + watchAge(age)
	}
	if ansi.StringWidth(fullLeft)+ansi.StringWidth(right)+1 > width {
		fullLeft = left
	}
	if ansi.StringWidth(fullLeft)+ansi.StringWidth(right)+1 > width {
		fullLeft = now.Local().Format("15:04")
	}
	if ansi.StringWidth(fullLeft)+ansi.StringWidth(right)+1 > width {
		fullLeft = ""
	}
	right = ansi.Truncate(right, width, "…")
	header := fullLeft + strings.Repeat(" ", width-ansi.StringWidth(fullLeft)-ansi.StringWidth(right)) + line(right, tint)
	rule := strings.Repeat("─", width)
	if v == nil {
		return []string{header, rule}[:min(2, height)]
	}
	groups := [3][][]string{}
	for _, n := range v.NeedsYou {
		first, text := "» "+watchText(n.Campaign)+"  "+ageOf(n.AgeMS), n.Text
		if n.Kind == "owner_todo" {
			first = "! " + watchText(n.Campaign) + "  " + ageOf(n.AgeMS)
			text = ""
			if len(n.Items) > 0 {
				text = watchText(n.Items[0])
			}
			if len(n.Items) > 1 {
				text += fmt.Sprintf("  (+%d more)", len(n.Items)-1)
			}
		} else {
			if n.Blocking != nil && *n.Blocking {
				first += "  BLOCKING"
			}
			if n.PaneID != "" {
				first += "  → " + watchText(n.PaneID)
			}
			if len(n.Also) > 0 {
				first += "  " + watchText(n.Also[0])
			}
		}
		groups[0] = append(groups[0], []string{line(first, "31"), line("  "+watchText(text), "")})
	}
	for _, a := range v.Attention {
		sym, word := "?", "unknown"
		switch a.Kind {
		case "lane_failed":
			sym, word = "✗", "failed"
		case "lane_blocked":
			sym, word = "!", "blocked"
		case "lead_gone":
			sym, word = "✗", "lead gone"
		case "lead_blocked":
			sym, word = "!", "lead blocked"
		case "lead_unknown":
			word = "lead unknown"
		case "owner_unclear":
			word = "owner unclear"
		case "lane_missing":
			word = "missing"
		case "results_waiting":
			sym, word = "⌛", "waiting"
		case "host_stale":
			sym, word = "⚠", "stale"
		case "daemon_unhealthy":
			sym, word = "⚠", "daemon"
		}
		name := strings.Trim(watchText(a.Campaign)+"/"+watchText(a.Lane), "/")
		if name == "" {
			name = firstNonEmpty(watchText(a.Host), "hub")
		}
		when := ""
		if a.Since != "" {
			when = " " + ageOf(a.AgeMS)
		} else if a.Kind == "lead_unknown" {
			when = " never"
		}
		first := fmt.Sprintf("%s %s  %s%s", sym, name, word, when)
		if a.Kind == "results_waiting" {
			first = fmt.Sprintf("%s %s: %d waiting%s", sym, watchText(a.Recipient), a.Count, when)
		}
		groups[1] = append(groups[1], []string{line(first, "33"), line("  "+watchText(a.Text), "")})
	}
	pad := func(s string, n int) string {
		s = ansi.Truncate(watchText(s), n, "…")
		return s + strings.Repeat(" ", n-ansi.StringWidth(s))
	}
	for _, c := range v.Campaigns {
		sym, text, ms := "○", "", c.ActivityAgeMS
		if c.Lanes.Working > 0 {
			sym = "●"
		}
		if c.Last != nil {
			text, ms = c.Last.Text, c.Last.AgeMS
		}
		groups[2] = append(groups[2], []string{line(sym+" "+pad(c.Name, 18)+" "+pad(fmt.Sprintf("%d/%d", c.Lanes.Working, c.Lanes.Open), 6)+" "+pad(ageOf(ms), 4)+" "+watchText(text), "")})
	}
	quiet := ""
	if v.Quiet.Count > 0 {
		base := ansi.Truncate(fmt.Sprintf("· %d quiet", v.Quiet.Count), width, "…")
		quiet = line(base, "2")
		if v.Quiet.WithBacklog > 0 {
			suffix := ansi.Truncate(fmt.Sprintf(" (%d with backlog)", v.Quiet.WithBacklog), max(0, width-ansi.StringWidth(base)), "…")
			quiet += line(suffix, "33")
		}
	}
	keep := [3]int{len(groups[0]), len(groups[1]), len(groups[2])}
	labels := [3]string{"need you", "to check", "campaigns"}
	compose := func() []string {
		rows := []string{header, rule}
		for g, items := range groups {
			for _, item := range items[:keep[g]] {
				rows = append(rows, item...)
			}
			if n := len(items) - keep[g]; n > 0 {
				prefix := "+"
				if g == 2 {
					prefix = "· +"
				}
				rows = append(rows, line(fmt.Sprintf("%s%d more %s", prefix, n, labels[g]), []string{"31", "33", ""}[g]))
			}
			follows := quiet != ""
			for j := g + 1; j < 3; j++ {
				follows = follows || len(groups[j]) > 0
			}
			if g < 2 && len(items) > 0 && follows {
				rows = append(rows, rule)
			}
		}
		if quiet != "" {
			rows = append(rows, quiet)
		}
		return rows
	}
	rows := compose()
	for len(rows) > height {
		dropped := false
		for g := 2; g >= 0; g-- {
			before := keep[g]
			for keep[g] > 0 {
				keep[g]--
				if next := compose(); len(next) < len(rows) {
					rows, dropped = next, true
					break
				}
			}
			if dropped {
				break
			}
			keep[g] = before
		}
		if !dropped {
			break
		}
	}
	// Counts survive when the minimal composition fits; tiny heights shed rules,
	// then bottom rows, preserving the header verdict and higher-priority counts.
	for i := len(rows) - 1; i > 0 && len(rows) > height; i-- {
		if rows[i] == rule {
			rows = append(rows[:i], rows[i+1:]...)
		}
	}
	return rows[:min(height, len(rows))]
}

func runWatch(cx context.Context, fetch func(context.Context) (*glanceView, error), in io.Reader, out io.Writer, size func() (int, int), every time.Duration, now func() time.Time) error {
	cx, cancel := context.WithCancel(cx)
	defer io.WriteString(out, "\x1b[?25h\x1b[?1049l")
	defer cancel()
	if _, err := io.WriteString(out, "\x1b[?1049h\x1b[?25l"); err != nil {
		return err
	}
	quit := make(chan struct{})
	go func() {
		var b [1]byte
		for {
			n, err := in.Read(b[:])
			if err != nil || n > 0 && (b[0] == 'q' || b[0] == 'Q' || b[0] == 3) {
				close(quit)
				return
			}
		}
	}()
	type result struct {
		v   *glanceView
		err error
	}
	results := make(chan result, 1)
	tick := time.NewTicker(time.Second)
	defer tick.Stop()
	next := time.NewTimer(0)
	defer next.Stop()
	resize, _ := cx.Value(watchResizeKey{}).(<-chan os.Signal)
	var v *glanceView
	var at time.Time
	fetchErr := ""
	_, noColor := os.LookupEnv("NO_COLOR")
	draw := func() error {
		age := time.Duration(0)
		if v != nil {
			age = max(0, now().Sub(at))
		}
		w, h := size()
		rows := renderGlance(v, w, h, age, fetchErr, !noColor && os.Getenv("TERM") != "dumb", now())
		var body strings.Builder
		for i := 0; i < h; i++ {
			fmt.Fprintf(&body, "\x1b[%d;1H\x1b[2K", i+1)
			if i < len(rows) {
				body.WriteString(rows[i])
			}
		}
		_, err := io.WriteString(out, body.String())
		return err
	}
	if err := draw(); err != nil {
		return err
	}
	for {
		select {
		case <-cx.Done():
			return nil
		case <-quit:
			return nil
		case <-next.C:
			go func() {
				var r result
				defer func() {
					if p := recover(); p != nil {
						r.err = fmt.Errorf("fetch panic: %v", p)
					}
					results <- r
				}()
				r.v, r.err = fetch(cx)
			}()
			continue
		case r := <-results:
			if r.err != nil {
				fetchErr = r.err.Error()
			} else {
				v, at, fetchErr = r.v, now(), ""
			}
			next.Reset(every)
		case <-tick.C:
		case <-resize:
		}
		if err := draw(); err != nil {
			return err
		}
	}
}

func watchGlance(c *ctx, every time.Duration) (any, int, error) {
	var fetch func(context.Context) (*glanceView, error)
	if c.client {
		cl, e := newRPCClient(c.server)
		if e != nil {
			return nil, 0, e
		}
		cwd, err := os.Getwd()
		if err != nil {
			return nil, 0, usageErr("current directory: %v", err)
		}
		fetch = func(cx context.Context) (*glanceView, error) {
			cx, cancel := context.WithTimeout(cx, min(every, 10*time.Second))
			defer cancel()
			rep, e, _, _ := cl.callOnce(cx, []string{"--json", "glance"}, cwd, newRequestKey(), nil)
			if e != nil {
				return nil, e
			}
			if rep.Exit != exitOK {
				return nil, fmt.Errorf("glance: %s", strings.TrimSpace(rep.Stdout))
			}
			var v glanceView
			err := json.Unmarshal([]byte(rep.Stdout), &v)
			return &v, err
		}
	} else {
		db, err := openDB(c)
		if err != nil {
			return nil, 1, &exitErr{1, "watch", err.Error()}
		}
		defer closeDB(c, db)
		fetch = func(context.Context) (*glanceView, error) { return readGlance(db, time.Now()) }
	}
	c.lines = true
	file, ok := c.out.(*os.File)
	if !ok || !term.IsTerminal(int(file.Fd())) || !term.IsTerminal(int(os.Stdin.Fd())) || os.Getenv("TERM") == "dumb" {
		v, err := fetch(context.Background())
		if err == nil {
			_, err = fmt.Fprintln(c.out, strings.Join(renderGlance(v, 80, int(^uint(0)>>1), 0, "", false, time.Now()), "\n"))
		}
		if err != nil {
			return nil, 1, &exitErr{1, "watch", err.Error()}
		}
		return nil, exitOK, nil
	}
	cx, cancel := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM, syscall.SIGHUP)
	defer cancel()
	state, err := term.MakeRaw(int(os.Stdin.Fd()))
	if err != nil {
		return nil, 1, &exitErr{1, "watch", err.Error()}
	}
	defer term.Restore(int(os.Stdin.Fd()), state)
	resize := make(chan os.Signal, 1)
	signal.Notify(resize, syscall.SIGWINCH)
	defer signal.Stop(resize)
	cx = context.WithValue(cx, watchResizeKey{}, (<-chan os.Signal)(resize))
	size := func() (int, int) {
		w, h, e := term.GetSize(int(file.Fd()))
		if e != nil {
			return 80, 24
		}
		return w, h
	}
	if err := runWatch(cx, fetch, os.Stdin, c.out, size, every, time.Now); err != nil {
		return nil, 1, &exitErr{1, "watch", err.Error()}
	}
	return nil, exitOK, nil
}
