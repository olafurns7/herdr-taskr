package main

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"reflect"
	"testing"
)

const (
	setShared   = "pane|report-metadata|" + sharedPane + "|--source|taskr|--token|taskr_owner_ask=1|"
	clearShared = "pane|report-metadata|" + sharedPane + "|--source|taskr|--clear-token|taskr_owner_ask|"
)

// A host-a lane's owner ask marks the host-a pane through the client daemon,
// never the server's pane of the same id; answering clears it.
func TestRelayOwnerAskToken(t *testing.T) {
	r, client, h := clientCampaignHarness(t)
	_, _, top, lane, launch := r.lanes()
	r.agents(r.hostADir, sharedPane+"/working/1")
	ownerAskPanes(client, map[string]string{sharedPane: ""})
	ask := num(r.want(0, "host-a", as(lane, launch), "ask", "ship it?", "--owner"), "ask_id")

	if err := h.pass(); err != nil {
		t.Fatal(err)
	}
	ownerAskPanes(client, map[string]string{sharedPane: "1"})
	if err := h.pass(); err != nil {
		t.Fatal(err)
	}
	wantCalls(t, client.calls("pane|report-metadata|"), setShared)
	// Another host's reply never carries host-a's pane.
	if m := clientCampaignReply(r, "host-b"); !reflect.DeepEqual(m["owner_ask_tokens"], map[string]any{}) {
		t.Fatalf("host-b owner_ask_tokens = %#v, want {}", m["owner_ask_tokens"])
	}
	r.caller.Store("host-a")
	if got := r.calls("pane|report-metadata|"); len(got) != 0 {
		t.Fatalf("client pass wrote server panes: %q", got)
	}

	r.want(0, "host-a", nil, "answer", id(ask), "yes", "--as", id(top))
	if err := h.pass(); err != nil {
		t.Fatal(err)
	}
	ownerAskPanes(client, map[string]string{sharedPane: ""})
	if err := h.pass(); err != nil {
		t.Fatal(err)
	}
	wantCalls(t, client.calls("pane|report-metadata|"), setShared, clearShared)
}

// An older server sends no owner_ask_tokens: the client neither lists nor
// clears. An empty map from a current server clears the stale token.
func TestRelayOwnerAskTokenOlderServer(t *testing.T) {
	for _, older := range []bool{true, false} {
		r, client, h := clientCampaignHarness(t)
		ownerAskPanes(client, map[string]string{sharedPane: "1"})
		url, ln, srv := retryEndpoint(t, r.twoHost, func(w http.ResponseWriter, q *http.Request) {
			rec := httptest.NewRecorder()
			r.d.ServeHTTP(rec, q)
			var rep rpcReply
			if err := json.Unmarshal(rec.Body.Bytes(), &rep); err != nil {
				t.Error(err)
				return
			}
			if older {
				m := lastJSON(rep.Stdout)
				delete(m, "owner_ask_tokens")
				b, _ := json.Marshal(m)
				rep.Stdout = string(b)
			}
			json.NewEncoder(w).Encode(rep)
		})
		go srv.Serve(ln)
		h.raw = url
		if err := h.pass(); err != nil {
			t.Fatal(err)
		}
		if older {
			if got := client.calls("pane|"); len(got) != 0 {
				t.Fatalf("older server reply caused pane calls: %q", got)
			}
		} else {
			wantCalls(t, client.calls("pane|report-metadata|"), clearShared)
		}
	}
}
