package server

import (
	"bufio"
	"bytes"
	"context"
	"errors"
	"io"
	"log/slog"
	"regexp"
	"strings"
	"time"
	"unicode/utf8"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/types/known/timestamppb"
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/runtime/schema"

	axiomv1 "github.com/dhilipkumars/axiom/gateway/gen/axiom/v1"
	"github.com/dhilipkumars/axiom/gateway/internal/k8s"
)

// Pod log bounds (issue #105).
//
// MaxLogBytes is the most log text one reply carries. It is half the message
// limit because each line also carries a timestamp and framing, and
// maxLogTailLines bounds how many lines that overhead is paid on: 100,000
// lines at a few dozen bytes each is a few MiB, so text and overhead together
// stay well under MaxMessageBytes.
//
// maxLogTransferBytes bounds what the gateway reads from the API server for
// one call. The newest max_bytes are kept, which means reading the whole
// selected window, so the window itself needs a ceiling; a window larger than
// this is refused rather than served from its oldest part.
const (
	MaxLogBytes         = 8 << 20
	maxLogTailLines     = 100_000
	maxLogTransferBytes = 64 << 20
	// The longest timestamp prefix the container runtime writes with
	// timestamps=true: RFC 3339 with nanoseconds and a numeric offset, plus
	// the separating space. A line is read up to max_bytes beyond it.
	maxTimestampPrefix = len("2006-01-02T15:04:05.999999999-07:00 ")
	// defaultContainerAnnotation is how a pod names the container `kubectl
	// logs` reads when none is given. The API server does not honour it.
	defaultContainerAnnotation = "kubectl.kubernetes.io/default-container"
)

// dns1123Label matches a valid container name.
var dns1123Label = regexp.MustCompile(`^[a-z0-9]([-a-z0-9]*[a-z0-9])?$`)

var corePodGVK = schema.GroupVersionKind{Version: "v1", Kind: "Pod"}

// ServePodLogs turns on PodLog, reading through src. A gateway that never
// calls this refuses every PodLog with FAILED_PRECONDITION: logs are off
// unless the operator asked for them (--serve-pod-logs).
func (s *Server) ServePodLogs(src k8s.LogSource) {
	s.logs = src
}

// PodLog reads the newest lines of one container's log.
//
// Contract: see axiom.proto. Validates every field before any cluster call,
// then reads the pod (to choose a container and to tell "no log yet" from an
// error) and its log. Logs one line per call naming the pod and container,
// never the log text. No side effects on the cluster.
func (s *Server) PodLog(ctx context.Context, req *axiomv1.PodLogRequest) (*axiomv1.PodLogResponse, error) {
	if req == nil {
		return nil, status.Error(codes.InvalidArgument, "pod log: request must not be nil")
	}
	if s.logs == nil {
		return nil, status.Error(codes.FailedPrecondition,
			"pod log: this gateway does not serve pod logs; start it with --serve-pod-logs")
	}
	if req.GetNamespace() == "" || req.GetPod() == "" {
		return nil, status.Error(codes.InvalidArgument, "pod log: namespace and pod are required")
	}
	if err := validName("namespace", req.GetNamespace()); err != nil {
		return nil, err
	}
	if err := validName("pod", req.GetPod()); err != nil {
		return nil, err
	}
	if c := req.GetContainer(); c != "" && (len(c) > 63 || !dns1123Label.MatchString(c)) {
		return nil, status.Errorf(codes.InvalidArgument, "pod log: container %q is not a valid container name", c)
	}
	if n := req.GetTailLines(); n <= 0 || n > maxLogTailLines {
		return nil, status.Errorf(codes.InvalidArgument,
			"pod log: tail_lines must be between 1 and %d, got %d", maxLogTailLines, n)
	}
	if n := req.GetMaxBytes(); n <= 0 || n > MaxLogBytes {
		return nil, status.Errorf(codes.InvalidArgument,
			"pod log: max_bytes must be between 1 and %d, got %d", MaxLogBytes, n)
	}
	if req.GetSinceTime() != nil {
		if err := req.GetSinceTime().CheckValid(); err != nil {
			return nil, status.Errorf(codes.InvalidArgument, "pod log: since_time: %v", err)
		}
	}

	pod, err := s.readPod(ctx, req.GetNamespace(), req.GetPod())
	if err != nil {
		return nil, err
	}
	container, err := chooseContainer(pod, req.GetContainer())
	if err != nil {
		return nil, err
	}
	resp := &axiomv1.PodLogResponse{Container: container}
	if !hasLog(pod, container, req.GetPrevious()) {
		s.logPodLog(ctx, req, container, resp, "no_log")
		return resp, nil
	}

	tail := req.GetTailLines()
	limit := int64(maxLogTransferBytes + 1)
	opts := &corev1.PodLogOptions{
		Container:  container,
		Previous:   req.GetPrevious(),
		Timestamps: true,
		TailLines:  &tail,
		// One byte past the ceiling, so a window that is exactly at it can be
		// told from one that is over.
		LimitBytes: &limit,
	}
	if st := req.GetSinceTime(); st != nil {
		opts.SinceTime = &metav1.Time{Time: st.AsTime()}
	}
	rc, err := s.logs.Stream(ctx, req.GetNamespace(), req.GetPod(), opts)
	if err != nil {
		return nil, toGRPC(err)
	}
	// A read-only stream that has been read to the end or abandoned: a close
	// error has nothing left to report.
	defer func() { _ = rc.Close() }()

	buf := newTailBuffer(int(req.GetMaxBytes()))
	if err := readLogLines(rc, int(req.GetMaxBytes())+maxTimestampPrefix, buf.add); err != nil {
		return nil, err
	}
	if kubeletLostTheLog(buf.lines) {
		// The instance was removed after the status said it existed: the
		// kubelet keeps one exited instance per container, so a crash-looping
		// container's `previous` is usually gone while the latest crash waits
		// out its back-off. Like a log that never existed, that is no lines.
		s.logPodLog(ctx, req, container, resp, "gone")
		return resp, nil
	}
	resp.Lines, resp.Truncated = buf.lines, buf.truncated
	s.logPodLog(ctx, req, container, resp, "read")
	return resp, nil
}

