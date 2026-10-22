package server

import (
	"context"
	"io"
	"strings"
	"sync"
	"testing"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/types/known/timestamppb"
	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	dynamicfake "k8s.io/client-go/dynamic/fake"
	"k8s.io/client-go/kubernetes/scheme"

	axiomv1 "github.com/dhilipkumars/axiom/gateway/gen/axiom/v1"
	"github.com/dhilipkumars/axiom/gateway/internal/k8s"
)

// fakeLogs is a LogSource that serves fixed text and records what it was asked.
type fakeLogs struct {
	mu    sync.Mutex
	body  func() io.Reader
	err   error
	calls []*corev1.PodLogOptions
}

func (f *fakeLogs) Stream(_ context.Context, _, _ string, opts *corev1.PodLogOptions) (io.ReadCloser, error) {
	f.mu.Lock()
	f.calls = append(f.calls, opts)
	f.mu.Unlock()
	if f.err != nil {
		return nil, f.err
	}
	return io.NopCloser(f.body()), nil
}

func (f *fakeLogs) opts(t *testing.T) []*corev1.PodLogOptions {
	t.Helper()
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]*corev1.PodLogOptions(nil), f.calls...)
}

func textLogs(s string) *fakeLogs {
	return &fakeLogs{body: func() io.Reader { return strings.NewReader(s) }}
}

func running(name string) corev1.ContainerStatus {
	return corev1.ContainerStatus{
		Name:  name,
		State: corev1.ContainerState{Running: &corev1.ContainerStateRunning{}},
	}
}

func terminated(id string) *corev1.ContainerStateTerminated {
	return &corev1.ContainerStateTerminated{ContainerID: id}
}

// logPod is a pod with the named app containers, all running unless statuses
// says otherwise.
func logPod(name string, annotations map[string]string, statuses []corev1.ContainerStatus, containers ...string) *corev1.Pod {
	p := &corev1.Pod{
		TypeMeta:   metav1.TypeMeta{APIVersion: "v1", Kind: "Pod"},
		ObjectMeta: metav1.ObjectMeta{Namespace: "default", Name: name, Annotations: annotations},
	}
	for _, c := range containers {
		p.Spec.Containers = append(p.Spec.Containers, corev1.Container{Name: c})
	}
	if statuses == nil {
		for _, c := range containers {
			statuses = append(statuses, running(c))
		}
	}
	p.Status.ContainerStatuses = statuses
	return p
}

func newLogServer(t *testing.T, logs k8s.LogSource, pods ...runtime.Object) *Server {
	t.Helper()
	dyn := dynamicfake.NewSimpleDynamicClient(scheme.Scheme, pods...)
	srv := New("t", nil, k8s.NewDynamic(dyn, k8s.NewStaticMapper(k8s.BuiltinKinds()...)), nil)
	if logs != nil {
		srv.ServePodLogs(logs)
	}
	return srv
}

func logReq(pod string) *axiomv1.PodLogRequest {
	return &axiomv1.PodLogRequest{Namespace: "default", Pod: pod, TailLines: 100, MaxBytes: 1 << 20}
}

func texts(resp *axiomv1.PodLogResponse) []string {
	var out []string
	for _, l := range resp.GetLines() {
		out = append(out, l.GetText())
	}
	return out
}

func TestPodLogIsOffUnlessServed(t *testing.T) {
	t.Parallel()
	srv := newLogServer(t, nil, logPod("web", nil, nil, "app"))
	_, err := srv.PodLog(context.Background(), logReq("web"))
	if status.Code(err) != codes.FailedPrecondition || !strings.Contains(err.Error(), "--serve-pod-logs") {
		t.Fatalf("err = %v, want FAILED_PRECONDITION naming --serve-pod-logs", err)
	}
}

