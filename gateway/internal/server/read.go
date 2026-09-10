package server

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"regexp"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime/schema"

	axiomv1 "github.com/dhilipkumars/axiom/gateway/gen/axiom/v1"
	"github.com/dhilipkumars/axiom/gateway/internal/k8s"
)

// dns1123Subdomain matches valid Kubernetes object names and namespaces.
// Checked before anything reaches client-go so a caller can never smuggle
// path segments or selectors through these fields (docs/RULES.md §3).
var dns1123Subdomain = regexp.MustCompile(`^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*$`)

const maxNameLen = 253

// gvkFromProto validates and converts the wire GVK.
func gvkFromProto(g *axiomv1.GroupVersionKind) (schema.GroupVersionKind, error) {
	if g == nil || g.GetVersion() == "" || g.GetKind() == "" {
		return schema.GroupVersionKind{}, status.Error(codes.InvalidArgument, "gvk: version and kind are required")
	}
	return schema.GroupVersionKind{Group: g.GetGroup(), Version: g.GetVersion(), Kind: g.GetKind()}, nil
}

// validName checks a namespace or name. Empty is accepted (callers decide
// whether empty is allowed for the field in question).
func validName(field, v string) error {
	if v == "" {
		return nil
	}
	if len(v) > maxNameLen || !dns1123Subdomain.MatchString(v) {
		return status.Errorf(codes.InvalidArgument, "%s %q is not a valid DNS-1123 subdomain", field, v)
	}
	return nil
}

// toGRPC maps k8s/client errors onto gRPC status codes. The message never
// includes request headers or credentials: apimachinery StatusError messages
// describe the resource, and client-go transport errors describe the endpoint.
func toGRPC(err error) error {
	switch {
	case err == nil:
		return nil
	case errors.Is(err, k8s.ErrNoCluster):
		return status.Error(codes.FailedPrecondition, err.Error())
	case errors.Is(err, k8s.ErrUnsupportedKind):
		return status.Error(codes.InvalidArgument, err.Error())
	case apierrors.IsNotFound(err):
		return status.Error(codes.NotFound, err.Error())
	case apierrors.IsForbidden(err), apierrors.IsUnauthorized(err):
		return status.Error(codes.PermissionDenied, err.Error())
	case apierrors.IsInvalid(err), apierrors.IsBadRequest(err):
		return status.Error(codes.InvalidArgument, err.Error())
	case apierrors.IsTimeout(err), apierrors.IsServerTimeout(err), apierrors.IsServiceUnavailable(err), apierrors.IsTooManyRequests(err):
		return status.Error(codes.Unavailable, err.Error())
	case errors.Is(err, context.DeadlineExceeded):
		return status.Error(codes.DeadlineExceeded, err.Error())
	default:
		// Transport-level failures (connection refused, DNS, TLS) arrive as
		// plain errors from client-go; the API server is unreachable.
		if _, isStatus := err.(apierrors.APIStatus); !isStatus {
			return status.Error(codes.Unavailable, fmt.Sprintf("kubernetes api server unreachable: %v", err))
		}
		return status.Error(codes.Internal, err.Error())
	}
}

// objectToProto serialises one object. Serialisation failure is Internal:
// the API server handed us something we cannot re-encode.
func objectToProto(u *unstructured.Unstructured) (*axiomv1.Object, error) {
	raw, err := u.MarshalJSON()
	if err != nil {
		return nil, status.Errorf(codes.Internal, "marshal %s/%s: %v", u.GetNamespace(), u.GetName(), err)
	}
	return &axiomv1.Object{
		Namespace:       u.GetNamespace(),
		Name:            u.GetName(),
		ResourceVersion: u.GetResourceVersion(),
		Json:            raw,
	}, nil
}

// Get fetches one object from the API server.
//
// Contract: see axiom.proto. Validates gvk/namespace/name before any cluster
// call; name is required. Errors are mapped by toGRPC. No side effects.
func (s *Server) Get(ctx context.Context, req *axiomv1.GetRequest) (*axiomv1.GetResponse, error) {
	if req == nil {
		return nil, status.Error(codes.InvalidArgument, "get: request must not be nil")
	}
	gvk, err := gvkFromProto(req.GetGvk())
	if err != nil {
		return nil, err
	}
	if req.GetName() == "" {
		return nil, status.Error(codes.InvalidArgument, "get: name is required")
	}
	if err := validName("namespace", req.GetNamespace()); err != nil {
		return nil, err
	}
	if err := validName("name", req.GetName()); err != nil {
		return nil, err
	}
	obj, err := s.k8s.Get(ctx, gvk, req.GetNamespace(), req.GetName())
	if err != nil {
		return nil, toGRPC(err)
	}
	po, err := objectToProto(obj)
	if err != nil {
		return nil, err
	}
	return &axiomv1.GetResponse{Object: po}, nil
}

// List fetches objects of one kind, narrowed server-side by namespace and/or
// name.
//
// Contract: see axiom.proto. Validates inputs before any cluster call. Logs
// one line per call with the filters applied so pushdown is observable
// (the Phase 1 E2E asserts on it). No side effects.
func (s *Server) List(ctx context.Context, req *axiomv1.ListRequest) (*axiomv1.ListResponse, error) {
	if req == nil {
		return nil, status.Error(codes.InvalidArgument, "list: request must not be nil")
	}
	gvk, err := gvkFromProto(req.GetGvk())
	if err != nil {
		return nil, err
	}
	if err := validName("namespace", req.GetNamespace()); err != nil {
		return nil, err
	}
	if err := validName("name", req.GetName()); err != nil {
		return nil, err
	}
	list, err := s.k8s.List(ctx, gvk, req.GetNamespace(), req.GetName())
	if err != nil {
		return nil, toGRPC(err)
	}
	resp := &axiomv1.ListResponse{
		Objects:         make([]*axiomv1.Object, 0, len(list.Items)),
		ResourceVersion: list.GetResourceVersion(),
	}
	for i := range list.Items {
		po, err := objectToProto(&list.Items[i])
		if err != nil {
			return nil, err
		}
		resp.Objects = append(resp.Objects, po)
	}
	s.log.LogAttrs(ctx, slog.LevelInfo, "list",
		slog.String("gvk", gvk.String()),
		slog.String("namespace", req.GetNamespace()),
		slog.String("name", req.GetName()),
		slog.Int("count", len(resp.Objects)))
	return resp, nil
}
