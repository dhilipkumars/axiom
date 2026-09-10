// Package server implements the axiom.v1.GatewayService gRPC handlers.
//
// Phase 0 ships only Ping. Every handler treats its request as untrusted input
// (docs/RULES.md §3): fields are validated before use and nothing from the
// request is ever interpreted as a path, selector, or template.
package server

import (
	"context"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/types/known/timestamppb"

	axiomv1 "github.com/dhilipkumars/axiom/gateway/gen/axiom/v1"
)

// Clock returns the current time. It is injected so handlers are testable with
// a fixed clock.
type Clock func() time.Time

// Server implements axiomv1.GatewayServiceServer.
type Server struct {
	axiomv1.UnimplementedGatewayServiceServer

	version string
	now     Clock
}

// New returns a Server that reports version in Ping replies and uses now as
// its clock. A nil now defaults to time.Now.
func New(version string, now Clock) *Server {
	if now == nil {
		now = time.Now
	}
	return &Server{version: version, now: now}
}

// Ping echoes the caller's nonce and reports gateway version and clock.
//
// Contract: returns codes.InvalidArgument when req is nil or req.Nonce is 0
// (0 is reserved as "unset" by the proto contract). Has no side effects and
// never touches the Kubernetes API.
func (s *Server) Ping(_ context.Context, req *axiomv1.PingRequest) (*axiomv1.PingResponse, error) {
	if req == nil {
		return nil, status.Error(codes.InvalidArgument, "ping: request must not be nil")
	}
	if req.GetNonce() == 0 {
		return nil, status.Error(codes.InvalidArgument, "ping: nonce must be non-zero")
	}
	return &axiomv1.PingResponse{
		Nonce:          req.GetNonce(),
		GatewayVersion: s.version,
		ServerTime:     timestamppb.New(s.now()),
	}, nil
}
