// Package coverflush writes a coverage-instrumented gateway's counters the
// moment it is told to stop. It is imported only by the e2e suite's gateway
// image (cmd/gateway's axiomcover-tagged file, set by gateway/Dockerfile with
// COVER=1), never by a release, and it is its own package so the coverage
// report can leave it out of the figures it produces (#90).
//
// A binary built with -cover writes its counters when it exits. A Pod stopped
// at the end of its grace period never exits -- one still serving a watch
// stream is killed rather than drained -- so everything it executed would be
// missing from the report. Writing the counters as soon as the termination
// signal arrives means every Pod that is stopped at all leaves its data behind.
// A normal exit afterwards writes them again, which the merge tolerates.
package coverflush

import (
	"fmt"
	"os"
	"os/signal"
	"runtime/coverage"
	"syscall"
)

func init() {
	dir := os.Getenv("GOCOVERDIR")
	if dir == "" {
		return
	}
	ch := make(chan os.Signal, 1)
	signal.Notify(ch, syscall.SIGTERM, syscall.SIGINT)
	go func() {
		<-ch
		if err := coverage.WriteMetaDir(dir); err != nil {
			fmt.Fprintf(os.Stderr, "axiom: coverage metadata not written: %v\n", err)
			return
		}
		if err := coverage.WriteCountersDir(dir); err != nil {
			fmt.Fprintf(os.Stderr, "axiom: coverage counters not written: %v\n", err)
		}
	}()
}
