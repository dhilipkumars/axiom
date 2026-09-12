package k8s

import (
	"context"
	"errors"
	"strings"
	"sync/atomic"
	"testing"

	authv1 "k8s.io/api/authorization/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/runtime/schema"
	fakeclient "k8s.io/client-go/kubernetes/fake"
	k8stesting "k8s.io/client-go/testing"
)

var podsGVR = schema.GroupVersionResource{Group: "", Version: "v1", Resource: "pods"}

// ssarClient builds a SelfAccess over a fake clientset whose
// SelfSubjectAccessReview responses come from `reply`, and returns the
// reviews it was asked to evaluate.
func ssarClient(t *testing.T, reply func(*authv1.SelfSubjectAccessReview) (*authv1.SelfSubjectAccessReview, error)) (*SelfAccess, *[]*authv1.SelfSubjectAccessReview, *atomic.Int32) {
	t.Helper()
	cs := fakeclient.NewSimpleClientset()
	var seen []*authv1.SelfSubjectAccessReview
	var calls atomic.Int32
	cs.PrependReactor("create", "selfsubjectaccessreviews",
		func(action k8stesting.Action) (bool, runtime.Object, error) {
			calls.Add(1)
			in := action.(k8stesting.CreateAction).GetObject().(*authv1.SelfSubjectAccessReview)
			seen = append(seen, in)
			out, err := reply(in)
			return true, out, err
		})
	return NewSelfAccess(cs.AuthorizationV1().SelfSubjectAccessReviews()), &seen, &calls
}

// allow returns a canned SSAR response.
func allow(allowed bool) func(*authv1.SelfSubjectAccessReview) (*authv1.SelfSubjectAccessReview, error) {
	return func(in *authv1.SelfSubjectAccessReview) (*authv1.SelfSubjectAccessReview, error) {
		out := in.DeepCopy()
		out.Status = authv1.SubjectAccessReviewStatus{Allowed: allowed}
		return out, nil
	}
}

func TestSelfAccessBuildsTheReviewFromTheResource(t *testing.T) {
	t.Parallel()
	a, seen, _ := ssarClient(t, allow(true))
	gvr := schema.GroupVersionResource{Group: "apps", Version: "v1", Resource: "deployments"}
	if _, err := a.CanList(context.Background(), gvr); err != nil {
		t.Fatal(err)
	}
	if len(*seen) != 1 {
		t.Fatalf("issued %d reviews, want 1", len(*seen))
	}
	got := (*seen)[0].Spec.ResourceAttributes
	if got == nil {
		t.Fatal("review carried no resource attributes")
	}
	if got.Verb != "list" {
		t.Errorf("verb = %q, want list", got.Verb)
	}
	if got.Group != "apps" || got.Version != "v1" || got.Resource != "deployments" {
		t.Errorf("resource attributes = %+v, want apps/v1 deployments", got)
	}
	// Empty namespace means "all namespaces" for a SelfSubjectAccessReview,
	// which is the scope the gateway serves. A namespace here would ask a
	// narrower question than the one that matters.
	if got.Namespace != "" {
		t.Errorf("namespace = %q, want empty (all namespaces)", got.Namespace)
	}
}

func TestSelfAccessReportsAllowedAndDenied(t *testing.T) {
	t.Parallel()
	for _, want := range []bool{true, false} {
		a, _, _ := ssarClient(t, allow(want))
		got, err := a.CanList(context.Background(), podsGVR)
		if err != nil {
			t.Fatalf("CanList = %v", err)
		}
		if got != want {
			t.Errorf("CanList = %v, want %v", got, want)
		}
	}
}

func TestSelfAccessCachesBothAnswers(t *testing.T) {
	t.Parallel()
	// A denial must be cached too: an import asks about every kind, and
	// re-asking the denied ones would double the round-trips for no benefit.
	for _, answer := range []bool{true, false} {
		a, _, calls := ssarClient(t, allow(answer))
		for range 4 {
			if _, err := a.CanList(context.Background(), podsGVR); err != nil {
				t.Fatal(err)
			}
		}
		if got := calls.Load(); got != 1 {
			t.Errorf("allowed=%v: issued %d reviews for 4 calls, want 1", answer, got)
		}
	}
}

