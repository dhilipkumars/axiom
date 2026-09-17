package server

import (
	"os"
	"regexp"
	"strconv"
	"testing"
	"time"
)

// rustClientPingInterval reads KEEPALIVE_INTERVAL out of the extension's
// source.
//
// Reading the real value rather than restating it here is the point. A Go
// constant claiming "the client pings every 30s" is just a second literal: if
// the extension moved to 5s it would still say 30, the compatibility test
// below would still pass, and the gateway would answer those pings with GOAWAY
// ENHANCE_YOUR_CALM. The two settings live in different languages with no
// shared artifact, so the only way to check them against each other is to go
// and look.
//
// If the constant is renamed or moved, this fails loudly rather than silently
// falling back to an assumption.
func rustClientPingInterval(t *testing.T) time.Duration {
	t.Helper()
	// A const, not a filepath.Join: gosec's G304 flags a read from a computed
	// path, and it is satisfied by a constant one. Go accepts forward slashes
	// on every platform, so nothing is lost by not joining.
	const path = "../../../extension/src/transport.rs"
	src, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("cannot read %s, which holds the client's ping interval: %v", path, err)
	}
	re := regexp.MustCompile(`KEEPALIVE_INTERVAL:\s*Duration\s*=\s*Duration::from_secs\((\d+)\)`)
	m := re.FindSubmatch(src)
	if m == nil {
		t.Fatalf("KEEPALIVE_INTERVAL not found in %s; if it was renamed, update this test "+
			"rather than deleting it, or the two sides can drift apart unnoticed", path)
	}
	secs, err := strconv.Atoi(string(m[1]))
	if err != nil {
		t.Fatalf("KEEPALIVE_INTERVAL is not a whole number of seconds: %v", err)
	}
	return time.Duration(secs) * time.Second
}

// The gateway must not punish the extension for pinging at the interval the
// extension is actually configured to use. If MinTime ever reaches or exceeds
// that interval, gRPC-Go answers pings with GOAWAY ENHANCE_YOUR_CALM and kills
// long-lived watch streams: the exact failure the keepalive was added to
// prevent, now caused by it.
func TestKeepaliveEnforcementPermitsTheExtensionsPings(t *testing.T) {
	clientInterval := rustClientPingInterval(t)
	kep := KeepaliveEnforcement()
	if kep.MinTime >= clientInterval {
		t.Errorf("MinTime %v must be below the extension's ping interval %v (from "+
			"extension/src/transport.rs), or the server answers pings with GOAWAY "+
			"ENHANCE_YOUR_CALM", kep.MinTime, clientInterval)
	}
	if !kep.PermitWithoutStream {
		t.Error("PermitWithoutStream must be true: a subscription channel pings while " +
			"idle, because an idle watch stream is precisely what a middlebox reaps")
	}
}

// rustClientMaxIdle reads MAX_IDLE out of the extension's source, for the same
// reason rustClientPingInterval does: restating it as a Go literal here would
// make this test agree with itself rather than with the extension.
func rustClientMaxIdle(t *testing.T) time.Duration {
	t.Helper()
	const path = "../../../extension/src/client.rs"
	src, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("cannot read %s, which holds the client's channel idle limit: %v", path, err)
	}
	re := regexp.MustCompile(`MAX_IDLE:\s*Duration\s*=\s*Duration::from_secs\((\d+)\)`)
	m := re.FindSubmatch(src)
	if m == nil {
		t.Fatalf("MAX_IDLE not found in %s; if it was renamed, update this test rather "+
			"than deleting it, or the two sides can drift apart unnoticed", path)
	}
	secs, err := strconv.Atoi(string(m[1]))
	if err != nil {
		t.Fatalf("MAX_IDLE is not a whole number of seconds: %v", err)
	}
	return time.Duration(secs) * time.Second
}

// The extension must stop reusing a cached connection before this server
// starts asking whether it is still there.
//
// A Postgres backend cannot answer that ping: its unary channels are built
// WhileActive on a current-thread runtime, so between queries nothing polls
// the connection. Whatever Time is set to here, the gateway will close an idle
// backend's connection Time+Timeout later -- and the backend finds out by
// failing someone's statement, which inside a transaction loses their work
// (issue #51).
//
// So the two numbers are a contract across two languages with no shared
// artifact. Lowering Time here without lowering MAX_IDLE there reintroduces
// the bug silently, which is exactly what this test exists to prevent.
func TestExtensionRebuildsBeforeThisServerClosesAnIdleConnection(t *testing.T) {
	maxIdle := rustClientMaxIdle(t)
	p := KeepaliveParams()
	if maxIdle >= p.Time {
		t.Errorf("MAX_IDLE %v (extension/src/client.rs) must be below Time %v, or a backend "+
			"can present a connection this server has already begun probing", maxIdle, p.Time)
	}
	if maxIdle >= p.Time+p.Timeout {
		t.Errorf("MAX_IDLE %v must be below Time+Timeout %v, the point at which this server "+
			"drops an unanswered peer", maxIdle, p.Time+p.Timeout)
	}
}

// A server that probes its peers must wait longer for the answer than zero,
// and must not probe so rarely that a vanished Postgres holds a connection for
// hours.
func TestKeepaliveParamsAreUsable(t *testing.T) {
	p := KeepaliveParams()
	if p.Timeout <= 0 || p.Timeout >= p.Time {
		t.Errorf("Timeout %v must be positive and shorter than Time %v", p.Timeout, p.Time)
	}
	if p.Time > 4*rustClientPingInterval(t) {
		t.Errorf("Time %v is too long to detect a vanished peer usefully", p.Time)
	}
}