func TestPodLogRejectsBadRequests(t *testing.T) {
	t.Parallel()
	srv := newLogServer(t, textLogs("x\n"), logPod("web", nil, nil, "app"))
	with := func(f func(r *axiomv1.PodLogRequest)) *axiomv1.PodLogRequest {
		r := logReq("web")
		f(r)
		return r
	}
	tests := []struct {
		name string
		req  *axiomv1.PodLogRequest
		want codes.Code
	}{
		{"nil", nil, codes.InvalidArgument},
		{"no namespace", with(func(r *axiomv1.PodLogRequest) { r.Namespace = "" }), codes.InvalidArgument},
		{"no pod", with(func(r *axiomv1.PodLogRequest) { r.Pod = "" }), codes.InvalidArgument},
		{"bad pod", with(func(r *axiomv1.PodLogRequest) { r.Pod = "../x" }), codes.InvalidArgument},
		{"bad container", with(func(r *axiomv1.PodLogRequest) { r.Container = "a.b" }), codes.InvalidArgument},
		{"zero tail", with(func(r *axiomv1.PodLogRequest) { r.TailLines = 0 }), codes.InvalidArgument},
		{"tail over ceiling", with(func(r *axiomv1.PodLogRequest) { r.TailLines = maxLogTailLines + 1 }), codes.InvalidArgument},
		{"zero bytes", with(func(r *axiomv1.PodLogRequest) { r.MaxBytes = 0 }), codes.InvalidArgument},
		{"bytes over ceiling", with(func(r *axiomv1.PodLogRequest) { r.MaxBytes = MaxLogBytes + 1 }), codes.InvalidArgument},
		{"bad since", with(func(r *axiomv1.PodLogRequest) { r.SinceTime = &timestamppb.Timestamp{Nanos: -1} }), codes.InvalidArgument},
		{"unknown container", with(func(r *axiomv1.PodLogRequest) { r.Container = "nope" }), codes.InvalidArgument},
		{"no such pod", with(func(r *axiomv1.PodLogRequest) { r.Pod = "gone" }), codes.NotFound},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			_, err := srv.PodLog(context.Background(), tc.req)
			if got := status.Code(err); got != tc.want {
				t.Fatalf("code = %v, want %v (err=%v)", got, tc.want, err)
			}
		})
	}
}

func TestPodLogReadsTimestampedLines(t *testing.T) {
	t.Parallel()
	logs := textLogs("2026-10-19T12:00:00.5Z first line\n" +
		"2026-10-19T12:00:01Z second line\n" +
		"no timestamp here\n" +
		"2026-10-19T12:00:02Z last, with no newline")
	srv := newLogServer(t, logs, logPod("web", nil, nil, "app"))
	since := time.Date(2026, 10, 19, 11, 0, 0, 0, time.UTC)
	req := logReq("web")
	req.TailLines = 50
	req.SinceTime = timestamppb.New(since)
	resp, err := srv.PodLog(context.Background(), req)
	if err != nil {
		t.Fatal(err)
	}
	want := []string{"first line", "second line", "no timestamp here", "last, with no newline"}
	if got := texts(resp); strings.Join(got, "|") != strings.Join(want, "|") {
		t.Fatalf("lines = %q, want %q", got, want)
	}
	if resp.GetContainer() != "app" || resp.GetTruncated() {
		t.Fatalf("container = %q, truncated = %t", resp.GetContainer(), resp.GetTruncated())
	}
	if got := resp.GetLines()[0].GetTime().AsTime(); !got.Equal(time.Date(2026, 10, 19, 12, 0, 0, 5e8, time.UTC)) {
		t.Fatalf("first time = %v", got)
	}
	if resp.GetLines()[2].GetTime() != nil {
		t.Fatalf("a line with no timestamp got one: %v", resp.GetLines()[2].GetTime())
	}
	calls := logs.opts(t)
	if len(calls) != 1 {
		t.Fatalf("Stream called %d times", len(calls))
	}
	o := calls[0]
	if o.Container != "app" || !o.Timestamps || o.Previous || *o.TailLines != 50 ||
		o.SinceTime == nil || !o.SinceTime.Equal(&metav1.Time{Time: since}) ||
		*o.LimitBytes != maxLogTransferBytes+1 {
		t.Fatalf("options = %+v", o)
	}
}

func TestPodLogChoosesTheContainerKubectlWould(t *testing.T) {
	t.Parallel()
	ann := map[string]string{defaultContainerAnnotation: "app"}
	stale := map[string]string{defaultContainerAnnotation: "renamed"}
	srv := newLogServer(t, textLogs("x\n"),
		logPod("annotated", ann, nil, "istio-proxy", "app"),
		logPod("stale", stale, nil, "first", "second"),
		logPod("plain", nil, nil, "first", "second"))
	for pod, want := range map[string]string{"annotated": "app", "stale": "first", "plain": "first"} {
		resp, err := srv.PodLog(context.Background(), logReq(pod))
		if err != nil {
			t.Fatalf("%s: %v", pod, err)
		}
		if resp.GetContainer() != want {
			t.Fatalf("%s: container = %q, want %q", pod, resp.GetContainer(), want)
		}
	}
	req := logReq("plain")
	req.Container = "second"
	resp, err := srv.PodLog(context.Background(), req)
	if err != nil || resp.GetContainer() != "second" {
		t.Fatalf("named container: %v, %q", err, resp.GetContainer())
	}
}

