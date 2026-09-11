package server

import (
	"context"
	"encoding/json"
	"strings"
	"testing"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	dynamicfake "k8s.io/client-go/dynamic/fake"
	"k8s.io/client-go/kubernetes/scheme"

	axiomv1 "github.com/dhilipkumars/axiom/gateway/gen/axiom/v1"
	"github.com/dhilipkumars/axiom/gateway/internal/k8s"
)

var cmGVK = &axiomv1.GroupVersionKind{Version: "v1", Kind: "ConfigMap"}

func newWriteServer(t *testing.T) *Server {
	t.Helper()
	return New("t", nil, k8s.NewDynamic(dynamicfake.NewSimpleDynamicClient(scheme.Scheme)), nil)
}

func dataOf(t *testing.T, o *axiomv1.Object) map[string]any {
	t.Helper()
	var m map[string]any
	if err := json.Unmarshal(o.GetJson(), &m); err != nil {
		t.Fatal(err)
	}
	d, _ := m["data"].(map[string]any)
	return d
}

func TestCreateUpdateDeleteRoundTrip(t *testing.T) {
	t.Parallel()
	srv := newWriteServer(t)
	ctx := context.Background()

	cr, err := srv.Create(ctx, &axiomv1.CreateRequest{Gvk: cmGVK, Namespace: "default", Name: "app",
		Json: []byte(`{"data":{"LOG_LEVEL":"info"}}`)})
	if err != nil {
		t.Fatal(err)
	}
	if cr.GetObject().GetName() != "app" || cr.GetObject().GetNamespace() != "default" {
		t.Fatalf("created identity = %+v", cr.GetObject())
	}
	if dataOf(t, cr.GetObject())["LOG_LEVEL"] != "info" {
		t.Fatalf("created data = %v", dataOf(t, cr.GetObject()))
	}
	var m map[string]any
	_ = json.Unmarshal(cr.GetObject().GetJson(), &m)
	if m["apiVersion"] != "v1" || m["kind"] != "ConfigMap" {
		t.Fatalf("apiVersion/kind not pinned from gvk: %v %v", m["apiVersion"], m["kind"])
	}

	_, err = srv.Create(ctx, &axiomv1.CreateRequest{Gvk: cmGVK, Namespace: "default", Name: "app", Json: []byte(`{}`)})
	if status.Code(err) != codes.AlreadyExists {
		t.Fatalf("duplicate create code = %v, want AlreadyExists", status.Code(err))
	}

	// The fake tracker does not enforce resourceVersion, so this exercises the
	// happy path; the real 409 → Aborted mapping is covered by TestErrorMapping
	// and by the Phase 2 E2E against a kind cluster.
	up, err := srv.Update(ctx, &axiomv1.UpdateRequest{Gvk: cmGVK, Namespace: "default", Name: "app", ResourceVersion: "1",
		Json: []byte(`{"data":{"LOG_LEVEL":"debug"}}`)})
	if err != nil {
		t.Fatal(err)
	}
	if dataOf(t, up.GetObject())["LOG_LEVEL"] != "debug" {
		t.Fatalf("updated data = %v", dataOf(t, up.GetObject()))
	}

	if _, err := srv.Delete(ctx, &axiomv1.DeleteRequest{Gvk: cmGVK, Namespace: "default", Name: "app"}); err != nil {
		t.Fatal(err)
	}
	_, err = srv.Delete(ctx, &axiomv1.DeleteRequest{Gvk: cmGVK, Namespace: "default", Name: "app"})
	if status.Code(err) != codes.NotFound {
		t.Fatalf("second delete code = %v, want NotFound", status.Code(err))
	}
	_, err = srv.Update(ctx, &axiomv1.UpdateRequest{Gvk: cmGVK, Namespace: "default", Name: "app", ResourceVersion: "1", Json: []byte(`{}`)})
	if status.Code(err) != codes.NotFound {
		t.Fatalf("update after delete code = %v, want NotFound", status.Code(err))
	}
}

