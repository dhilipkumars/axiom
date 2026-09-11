# Build context: repository root (needs gateway/ only, kept root-relative so
# both images build with the same `docker compose build`).
FROM golang:1.26-bookworm AS build
WORKDIR /src
COPY gateway/go.mod gateway/go.sum ./
RUN --mount=type=cache,target=/go/pkg/mod go mod download
COPY gateway/ ./
ARG VERSION=dev
RUN --mount=type=cache,target=/go/pkg/mod --mount=type=cache,target=/root/.cache/go-build \
    CGO_ENABLED=0 go build -trimpath -ldflags "-s -w -X main.version=${VERSION}" -o /out/gateway ./cmd/gateway

# Least privilege: static binary, no shell, non-root (docs/RULES.md §3).
FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /out/gateway /gateway
USER nonroot:nonroot
EXPOSE 8443
ENTRYPOINT ["/gateway"]