func (s *Server) logPodLog(ctx context.Context, req *axiomv1.PodLogRequest, container string, resp *axiomv1.PodLogResponse, outcome string) {
	s.log.LogAttrs(ctx, slog.LevelInfo, "pod_log",
		slog.String("namespace", req.GetNamespace()),
		slog.String("pod", req.GetPod()),
		slog.String("container", container),
		slog.Bool("previous", req.GetPrevious()),
		slog.String("outcome", outcome),
		slog.Int("lines", len(resp.GetLines())),
		slog.Bool("truncated", resp.GetTruncated()))
}

// readPod fetches one pod through the same read path as a scan: a List
// narrowed to the name, so the pods kind has to be served for logs to be, and
// `list` stays the one verb the read path needs.
func (s *Server) readPod(ctx context.Context, namespace, name string) (*corev1.Pod, error) {
	list, err := s.k8s.List(ctx, corePodGVK, namespace, name, 1, "")
	if err != nil {
		return nil, toGRPC(err)
	}
	// Matched by name rather than trusting the selector: a server that ignored
	// it would otherwise hand back some other pod, and its log with it.
	for i := range list.Items {
		if list.Items[i].GetName() != name {
			continue
		}
		var pod corev1.Pod
		if err := runtime.DefaultUnstructuredConverter.FromUnstructured(list.Items[i].Object, &pod); err != nil {
			return nil, status.Errorf(codes.Internal, "pod log: decoding pod %s/%s: %v", namespace, name, err)
		}
		return &pod, nil
	}
	return nil, status.Errorf(codes.NotFound, "pod log: pod %s/%s not found", namespace, name)
}

// chooseContainer returns the container to read. A requested name must be
// one of the pod's containers, init or ephemeral containers. An empty request
// is resolved as `kubectl logs` resolves it: the default-container annotation
// when it names a container, otherwise the first app container.
func chooseContainer(pod *corev1.Pod, requested string) (string, error) {
	names := containerNames(pod)
	has := func(n string) bool {
		for _, c := range names {
			if c == n {
				return true
			}
		}
		return false
	}
	if requested != "" {
		if !has(requested) {
			return "", status.Errorf(codes.InvalidArgument, "pod log: pod %s/%s has no container %q; it has %s",
				pod.Namespace, pod.Name, requested, strings.Join(names, ", "))
		}
		return requested, nil
	}
	if d := pod.Annotations[defaultContainerAnnotation]; d != "" && has(d) {
		return d, nil
	}
	if len(pod.Spec.Containers) == 0 {
		return "", status.Errorf(codes.InvalidArgument, "pod log: pod %s/%s has no containers", pod.Namespace, pod.Name)
	}
	return pod.Spec.Containers[0].Name, nil
}

func containerNames(pod *corev1.Pod) []string {
	var names []string
	for _, c := range pod.Spec.Containers {
		names = append(names, c.Name)
	}
	for _, c := range pod.Spec.InitContainers {
		names = append(names, c.Name)
	}
	for _, c := range pod.Spec.EphemeralContainers {
		names = append(names, c.Name)
	}
	return names
}

// hasLog reports whether the kubelet has the requested log at all, using the
// same rules it applies before serving one. Without this, a container that
// has not started, or a `previous` read of one that never restarted, is a
// 400 from the API server, and one such pod would fail a query over many.
// Those have no log, so the answer is no lines. A race the status has not
// caught up with still reaches the kubelet and fails as before.
func hasLog(pod *corev1.Pod, container string, previous bool) bool {
	st := containerStatus(pod, container)
	if st == nil {
		return false
	}
	last := st.LastTerminationState.Terminated
	if previous {
		return last != nil && last.ContainerID != ""
	}
	switch {
	case st.State.Running != nil:
		return true
	case st.State.Terminated != nil:
		return st.State.Terminated.ContainerID != ""
	default:
		// Waiting: a crash-looping container's log is its last instance's.
		return last != nil && last.ContainerID != ""
	}
}

