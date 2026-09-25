# Build:  docker build -f crates/tsc-gateway/Dockerfile -t tsgw .
# Run:    docker run -v ./tsgw.toml:/etc/tsgw/tsgw.toml -p 7788:7788 tsgw
FROM rust:1.94-bookworm AS build
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends cmake libasound2-dev && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --release --locked -p tsc-gateway && cp target/release/tsgw /tsgw

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /tsgw /usr/local/bin/tsgw
ENV TSGW_CONFIG=/etc/tsgw/tsgw.toml
WORKDIR /var/lib/tsgw
EXPOSE 7788
ENTRYPOINT ["/usr/local/bin/tsgw"]