// A container with no log is no lines, not an error, and the kubelet is not
// asked: it would answer 400, failing a LATERAL query over many pods.
func TestPodLogWithNoLogIsEmpty(t *testing.T) {
	t.Parallel()
	waiting := corev1.ContainerStatus{Name: "app", State: corev1.ContainerState{Waiting: &corev1.ContainerStateWaiting{}}}
	crashLooping := waiting
	crashLooping.LastTerminationState = corev1.ContainerState{Terminated: terminated("containerd://old")}
	tests := []struct {
		name     string
		statuses []corev1.ContainerStatus
		previous bool
		wantRead bool
	}{
		{"never restarted, previous", []corev1.ContainerStatus{running("app")}, true, false},
		{"not started", []corev1.ContainerStatus{waiting}, false, false},
		{"no status yet", []corev1.ContainerStatus{}, false, false},
		{"crash looping", []corev1.ContainerStatus{crashLooping}, false, true},
		{"crash looping, previous", []corev1.ContainerStatus{crashLooping}, true, true},
		{"terminated without an id", []corev1.ContainerStatus{{Name: "app", State: corev1.ContainerState{Terminated: terminated("")}}}, false, false},
		{"terminated without an id, after an instance that ran", []corev1.ContainerStatus{{
			Name:                 "app",
			State:                corev1.ContainerState{Terminated: terminated("")},
			LastTerminationState: corev1.ContainerState{Terminated: terminated("containerd://old")},
		}}, false, true},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			logs := textLogs("line\n")
			srv := newLogServer(t, logs, logPod("web", nil, tc.statuses, "app"))
			req := logReq("web")
			req.Previous = tc.previous
			resp, err := srv.PodLog(context.Background(), req)
			if err != nil {
				t.Fatal(err)
			}
			read := len(logs.opts(t)) == 1
			if read != tc.wantRead || (len(resp.GetLines()) > 0) != tc.wantRead {
				t.Fatalf("read = %t, lines = %d, want read = %t", read, len(resp.GetLines()), tc.wantRead)
			}
			if read && logs.opts(t)[0].Previous != tc.previous {
				t.Fatalf("previous not passed through")
			}
		})
	}
}

// The newest bytes are kept, because the end of a log is where a crash is.
func TestPodLogKeepsTheNewestBytes(t *testing.T) {
	t.Parallel()
	var b strings.Builder
	for _, s := range []string{"aaaa", "bbbb", "cccc", "dddd"} {
		b.WriteString("2026-10-19T12:00:00Z " + s + "\n")
	}
	srv := newLogServer(t, textLogs(b.String()), logPod("web", nil, nil, "app"))
	req := logReq("web")
	req.MaxBytes = 9
	resp, err := srv.PodLog(context.Background(), req)
	if err != nil {
		t.Fatal(err)
	}
	if got := strings.Join(texts(resp), "|"); got != "cccc|dddd" || !resp.GetTruncated() {
		t.Fatalf("lines = %q, truncated = %t; want cccc|dddd, true", got, resp.GetTruncated())
	}
}

// One line over the limit is returned cut, not dropped, and the cut does not
// split a character.
func TestPodLogCutsALineLongerThanTheLimit(t *testing.T) {
	t.Parallel()
	long := strings.Repeat("é", 40) // 80 bytes
	srv := newLogServer(t, textLogs("2026-10-19T12:00:00Z "+long+"\n"), logPod("web", nil, nil, "app"))
	req := logReq("web")
	req.MaxBytes = 11
	resp, err := srv.PodLog(context.Background(), req)
	if err != nil {
		t.Fatal(err)
	}
	if got := texts(resp); len(got) != 1 || got[0] != strings.Repeat("é", 5) || !resp.GetTruncated() {
		t.Fatalf("lines = %q, truncated = %t", got, resp.GetTruncated())
	}
}

