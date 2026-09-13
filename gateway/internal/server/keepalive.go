package server

import (
	"time"

	"google.golang.org/grpc/keepalive"
)

// ClientPingInterval mirrors KEEPALIVE_INTERVAL in extension/src/transport.rs.
//
// Duplicated across two languages with no shared artifact, which is why the
// invariant below is asserted by a test rather than left to reviewers. Change
// one side and the test names the other.
const ClientPingInterval = 30 * time.Second

// KeepaliveParams returns the gateway's HTTP/2 keepalive settings.
//
// Postgres is deliberately outside the cluster (docs/DESIGN.md), so every
// connection crosses NAT, stateful firewalls and cloud load balancers. Those
// drop an idle flow without sending a FIN or an RST, leaving both ends holding
// a socket that will never deliver anything. A watch stream is idle whenever
// the cluster is quiet, which is most of the time.
//
// ServerParameters make the gateway probe its peers, so it reclaims
// connections to a Postgres that vanished without closing them.
func KeepaliveParams() keepalive.ServerParameters {
	return keepalive.ServerParameters{
		Time:    30 * time.Second,
		Timeout: 10 * time.Second,
	}
}

// KeepaliveEnforcement returns the policy governing client pings.
//
// This is the half that is easy to get wrong. gRPC-Go defaults to refusing
// pings more frequent than every 5 minutes and to refusing them entirely on a
// connection with no active streams, answering with GOAWAY ENHANCE_YOUR_CALM.
// The extension pings every ClientPingInterval and must do so while idle,
// because an idle watch stream is exactly what a middlebox reaps. Left at the
// defaults, the keepalive would tear down the connections it exists to
// protect.
//
// MinTime sits below ClientPingInterval to leave room for jitter and scheduling
// delay; see TestKeepaliveEnforcementPermitsTheExtensionsPings.
func KeepaliveEnforcement() keepalive.EnforcementPolicy {
	return keepalive.EnforcementPolicy{
		MinTime:             10 * time.Second,
		PermitWithoutStream: true,
	}
}
