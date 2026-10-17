package proxy

import (
	"io"
	"net/http"
	"strings"
	"testing"
)

// userBackend answers getUser and getUsers as Navidrome does, with the
// Remote-User value as the user name, and records the username parameter.
func userBackend(gotUsername *string) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Accept-Encoding") != "identity" {
			// The proxy must ask for a body it can rewrite.
			http.Error(w, "compressed response requested", http.StatusInternalServerError)
			return
		}
		*gotUsername = r.URL.Query().Get("username")
		id := r.Header.Get("Remote-User")
		if r.URL.Query().Get("f") == "json" {
			w.Header().Set("Content-Type", "application/json")
			_, _ = io.WriteString(w, `{"subsonic-response":{"status":"ok","users":{"user":[{"username":"`+id+`","adminRole":true}]},"user":{"username":"`+id+`","adminRole":true}}}`)
			return
		}
		w.Header().Set("Content-Type", "application/xml")
		_, _ = io.WriteString(w, `<subsonic-response status="ok"><user username="`+id+`" adminRole="true"></user></subsonic-response>`)
	})
}

func TestGetUserNameMapping(t *testing.T) {
	var got string
	e := newEnv(t, userBackend(&got), true)
	tests := []struct {
		name, call, username string
		wantErr              bool
	}{
		{"name", "getUser.view", "owner", false},
		{"name bare", "getUser", "owner", false},
		{"name upper", "getUser.view", "OWNER", false},
		{"id", "getUser.view", e.owner.ID, false},
		{"wn-server", "getUser.view", "wn-server", true},
	}
	for _, f := range []string{"json", "xml"} {
		for _, tt := range tests {
			t.Run(tt.name+" "+f, func(t *testing.T) {
				got = ""
				resp, err := http.Get(e.srv.URL + "/rest/" + tt.call + "?" + creds() + "&f=" + f + "&username=" + tt.username)
				if err != nil {
					t.Fatal(err)
				}
				body, _ := io.ReadAll(resp.Body)
				resp.Body.Close()
				s := string(body)
				if tt.wantErr {
					if got != "" || !strings.Contains(s, "failed") || !(strings.Contains(s, `"code":50`) || strings.Contains(s, `code="50"`)) {
						t.Fatalf("forwarded %q, body %s; want error 50 and no forward", got, s)
					}
					return
				}
				if got != e.owner.ID {
					t.Fatalf("backend username %q, want %q", got, e.owner.ID)
				}
				if strings.Contains(s, e.owner.ID) || !strings.Contains(s, "owner") {
					t.Fatalf("body %s: want the account name and not the id", s)
				}
			})
		}
	}
}

func TestGetUsersRewritesName(t *testing.T) {
	var got string
	e := newEnv(t, userBackend(&got), true)
	for _, call := range []string{"getUsers", "getUsers.view"} {
		resp, err := http.Get(e.srv.URL + "/rest/" + call + "?" + creds() + "&f=json")
		if err != nil {
			t.Fatal(err)
		}
		body, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		if resp.ContentLength != int64(len(body)) || strings.Contains(string(body), e.owner.ID) || !strings.Contains(string(body), `"username":"owner"`) {
			t.Fatalf("%s: body %s, content length %d", call, body, resp.ContentLength)
		}
	}
}
