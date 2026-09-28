package server

import (
	"context"
	"errors"
	"strings"
	"testing"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	dynamicfake "k8s.io/client-go/dynamic/fake"
	"k8s.io/client-go/kubernetes/scheme"

	axiomv1 "github.com/dhilipkumars/axiom/gateway/gen/axiom/v1"
	"github.com/dhilipkumars/axiom/gateway/internal/k8s"
)

// discoverServer serves the two built-in kinds through the static mapper, which
// produces exactly the columns discovery would for them.
func discoverServer(t *testing.T) *Server {
	t.Helper()
	return New("t", nil, k8s.NewDynamic(
		dynamicfake.NewSimpleDynamicClient(scheme.Scheme),
		k8s.NewStaticMapper(k8s.BuiltinKinds()...)), nil)
}

func columnNames(s *axiomv1.KindSchema) []string {
	out := make([]string, 0, len(s.GetColumns()))
	for _, c := range s.GetColumns() {
		out = append(out, c.GetName())
	}
	return out
}

func TestDiscoverSchema(t *testing.T) {
	t.Parallel()
	s := discoverServer(t)
	resp, err := s.DiscoverSchema(context.Background(), &axiomv1.DiscoverSchemaRequest{
		Gvk: &axiomv1.GroupVersionKind{Version: "v1", Kind: "Pod"},
	})
	if err != nil {
		t.Fatalf("DiscoverSchema(Pod) = %v", err)
	}
	got := resp.GetSchema()
	if got.GetPlural() != "pods" || !got.GetNamespaced() {
		t.Errorf("plural=%q namespaced=%v", got.GetPlural(), got.GetNamespaced())
	}
	want := "api_version,kind,name,namespace,uid,resource_version,creation_timestamp,labels,annotations,metadata,phase,node,spec,status,raw"
	if strings.Join(columnNames(got), ",") != want {
		t.Errorf("columns = %v\n     want %s", columnNames(got), want)
	}
	// Every column carries a wire type the extension can act on.
	for _, c := range got.GetColumns() {
		if c.GetSqlType() == axiomv1.SqlType_SQL_TYPE_UNSPECIFIED {
			t.Errorf("column %q has no SQL type", c.GetName())
		}
		if c.GetSource() == "" {
			t.Errorf("column %q has no source", c.GetName())
		}
	}
	if raw := got.GetColumns()[len(got.GetColumns())-1]; raw.GetName() != "raw" ||
		raw.GetSqlType() != axiomv1.SqlType_SQL_TYPE_JSONB {
		t.Errorf("last column = %v, want raw jsonb", raw)
	}
}

// TestTypedColumnsOnlyForACallerThatAsks keeps typed columns away from an
// extension that predates them (#79). It maps an unknown wire type to no column
// at all, so a typed column sent to it would silently vanish from the table it
// generates. Asked, it gets the types; not asked, what it always got.
func TestTypedColumnsOnlyForACallerThatAsks(t *testing.T) {
	t.Parallel()
	s := discoverServer(t)
	cm := &axiomv1.GroupVersionKind{Version: "v1", Kind: "ConfigMap"}
	typesOf := func(k *axiomv1.KindSchema) map[string]axiomv1.SqlType {
		out := map[string]axiomv1.SqlType{}
		for _, c := range k.GetColumns() {
			if c.GetSqlType() == axiomv1.SqlType_SQL_TYPE_UNSPECIFIED {
				t.Errorf("column %q has no SQL type", c.GetName())
			}
			out[c.GetName()] = c.GetSqlType()
		}
		return out
	}
	for _, tc := range []struct {
		typed               bool
		creation, immutable axiomv1.SqlType
	}{
		{typed: true, creation: axiomv1.SqlType_SQL_TYPE_TIMESTAMPTZ, immutable: axiomv1.SqlType_SQL_TYPE_BOOLEAN},
		{typed: false, creation: axiomv1.SqlType_SQL_TYPE_TEXT, immutable: axiomv1.SqlType_SQL_TYPE_JSONB},
	} {
		d, err := s.DiscoverSchema(context.Background(), &axiomv1.DiscoverSchemaRequest{Gvk: cm, TypedColumns: tc.typed})
		if err != nil {
			t.Fatalf("DiscoverSchema(typed=%v) = %v", tc.typed, err)
		}
		l, err := s.ListKinds(context.Background(), &axiomv1.ListKindsRequest{Plurals: []string{"configmaps"}, TypedColumns: tc.typed})
		if err != nil || len(l.GetKinds()) != 1 {
			t.Fatalf("ListKinds(typed=%v) = %v, %v", tc.typed, l, err)
		}
		for rpc, k := range map[string]*axiomv1.KindSchema{"DiscoverSchema": d.GetSchema(), "ListKinds": l.GetKinds()[0]} {
			got := typesOf(k)
			if got["creation_timestamp"] != tc.creation || got["immutable"] != tc.immutable {
				t.Errorf("%s(typed=%v): creation_timestamp %v, immutable %v; want %v, %v",
					rpc, tc.typed, got["creation_timestamp"], got["immutable"], tc.creation, tc.immutable)
			}
			if got["name"] != axiomv1.SqlType_SQL_TYPE_TEXT || got["data"] != axiomv1.SqlType_SQL_TYPE_JSONB {
				t.Errorf("%s(typed=%v): name %v, data %v; the untyped columns must not change", rpc, tc.typed, got["name"], got["data"])
			}
		}
	}
}