func TestSelfAccessDistinctResourcesAreAskedSeparately(t *testing.T) {
	t.Parallel()
	a, _, calls := ssarClient(t, allow(true))
	ctx := context.Background()
	for _, gvr := range []schema.GroupVersionResource{
		podsGVR,
		{Group: "", Version: "v1", Resource: "secrets"},
		{Group: "apps", Version: "v1", Resource: "deployments"},
	} {
		if _, err := a.CanList(ctx, gvr); err != nil {
			t.Fatal(err)
		}
	}
	if got := calls.Load(); got != 3 {
		t.Errorf("issued %d reviews for 3 distinct resources, want 3", got)
	}
}

func TestSelfAccessTransportErrorIsNotADenial(t *testing.T) {
	t.Parallel()
	a, _, calls := ssarClient(t, func(*authv1.SelfSubjectAccessReview) (*authv1.SelfSubjectAccessReview, error) {
		return nil, errors.New("connection refused")
	})
	_, err := a.CanList(context.Background(), podsGVR)
	if err == nil {
		t.Fatal("a failed review must be an error, not a denial: callers fail the " +
			"enumeration rather than silently dropping the kind")
	}
	if !strings.Contains(err.Error(), "pods") {
		t.Errorf("err = %v, want it to name the resource", err)
	}
	// A failure must not be cached, or one blip would hide the kind for the
	// rest of the process's life.
	_, _ = a.CanList(context.Background(), podsGVR)
	if got := calls.Load(); got != 2 {
		t.Errorf("issued %d reviews, want 2: a failed check must not be cached", got)
	}
}

func TestSelfAccessUnevaluatedReviewIsAnErrorNotADenial(t *testing.T) {
	t.Parallel()
	// The authorizer answered, but could not evaluate the question -- a failed
	// webhook, typically. Allowed is false, yet that false means "unknown".
	a, _, calls := ssarClient(t, func(in *authv1.SelfSubjectAccessReview) (*authv1.SelfSubjectAccessReview, error) {
		out := in.DeepCopy()
		out.Status = authv1.SubjectAccessReviewStatus{
			Allowed:         false,
			EvaluationError: "webhook authorizer unavailable",
		}
		return out, nil
	})
	_, err := a.CanList(context.Background(), podsGVR)
	if err == nil {
		t.Fatal("an unevaluated review must be an error: treating it as a denial " +
			"silently drops the kind, which CanList's contract forbids")
	}
	if !strings.Contains(err.Error(), "webhook authorizer unavailable") {
		t.Errorf("err = %v, want it to carry the evaluation error", err)
	}
	_, _ = a.CanList(context.Background(), podsGVR)
	if got := calls.Load(); got != 2 {
		t.Errorf("issued %d reviews, want 2: an unevaluated answer must not be cached", got)
	}
}

func TestAllowAllAndDenyAll(t *testing.T) {
	t.Parallel()
	ok, err := AllowAll{}.CanList(context.Background(), podsGVR)
	if !ok || err != nil {
		t.Errorf("AllowAll = %v, %v, want true, nil", ok, err)
	}
	ok, err = DenyAll{}.CanList(context.Background(), podsGVR)
	if ok || err != nil {
		t.Errorf("DenyAll = %v, %v, want false, nil", ok, err)
	}
}

func TestSelfAccessTracksReviews(t *testing.T) {
	t.Parallel()
	a, _, _ := ssarClient(t, allow(true))
	if got := a.AccessReviews(); got != 0 {
		t.Fatalf("initial reviews = %d, want 0", got)
	}
	if _, err := a.CanList(context.Background(), podsGVR); err != nil {
		t.Fatal(err)
	}
	if got := a.AccessReviews(); got != 1 {
		t.Fatalf("reviews after 1 CanList = %d, want 1", got)
	}
	// Cached query does not increment reviews.
	if _, err := a.CanList(context.Background(), podsGVR); err != nil {
		t.Fatal(err)
	}
	if got := a.AccessReviews(); got != 1 {
		t.Fatalf("reviews after cached CanList = %d, want 1", got)
	}
}
