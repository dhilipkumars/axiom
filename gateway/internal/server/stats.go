package server

import (
	"context"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	axiomv1 "github.com/dhilipkumars/axiom/gateway/gen/axiom/v1"
	"github.com/dhilipkumars/axiom/gateway/internal/k8s"
)

// Stats reports runtime counters and process start time for the gateway.
//
// Contract: see axiom.proto. Calling Stats is side-effect free and does not
// mutate counters or issue cluster requests. If req is nil, returns
// codes.InvalidArgument.
func (s *Server) Stats(_ context.Context, req *axiomv1.StatsRequest) (*axiomv1.StatsResponse, error) {
	if req == nil {
		return nil, status.Error(codes.InvalidArgument, "stats: request must not be nil")
	}

	var openapiFetches, openapiGroupVersions, accessReviews uint64
	if sr, ok := s.k8s.(k8s.StatsReporter); ok {
		openapiFetches = sr.OpenAPIFetches()
		openapiGroupVersions = sr.OpenAPIGroupVersions()
		accessReviews = sr.AccessReviews()
	} else {
		openapiFetches = s.openapiFetches.Load()
		openapiGroupVersions = s.openapiGroupVersions.Load()
		accessReviews = s.accessReviews.Load()
	}

	return &axiomv1.StatsResponse{
		StartedAtUnixSeconds: s.startedAtUnixSeconds,
		GetCalls:             s.getCalls.Load(),
		ListCalls:            s.listCalls.Load(),
		CreateCalls:          s.createCalls.Load(),
		UpdateCalls:          s.updateCalls.Load(),
		DeleteCalls:          s.deleteCalls.Load(),
		SubscribeCalls:       s.subscribeCalls.Load(),
		SubscribeListCalls:   s.subscribeListCalls.Load(),
		OpenapiFetches:       openapiFetches,
		OpenapiGroupVersions: openapiGroupVersions,
		AccessReviews:        accessReviews,
	}, nil
}
