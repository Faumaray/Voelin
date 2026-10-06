# Build:  docker build -f crates/voelin-gateway/Dockerfile -t tsgw .
# Run:    docker run -d --name tsgw --init --restart unless-stopped \
#           -v ./tsgw.toml:/etc/tsgw/tsgw.toml:ro -v tsgw-data:/var/lib/tsgw \
#           -e TSGW_QUERY_PASSWORD=… -p 7788:7788 tsgw
# Logs:   docker logs -f tsgw (more: -e TSGW_LOG_LEVEL=debug; also to a file in
#         the volume: -e TSGW_LOG_FILE=logs/tsgw.log). See docs/gateway-admin.md.
FROM rust:1.94-bookworm AS build
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends cmake libasound2-dev && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --release --locked -p voelin-gateway && cp target/release/tsgw /tsgw && mkdir /data

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /tsgw /usr/local/bin/tsgw
# The database (and a log file, if set) live here; owned by the image's user
# (nonroot, 65532), so a new volume mounted here is writable too.
COPY --from=build --chown=65532:65532 /data /var/lib/tsgw
ENV TSGW_CONFIG=/etc/tsgw/tsgw.toml
WORKDIR /var/lib/tsgw
EXPOSE 7788
ENTRYPOINT ["/usr/local/bin/tsgw"]
