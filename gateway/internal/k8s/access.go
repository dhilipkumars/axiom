package k8s

import (
	"context"
	"fmt"
	"sync"
	"sync/atomic"

	authv1 "k8s.io/api/authorization/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime/schema"
	authv1client "k8s.io/client-go/kubernetes/typed/authorization/v1"
)

// AccessChecker answers whether the gateway's own identity may perform a verb
// on a resource.
//
// This is what lets the served set follow RBAC instead of a hand-maintained
// list. Phase 4 bounded discovery with a `--serve` allowlist that had to be
// kept in step with the ServiceAccount's ClusterRole by hand -- two sources of
// truth that drift. The API server already knows the answer authoritatively,
// so it is asked rather than duplicated.
//
// An interface so the enumeration path is unit-testable without a cluster
// (docs/RULES.md §2).
type AccessChecker interface {
	// CanList reports whether the caller may list gvr across all namespaces.
	//
	// An error means the question could not be answered, which is different
	// from a "no": callers must not treat a failed check as a denial, or a
	// blip in the authorization API would silently shrink the served set
	// (docs/RULES.md §1).
	CanList(ctx context.Context, gvr schema.GroupVersionResource) (bool, error)
}

// SelfAccess is an AccessChecker backed by SelfSubjectAccessReview.
//
// SSAR is granted to every authenticated identity by the built-in
// `system:basic-user` ClusterRole, so this needs no permission beyond what the
// gateway already has.
//
// Allowed answers are cached for the process lifetime: an import asks about
// every served kind at once, and RBAC does not change mid-import. Denials are
// not cached. The shipped read role is an aggregated ClusterRole that the
// controller fills in asynchronously after it is applied, so a gateway that
// starts inside that window would otherwise cache "no" for pods and keep it
// until restarted. Re-asking costs one review per denied kind per import, and
// it also means a newly granted kind -- a labelled ClusterRole for a CRD, say --
// appears without a restart. A revoked grant still needs one.
type SelfAccess struct {
	client authv1client.SelfSubjectAccessReviewInterface

	reviews atomic.Uint64

	mu     sync.Mutex
	cached map[schema.GroupVersionResource]bool // allowed answers only
}

// AccessReviews reports the number of SelfSubjectAccessReview calls issued to the
// API server so far.
func (s *SelfAccess) AccessReviews() uint64 {
	return s.reviews.Load()
}

// NewSelfAccess builds an AccessChecker over the authorization API.
func NewSelfAccess(c authv1client.SelfSubjectAccessReviewInterface) *SelfAccess {
	return &SelfAccess{
		client: c,
		cached: make(map[schema.GroupVersionResource]bool),
	}
}

// CanList implements AccessChecker.
func (s *SelfAccess) CanList(ctx context.Context, gvr schema.GroupVersionResource) (bool, error) {
	s.mu.Lock()
	if allowed, ok := s.cached[gvr]; ok {
		s.mu.Unlock()
		return allowed, nil
	}
	s.mu.Unlock()

	// Empty namespace means "all namespaces" for a SelfSubjectAccessReview on
	// a namespaced resource, which is the scope the gateway serves.
	review := &authv1.SelfSubjectAccessReview{
		Spec: authv1.SelfSubjectAccessReviewSpec{
			ResourceAttributes: &authv1.ResourceAttributes{
				Verb:     "list",
				Group:    gvr.Group,
				Version:  gvr.Version,
				Resource: gvr.Resource,
			},
		},
	}
	s.reviews.Add(1)
	got, err := s.client.Create(ctx, review, metav1.CreateOptions{})
	if err != nil {
		return false, fmt.Errorf("access review for %s: %w", gvr.String(), err)
	}
	// EvaluationError means the authorizer could not answer -- a webhook that
	// failed, say. Allowed is false in that case, but it is false because the
	// question went unanswered, not because the answer was no. Treating it as
	// a denial would silently drop the kind, which is exactly what CanList's
	// contract forbids.
	if e := got.Status.EvaluationError; e != "" {
		return false, fmt.Errorf("access review for %s was not evaluated: %s", gvr.String(), e)
	}
	allowed := got.Status.Allowed
	if allowed {
		s.mu.Lock()
		s.cached[gvr] = true
		s.mu.Unlock()
	}
	return allowed, nil
}

// AllowAll is an AccessChecker that permits everything.
//
// It is the fallback when the gateway has no authorization client, and the
// default in unit tests that are not about RBAC filtering. It never hides a
// kind, so a misconfiguration shows up as a permission error at query time
// rather than as a silently missing table.
type AllowAll struct{}

// CanList implements AccessChecker.
func (AllowAll) CanList(context.Context, schema.GroupVersionResource) (bool, error) {
	return true, nil
}

// DenyAll is an AccessChecker that permits nothing, for tests.
type DenyAll struct{}

// CanList implements AccessChecker.
func (DenyAll) CanList(context.Context, schema.GroupVersionResource) (bool, error) {
	return false, nil
}