func containerStatus(pod *corev1.Pod, container string) *corev1.ContainerStatus {
	for _, list := range [][]corev1.ContainerStatus{
		pod.Status.ContainerStatuses, pod.Status.InitContainerStatuses, pod.Status.EphemeralContainerStatuses,
	} {
		for i := range list {
			if list[i].Name == container {
				return &list[i]
			}
		}
	}
	return nil
}

// kubeletLostTheLog reports whether a reply is the kubelet saying it could
// not read the container's log rather than the log itself.
//
// The kubelet reports that in the body of a successful response, as one line
// of text, so it would otherwise reach SQL as a log line. It is recognisable
// because every real line carries a timestamp when timestamps are requested,
// and this one does not.
func kubeletLostTheLog(lines []*axiomv1.LogLine) bool {
	return len(lines) == 1 && lines[0].GetTime() == nil &&
		strings.HasPrefix(lines[0].GetText(), "unable to retrieve container logs for ")
}

// readLogLines splits a timestamped log stream into lines and hands each to
// emit. A line is read up to lineCap bytes; the rest of it is discarded, and
// the line is passed on marked as cut, so one enormous line cannot grow the
// gateway's memory past the reply it is building. More than
// maxLogTransferBytes in total is refused.
func readLogLines(r io.Reader, lineCap int, emit func(line *axiomv1.LogLine, cut bool)) error {
	br := bufio.NewReaderSize(r, 64<<10)
	var cur []byte
	cut := false
	total := 0
	for {
		chunk, err := br.ReadSlice('\n')
		total += len(chunk)
		if total > maxLogTransferBytes {
			return status.Errorf(codes.ResourceExhausted,
				"pod log: the requested window is over %d bytes, more than the gateway reads for one call; "+
					"ask for fewer tail_lines or a later since", maxLogTransferBytes)
		}
		ended := err == nil
		data := chunk
		if ended {
			data = chunk[:len(chunk)-1]
		}
		if room := lineCap - len(cur); len(data) > room {
			cur = append(cur, data[:max(room, 0)]...)
			cut = true
		} else {
			cur = append(cur, data...)
		}
		switch {
		case ended:
			emit(parseLogLine(cur), cut)
			cur, cut = cur[:0], false
		case errors.Is(err, bufio.ErrBufferFull):
			// The line continues past the buffer; keep reading it.
		case errors.Is(err, io.EOF):
			// A last line with no newline: the container is mid-write, or
			// the API server stopped at LimitBytes.
			if len(cur) > 0 {
				emit(parseLogLine(cur), cut)
			}
			return nil
		default:
			return status.Errorf(codes.Unavailable, "pod log: reading the log stream: %v", err)
		}
	}
}

// parseLogLine splits the runtime's timestamp from a line and makes the text
// safe for a Postgres text value. A line whose prefix is not a timestamp is
// kept whole, with no time, rather than dropped.
func parseLogLine(raw []byte) *axiomv1.LogLine {
	line := &axiomv1.LogLine{}
	text := raw
	if i := bytes.IndexByte(raw, ' '); i > 0 {
		if t, err := time.Parse(time.RFC3339Nano, string(raw[:i])); err == nil {
			line.Time = timestamppb.New(t)
			text = raw[i+1:]
		}
	}
	s := strings.ToValidUTF8(string(text), "�")
	line.Text = strings.ReplaceAll(s, "\x00", "")
	return line
}

// tailBuffer keeps the newest lines whose text fits in max bytes.
type tailBuffer struct {
	lines     []*axiomv1.LogLine
	bytes     int
	max       int
	truncated bool
}

func newTailBuffer(maxBytes int) *tailBuffer { return &tailBuffer{max: maxBytes} }

// add appends a line, cutting it to max bytes if it alone is over, then drops
// the oldest lines until the total fits. Either is recorded as truncation.
func (b *tailBuffer) add(line *axiomv1.LogLine, cut bool) {
	if cut {
		b.truncated = true
	}
	if len(line.Text) > b.max {
		line.Text = cutUTF8(line.Text, b.max)
		b.truncated = true
	}
	b.lines = append(b.lines, line)
	b.bytes += len(line.Text)
	for b.bytes > b.max && len(b.lines) > 1 {
		b.bytes -= len(b.lines[0].Text)
		b.lines[0] = nil
		b.lines = b.lines[1:]
		b.truncated = true
	}
}

// cutUTF8 returns the longest prefix of s no longer than n bytes that does not
// split a character.
func cutUTF8(s string, n int) string {
	if len(s) <= n {
		return s
	}
	for n > 0 && !utf8.RuneStart(s[n]) {
		n--
	}
	return s[:n]
}
