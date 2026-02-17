FROM rust:1.85-bookworm AS builder

WORKDIR /app
COPY . .

# Build only the headless server (no Tauri deps required).
RUN cargo build -p clawdorio-server --release

FROM debian:bookworm-slim

RUN useradd -m -u 10001 clawdorio
USER clawdorio

WORKDIR /home/clawdorio
COPY --from=builder /app/target/release/clawdorio-server /usr/local/bin/clawdorio-server

ENV CLAWDORIO_DB=/home/clawdorio/data/clawdorio.db
EXPOSE 39333

CMD ["clawdorio-server", "--host", "0.0.0.0", "--port", "39333"]
