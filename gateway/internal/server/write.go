package server

import (
	"context"
	"encoding/json"
	"log/slog"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime/schema"

	axiomv1 "github.com/dhilipkumars/axiom/gateway/gen/axiom/v1"
)

// maxBodyBytes bounds an inbound object body. gRPC already caps messages at
// 4 MiB; this is enforced here too so the handler never relies on transport
// configuration for it (docs/RULES.md §3).
const maxBodyBytes = 4 * 1024 * 1024

// identity is the validated (gvk, namespace, name) triple shared by all writes.
type identity struct {
	gvk       schema.GroupVersionKind
	namespace string
	name      string
}

// writeIdentity validates the common write fields. Name is always required.
func writeIdentity(op string, g *axiomv1.GroupVersionKind, namespace, name string) (identity, error) {
	gvk, err := gvkFromProto(g)
	if err != nil {
		return identity{}, err
	}
	if name == "" {
		return identity{}, status.Errorf(codes.InvalidArgument, "%s: name is required", op)
	}
	if err := validName("namespace", namespace); err != nil {
		return identity{}, err
	}
	if err := validName("name", name); err != nil {
		return identity{}, err
	}
	return identity{gvk: gvk, namespace: namespace, name: name}, nil
}

// decodeBody parses an untrusted object body and pins its identity to the
// request: apiVersion/kind are forced from gvk; metadata.name/namespace, if
// present, must equal the request's; if absent they are filled in. The body
// is never interpreted beyond JSON decoding.
func decodeBody(op string, id identity, body []byte) (*unstructured.Unstructured, error) {
	if len(body) == 0 {
		return nil, status.Errorf(codes.InvalidArgument, "%s: json body is required", op)
	}
	if len(body) > maxBodyBytes {
		return nil, status.Errorf(codes.InvalidArgument, "%s: json body of %d bytes exceeds %d byte limit", op, len(body), maxBodyBytes)
	}
	var m map[string]any
	if err := json.Unmarshal(body, &m); err != nil {
		return nil, status.Errorf(codes.InvalidArgument, "%s: json body is not a JSON object: %v", op, err)
	}
	if m == nil {
		// `null` unmarshals into a nil map without error; assigning into it would panic.
		return nil, status.Errorf(codes.InvalidArgument, "%s: json body must be a JSON object, got null", op)
	}
	u := &unstructured.Unstructured{Object: m}
	if _, ok := m["metadata"]; !ok {
		u.Object["metadata"] = map[string]any{}
	} else if _, ok := m["metadata"].(map[string]any); !ok {
		return nil, status.Errorf(codes.InvalidArgument, "%s: metadata must be an object", op)
	}
	if n := u.GetName(); n != "" && n != id.name {
		return nil, status.Errorf(codes.InvalidArgument, "%s: body metadata.name %q does not match request name %q", op, n, id.name)
	}
	if ns := u.GetNamespace(); ns != "" && ns != id.namespace {
		return nil, status.Errorf(codes.InvalidArgument, "%s: body metadata.namespace %q does not match request namespace %q", op, ns, id.namespace)
	}
	u.SetName(id.name)
	if id.namespace != "" {
		u.SetNamespace(id.namespace)
	}
	u.SetAPIVersion(id.gvk.GroupVersion().String())
	u.SetKind(id.gvk.Kind)
	return u, nil
}

// Create creates one object. Contract: see axiom.proto.
func (s *Server) Create(ctx context.Context, req *axiomv1.CreateRequest) (*axiomv1.CreateResponse, error) {
	s.createCalls.Add(1)
	if req == nil {
		return nil, status.Error(codes.InvalidArgument, "create: request must not be nil")
	}
	id, err := writeIdentity("create", req.GetGvk(), req.GetNamespace(), req.GetName())
	if err != nil {
		return nil, err
	}
	obj, err := decodeBody("create", id, req.GetJson())
	if err != nil {
		return nil, err
	}
	out, err := s.k8s.Create(ctx, id.gvk, id.namespace, obj)
	if err != nil {
		return nil, toGRPC(err)
	}
	po, err := objectToProto(out)
	if err != nil {
		return nil, err
	}
	s.logWrite(ctx, "create", id, out.GetResourceVersion())
	return &axiomv1.CreateResponse{Object: po}, nil
}

// Update replaces one object with optimistic concurrency. Contract: see
// axiom.proto. A stale resource_version surfaces as codes.Aborted.
func (s *Server) Update(ctx context.Context, req *axiomv1.UpdateRequest) (*axiomv1.UpdateResponse, error) {
	s.updateCalls.Add(1)
	if req == nil {
		return nil, status.Error(codes.InvalidArgument, "update: request must not be nil")
	}
	id, err := writeIdentity("update", req.GetGvk(), req.GetNamespace(), req.GetName())
	if err != nil {
		return nil, err
	}
	if req.GetResourceVersion() == "" {
		return nil, status.Error(codes.InvalidArgument, "update: resource_version is required (read the object first)")
	}
	obj, err := decodeBody("update", id, req.GetJson())
	if err != nil {
		return nil, err
	}
	if rv := obj.GetResourceVersion(); rv != "" && rv != req.GetResourceVersion() {
		return nil, status.Errorf(codes.InvalidArgument, "update: body metadata.resourceVersion %q does not match request resource_version %q", rv, req.GetResourceVersion())
	}
	obj.SetResourceVersion(req.GetResourceVersion())
	out, err := s.k8s.Update(ctx, id.gvk, id.namespace, obj)
	if err != nil {
		return nil, toGRPC(err)
	}
	po, err := objectToProto(out)
	if err != nil {
		return nil, err
	}
	s.logWrite(ctx, "update", id, out.GetResourceVersion())
	return &axiomv1.UpdateResponse{Object: po}, nil
}

// Delete deletes one object. Contract: see axiom.proto.
func (s *Server) Delete(ctx context.Context, req *axiomv1.DeleteRequest) (*axiomv1.DeleteResponse, error) {
	s.deleteCalls.Add(1)
	if req == nil {
		return nil, status.Error(codes.InvalidArgument, "delete: request must not be nil")
	}
	id, err := writeIdentity("delete", req.GetGvk(), req.GetNamespace(), req.GetName())
	if err != nil {
		return nil, err
	}
	if err := s.k8s.Delete(ctx, id.gvk, id.namespace, id.name); err != nil {
		return nil, toGRPC(err)
	}
	s.logWrite(ctx, "delete", id, "")
	return &axiomv1.DeleteResponse{}, nil
}

func (s *Server) logWrite(ctx context.Context, op string, id identity, rv string) {
	s.log.LogAttrs(ctx, slog.LevelInfo, op,
		slog.String("gvk", id.gvk.String()),
		slog.String("namespace", id.namespace),
		slog.String("name", id.name),
		slog.String("resource_version", rv))
}
