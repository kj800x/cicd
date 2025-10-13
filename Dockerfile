# Build Stage
FROM rust:1.90-alpine AS builder
WORKDIR /usr/src/
# Install required build dependencies
RUN apk add --no-cache musl-dev pkgconfig openssl-dev openssl-libs-static gcc g++ make

# - Install dependencies
WORKDIR /usr/src
RUN USER=root cargo new cicd
WORKDIR /usr/src/cicd
COPY Cargo.toml Cargo.lock ./
RUN cargo build --release

# - Copy source
COPY src ./src
RUN touch src/main.rs && cargo build --release

# ---- Runtime Stage ----
FROM alpine:latest AS runtime
COPY --from=builder /usr/src/cicd/target/release/cicd /usr/local/bin/cicd
USER 1000
CMD ["cicd"]
