package relayreq

import (
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestClientAddr(t *testing.T) {
	r := httptest.NewRequest(http.MethodGet, "/", nil)
	r.RemoteAddr = "192.168.1.5:51234"
	r.Header.Set("X-Relay-Client", "203.0.113.9")
	r.Header.Set("X-Forwarded-For", "203.0.113.9")
	if IsRelayed(r.Context()) {
		t.Fatal("plain request is marked relayed")
	}
	if got := ClientAddr(r); got != "192.168.1.5" {
		t.Fatalf("ClientAddr = %q, want 192.168.1.5", got)
	}

	r = r.WithContext(WithRelay(r.Context(), "203.0.113.9"))
	if !IsRelayed(r.Context()) {
		t.Fatal("marked request is not relayed")
	}
	if got := ClientAddr(r); got != "203.0.113.9" {
		t.Fatalf("ClientAddr = %q, want 203.0.113.9", got)
	}
}

func TestClientAddrWithoutPort(t *testing.T) {
	r := httptest.NewRequest(http.MethodGet, "/", nil)
	r.RemoteAddr = "pipe"
	if got := ClientAddr(r); got != "pipe" {
		t.Fatalf("ClientAddr = %q, want pipe", got)
	}
}
