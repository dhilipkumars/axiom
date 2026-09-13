package server

import "testing"

// The gateway must not punish the extension for pinging at the interval the
// extension is actually configured to use. If MinTime ever rises above (or to)
// ClientPingInterval, gRPC-Go answers pings with GOAWAY ENHANCE_YOUR_CALM and
// kills long-lived watch streams -- the exact failure the keepalive was added
// to prevent, now caused by it.
//
// The two values live in different languages with no shared artifact
// (extension/src/transport.rs holds the client side), so this is the only
// place the relationship between them is checked.
func TestKeepaliveEnforcementPermitsTheExtensionsPings(t *testing.T) {
	kep := KeepaliveEnforcement()
	if kep.MinTime >= ClientPingInterval {
		t.Errorf("MinTime %v must be below the extension's ping interval %v, "+
			"or the server answers pings with GOAWAY ENHANCE_YOUR_CALM",
			kep.MinTime, ClientPingInterval)
	}
	if !kep.PermitWithoutStream {
		t.Error("PermitWithoutStream must be true: an idle watch stream is " +
			"precisely what a middlebox reaps, so idle pings are the point")
	}
}

// A server that probes its peers must wait longer for the answer than zero,
// and must not probe so rarely that a dead Postgres holds a connection for
// hours.
func TestKeepaliveParamsAreUsable(t *testing.T) {
	p := KeepaliveParams()
	if p.Timeout <= 0 || p.Timeout >= p.Time {
		t.Errorf("Timeout %v must be positive and shorter than Time %v", p.Timeout, p.Time)
	}
	if p.Time > ClientPingInterval*4 {
		t.Errorf("Time %v is too long to detect a vanished peer usefully", p.Time)
	}
}