// A line far longer than the read buffer is cut while it is read, so the
// gateway never holds more of it than the reply can carry.
func TestPodLogCutsAHugeLineWhileReading(t *testing.T) {
	t.Parallel()
	huge := "2026-10-19T12:00:00Z before\n2026-10-19T12:00:01Z " + strings.Repeat("x", 1<<20) + "\n"
	srv := newLogServer(t, textLogs(huge), logPod("web", nil, nil, "app"))
	req := logReq("web")
	req.MaxBytes = 100
	resp, err := srv.PodLog(context.Background(), req)
	if err != nil {
		t.Fatal(err)
	}
	// The cut line fills the budget on its own, so the older line goes.
	got := texts(resp)
	if len(got) != 1 || got[0] != strings.Repeat("x", 100) || !resp.GetTruncated() {
		t.Fatalf("lines = %d (first %d bytes), truncated = %t", len(got), len(got[0]), resp.GetTruncated())
	}
	if resp.GetLines()[0].GetTime() == nil {
		t.Fatal("the cut line lost its timestamp")
	}
}

func TestPodLogMakesTextSafeForPostgres(t *testing.T) {
	t.Parallel()
	srv := newLogServer(t, textLogs("2026-10-19T12:00:00Z ok\xff\x00done\n"), logPod("web", nil, nil, "app"))
	resp, err := srv.PodLog(context.Background(), logReq("web"))
	if err != nil {
		t.Fatal(err)
	}
	if got := texts(resp); len(got) != 1 || got[0] != "ok�done" {
		t.Fatalf("lines = %q", got)
	}
}

func TestPodLogRefusesAWindowOverTheTransferCeiling(t *testing.T) {
	t.Parallel()
	line := "2026-10-19T12:00:00Z " + strings.Repeat("x", 1000) + "\n"
	logs := &fakeLogs{body: func() io.Reader {
		return io.LimitReader(repeatReader(line), maxLogTransferBytes+1)
	}}
	srv := newLogServer(t, logs, logPod("web", nil, nil, "app"))
	_, err := srv.PodLog(context.Background(), logReq("web"))
	if status.Code(err) != codes.ResourceExhausted {
		t.Fatalf("err = %v, want RESOURCE_EXHAUSTED", err)
	}
}

// The kubelet reports an instance it has already removed in the body of a
// successful response. That is no log, not a line of one.
func TestPodLogTreatsAnInstanceTheKubeletLostAsNoLog(t *testing.T) {
	t.Parallel()
	crashed := corev1.ContainerStatus{
		Name:                 "app",
		State:                corev1.ContainerState{Terminated: terminated("containerd://new")},
		LastTerminationState: corev1.ContainerState{Terminated: terminated("containerd://old")},
	}
	srv := newLogServer(t, textLogs("unable to retrieve container logs for containerd://old"),
		logPod("web", nil, []corev1.ContainerStatus{crashed}, "app"))
	req := logReq("web")
	req.Previous = true
	resp, err := srv.PodLog(context.Background(), req)
	if err != nil {
		t.Fatal(err)
	}
	if len(resp.GetLines()) != 0 || resp.GetTruncated() {
		t.Fatalf("lines = %q, truncated = %t; want no lines", texts(resp), resp.GetTruncated())
	}
	// Recognised before max_bytes cuts it, or the cut copy would leak through.
	req.MaxBytes = 20
	resp, err = srv.PodLog(context.Background(), req)
	if err != nil || len(resp.GetLines()) != 0 {
		t.Fatalf("with max_bytes 20: %v, lines = %q; want no lines", err, texts(resp))
	}

	// A real line that happens to say the same thing carries a timestamp.
	srv = newLogServer(t, textLogs("2026-10-19T12:00:00Z unable to retrieve container logs for x\n"),
		logPod("web", nil, nil, "app"))
	resp, err = srv.PodLog(context.Background(), logReq("web"))
	if err != nil || len(resp.GetLines()) != 1 {
		t.Fatalf("a timestamped line was taken for the kubelet's error: %v, %q", err, texts(resp))
	}
}

func TestPodLogMapsAPIErrors(t *testing.T) {
	t.Parallel()
	logs := &fakeLogs{err: apierrors.NewBadRequest(`container "app" in pod "web" is waiting to start`)}
	srv := newLogServer(t, logs, logPod("web", nil, nil, "app"))
	_, err := srv.PodLog(context.Background(), logReq("web"))
	if status.Code(err) != codes.InvalidArgument || !strings.Contains(err.Error(), "waiting to start") {
		t.Fatalf("err = %v, want INVALID_ARGUMENT carrying the API server's reason", err)
	}
}

// repeatReader yields s forever.
type repeatReader string

func (r repeatReader) Read(p []byte) (int, error) {
	n := 0
	for n < len(p) {
		n += copy(p[n:], r)
	}
	return n, nil
}
