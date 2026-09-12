package server

import (
	"context"
	"log/slog"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	axiomv1 "github.com/dhilipkumars/axiom/gateway/gen/axiom/v1"
	"github.com/dhilipkumars/axiom/gateway/internal/k8s"
)

// sqlTypeToProto maps the discovered column type onto the wire enum.
func sqlTypeToProto(t k8s.ColumnType) axiomv1.SqlType {
	if t == k8s.ColumnText {
		return axiomv1.SqlType_SQL_TYPE_TEXT
	}
	return axiomv1.SqlType_SQL_TYPE_JSONB
}

// kindToProto converts a discovered kind to its wire form.
func kindToProto(k k8s.KindInfo) *axiomv1.KindSchema {
	cols := make([]*axiomv1.ColumnSchema, 0, len(k.Columns))
	for _, c := range k.Columns {
		cols = append(cols, &axiomv1.ColumnSchema{
			Name:    c.Name,
			SqlType: sqlTypeToProto(c.Type),
			Source:  c.Source,
		})
	}
	return &axiomv1.KindSchema{
		Gvk: &axiomv1.GroupVersionKind{
			Group:   k.GVK.Group,
			Version: k.GVK.Version,
			Kind:    k.GVK.Kind,
		},
		Plural:     k.Plural,
		Namespaced: k.Namespaced,
		Columns:    cols,
		Writable:   k.Writable,
		Watchable:  k.Watchable,
	}
}

// DiscoverSchema resolves one kind and returns its foreign-table shape.
//
// Contract: see axiom.proto. Validates the gvk before any cluster call. An
// unsupported kind and a kind outside the gateway's serve allowlist are the
// same error by design, so the allowlist cannot be enumerated by probing.
// No side effects beyond populating the gateway's discovery cache.
func (s *Server) DiscoverSchema(ctx context.Context, req *axiomv1.DiscoverSchemaRequest) (*axiomv1.DiscoverSchemaResponse, error) {
	if req == nil {
		return nil, status.Error(codes.InvalidArgument, "discover_schema: request must not be nil")
	}
	gvk, err := gvkFromProto(req.GetGvk())
	if err != nil {
		return nil, err
	}
	info, err := s.k8s.Describe(ctx, gvk)
	if err != nil {
		return nil, toGRPC(err)
	}
	s.log.LogAttrs(ctx, slog.LevelInfo, "discover_schema",
		slog.String("gvk", gvk.String()),
		slog.String("plural", info.Plural),
		slog.Int("columns", len(info.Columns)))
	return &axiomv1.DiscoverSchemaResponse{Schema: kindToProto(info)}, nil
}

// maxPluralFilter bounds the LIMIT TO list a caller may send. IMPORT FOREIGN
// SCHEMA passes one name per requested table; a list far past this is a
// malformed or hostile caller rather than a real import, and each entry costs
// a map insertion and a comparison per served kind.
const maxPluralFilter = 512

// ListKinds enumerates the kinds this gateway serves.
//
// Contract: see axiom.proto. The plural filter is validated as DNS-1123 names
// before use so nothing from the request can be interpreted as a selector or
// path (docs/RULES.md §3). The response is bounded by the serve allowlist, not
// by what the cluster contains.
func (s *Server) ListKinds(ctx context.Context, req *axiomv1.ListKindsRequest) (*axiomv1.ListKindsResponse, error) {
	if req == nil {
		return nil, status.Error(codes.InvalidArgument, "list_kinds: request must not be nil")
	}
	plurals := req.GetPlurals()
	if len(plurals) > maxPluralFilter {
		return nil, status.Errorf(codes.InvalidArgument,
			"list_kinds: at most %d resource names may be requested, got %d", maxPluralFilter, len(plurals))
	}
	for _, p := range plurals {
		if err := validName("resource", p); err != nil {
			return nil, err
		}
	}
	// group is optional on the wire because "" is the core group and so cannot
	// double as "unset"; preserve that distinction rather than flattening it.
	var group *string
	if req.Group != nil {
		g := req.GetGroup()
		if g != "" {
			if err := validName("group", g); err != nil {
				return nil, err
			}
		}
		group = &g
	}

	kinds, err := s.k8s.Kinds(ctx, group, plurals)
	if err != nil {
		return nil, toGRPC(err)
	}
	resp := &axiomv1.ListKindsResponse{Kinds: make([]*axiomv1.KindSchema, 0, len(kinds))}
	for _, k := range kinds {
		resp.Kinds = append(resp.Kinds, kindToProto(k))
	}
	s.log.LogAttrs(ctx, slog.LevelInfo, "list_kinds",
		slog.Bool("group_filter", group != nil),
		slog.Int("requested", len(plurals)),
		slog.Int("count", len(resp.Kinds)))
	return resp, nil
}
