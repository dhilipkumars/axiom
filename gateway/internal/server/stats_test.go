package server

import (
	"context"
	"sync/atomic"
	"testing"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	dynamicfake "k8s.io/client-go/dynamic/fake"
	"k8s.io/client-go/kubernetes/scheme"

	axiomv1 "github.com/dhilipkumars/axiom/gateway/gen/axiom/v1"
	"github.com/dhilipkumars/axiom/gateway/internal/k8s"
)

type fakeStatsClient struct {
	k8s.Client
	fetches       atomic.Uint64
	groupVersions atomic.Uint64
	reviews       atomic.Uint64
}

func (f *fakeStatsClient) OpenAPIFetches() uint64 {
	return f.fetches.Load()
}

func (f *fakeStatsClient) OpenAPIGroupVersions() uint64 {
	return f.groupVersions.Load()
}

func (f *fakeStatsClient) AccessReviews() uint64 {
	return f.reviews.Load()
}

func TestStatsHandler(t *testing.T) {
	t.Parallel()

	fixedTime := time.Unix(1700000000, 0)
	clock := func() time.Time { return fixedTime }

	dyn := dynamicfake.NewSimpleDynamicClient(scheme.Scheme,
		testPod("default", "web", "Running", "node-a"))
	baseClient := k8s.NewDynamic(dyn, k8s.NewStaticMapper(k8s.BuiltinKinds()...))
	fakeStats := &fakeStatsClient{Client: baseClient}
	fakeStats.fetches.Store(4)
	fakeStats.groupVersions.Store(2)
	fakeStats.reviews.Store(10)

	srv := New("test-version", clock, fakeStats, nil)
	ctx := context.Background()

	t.Run("nil request returns InvalidArgument", func(t *testing.T) {
		_, err := srv.Stats(ctx, nil)
		if status.Code(err) != codes.InvalidArgument {
			t.Fatalf("code = %v, want %v", status.Code(err), codes.InvalidArgument)
		}
	})

	t.Run("initial counters are zero and start time is recorded", func(t *testing.T) {
		resp, err := srv.Stats(ctx, &axiomv1.StatsRequest{})
		if err != nil {
			t.Fatalf("Stats() failed: %v", err)
		}
		if resp.GetStartedAtUnixSeconds() != fixedTime.Unix() {
			t.Errorf("StartedAtUnixSeconds = %d, want %d", resp.GetStartedAtUnixSeconds(), fixedTime.Unix())
		}
		if resp.GetGetCalls() != 0 {
			t.Errorf("GetCalls = %d, want 0", resp.GetGetCalls())
		}
		if resp.GetListCalls() != 0 {
			t.Errorf("ListCalls = %d, want 0", resp.GetListCalls())
		}
		if resp.GetCreateCalls() != 0 {
			t.Errorf("CreateCalls = %d, want 0", resp.GetCreateCalls())
		}
		if resp.GetUpdateCalls() != 0 {
			t.Errorf("UpdateCalls = %d, want 0", resp.GetUpdateCalls())
		}
		if resp.GetDeleteCalls() != 0 {
			t.Errorf("DeleteCalls = %d, want 0", resp.GetDeleteCalls())
		}
		if resp.GetSubscribeCalls() != 0 {
			t.Errorf("SubscribeCalls = %d, want 0", resp.GetSubscribeCalls())
		}
		if resp.GetSubscribeListCalls() != 0 {
			t.Errorf("SubscribeListCalls = %d, want 0", resp.GetSubscribeListCalls())
		}
		if resp.GetOpenapiFetches() != 4 {
			t.Errorf("OpenapiFetches = %d, want 4", resp.GetOpenapiFetches())
		}
		if resp.GetOpenapiGroupVersions() != 2 {
			t.Errorf("OpenapiGroupVersions = %d, want 2", resp.GetOpenapiGroupVersions())
		}
		if resp.GetAccessReviews() != 10 {
			t.Errorf("AccessReviews = %d, want 10", resp.GetAccessReviews())
		}
	})

	t.Run("Stats does not mutate counters", func(t *testing.T) {
		before, err := srv.Stats(ctx, &axiomv1.StatsRequest{})
		if err != nil {
			t.Fatal(err)
		}
		after, err := srv.Stats(ctx, &axiomv1.StatsRequest{})
		if err != nil {
			t.Fatal(err)
		}
		if before.GetGetCalls() != after.GetGetCalls() ||
			before.GetListCalls() != after.GetListCalls() ||
			before.GetCreateCalls() != after.GetCreateCalls() ||
			before.GetUpdateCalls() != after.GetUpdateCalls() ||
			before.GetDeleteCalls() != after.GetDeleteCalls() ||
			before.GetSubscribeCalls() != after.GetSubscribeCalls() ||
			before.GetSubscribeListCalls() != after.GetSubscribeListCalls() {
			t.Errorf("Stats mutated counters: before=%+v, after=%+v", before, after)
		}
	})

	t.Run("each handler increments its corresponding counter", func(t *testing.T) {
		handlers := []struct {
			name      string
			invoke    func()
			checkStat func(*axiomv1.StatsResponse) uint64
			wantVal   uint64
		}{
			{
				name: "Get",
				invoke: func() {
					_, _ = srv.Get(ctx, &axiomv1.GetRequest{Gvk: podGVK, Namespace: "default", Name: "web"})
				},
				checkStat: func(s *axiomv1.StatsResponse) uint64 { return s.GetGetCalls() },
				wantVal:   1,
			},
			{
				name: "List",
				invoke: func() {
					_, _ = srv.List(ctx, &axiomv1.ListRequest{Gvk: podGVK, Namespace: "default"})
				},
				checkStat: func(s *axiomv1.StatsResponse) uint64 { return s.GetListCalls() },
				wantVal:   1,
			},
			{
				name: "Create",
				invoke: func() {
					body := []byte(`{"apiVersion":"v1","kind":"Pod","metadata":{"name":"newpod","namespace":"default"}}`)
					_, _ = srv.Create(ctx, &axiomv1.CreateRequest{Gvk: podGVK, Namespace: "default", Name: "newpod", Json: body})
				},
				checkStat: func(s *axiomv1.StatsResponse) uint64 { return s.GetCreateCalls() },
				wantVal:   1,
			},
			{
				name: "Update",
				invoke: func() {
					body := []byte(`{"apiVersion":"v1","kind":"Pod","metadata":{"name":"web","namespace":"default","resourceVersion":"7"}}`)
					_, _ = srv.Update(ctx, &axiomv1.UpdateRequest{Gvk: podGVK, Namespace: "default", Name: "web", ResourceVersion: "7", Json: body})
				},
				checkStat: func(s *axiomv1.StatsResponse) uint64 { return s.GetUpdateCalls() },
				wantVal:   1,
			},
			{
				name: "Delete",
				invoke: func() {
					_, _ = srv.Delete(ctx, &axiomv1.DeleteRequest{Gvk: podGVK, Namespace: "default", Name: "web"})
				},
				checkStat: func(s *axiomv1.StatsResponse) uint64 { return s.GetDeleteCalls() },
				wantVal:   1,
			},
		}

		for _, tc := range handlers {
			t.Run(tc.name, func(t *testing.T) {
				tc.invoke()
				resp, err := srv.Stats(ctx, &axiomv1.StatsRequest{})
				if err != nil {
					t.Fatal(err)
				}
				if got := tc.checkStat(resp); got != tc.wantVal {
					t.Errorf("%s counter = %d, want %d", tc.name, got, tc.wantVal)
				}
			})
		}
	})

	t.Run("Subscribe counter behavior on fresh vs resumed stream", func(t *testing.T) {
		subSrv := New("test", clock, baseClient, nil)

		// Fresh subscribe (empty resource_version)
		rec1 := newRecorder(ctx)
		subCtx1, cancel1 := context.WithCancel(ctx)
		go func() {
			_ = subSrv.Subscribe(&axiomv1.SubscribeRequest{Gvk: podGVK, Namespace: "default", ResourceVersion: ""}, &recorder{ctx: subCtx1, events: rec1.events})
		}()
		// Wait for initial SYNCED event so listing is definitely done
		for {
			ev := rec1.next(t)
			if ev.Type == axiomv1.SubscribeResponse_TYPE_SYNCED {
				break
			}
		}
		cancel1()

		resp1, err := subSrv.Stats(ctx, &axiomv1.StatsRequest{})
		if err != nil {
			t.Fatal(err)
		}
		if resp1.GetSubscribeCalls() != 1 {
			t.Errorf("SubscribeCalls = %d, want 1", resp1.GetSubscribeCalls())
		}
		if resp1.GetSubscribeListCalls() != 1 {
			t.Errorf("SubscribeListCalls = %d, want 1", resp1.GetSubscribeListCalls())
		}

		// Resumed subscribe (non-empty resource_version)
		rec2 := newRecorder(ctx)
		subCtx2, cancel2 := context.WithCancel(ctx)
		go func() {
			_ = subSrv.Subscribe(&axiomv1.SubscribeRequest{Gvk: podGVK, Namespace: "default", ResourceVersion: "7"}, &recorder{ctx: subCtx2, events: rec2.events})
		}()
		// Cancel immediately after starting
		time.Sleep(10 * time.Millisecond)
		cancel2()

		resp2, err := subSrv.Stats(ctx, &axiomv1.StatsRequest{})
		if err != nil {
			t.Fatal(err)
		}
		if resp2.GetSubscribeCalls() != 2 {
			t.Errorf("SubscribeCalls = %d, want 2", resp2.GetSubscribeCalls())
		}
		if resp2.GetSubscribeListCalls() != 1 {
			t.Errorf("SubscribeListCalls = %d, want 1 (should NOT increment on resumed stream)", resp2.GetSubscribeListCalls())
		}
	})
}
