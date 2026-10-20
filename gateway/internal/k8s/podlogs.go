package k8s

import (
	"context"
	"io"

	corev1 "k8s.io/api/core/v1"
	corev1client "k8s.io/client-go/kubernetes/typed/core/v1"
	"k8s.io/client-go/rest"
)

// LogSource streams one container's log (issue #105).
//
// It is separate from Client because a log is a subresource that is neither
// JSON nor a kind, so the dynamic client cannot read it, and because most
// gateways do not serve logs at all: a nil LogSource is how the server knows
// it was started without --serve-pod-logs.
type LogSource interface {
	// Stream opens the log of one container of namespace/pod as opts select
	// it. The caller closes the reader. Errors are apimachinery StatusErrors,
	// as for Client.
	Stream(ctx context.Context, namespace, pod string, opts *corev1.PodLogOptions) (io.ReadCloser, error)
}

// PodLogs is a LogSource over the typed core/v1 client. Only that one typed
// client is built, rather than the whole clientset, which would link every
// API group's client into the gateway for the sake of one call.
type PodLogs struct {
	pods corev1client.PodsGetter
}

// NewPodLogs builds a LogSource from REST config.
func NewPodLogs(cfg *rest.Config) (*PodLogs, error) {
	c, err := corev1client.NewForConfig(cfg)
	if err != nil {
		return nil, err
	}
	return &PodLogs{pods: c}, nil
}

// Stream implements LogSource.
func (p *PodLogs) Stream(ctx context.Context, namespace, pod string, opts *corev1.PodLogOptions) (io.ReadCloser, error) {
	return p.pods.Pods(namespace).GetLogs(pod, opts).Stream(ctx)
}