func TestEveryColumnTypeHasAWireName(t *testing.T) {
	t.Parallel()
	for _, ct := range []k8s.ColumnType{k8s.ColumnText, k8s.ColumnJSONB, k8s.ColumnBigint, k8s.ColumnBoolean, k8s.ColumnTimestamptz} {
		if sqlTypeToProto(ct) == axiomv1.SqlType_SQL_TYPE_UNSPECIFIED {
			t.Errorf("%v has no wire type", ct)
		}
	}
	if got := sqlTypeToProto(k8s.ColumnType(99)); got != axiomv1.SqlType_SQL_TYPE_UNSPECIFIED {
		t.Errorf("an unknown type went on the wire as %v", got)
	}
}

func TestDiscoverSchemaErrors(t *testing.T) {
	t.Parallel()
	s := discoverServer(t)
	for _, tc := range []struct {
		name string
		req  *axiomv1.DiscoverSchemaRequest
		want codes.Code
	}{
		{name: "nil request", req: nil, want: codes.InvalidArgument},
		{name: "nil gvk", req: &axiomv1.DiscoverSchemaRequest{}, want: codes.InvalidArgument},
		{
			name: "gvk without kind",
			req:  &axiomv1.DiscoverSchemaRequest{Gvk: &axiomv1.GroupVersionKind{Version: "v1"}},
			want: codes.InvalidArgument,
		},
		{
			name: "gvk without version",
			req:  &axiomv1.DiscoverSchemaRequest{Gvk: &axiomv1.GroupVersionKind{Kind: "Pod"}},
			want: codes.InvalidArgument,
		},
		{
			name: "kind this gateway does not serve",
			req:  &axiomv1.DiscoverSchemaRequest{Gvk: &axiomv1.GroupVersionKind{Group: "apps", Version: "v1", Kind: "Deployment"}},
			want: codes.InvalidArgument,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			_, err := s.DiscoverSchema(context.Background(), tc.req)
			if status.Code(err) != tc.want {
				t.Fatalf("code = %v, want %v (err %v)", status.Code(err), tc.want, err)
			}
		})
	}
}

func TestDiscoverSchemaWithoutClusterIsLoud(t *testing.T) {
	t.Parallel()
	s := New("t", nil, k8s.Unconfigured{}, nil)
	_, err := s.DiscoverSchema(context.Background(), &axiomv1.DiscoverSchemaRequest{
		Gvk: &axiomv1.GroupVersionKind{Version: "v1", Kind: "Pod"},
	})
	if status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("code = %v, want FailedPrecondition", status.Code(err))
	}
}

func TestListKinds(t *testing.T) {
	t.Parallel()
	s := discoverServer(t)
	resp, err := s.ListKinds(context.Background(), &axiomv1.ListKindsRequest{})
	if err != nil {
		t.Fatalf("ListKinds = %v", err)
	}
	var plurals []string
	for _, k := range resp.GetKinds() {
		plurals = append(plurals, k.GetPlural())
	}
	if strings.Join(plurals, ",") != "configmaps,pods" {
		t.Errorf("ListKinds = %v, want configmaps,pods sorted", plurals)
	}
}

