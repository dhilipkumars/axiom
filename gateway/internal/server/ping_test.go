package server

import (
	"context"
	"testing"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	axiomv1 "github.com/dhilipkumars/axiom/gateway/gen/axiom/v1"
)

func TestPing(t *testing.T) {
	t.Parallel()
	fixed := time.Date(2026, 9, 10, 12, 0, 0, 0, time.UTC)
	srv := New("test-version", func() time.Time { return fixed })

	tests := []struct {
		name      string
		req       *axiomv1.PingRequest
		wantCode  codes.Code
		wantNonce uint64
	}{
		{name: "echoes nonce", req: &axiomv1.PingRequest{Nonce: 42}, wantCode: codes.OK, wantNonce: 42},
		{name: "max nonce", req: &axiomv1.PingRequest{Nonce: ^uint64(0)}, wantCode: codes.OK, wantNonce: ^uint64(0)},
		{name: "zero nonce rejected", req: &axiomv1.PingRequest{Nonce: 0}, wantCode: codes.InvalidArgument},
		{name: "nil request rejected", req: nil, wantCode: codes.InvalidArgument},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			resp, err := srv.Ping(context.Background(), tc.req)
			if got := status.Code(err); got != tc.wantCode {
				t.Fatalf("code = %v, want %v (err=%v)", got, tc.wantCode, err)
			}
			if tc.wantCode != codes.OK {
				if resp != nil {
					t.Fatalf("expected nil response on error, got %v", resp)
				}
				return
			}
			if resp.GetNonce() != tc.wantNonce {
				t.Errorf("nonce = %d, want %d", resp.GetNonce(), tc.wantNonce)
			}
			if resp.GetGatewayVersion() != "test-version" {
				t.Errorf("version = %q, want %q", resp.GetGatewayVersion(), "test-version")
			}
			if got := resp.GetServerTime().AsTime(); !got.Equal(fixed) {
				t.Errorf("server_time = %v, want %v", got, fixed)
			}
		})
	}
}

func TestNewDefaultsClock(t *testing.T) {
	t.Parallel()
	srv := New("v", nil)
	before := time.Now().Add(-time.Second)
	resp, err := srv.Ping(context.Background(), &axiomv1.PingRequest{Nonce: 1})
	if err != nil {
		t.Fatal(err)
	}
	if resp.GetServerTime().AsTime().Before(before) {
		t.Errorf("default clock returned stale time %v", resp.GetServerTime().AsTime())
	}
}