func TestWriteValidation(t *testing.T) {
	t.Parallel()
	srv := newWriteServer(t)
	ctx := context.Background()
	big := `{"data":{"k":"` + strings.Repeat("x", maxBodyBytes) + `"}}`

	creates := []struct {
		name string
		req  *axiomv1.CreateRequest
		want codes.Code
		msg  string
	}{
		{name: "nil", req: nil, want: codes.InvalidArgument},
		{name: "missing gvk", req: &axiomv1.CreateRequest{Namespace: "d", Name: "a", Json: []byte(`{}`)}, want: codes.InvalidArgument},
		{name: "missing name", req: &axiomv1.CreateRequest{Gvk: cmGVK, Namespace: "d", Json: []byte(`{}`)}, want: codes.InvalidArgument, msg: "name is required"},
		{name: "bad name", req: &axiomv1.CreateRequest{Gvk: cmGVK, Namespace: "d", Name: "A/b", Json: []byte(`{}`)}, want: codes.InvalidArgument},
		{name: "empty body", req: &axiomv1.CreateRequest{Gvk: cmGVK, Namespace: "d", Name: "a"}, want: codes.InvalidArgument, msg: "json body is required"},
		{name: "not json", req: &axiomv1.CreateRequest{Gvk: cmGVK, Namespace: "d", Name: "a", Json: []byte(`[1,2]`)}, want: codes.InvalidArgument, msg: "not a JSON object"},
		{name: "null body must not panic", req: &axiomv1.CreateRequest{Gvk: cmGVK, Namespace: "d", Name: "a", Json: []byte(`null`)}, want: codes.InvalidArgument, msg: "got null"},
		{name: "metadata not object", req: &axiomv1.CreateRequest{Gvk: cmGVK, Namespace: "d", Name: "a", Json: []byte(`{"metadata":"x"}`)}, want: codes.InvalidArgument, msg: "metadata must be an object"},
		{name: "name mismatch", req: &axiomv1.CreateRequest{Gvk: cmGVK, Namespace: "d", Name: "a", Json: []byte(`{"metadata":{"name":"b"}}`)}, want: codes.InvalidArgument, msg: "does not match request name"},
		{name: "namespace mismatch", req: &axiomv1.CreateRequest{Gvk: cmGVK, Namespace: "d", Name: "a", Json: []byte(`{"metadata":{"namespace":"e"}}`)}, want: codes.InvalidArgument, msg: "does not match request namespace"},
		{name: "too large", req: &axiomv1.CreateRequest{Gvk: cmGVK, Namespace: "d", Name: "a", Json: []byte(big)}, want: codes.InvalidArgument, msg: "exceeds"},
		{name: "unsupported kind", req: &axiomv1.CreateRequest{Gvk: &axiomv1.GroupVersionKind{Version: "v1", Kind: "Secret"}, Namespace: "d", Name: "a", Json: []byte(`{}`)}, want: codes.InvalidArgument, msg: "unsupported kind"},
	}
	for _, tc := range creates {
		t.Run("create/"+tc.name, func(t *testing.T) {
			t.Parallel()
			_, err := srv.Create(ctx, tc.req)
			if got := status.Code(err); got != tc.want {
				t.Fatalf("code = %v, want %v (err=%v)", got, tc.want, err)
			}
			if tc.msg != "" && !strings.Contains(err.Error(), tc.msg) {
				t.Fatalf("err = %v, want substring %q", err, tc.msg)
			}
		})
	}

	updates := []struct {
		name string
		req  *axiomv1.UpdateRequest
		msg  string
	}{
		{name: "nil", req: nil},
		{name: "missing resource_version", req: &axiomv1.UpdateRequest{Gvk: cmGVK, Namespace: "d", Name: "a", Json: []byte(`{}`)}, msg: "resource_version is required"},
		{name: "body rv mismatch", req: &axiomv1.UpdateRequest{Gvk: cmGVK, Namespace: "d", Name: "a", ResourceVersion: "7", Json: []byte(`{"metadata":{"resourceVersion":"6"}}`)}, msg: "does not match request resource_version"},
		{name: "null body must not panic", req: &axiomv1.UpdateRequest{Gvk: cmGVK, Namespace: "d", Name: "a", ResourceVersion: "7", Json: []byte(`null`)}, msg: "got null"},
		{name: "missing name", req: &axiomv1.UpdateRequest{Gvk: cmGVK, Namespace: "d", ResourceVersion: "7", Json: []byte(`{}`)}, msg: "name is required"},
	}
	for _, tc := range updates {
		t.Run("update/"+tc.name, func(t *testing.T) {
			t.Parallel()
			_, err := srv.Update(ctx, tc.req)
			if got := status.Code(err); got != codes.InvalidArgument {
				t.Fatalf("code = %v, want InvalidArgument (err=%v)", got, err)
			}
			if tc.msg != "" && !strings.Contains(err.Error(), tc.msg) {
				t.Fatalf("err = %v, want substring %q", err, tc.msg)
			}
		})
	}

	deletes := []*axiomv1.DeleteRequest{
		nil,
		{Namespace: "d", Name: "a"},
		{Gvk: cmGVK, Namespace: "d"},
		{Gvk: cmGVK, Namespace: "d", Name: "Bad_Name"},
	}
	for i, req := range deletes {
		_, err := srv.Delete(ctx, req)
		if got := status.Code(err); got != codes.InvalidArgument {
			t.Fatalf("delete[%d] code = %v, want InvalidArgument (err=%v)", i, got, err)
		}
	}
}

func TestWritesWithoutCluster(t *testing.T) {
	t.Parallel()
	srv := New("t", nil, nil, nil)
	ctx := context.Background()
	if _, err := srv.Create(ctx, &axiomv1.CreateRequest{Gvk: cmGVK, Namespace: "d", Name: "a", Json: []byte(`{}`)}); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("create code = %v", status.Code(err))
	}
	if _, err := srv.Update(ctx, &axiomv1.UpdateRequest{Gvk: cmGVK, Namespace: "d", Name: "a", ResourceVersion: "1", Json: []byte(`{}`)}); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("update code = %v", status.Code(err))
	}
	if _, err := srv.Delete(ctx, &axiomv1.DeleteRequest{Gvk: cmGVK, Namespace: "d", Name: "a"}); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("delete code = %v", status.Code(err))
	}
}