func TestListKindsFilters(t *testing.T) {
	t.Parallel()
	s := discoverServer(t)
	ctx := context.Background()

	only, err := s.ListKinds(ctx, &axiomv1.ListKindsRequest{Plurals: []string{"pods"}})
	if err != nil {
		t.Fatal(err)
	}
	if len(only.GetKinds()) != 1 || only.GetKinds()[0].GetPlural() != "pods" {
		t.Errorf("plural filter = %v, want just pods", only.GetKinds())
	}

	// An unmatched name is absent from the response, not an error, so a caller
	// may pass a speculative LIMIT TO list.
	none, err := s.ListKinds(ctx, &axiomv1.ListKindsRequest{Plurals: []string{"nonexistent"}})
	if err != nil {
		t.Fatalf("an unmatched filter must not be an error: %v", err)
	}
	if len(none.GetKinds()) != 0 {
		t.Errorf("got %v, want no kinds", none.GetKinds())
	}

	// The empty group is the core group, and is distinct from "unset".
	core := ""
	byCore, err := s.ListKinds(ctx, &axiomv1.ListKindsRequest{Group: &core})
	if err != nil {
		t.Fatal(err)
	}
	if len(byCore.GetKinds()) != 2 {
		t.Errorf("core group = %v, want both built-ins", byCore.GetKinds())
	}
	other := "example.com"
	byOther, err := s.ListKinds(ctx, &axiomv1.ListKindsRequest{Group: &other})
	if err != nil {
		t.Fatal(err)
	}
	if len(byOther.GetKinds()) != 0 {
		t.Errorf("unserved group = %v, want none", byOther.GetKinds())
	}
}

func TestListKindsRejectsHostileFilters(t *testing.T) {
	t.Parallel()
	s := discoverServer(t)
	ctx := context.Background()

	if _, err := s.ListKinds(ctx, nil); status.Code(err) != codes.InvalidArgument {
		t.Errorf("nil request code = %v, want InvalidArgument", status.Code(err))
	}

	// Nothing from the request may reach client-go as a path or selector.
	for _, bad := range []string{"pods/../secrets", "pods,secrets", "Pods Secrets", strings.Repeat("a", 300)} {
		_, err := s.ListKinds(ctx, &axiomv1.ListKindsRequest{Plurals: []string{bad}})
		if status.Code(err) != codes.InvalidArgument {
			t.Errorf("plural %q code = %v, want InvalidArgument", bad, status.Code(err))
		}
	}

	group := "not a group!"
	if _, err := s.ListKinds(ctx, &axiomv1.ListKindsRequest{Group: &group}); status.Code(err) != codes.InvalidArgument {
		t.Errorf("malformed group code = %v, want InvalidArgument", status.Code(err))
	}

	huge := make([]string, maxPluralFilter+1)
	for i := range huge {
		huge[i] = "pods"
	}
	_, err := s.ListKinds(ctx, &axiomv1.ListKindsRequest{Plurals: huge})
	if status.Code(err) != codes.InvalidArgument {
		t.Errorf("oversized filter code = %v, want InvalidArgument", status.Code(err))
	}
}

func TestListKindsWithoutClusterIsLoudNotEmpty(t *testing.T) {
	t.Parallel()
	s := New("t", nil, k8s.Unconfigured{}, nil)
	resp, err := s.ListKinds(context.Background(), &axiomv1.ListKindsRequest{})
	if err == nil {
		t.Fatalf("want an error, got %d kinds: an empty list would read as "+
			"'this cluster has nothing' instead of 'this gateway has no cluster'", len(resp.GetKinds()))
	}
	if status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("code = %v, want FailedPrecondition", status.Code(err))
	}
}

func TestDiscoveryErrorsMapThroughToGRPC(t *testing.T) {
	t.Parallel()
	s := New("t", nil, errClient{err: errors.New("dial tcp: connection refused")}, nil)
	_, err := s.DiscoverSchema(context.Background(), &axiomv1.DiscoverSchemaRequest{
		Gvk: &axiomv1.GroupVersionKind{Version: "v1", Kind: "Pod"},
	})
	if status.Code(err) != codes.Unavailable {
		t.Fatalf("code = %v, want Unavailable", status.Code(err))
	}
	if _, err := s.ListKinds(context.Background(), &axiomv1.ListKindsRequest{}); status.Code(err) != codes.Unavailable {
		t.Fatalf("code = %v, want Unavailable", status.Code(err))
	}
}
