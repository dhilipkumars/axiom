package server

import (
	"context"
	"net"
	"testing"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/test/bufconn"

	axiomv1 "github.com/dhilipkumars/axiom/gateway/gen/axiom/v1"
)

// TestPingOverGRPC exercises the real grpc-go transport in-process (bufconn) so
// the generated stubs and service registration are covered, not just the
// handler method. TLS is intentionally out of scope here: it is covered by the
// tlsconfig package and by the phase-0 E2E test over a real socket.
func TestPingOverGRPC(t *testing.T) {
	t.Parallel()
	lis := bufconn.Listen(1 << 20)
	gs := grpc.NewServer()
	axiomv1.RegisterGatewayServiceServer(gs, New("bufconn", nil, nil, nil))
	go func() {
		// Serve returns a non-nil error only if the listener fails; bufconn's
		// Close during teardown makes that error expected and uninteresting.
		_ = gs.Serve(lis) // error is only ever the expected teardown-induced listener close
	}()
	t.Cleanup(gs.Stop)

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	conn, err := grpc.NewClient("passthrough:///bufconn",
		grpc.WithContextDialer(func(context.Context, string) (net.Conn, error) { return lis.Dial() }),
		grpc.WithTransportCredentials(insecure.NewCredentials()),
	)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if cerr := conn.Close(); cerr != nil {
			t.Errorf("close: %v", cerr)
		}
	})

	resp, err := axiomv1.NewGatewayServiceClient(conn).Ping(ctx, &axiomv1.PingRequest{Nonce: 7})
	if err != nil {
		t.Fatal(err)
	}
	if resp.GetNonce() != 7 {
		t.Fatalf("nonce = %d, want 7", resp.GetNonce())
	}
}
